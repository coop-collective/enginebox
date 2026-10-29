//! Catalog HTTP operations (docs/CONNECTIONS.md): connections, attaching them
//! to profiles, the audit log, and declarative apply ("profiles as code").
//!
//! Every route here is owner-token only: service tokens (including OAuth
//! access tokens) are refused before they reach it, and the dashboard checks
//! that the member is an org owner or admin before it forwards a write. Each
//! write is audited with names only.
use crate::{audit::Audit, connections::Connections, profiles::Profiles};
use anyhow::{Context, Result, bail, ensure};
use enginebox_agents::profile::{self, Profile};
use serde_json::{Value, json};
use std::{collections::BTreeSet, sync::Arc};

/// The catalog file format this release reads.
pub const SCHEMA: &str = "enginebox.catalog/v1";
/// Header naming the member the dashboard acts for (owner token only).
pub const ACTOR_HEADER: &str = "x-enginebox-actor";
/// Operator seed file applied to the platform org on activation.
pub const SEED_ENV: &str = "ENGINEBOX_CATALOG_SEED";
pub const SEED_PRUNE_ENV: &str = "ENGINEBOX_CATALOG_SEED_PRUNE";

#[derive(Clone)]
pub struct Catalog {
    pub connections: Connections,
    pub profiles: Profiles,
    pub audit: Audit,
    probe: Arc<dyn crate::connections::HttpProbe + Send + Sync>,
}

/// A refusal message safe to return: built from our own messages, never from
/// a deserializer (which can quote the offending input, a secret included).
pub fn message(error: &anyhow::Error) -> String {
    if error.chain().any(|e| e.is::<serde_json::Error>()) {
        "Request fields have the wrong shape. Check field names and types.".into()
    } else {
        error.to_string()
    }
}

fn status_of(error: &anyhow::Error) -> u16 {
    let text = error.to_string();
    if text.starts_with("Unknown connection") || text.starts_with("Unknown profile") {
        404
    } else if text.contains("already exists") || text.contains("concurrently") {
        409
    } else {
        400
    }
}

impl Catalog {
    pub fn new(connections: Connections, profiles: Profiles, audit: Audit) -> Self {
        Self {
            connections,
            profiles,
            audit,
            probe: Arc::new(crate::connections::NetworkProbe),
        }
    }
    pub fn with_probe(
        mut self,
        probe: Arc<dyn crate::connections::HttpProbe + Send + Sync>,
    ) -> Self {
        self.probe = probe;
        self
    }
    /// GET routes this module serves, or `None`.
    pub fn read(&self, org: &str, parts: &[&str]) -> Option<Result<Value>> {
        let used_by = |name: &str| self.profiles.using(org, name);
        Some(match parts {
            ["v1", "connections"] => Ok(self.connections.list(org, &used_by)),
            ["v1", "connections", name] => self.connections.public(org, name, &used_by(name)),
            ["v1", "audit"] => self.audit.list(org, crate::audit::MAX_READ),
            _ => return None,
        })
    }
    /// Profiles of another org are invisible to an org request; the platform
    /// org (no org header, as before orgs) keeps seeing every profile.
    pub fn profile_visible(&self, org: &str, name: &str) -> bool {
        org == enginebox_agents::pools::DEFAULT_ORG
            || self.profiles.org_of(name).as_deref() == Some(org)
    }
    /// Checks before `POST /v1/profiles` stores a profile for `org`: it may
    /// not replace another org's profile, and every connection it names must
    /// exist in the profile's org.
    pub fn check_profile_put(&self, org: &str, body: &Value) -> Result<()> {
        let name = body["name"].as_str().unwrap_or_default();
        if let Some(existing) = self.profiles.org_of(name) {
            ensure!(
                org == enginebox_agents::pools::DEFAULT_ORG || existing == org,
                "A profile with that name belongs to another organization."
            );
        }
        let target = body["org_id"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| self.profiles.org_of(name))
            .unwrap_or_else(|| org.to_owned());
        if let Some(names) = body.get("connections").and_then(Value::as_array) {
            for connection in names {
                let connection = connection.as_str().unwrap_or_default();
                ensure!(
                    self.connections.exists(&target, connection),
                    "The profile names unknown connection {connection}."
                );
            }
        }
        Ok(())
    }
    /// POST routes this module serves: `(status, body)`, or `None`.
    pub fn write(
        &self,
        org: &str,
        actor: &str,
        parts: &[&str],
        body: &Value,
    ) -> Option<(u16, Value)> {
        let (action, target, ok_status) = match parts {
            ["v1", "connections"] => (
                "connection.create",
                body["name"].as_str().unwrap_or("").to_owned(),
                201,
            ),
            ["v1", "connections", name] => ("connection.update", name.to_string(), 200),
            ["v1", "connections", name, "delete"] => ("connection.delete", name.to_string(), 200),
            ["v1", "connections", name, "test"] => ("connection.test", name.to_string(), 200),
            ["v1", "profiles", name, "connections", "attach"] => {
                ("profile.attach_connection", name.to_string(), 200)
            }
            ["v1", "profiles", name, "connections", "detach"] => {
                ("profile.detach_connection", name.to_string(), 200)
            }
            ["v1", "profiles", name, "connections", "policy"] => {
                ("profile.set_policy", name.to_string(), 200)
            }
            ["v1", "catalog", "apply"] => ("catalog.apply", String::new(), 200),
            _ => return None,
        };
        let result = self.dispatch(org, actor, parts, body);
        // Names only: the connection named, secrets set or removed by name.
        let mut detail = json!({});
        if let Some(secrets) = body.get("secrets").and_then(Value::as_object) {
            detail["secrets_set"] = json!(secrets.keys().collect::<Vec<_>>());
        }
        if let Some(removed) = body.get("remove_secrets") {
            detail["secrets_removed"] = removed.clone();
        }
        if let Some(connection) = body.get("connection").and_then(Value::as_str) {
            detail["connection"] = json!(connection);
        }
        match &result {
            Ok(value) if action == "catalog.apply" => {
                detail["dry_run"] = value["dry_run"].clone();
                detail["changes"] = json!(
                    value["changes"]
                        .as_array()
                        .map(|c| c.iter().filter(|c| c["action"] != "unchanged").count())
                        .unwrap_or(0)
                );
            }
            Ok(value) if action == "connection.test" => {
                detail["ok"] = value["test"]["ok"].clone();
            }
            Err(error) => detail["reason"] = json!(message(error)),
            _ => {}
        }
        self.audit
            .record(org, actor, action, &target, result.is_ok(), detail);
        Some(match result {
            Ok(value) => (ok_status, value),
            Err(error) => (status_of(&error), json!({"error": message(&error)})),
        })
    }
    fn dispatch(&self, org: &str, actor: &str, parts: &[&str], body: &Value) -> Result<Value> {
        match parts {
            ["v1", "connections"] => self.connections.create(org, actor, body),
            ["v1", "connections", name] => self.connections.update(org, name, actor, body),
            ["v1", "connections", name, "delete"] => {
                crate::empty(body)?;
                self.connections
                    .delete(org, name, &self.profiles.using(org, name))
            }
            ["v1", "connections", name, "test"] => {
                crate::empty(body)?;
                self.connections.test(org, name, self.probe.as_ref())
            }
            ["v1", "profiles", name, "connections", "attach"] => {
                crate::keys(body, &["connection", "run_kinds"], &["connection"])?;
                let connection = crate::string(body, "connection")?.to_owned();
                let kinds: Option<Vec<String>> = match body.get("run_kinds") {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(
                        serde_json::from_value(v.clone())
                            .context("run_kinds must list run kinds.")?,
                    ),
                };
                self.attach(org, name, &connection, kinds)
            }
            ["v1", "profiles", name, "connections", "detach"] => {
                crate::keys(body, &["connection"], &["connection"])?;
                let connection = crate::string(body, "connection")?.to_owned();
                self.detach(org, name, &connection)
            }
            ["v1", "profiles", name, "connections", "policy"] => {
                crate::keys(body, &["connection_policy"], &["connection_policy"])?;
                let policy: std::collections::BTreeMap<String, Vec<String>> =
                    serde_json::from_value(body["connection_policy"].clone())
                        .context("connection_policy maps run kinds to connection names.")?;
                self.own_profile(org, name)?;
                self.profiles.modify(name, move |p| {
                    p.connection_policy = policy;
                    Ok(())
                })
            }
            ["v1", "catalog", "apply"] => {
                crate::keys(body, &["document", "dry_run", "prune"], &["document"])?;
                let flag = |key: &str| -> Result<bool> {
                    body.get(key)
                        .map(|v| {
                            v.as_bool()
                                .with_context(|| format!("{key} must be a boolean."))
                        })
                        .transpose()
                        .map(|v| v.unwrap_or(false))
                };
                self.apply(
                    org,
                    actor,
                    &body["document"],
                    flag("dry_run")?,
                    flag("prune")?,
                )
            }
            _ => bail!("Unknown catalog operation"),
        }
    }
    fn own_profile(&self, org: &str, name: &str) -> Result<()> {
        ensure!(
            self.profiles.org_of(name).as_deref() == Some(org),
            "Unknown profile"
        );
        Ok(())
    }
    /// Attach a connection to a profile. Without `run_kinds` it applies to
    /// every kind the policy does not restrict (and is added to `*` when that
    /// entry exists). With `run_kinds` it applies only to those kinds: it is
    /// added to each one's list (a new list starts empty), and a `*` entry
    /// holding the profile's other connections is created if none exists, so
    /// other kinds do not gain it.
    pub fn attach(
        &self,
        org: &str,
        name: &str,
        connection: &str,
        kinds: Option<Vec<String>>,
    ) -> Result<Value> {
        self.own_profile(org, name)?;
        ensure!(
            self.connections.exists(org, connection),
            "Unknown connection {connection}."
        );
        if let Some(kinds) = &kinds {
            ensure!(
                !kinds.is_empty() && kinds.iter().all(|k| profile::valid_run_kind(k)),
                "run_kinds must list run kinds matching ^[a-z][a-z0-9_-]{{0,31}}$."
            );
        }
        self.profiles.modify(name, |p| {
            let already = p.connections.iter().any(|c| c == connection);
            match kinds {
                None => {
                    if !already {
                        p.connections.push(connection.to_owned());
                    }
                    if let Some(all) = p.connection_policy.get_mut("*")
                        && !all.iter().any(|c| c == connection)
                    {
                        all.push(connection.to_owned());
                    }
                }
                Some(kinds) => {
                    if !p.connection_policy.contains_key("*") {
                        let others: Vec<String> = p
                            .connections
                            .iter()
                            .filter(|c| *c != connection)
                            .cloned()
                            .collect();
                        p.connection_policy.insert("*".into(), others);
                    }
                    if !already {
                        p.connections.push(connection.to_owned());
                    }
                    for kind in kinds {
                        let list = p.connection_policy.entry(kind).or_default();
                        if !list.iter().any(|c| c == connection) {
                            list.push(connection.to_owned());
                        }
                    }
                }
            }
            Ok(())
        })
    }
    pub fn detach(&self, org: &str, name: &str, connection: &str) -> Result<Value> {
        self.own_profile(org, name)?;
        self.profiles.modify(name, |p| {
            ensure!(
                p.connections.iter().any(|c| c == connection),
                "The profile does not use connection {connection}."
            );
            p.connections.retain(|c| c != connection);
            for list in p.connection_policy.values_mut() {
                list.retain(|c| c != connection);
            }
            Ok(())
        })
    }

    /// `POST /v1/catalog/apply`: make the org's catalog match `document`.
    /// Idempotent: unchanged records are reported and left alone. With
    /// `dry_run` nothing is written. Records the document does not mention
    /// are kept unless `prune`. Everything is validated before the first
    /// write; a later failure (for example a concurrent edit) stops the apply
    /// and a repeat converges.
    pub fn apply(
        &self,
        org: &str,
        actor: &str,
        document: &Value,
        dry_run: bool,
        prune: bool,
    ) -> Result<Value> {
        let doc = document
            .as_object()
            .context("document must be an object.")?;
        ensure!(
            doc.keys()
                .all(|k| ["schema", "connections", "profiles", "$comment"].contains(&k.as_str())),
            "A catalog document has schema, connections and profiles."
        );
        ensure!(
            doc.get("schema").and_then(Value::as_str) == Some(SCHEMA),
            "schema must be {SCHEMA}."
        );
        let list = |key: &str| -> Result<Vec<Value>> {
            Ok(match doc.get(key) {
                None => Vec::new(),
                Some(v) => v
                    .as_array()
                    .with_context(|| format!("{key} must be a list."))?
                    .clone(),
            })
        };
        let connection_specs = list("connections")?;
        let profile_specs = list("profiles")?;
        ensure!(
            connection_specs.len() <= crate::connections::MAX_CONNECTIONS
                && profile_specs.len() <= 256,
            "The document lists too many records."
        );
        let mut changes = Vec::new();

        // 1. Connections: build and validate every desired record first.
        let mut desired_connections = Vec::new();
        let mut doc_connection_names = BTreeSet::new();
        for spec in &connection_specs {
            let object = spec
                .as_object()
                .context("Each connection must be an object.")?;
            ensure!(
                object
                    .keys()
                    .all(|k| crate::connections::SPEC_FIELDS.contains(&k.as_str())),
                "Catalog connections take declarative fields only; secret values never go in a file."
            );
            let name = spec["name"]
                .as_str()
                .context("Each connection needs a name.")?;
            ensure!(
                doc_connection_names.insert(name.to_owned()),
                "The document repeats connection {name}."
            );
            let action = self.connections.plan(org, spec)?;
            desired_connections.push((spec.clone(), action));
        }
        // 2. Profiles: same, against connections that exist or will.
        let known: BTreeSet<String> = self
            .connections
            .names(org)
            .into_iter()
            .chain(doc_connection_names.iter().cloned())
            .collect();
        let mut desired_profiles = Vec::new();
        let mut doc_profile_names = BTreeSet::new();
        for spec in &profile_specs {
            let object = spec
                .as_object()
                .context("Each profile must be an object.")?;
            ensure!(
                object
                    .keys()
                    .all(|k| PROFILE_SPEC_FIELDS.contains(&k.as_str()) || k.starts_with('$')),
                "Catalog profiles take name, description, template, providers, connections and connection_policy."
            );
            let mut clean = spec.clone();
            clean
                .as_object_mut()
                .unwrap()
                .retain(|k, _| !k.starts_with('$'));
            let desired: Profile =
                serde_json::from_value(clean.clone()).context("A profile has the wrong shape.")?;
            ensure!(
                doc_profile_names.insert(desired.name.clone()),
                "The document repeats profile {}.",
                desired.name
            );
            for c in &desired.connections {
                ensure!(
                    known.contains(c),
                    "Profile {} names unknown connection {c}.",
                    desired.name
                );
            }
            let action = match self.profiles.record(&desired.name) {
                None => "create",
                Some(existing) => {
                    let existing_org = existing
                        .org_id
                        .clone()
                        .unwrap_or_else(|| enginebox_agents::pools::DEFAULT_ORG.into());
                    ensure!(
                        existing_org == org,
                        "Profile {} belongs to another organization.",
                        desired.name
                    );
                    if same_profile(&existing, &desired) {
                        "unchanged"
                    } else {
                        "update"
                    }
                }
            };
            let mut candidate = desired.clone();
            candidate.org_id =
                (org != enginebox_agents::pools::DEFAULT_ORG).then(|| org.to_owned());
            self.profiles.check(&candidate)?;
            desired_profiles.push((clean, desired, action));
        }
        // 3. Pruning plan.
        let prune_profiles: Vec<String> = if prune {
            self.profiles
                .names(org)
                .into_iter()
                .filter(|n| !doc_profile_names.contains(n))
                .collect()
        } else {
            Vec::new()
        };
        let prune_connections: Vec<String> = if prune {
            self.connections
                .names(org)
                .into_iter()
                .filter(|n| !doc_connection_names.contains(n))
                .collect()
        } else {
            Vec::new()
        };
        for name in &prune_connections {
            let users: Vec<String> = self
                .profiles
                .using(org, name)
                .into_iter()
                .filter(|p| !prune_profiles.contains(p))
                .filter(|p| {
                    desired_profiles
                        .iter()
                        .find(|(_, d, _)| &d.name == p)
                        .is_none_or(|(_, d, _)| d.connections.contains(name))
                })
                .collect();
            ensure!(
                users.is_empty(),
                "Pruning connection {name} would break profiles {}.",
                users.join(", ")
            );
        }

        for (spec, action) in &desired_connections {
            changes.push(json!({"type":"connection","name":spec["name"],"action":action}));
        }
        for (_, desired, action) in &desired_profiles {
            changes.push(json!({"type":"profile","name":desired.name,"action":action}));
        }
        for name in &prune_profiles {
            changes.push(json!({"type":"profile","name":name,"action":"delete"}));
        }
        for name in &prune_connections {
            changes.push(json!({"type":"connection","name":name,"action":"delete"}));
        }
        if dry_run {
            return Ok(json!({"dry_run": true, "changes": changes}));
        }

        // Writes: connections, profiles, then pruning.
        for (spec, action) in &desired_connections {
            if *action != "unchanged" {
                self.connections.put_spec(org, actor, spec)?;
            }
        }
        for (clean, desired, action) in &desired_profiles {
            match *action {
                "create" => {
                    let mut body = clean.clone();
                    if org != enginebox_agents::pools::DEFAULT_ORG {
                        body["org_id"] = json!(org);
                    }
                    self.profiles.put(body)?;
                }
                "update" => {
                    let desired = desired.clone();
                    // The callback key and bookkeeping are kept; the rest is the document's.
                    self.profiles.modify(&desired.name, move |p| {
                        p.description = desired.description;
                        p.template = desired.template;
                        p.providers = desired.providers;
                        p.connections = desired.connections;
                        p.connection_policy = desired.connection_policy;
                        Ok(())
                    })?;
                }
                _ => {}
            }
        }
        for name in &prune_profiles {
            self.profiles.delete(name)?;
        }
        for name in &prune_connections {
            self.connections
                .delete(org, name, &self.profiles.using(org, name))?;
        }
        Ok(json!({"dry_run": false, "changes": changes}))
    }

    /// Apply the operator's seed file (`ENGINEBOX_CATALOG_SEED`) to the
    /// platform org. Called on activation; a failure is reported, never fatal.
    pub fn seed_from_env(&self) {
        let Some(path) = std::env::var_os(SEED_ENV) else {
            return;
        };
        let prune = std::env::var(SEED_PRUNE_ENV).is_ok_and(|v| v == "1");
        let org = enginebox_agents::pools::DEFAULT_ORG;
        let outcome = (|| -> Result<Value> {
            let path = std::path::PathBuf::from(path);
            ensure!(path.is_absolute(), "{SEED_ENV} must be an absolute path.");
            let meta = std::fs::symlink_metadata(&path)?;
            ensure!(
                meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= 1024 * 1024,
                "The catalog seed must be a regular file of at most 1 MiB."
            );
            let document: Value = serde_json::from_slice(&std::fs::read(&path)?)
                .context("The catalog seed is not JSON.")?;
            self.apply(org, "owner", &document, false, prune)
        })();
        let detail = match &outcome {
            Ok(v) => {
                json!({"changes": v["changes"].as_array().map(|c| c.iter().filter(|c| c["action"] != "unchanged").count()).unwrap_or(0), "prune": prune})
            }
            Err(e) => json!({"reason": message(e)}),
        };
        if let Err(error) = &outcome {
            eprintln!("catalog seed not applied: {}", message(error));
        }
        self.audit
            .record(org, "owner", "catalog.seed", "", outcome.is_ok(), detail);
    }
}

const PROFILE_SPEC_FIELDS: &[&str] = &[
    "name",
    "description",
    "template",
    "providers",
    "connections",
    "connection_policy",
];
fn same_profile(existing: &Profile, desired: &Profile) -> bool {
    existing.description == desired.description
        && existing.template == desired.template
        && existing.providers == desired.providers
        && existing.connections == desired.connections
        && existing.connection_policy == desired.connection_policy
}

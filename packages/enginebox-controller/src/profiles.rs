//! Owner-managed workspace profiles and their content-addressed file store.
//! Profiles are records under `profiles/`; files are blobs under
//! `profile-files/<sha256>`, written once and verified again on every read.
//! Nothing here fetches a URL: files arrive only by owner upload.
use anyhow::{Context, Result, ensure};
use base64::Engine;
use enginebox_agents::{
    profile::{self, Profile},
    security,
    service::ResolvedProfile,
};
use enginebox_core::state;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const MAX_PROFILES: usize = 256;
const MAX_STORED_FILES: usize = 4096;

#[derive(Clone)]
pub struct Profiles {
    records: PathBuf,
    files: PathBuf,
    profiles: Arc<Mutex<BTreeMap<String, Profile>>>,
}
impl Profiles {
    pub fn open(root: &Path) -> Result<Self> {
        let records = root.join("profiles");
        let files = root.join("profile-files");
        state::private_dir(&records)?;
        security::private_dir(&files)?;
        let this = Self {
            records,
            files,
            profiles: Arc::new(Mutex::new(BTreeMap::new())),
        };
        let mut profiles = BTreeMap::new();
        for path in state::json_files(&this.records)? {
            let profile: Profile = serde_json::from_value(state::read_json(&path)?)?;
            ensure!(
                path.file_stem().and_then(|s| s.to_str()) == Some(profile.name.as_str()),
                "Invalid profile record"
            );
            profile.validate(&|sha| this.has_file(sha))?;
            profiles.insert(profile.name.clone(), profile);
        }
        *this.profiles.lock().unwrap() = profiles;
        Ok(this)
    }
    fn has_file(&self, sha: &str) -> bool {
        profile::valid_sha256(sha) && self.files.join(sha).is_file()
    }
    fn read_file(&self, sha: &str) -> Result<Vec<u8>> {
        ensure!(profile::valid_sha256(sha), "Invalid file digest");
        let bytes = security::read_bytes(&self.files, sha, profile::MAX_FILE_BYTES, true)?
            .context("Profile file is missing")?;
        ensure!(
            profile::sha256_hex(&bytes) == sha,
            "Stored profile file failed its digest check"
        );
        Ok(bytes)
    }
    /// Store one uploaded file (`{"content_base64": "..."}`) under its digest.
    pub fn upload(&self, body: &Value) -> Result<Value> {
        let object = body.as_object().context("Expected object")?;
        enginebox_agents::require(
            object.len() == 1 && object.contains_key("content_base64"),
            "Upload accepts only content_base64",
        )?;
        let bytes = base64::engine::general_purpose::STANDARD.decode(
            body["content_base64"]
                .as_str()
                .context("content_base64 must be a string")?,
        )?;
        enginebox_agents::require(
            !bytes.is_empty() && bytes.len() <= profile::MAX_FILE_BYTES,
            "A profile file must contain 1 byte to 1 MiB",
        )?;
        let sha = profile::sha256_hex(&bytes);
        if !self.has_file(&sha) {
            ensure!(
                std::fs::read_dir(&self.files)?.count() < MAX_STORED_FILES,
                "Profile file store is full"
            );
            security::write_bytes(&self.files, &sha, &bytes, profile::MAX_FILE_BYTES)?;
        }
        Ok(json!({"sha256":sha,"size":bytes.len()}))
    }
    /// Create or replace a profile. The owner and bookkeeping fields are set
    /// here; the body may not supply them.
    pub fn put(&self, body: Value) -> Result<Value> {
        let object = body.as_object().context("Expected object")?;
        enginebox_agents::require(
            object.keys().all(|k| {
                [
                    "name",
                    "org_id",
                    "description",
                    "template",
                    "providers",
                    "callback_secret",
                    "connections",
                    "connection_policy",
                ]
                .contains(&k.as_str())
            }),
            "Unsupported profile fields",
        )?;
        let named_org = object.contains_key("org_id");
        let mut profile: Profile = serde_json::from_value(body)?;
        let mut profiles = self.profiles.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        match profiles.get(&profile.name) {
            Some(existing) => {
                // A profile changes org only when a request names one.
                if !named_org {
                    profile.org_id = existing.org_id.clone();
                }
                profile.revision = existing.revision + 1;
                profile.created_at = existing.created_at.clone();
            }
            None => {
                enginebox_agents::require(profiles.len() < MAX_PROFILES, "Profile limit reached")?;
                profile.revision = 1;
                profile.created_at = now.clone();
            }
        }
        profile.owner = profile::OWNER.into();
        profile.updated_at = now;
        profile
            .validate(&|sha| self.has_file(sha))
            .map_err(enginebox_agents::rule)?;
        state::write_json(
            &self.records.join(format!("{}.json", profile.name)),
            &serde_json::to_value(&profile)?,
        )?;
        let public = profile.public();
        profiles.insert(profile.name.clone(), profile);
        Ok(json!({"profile":public}))
    }
    pub fn list(&self) -> Value {
        json!({"profiles":self.profiles.lock().unwrap().values().map(Profile::public).collect::<Vec<_>>()})
    }
    pub fn get(&self, name: &str) -> Result<Value> {
        let profiles = self.profiles.lock().unwrap();
        Ok(json!({"profile":profiles.get(name).context("Unknown profile")?.public()}))
    }
    pub fn delete(&self, name: &str) -> Result<Value> {
        ensure!(profile::valid_name(name), "Invalid profile name");
        let mut profiles = self.profiles.lock().unwrap();
        ensure!(profiles.contains_key(name), "Unknown profile");
        std::fs::remove_file(self.records.join(format!("{name}.json")))?;
        profiles.remove(name);
        Ok(json!({"deleted":name}))
    }
    /// Validate a candidate record against the stored files (dry runs).
    pub fn check(&self, profile: &Profile) -> Result<()> {
        profile.validate(&|sha| self.has_file(sha))
    }
    /// The stored record (internal use: env values and keys included).
    pub fn record(&self, name: &str) -> Option<Profile> {
        self.profiles.lock().unwrap().get(name).cloned()
    }
    /// The org that owns a profile.
    pub fn org_of(&self, name: &str) -> Option<String> {
        self.profiles.lock().unwrap().get(name).map(|p| {
            p.org_id
                .clone()
                .unwrap_or_else(|| enginebox_agents::pools::DEFAULT_ORG.into())
        })
    }
    /// Names of the profiles of `org`.
    pub fn names(&self, org: &str) -> Vec<String> {
        let profiles = self.profiles.lock().unwrap();
        profiles
            .values()
            .filter(|p| {
                p.org_id
                    .as_deref()
                    .unwrap_or(enginebox_agents::pools::DEFAULT_ORG)
                    == org
            })
            .map(|p| p.name.clone())
            .collect()
    }
    /// The profiles of `org` that reference connection `name`.
    pub fn using(&self, org: &str, name: &str) -> Vec<String> {
        let profiles = self.profiles.lock().unwrap();
        profiles
            .values()
            .filter(|p| {
                p.org_id
                    .as_deref()
                    .unwrap_or(enginebox_agents::pools::DEFAULT_ORG)
                    == org
                    && p.connections.iter().any(|c| c == name)
            })
            .map(|p| p.name.clone())
            .collect()
    }
    /// Change one profile in place (attach/detach a connection): validated,
    /// revision bumped and written like a replacement.
    pub fn modify(
        &self,
        name: &str,
        change: impl FnOnce(&mut Profile) -> Result<()>,
    ) -> Result<Value> {
        let mut profiles = self.profiles.lock().unwrap();
        let mut profile = profiles.get(name).cloned().context("Unknown profile")?;
        let before = profile.clone();
        change(&mut profile)?;
        if profile == before {
            return Ok(json!({"profile":profile.public(),"changed":false}));
        }
        profile.revision += 1;
        profile.updated_at = chrono::Utc::now().to_rfc3339();
        profile.validate(&|sha| self.has_file(sha))?;
        state::write_json(
            &self.records.join(format!("{}.json", profile.name)),
            &serde_json::to_value(&profile)?,
        )?;
        let public = profile.public();
        profiles.insert(profile.name.clone(), profile);
        Ok(json!({"profile":public,"changed":true}))
    }
    pub fn exists(&self, name: &str) -> bool {
        self.profiles.lock().unwrap().contains_key(name)
    }
    /// The profile's callback key, for owner-started runs.
    pub fn callback_secret(&self, name: &str) -> Option<String> {
        self.profiles
            .lock()
            .unwrap()
            .get(name)
            .and_then(|p| p.callback_secret.clone())
    }
    /// Resolve a profile for one launch: its block for `provider` and the
    /// bytes of every referenced file, each re-verified against its digest.
    /// `run_kind` selects the connections that apply (the profile's
    /// `connection_policy`); they are merged from `connections` into the block.
    pub fn resolve(
        &self,
        name: &str,
        provider: &str,
        run_kind: Option<&str>,
        connections: &crate::connections::Connections,
    ) -> Result<ResolvedProfile> {
        let profile = self
            .profiles
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .context("Unknown profile")?;
        let mut block = profile
            .providers
            .get(provider)
            .cloned()
            .context("The profile does not configure this provider")?;
        let selected =
            profile::connections_for(&profile.connections, &profile.connection_policy, run_kind);
        let used = if selected.is_empty() {
            Vec::new()
        } else {
            let org = profile
                .org_id
                .as_deref()
                .unwrap_or(enginebox_agents::pools::DEFAULT_ORG);
            connections
                .apply(
                    org,
                    selected,
                    provider,
                    profile.template.as_deref(),
                    &mut block,
                )?
                .used
        };
        let mut files = BTreeMap::new();
        let mut total = 0;
        for sha in block.digests() {
            let bytes = self.read_file(&sha)?;
            total += bytes.len();
            ensure!(
                total <= profile::MAX_BUNDLE_BYTES,
                "Profile files exceed the bundle bound"
            );
            files.insert(
                sha,
                base64::engine::general_purpose::STANDARD.encode(&bytes),
            );
        }
        Ok(ResolvedProfile {
            name: profile.name,
            revision: profile.revision,
            template: profile.template,
            block,
            files,
            run_kind: run_kind.map(str::to_owned),
            connections: used,
        })
    }
}

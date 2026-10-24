//! Durable desired toolchains and node-bound installation observations.
use anyhow::{Context, Result, ensure};
use enginebox_ci::{
    Node,
    templates::{Binding, Receipt, Template, digest},
};
use enginebox_core::state;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub manifest: Template,
    pub enabled: bool,
    pub epoch: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Placement {
    pub template_id: String,
    pub node_id: String,
    pub node_binding: String,
    pub manifest_digest: String,
    pub epoch: String,
    pub attempt: u64,
    pub reinstall: bool,
    pub status: String,
    pub error: Option<String>,
    pub job_id: Option<String>,
    pub receipt: Option<Receipt>,
}
impl Placement {
    pub fn identity(&self) -> String {
        digest(
            &json!({"template":self.template_id,"node":self.node_binding,"digest":self.manifest_digest,"epoch":self.epoch,"attempt":self.attempt}),
        )
    }
    pub fn current(&self, record: &Record, node: &Node) -> bool {
        self.node_binding == digest(node)
            && self.manifest_digest == record.manifest.digest()
            && self.epoch == record.epoch
    }
}
pub struct Registry {
    root: PathBuf,
    pub records: BTreeMap<String, Record>,
    pub placements: BTreeMap<String, Placement>,
}
pub fn index(template: &str, node: &str) -> String {
    digest(&json!([template, node]))
}
impl Registry {
    pub fn open(root: &Path) -> Result<Self> {
        let root = root.join("ci-templates");
        state::private_dir(&root.join("manifests"))?;
        state::private_dir(&root.join("nodes"))?;
        let mut registry = Self {
            root,
            records: BTreeMap::new(),
            placements: BTreeMap::new(),
        };
        for file in state::json_files(&registry.root.join("manifests"))? {
            let record: Record = serde_json::from_value(state::read_json(&file)?)?;
            record.manifest.validate()?;
            ensure!(
                file.file_stem().and_then(|v| v.to_str()) == Some(&record.manifest.id),
                "Invalid template registry filename"
            );
            registry.records.insert(record.manifest.id.clone(), record);
        }
        for file in state::json_files(&registry.root.join("nodes"))? {
            let mut placement: Placement = serde_json::from_value(state::read_json(&file)?)?;
            ensure!(
                file.file_stem().and_then(|v| v.to_str())
                    == Some(&index(&placement.template_id, &placement.node_id)),
                "Invalid template placement filename"
            );
            match placement.status.as_str() {
                "ready" => {
                    placement.status = "pending".into();
                    placement.attempt += 1;
                    placement.reinstall = false;
                    placement.job_id = None;
                }
                "queued" | "installing" => {
                    placement.status = "failed".into();
                    placement.error =
                        Some("controller-restarted-inspect-worker-before-retry".into());
                }
                "pending" | "failed" => {}
                _ => anyhow::bail!("Unknown template installation state"),
            }
            registry.save(placement)?;
        }
        Ok(registry)
    }
    pub fn register(&mut self, manifest: Template) -> Result<()> {
        manifest.validate()?;
        ensure!(
            self.records.len() < 32 || self.records.contains_key(&manifest.id),
            "CI template registry is full"
        );
        if let Some(old) = self.records.get(&manifest.id) {
            if old.manifest.digest() == manifest.digest() {
                return Ok(());
            }
            ensure!(
                manifest.revision > old.manifest.revision,
                "Changed templates require a greater revision"
            );
        }
        // New recipes start disabled; a versioned update preserves desired enablement.
        let enabled = self
            .records
            .get(&manifest.id)
            .is_some_and(|record| record.enabled);
        let record = Record {
            manifest,
            enabled,
            epoch: uuid::Uuid::new_v4().simple().to_string(),
        };
        self.write(record)
    }
    fn write(&mut self, record: Record) -> Result<()> {
        state::write_json(
            &self
                .root
                .join("manifests")
                .join(format!("{}.json", record.manifest.id)),
            &serde_json::to_value(&record)?,
        )?;
        self.records.insert(record.manifest.id.clone(), record);
        Ok(())
    }
    pub fn enable(&mut self, id: &str, enabled: bool) -> Result<()> {
        let mut record = self.records.get(id).context("Unknown CI template")?.clone();
        if record.enabled == enabled {
            return Ok(());
        }
        record.enabled = enabled;
        record.epoch = uuid::Uuid::new_v4().simple().to_string();
        self.write(record)
    }
    pub fn save(&mut self, placement: Placement) -> Result<()> {
        let key = index(&placement.template_id, &placement.node_id);
        state::write_json(
            &self.root.join("nodes").join(format!("{key}.json")),
            &serde_json::to_value(&placement)?,
        )?;
        self.placements.insert(key, placement);
        Ok(())
    }
    pub fn invalidate_node(&mut self, id: &str) -> Result<()> {
        let changed: Vec<_> = self
            .placements
            .values()
            .filter(|p| p.node_id == id)
            .cloned()
            .collect();
        for mut placement in changed {
            // Fence even a later change back to the original registration.
            placement.node_binding.clear();
            placement.status = "pending".into();
            placement.error = None;
            placement.job_id = None;
            placement.receipt = None;
            self.save(placement)?;
        }
        Ok(())
    }
    pub fn pending(&mut self, record: &Record, node: &Node) -> Result<Placement> {
        if let Some(existing) = self.placements.get(&index(&record.manifest.id, &node.id))
            && existing.current(record, node)
        {
            return Ok(existing.clone());
        }
        let placement = Placement {
            template_id: record.manifest.id.clone(),
            node_id: node.id.clone(),
            node_binding: digest(node),
            manifest_digest: record.manifest.digest(),
            epoch: record.epoch.clone(),
            attempt: self
                .placements
                .get(&index(&record.manifest.id, &node.id))
                .map_or(1, |p| p.attempt + 1),
            reinstall: false,
            status: "pending".into(),
            error: None,
            job_id: None,
            receipt: None,
        };
        self.save(placement.clone())?;
        Ok(placement)
    }
    pub fn retry(&mut self, id: &str, node: &Node) -> Result<()> {
        let record = self.records.get(id).context("Unknown CI template")?.clone();
        ensure!(
            record.enabled && record.manifest.compatible(node) && !node.disabled,
            "Template/node must be enabled and compatible"
        );
        let mut placement = self.pending(&record, node)?;
        ensure!(
            placement.status == "failed",
            "Only a failed installation can be retried"
        );
        placement.attempt += 1;
        placement.reinstall = true;
        placement.status = "pending".into();
        placement.error = None;
        placement.job_id = None;
        self.save(placement)
    }
    pub fn binding(&self, id: &str, node: &Node) -> Result<(Binding, String)> {
        let record = self.records.get(id).context("Unknown CI template")?;
        ensure!(
            record.enabled && record.manifest.compatible(node),
            "CI template is disabled or incompatible"
        );
        let placement = self
            .placements
            .get(&index(id, &node.id))
            .context("CI template is not installed")?;
        ensure!(
            placement.current(record, node) && placement.status == "ready",
            "CI toolchain is not ready"
        );
        let receipt = placement
            .receipt
            .clone()
            .context("CI toolchain receipt missing")?;
        receipt.validate(node, &record.manifest)?;
        Ok((
            Binding {
                manifest: record.manifest.clone(),
                receipt,
            },
            placement.identity(),
        ))
    }
    pub fn catalog(&self, nodes: &[Node], conflicts: impl Fn(&Node) -> bool) -> Value {
        let templates: Vec<_> = self.records.values().map(|record| {
            let observations: Vec<_> = nodes.iter().map(|node| {
                let state = self.placements.get(&index(&record.manifest.id, &node.id)).filter(|p| p.current(record, node));
                let status = if !record.manifest.compatible(node) { "incompatible" } else if conflicts(node) { "agent-host-conflict" } else if !record.enabled { "disabled" } else if node.disabled { "node-disabled" } else { state.map_or("pending", |s| s.status.as_str()) };
                json!({"node_id":node.id,"os":node.os,"architecture":node.architecture,"status":status,"error":state.and_then(|s|s.error.as_ref()),"job_id":state.and_then(|s|s.job_id.as_ref()),"verified_at":if status == "ready" { state.and_then(|s| s.receipt.as_ref()).map(|r|r.verified_at) } else { None },"capabilities_verified":status=="ready"})
            }).collect();
            json!({"manifest":record.manifest,"enabled":record.enabled,"manifest_digest":record.manifest.digest(),"nodes":observations})
        }).collect();
        json!({"schema_version":1,"templates":templates})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interrupted_installs_require_retry_and_changed_recipes_require_a_new_revision() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest: Template = serde_json::from_str(include_str!(
            "../../../examples/ci/templates/node24-linux-x64.json"
        ))
        .unwrap();
        let node: Node = serde_json::from_value(json!({"schema_version":1,"id":"build","target":"builder@ci.example","os":"linux","architecture":"x64","image":"base","capabilities":[]})).unwrap();
        let mut registry = Registry::open(tmp.path()).unwrap();
        registry.register(manifest.clone()).unwrap();
        registry.enable(&manifest.id, true).unwrap();
        let record = registry.records[&manifest.id].clone();
        let mut pending = registry.pending(&record, &node).unwrap();
        let interrupted = pending.identity();
        pending.status = "installing".into();
        registry.save(pending).unwrap();
        drop(registry);
        let mut registry = Registry::open(tmp.path()).unwrap();
        let failed = registry.pending(&record, &node).unwrap();
        assert_eq!(failed.status, "failed");
        assert_eq!(
            failed.identity(),
            interrupted,
            "Observation must not silently create a retry"
        );
        assert!(registry.binding(&manifest.id, &node).is_err());
        registry.retry(&manifest.id, &node).unwrap();
        let retried = registry.pending(&record, &node).unwrap();
        assert_eq!(retried.status, "pending");
        assert!(retried.reinstall);
        assert_ne!(retried.identity(), interrupted);
        let mut changed = manifest.clone();
        changed.label = "Changed".into();
        assert!(registry.register(changed.clone()).is_err());
        changed.revision += 1;
        registry.register(changed).unwrap();
        assert!(registry.records[&manifest.id].enabled);
        assert_ne!(registry.records[&manifest.id].epoch, record.epoch);
    }
}

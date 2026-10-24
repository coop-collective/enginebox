//! Explicit activation of an owner-approved preparation Store. Never replaces an existing Store.
use super::*;
use sha2::{Digest, Sha256};
use std::{io::Read, path::Path};

fn snapshot_digest(path: &Path, expected: &str, limit: u64) -> Result<u64> {
    ensure!(crate::config::digest(expected), "Invalid snapshot digest");
    real_path(path)?;
    let metadata = fs::metadata(path)?;
    ensure!(
        metadata.is_file() && metadata.len() > 0 && metadata.len() <= limit,
        "Invalid preparation snapshot size"
    );
    let mut input = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    let mut bytes = 0u64;
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        ensure!(bytes <= limit, "Snapshot grew beyond limit");
        hash.update(&buffer[..n]);
    }
    ensure!(
        format!("{:x}", hash.finalize()) == expected && bytes == metadata.len(),
        "Preparation snapshot digest differs"
    );
    Ok(bytes)
}
impl Runtime {
    /// Check an already activated immutable base without rehydrating or rescanning its snapshot.
    pub fn preparation_ready(
        &self,
        project: &str,
        hash: &str,
        key: &str,
        revision: &str,
        record_hash: &str,
    ) -> Result<Value> {
        identifier(project)?;
        ensure!(
            revision == crate::REVISION
                && [hash, key, record_hash]
                    .iter()
                    .all(|v| crate::config::digest(v)),
            "Invalid preparation identity"
        );
        self.root()?;
        let _guard = state::lock(&self.config.root.join(".lock"), false)?;
        self.bind_config()?;
        if !self.project_dir(project).exists() {
            return Ok(json!({"ok":true,"ready":false,"project":project,"key":key}));
        }
        let p = self.project(project)?;
        let prior = state::read_json(&self.project_dir(project).join("preparation.json"))?;
        ensure!(
            p.status == "ready"
                && prior["snapshot"] == hash
                && prior["key"] == key
                && prior["record"] == record_hash
                && prior["revision"] == revision,
            "Existing project is not this immutable preparation"
        );
        self.owned_volume(&p)?;
        ensure!(
            prior["main"]
                == state::digest(&self.bridge(&p, "project-head", json!({"branch":"main"}), 30)?),
            "Prepared main changed after activation"
        );
        Ok(json!({"ok":true,"ready":true,"project":project,"key":key}))
    }

    pub fn preparation_restore(
        &self,
        project: &str,
        snapshot: &Path,
        hash: &str,
        key: &str,
        revision: &str,
        record: (&Path, &str),
    ) -> Result<Value> {
        identifier(project)?;
        ensure!(
            revision == crate::REVISION && crate::config::digest(key),
            "Preparation requires the pinned LayerFS revision and exact key"
        );
        let bytes = snapshot_digest(
            snapshot,
            hash,
            self.config.import_limit_gib * enginebox_core::GIB,
        )?;
        snapshot_digest(record.0, record.1, 65535)?;
        self.doctor()?;
        self.root()?;
        let _guard = state::lock(&self.config.root.join(".lock"), false)?;
        self.bind_config()?;
        self.space(bytes.saturating_mul(2))?;
        let directory = self.project_dir(project);
        let binding = directory.join("preparation.json");
        if directory.exists() {
            let p = self.project(project)?;
            let prior = state::read_json(&binding)?;
            ensure!(
                p.status == "ready"
                    && prior["snapshot"] == hash
                    && prior["key"] == key
                    && prior["record"] == record.1
                    && prior["revision"] == revision,
                "Existing project is not this immutable preparation"
            );
            self.owned_volume(&p)?;
            ensure!(
                prior["main"]
                    == state::digest(&self.bridge(
                        &p,
                        "project-head",
                        json!({"branch":"main"}),
                        30
                    )?),
                "Prepared main changed after activation"
            );
            return Ok(json!({"ok":true,"project":project,"key":key,"reused":true}));
        }
        ensure!(
            self.active_count()? < self.config.max_workspaces,
            "No capacity to inspect imported preparation"
        );
        state::private_dir(&directory)?;
        let owner = uuid::Uuid::new_v4().simple().to_string();
        let mut p = Project {
            schema: 1,
            name: project.into(),
            volume: format!("ebox-lfs-{owner}"),
            owner,
            status: "importing".into(),
            commit: hash.into(),
            approval: key.into(),
            source_only: true,
        };
        state::write_json(&directory.join("project.json"), &json!(p))?;
        state::write_json(
            &binding,
            &json!({"schema":1,"snapshot":hash,"key":key,"revision":revision,"record":record.1}),
        )?;
        let outcome = (|| {
            let home = directory.join("home");
            state::private_dir(&home)?;
            let discovery = home.join(project);
            state::private_dir(&discovery)?;
            let target = discovery.join("project.record");
            use std::io::Write;
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&target)?;
            file.write_all(&fs::read(record.0)?)?;
            file.sync_all()?;
            snapshot_digest(&target, record.1, 65535)?;
            let found = self.docker(
                &[
                    "volume".into(),
                    "ls".into(),
                    "--format={{.Name}}".into(),
                    format!("--filter=name=^{}$", p.volume),
                ],
                None,
                30,
            )?;
            ensure!(
                found.iter().all(|b| b.is_ascii_whitespace()),
                "Preparation volume already exists"
            );
            self.docker(
                &[
                    "volume".into(),
                    "create".into(),
                    "--label".into(),
                    format!("enginebox.layerfs.owner={}", p.owner),
                    "--label".into(),
                    format!("enginebox.layerfs.revision={}", crate::REVISION),
                    p.volume.clone(),
                ],
                None,
                30,
            )?;
            self.owned_volume(&p)?;
            let script = r#"const fs=require('node:fs'),crypto=require('node:crypto'),{DatabaseSync}=require('node:sqlite');
(async()=>{fs.mkdirSync('/store/global',{recursive:true});fs.copyFileSync('/input.sqlite','/store/global/store.sqlite',fs.constants.COPYFILE_EXCL);
const h=crypto.createHash('sha256');for await(const chunk of fs.createReadStream('/store/global/store.sqlite'))h.update(chunk);
if(h.digest('hex')!==process.argv[1])throw Error('Snapshot changed during activation');
const fd=fs.openSync('/store/global/store.sqlite','r+');fs.fsyncSync(fd);fs.closeSync(fd);
const db=new DatabaseSync('/store/global/store.sqlite',{readOnly:true});
if(Object.values(db.prepare('PRAGMA integrity_check').get())[0]!=='ok'||db.prepare('PRAGMA foreign_key_check').all().length)throw Error('Invalid preparation Store');db.close();})();"#;
            self.docker(
                &[
                    "run".into(),
                    "--rm".into(),
                    "--network=none".into(),
                    "--cpus=1".into(),
                    "--memory=1g".into(),
                    "--pids-limit=128".into(),
                    "--label".into(),
                    format!("enginebox.layerfs.owner={}", p.owner),
                    "--mount".into(),
                    format!("type=volume,src={},dst=/store", p.volume),
                    "--mount".into(),
                    format!(
                        "type=bind,src={},dst=/input.sqlite,readonly",
                        snapshot.display()
                    ),
                    self.config.image.clone(),
                    "node".into(),
                    "-e".into(),
                    script.into(),
                    hash.into(),
                ],
                None,
                600,
            )?;
            self.bridge(&p, "preparation-inspect", json!({"key":key}), 120)?;
            let main =
                state::digest(&self.bridge(&p, "project-head", json!({"branch":"main"}), 30)?);
            state::write_json(
                &binding,
                &json!({"schema":1,"snapshot":hash,"key":key,"revision":revision,"main":main,"record":record.1}),
            )?;
            Ok(json!({"ok":true,"project":project,"key":key,"reused":false}))
        })();
        p.status = if outcome.is_ok() {
            "ready"
        } else {
            "uncertain"
        }
        .into();
        state::write_json(&directory.join("project.json"), &json!(p))?;
        outcome
    }
    pub fn preparation_snapshot(&self, project: &str, name: &str, key: &str) -> Result<Value> {
        identifier(project)?;
        identifier(name)?;
        ensure!(crate::config::digest(key), "Exact preparation key required");
        self.root()?;
        self.bind_config()?;
        let _guard = state::lock(&self.config.root.join(".lock"), false)?;
        let p = self.project(project)?;
        // A Store that has serviced jobs may retain their private history even after branch cleanup.
        for item in fs::read_dir(self.config.root.join("workspaces"))? {
            let path = item?.path();
            if path.extension().is_some_and(|v| v == "json") {
                let w = state::read_json(&path)?;
                ensure!(
                    w["project"] != project || w["status"] == "closed",
                    "Close all preparation workspaces before publication"
                );
                ensure!(
                    !(w["project"] == project
                        && w["name"].as_str().is_some_and(|n| n.starts_with("ci-"))),
                    "Cannot publish a Store that contains CI job history"
                );
            }
        }
        self.bridge(&p, "preparation-inspect", json!({"key":key}), 120)?;
        let directory = self.config.root.join("snapshots");
        state::private_dir(&directory)?;
        let snapshot = directory.join(format!("{name}.sqlite"));
        ensure!(!snapshot.exists(), "Preparation snapshot already exists");
        self.space(0)?;
        self.bridge(&p, "snapshot", json!({"output":snapshot}), 600)?;
        fs::File::open(&snapshot)?.sync_all()?;
        fs::File::open(&directory)?.sync_all()?;
        let record = self
            .project_dir(project)
            .join("home")
            .join(project)
            .join("project.record");
        real_path(&record)?;
        ensure!(
            fs::metadata(&record)?.len() <= 65535,
            "Project record exceeds limit"
        );
        let record_hash = format!("{:x}", Sha256::digest(fs::read(&record)?));
        Ok(
            json!({"ok":true,"snapshot":snapshot,"projectRecord":record,"projectRecordHash":record_hash,"layerfsRevision":crate::REVISION,"projectName":project,"preparationKey":key,"preparationRecipe":"pnpm-installed-v1"}),
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn warm_inspection_distinguishes_absence_from_uncertain_or_different_state() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let base = temp.path().canonicalize()?;
        let runtime = Runtime {
            config: serde_json::from_value(json!({
                "schema":1,"root":base.join("state"),"sdk":base.join("sdk"),"daemon":base.join("daemon"),
                "native_sha256":"a".repeat(64),"daemon_sha256":"b".repeat(64),"image":format!("sha256:{}", "c".repeat(64)),
                "max_workspaces":2,"cpus_per_workspace":1,"memory_mib_per_workspace":1024,"disk_reserve_gib":1,"import_limit_gib":1,"pnpm_store":null,"pnpm_metadata":null
            }))?,
        };
        let hash = "a".repeat(64);
        let key = "b".repeat(64);
        let record = "c".repeat(64);
        assert!(
            runtime
                .preparation_ready("fixture", &hash, "invalid", crate::REVISION, &record)
                .is_err()
        );
        assert!(!runtime.config.root.exists());
        assert_eq!(
            runtime.preparation_ready("fixture", &hash, &key, crate::REVISION, &record)?["ready"],
            false
        );
        let directory = runtime.project_dir("fixture");
        state::private_dir(&directory)?;
        state::write_json(
            &directory.join("project.json"),
            &json!({"schema":1,"name":"fixture","volume":format!("ebox-lfs-{}", "d".repeat(32)),"owner":"d".repeat(32),"status":"uncertain","commit":hash,"approval":key,"source_only":true}),
        )?;
        state::write_json(
            &directory.join("preparation.json"),
            &json!({"schema":1,"snapshot":hash,"key":key,"revision":crate::REVISION,"record":record,"main":"e".repeat(64)}),
        )?;
        assert!(
            runtime
                .preparation_ready("fixture", &hash, &key, crate::REVISION, &record)
                .unwrap_err()
                .to_string()
                .contains("immutable preparation")
        );
        let mut project = state::read_json(&directory.join("project.json"))?;
        project["status"] = json!("ready");
        state::write_json(&directory.join("project.json"), &project)?;
        assert!(
            runtime
                .preparation_ready("fixture", &hash, &key, crate::REVISION, &"f".repeat(64))
                .unwrap_err()
                .to_string()
                .contains("immutable preparation")
        );
        Ok(())
    }
    #[test]
    fn activation_requires_exact_bytes_before_any_runtime_mutation() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = fs::canonicalize(tmp.path()).unwrap();
        let path = dir.join("snapshot.sqlite");
        fs::write(&path, b"fixture").unwrap();
        let hash = format!("{:x}", Sha256::digest(b"fixture"));
        assert_eq!(snapshot_digest(&path, &hash, 100).unwrap(), 7);
        assert!(snapshot_digest(&path, &hash, 2).is_err());
        fs::write(&path, b"changed").unwrap();
        assert!(snapshot_digest(&path, &hash, 100).is_err());
    }
}

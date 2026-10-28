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
        ensure!(
            object.len() == 1 && object.contains_key("content_base64"),
            "Upload accepts only content_base64"
        );
        let bytes = base64::engine::general_purpose::STANDARD.decode(
            body["content_base64"]
                .as_str()
                .context("content_base64 must be a string")?,
        )?;
        ensure!(
            !bytes.is_empty() && bytes.len() <= profile::MAX_FILE_BYTES,
            "A profile file must contain 1 byte to 1 MiB"
        );
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
        ensure!(
            object.keys().all(|k| [
                "name",
                "org_id",
                "description",
                "template",
                "providers",
                "callback_secret"
            ]
            .contains(&k.as_str())),
            "Unsupported profile fields"
        );
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
                ensure!(profiles.len() < MAX_PROFILES, "Profile limit reached");
                profile.revision = 1;
                profile.created_at = now.clone();
            }
        }
        profile.owner = profile::OWNER.into();
        profile.updated_at = now;
        profile.validate(&|sha| self.has_file(sha))?;
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
    pub fn resolve(&self, name: &str, provider: &str) -> Result<ResolvedProfile> {
        let profile = self
            .profiles
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .context("Unknown profile")?;
        let block = profile
            .providers
            .get(provider)
            .cloned()
            .context("The profile does not configure this provider")?;
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
        })
    }
}

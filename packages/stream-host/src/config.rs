use anyhow::{Context, Result, bail};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub controller: String,
    pub project: String,
    pub command: EngineCommand,
    #[serde(default = "width")]
    pub width: u16,
    #[serde(default = "height")]
    pub height: u16,
    #[serde(default = "peers")]
    pub max_peers: usize,
    #[serde(default)]
    pub allow_local_controller: bool,
}
fn width() -> u16 {
    1280
}
fn height() -> u16 {
    720
}
fn peers() -> usize {
    4
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EngineCommand {
    pub executable: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    pub directory: PathBuf,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}
impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let config: Self = serde_json::from_slice(&fs::read(path)?)?;
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&self) -> Result<()> {
        let url = reqwest::Url::parse(&self.controller)?;
        // Explicit local mode also permits private Docker DNS for disposable development networks.
        if !(url.scheme() == "https" || self.allow_local_controller && url.scheme() == "http")
            || url.username() != ""
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
        {
            bail!(
                "controller must be an HTTPS origin (HTTP requires explicit allowLocalController)"
            )
        }
        if self.project.is_empty()
            || self.project.len() > 128
            || !self
                .project
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        {
            bail!("invalid project id")
        }
        if !(160..=1920).contains(&self.width)
            || !(90..=1080).contains(&self.height)
            || !(1..=8).contains(&self.max_peers)
        {
            bail!("capture dimensions or peer limit exceed supported bounds")
        }
        if !self.command.executable.is_absolute()
            || !self.command.directory.is_absolute()
            || self.command.args.len() > 64
            || self.command.args.iter().any(|v| v.len() > 4096)
        {
            bail!("command must use absolute operator-configured paths and bounded arguments")
        }
        // Children never inherit enrollment, controller or provider secrets from the daemon.
        for (key, value) in &self.command.env {
            if !matches!(
                key.as_str(),
                "LANG"
                    | "LC_ALL"
                    | "GODOT_SILENCE_ROOT_WARNING"
                    | "DOTNET_CLI_TELEMETRY_OPTOUT"
                    | "DOTNET_NOLOGO"
                    | "SDL_AUDIODRIVER"
            ) || value.len() > 256
            {
                bail!("unsupported engine environment key")
            }
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Identity {
    pub controller: String,
    pub project: String,
    pub token: String,
    pub enrolled: bool,
}
impl Identity {
    pub fn load(dir: &Path, config: &Config) -> Result<Option<Self>> {
        fs::create_dir_all(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        let path = dir.join("identity.json");
        if !path.exists() {
            return Ok(None);
        }
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
            bail!("identity file must be private mode0600 and not a symlink")
        }
        let identity: Self = serde_json::from_slice(&fs::read(path)?)?;
        if identity.controller != config.controller || identity.project != config.project {
            bail!(
                "identity is bound to a different controller/project; use its original config or a new state volume"
            )
        }
        Ok(Some(identity))
    }
    pub fn create(config: &Config, invitation: &str) -> Result<Self> {
        let (node, secret) = invitation
            .trim()
            .split_once(".enroll.")
            .context("invalid enrollment invitation")?;
        if node.is_empty()
            || node.len() > 128
            || !node
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
            || secret.len() != 64
            || !secret.bytes().all(|c| c.is_ascii_hexdigit())
        {
            bail!("invalid enrollment invitation")
        }
        let mut secret = [0; 32];
        rand::thread_rng().fill_bytes(&mut secret);
        Ok(Self {
            controller: config.controller.clone(),
            project: config.project.clone(),
            token: format!("{node}.{}", hex::encode(secret)),
            enrolled: false,
        })
    }
    pub fn hash(&self) -> String {
        hex::encode(Sha256::digest(self.token.as_bytes()))
    }
    pub fn save(&self, dir: &Path) -> Result<()> {
        let temporary = dir.join(format!(".identity-{}", uuid::Uuid::new_v4()));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec(self)?)?;
        file.sync_all()?;
        fs::rename(&temporary, dir.join("identity.json"))?;
        fs::File::open(dir)?.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> Config {
        serde_json::from_value(serde_json::json!({"controller":"https://controller.example","project":"demo","command":{"executable":"/usr/bin/fixture","directory":"/app"}})).unwrap()
    }
    #[test]
    fn secret_survives_lost_enrollment_ack_and_never_changes_scope() {
        let dir = tempfile::tempdir().unwrap();
        let c = config();
        assert!(Identity::load(dir.path(), &c).unwrap().is_none());
        let id = Identity::create(&c, &format!("node.enroll.{}", "a".repeat(64))).unwrap();
        id.save(dir.path()).unwrap();
        let loaded = Identity::load(dir.path(), &c).unwrap().unwrap();
        assert_eq!(id.token, loaded.token);
        assert!(!loaded.enrolled);
        let mut changed = c;
        changed.project = "other".into();
        assert!(Identity::load(dir.path(), &changed).is_err());
    }
    #[test]
    fn rejects_unsafe_configuration() {
        let mut c = config();
        c.validate().unwrap();
        c.controller = "http://localhost:8799".into();
        assert!(c.validate().is_err());
        c.allow_local_controller = true;
        c.validate().unwrap();
        c.command
            .env
            .insert("ENGINEBOX_TOKEN".into(), "secret".into());
        assert!(c.validate().is_err());
    }
}

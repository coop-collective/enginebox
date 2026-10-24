//! Owner-authored, immutable CI toolchain recipes. These never install an OS.
use crate::model::{Node, Os, Shell, identifier};
use anyhow::{Result, ensure};
use enginebox_core::state;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    pub cpu: u32,
    pub memory_mib: u64,
    /// Required free disk before installation, including downloads and expansion.
    pub disk_mib: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub sha256: String,
    pub max_bytes: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Probe {
    pub capability: String,
    pub run: String,
    pub expected_stdout: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Template {
    pub schema_version: u32,
    pub id: String,
    pub label: String,
    pub revision: u64,
    pub os: Os,
    pub architecture: String,
    pub image: String,
    pub capabilities: Vec<String>,
    pub minimum: Resources,
    pub shell: Shell,
    #[serde(default)]
    pub assets: Vec<Asset>,
    /// Run as the unprivileged CI account in the versioned installation directory.
    pub install: String,
    pub verify: Vec<Probe>,
    #[serde(default)]
    pub paths: Vec<String>,
    /// Toolchain location variables, relative to the installation directory.
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    pub timeout_minutes: u64,
}
pub fn relative(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 255
        && path.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        })
}
fn script(run: &str) -> bool {
    !run.trim().is_empty() && run.len() <= 8192 && !run.contains('\0')
}
pub fn digest(value: &impl Serialize) -> String {
    state::digest(&serde_json::to_value(value).expect("Template data serializable"))
}
impl Template {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1
                && identifier(&self.id)
                && self.id.len() <= 48
                && !self.id.bytes().any(|b| b.is_ascii_uppercase())
                && self.revision > 0,
            "Invalid template identity/revision"
        );
        ensure!(
            !self.label.is_empty()
                && self.label.len() <= 100
                && !self.label.chars().any(char::is_control),
            "Invalid template label"
        );
        self.node(&Node {
            schema_version: 1,
            id: "validation".into(),
            target: "local".into(),
            os: self.os.clone(),
            architecture: self.architecture.clone(),
            image: self.image.clone(),
            capabilities: vec![],
            disabled: false,
        })
        .validate()?;
        ensure!(
            self.os != Os::Windows || self.shell == Shell::Pwsh,
            "Windows templates require PowerShell"
        );
        ensure!(
            self.os == Os::Windows || self.shell == Shell::Bash,
            "Unix templates require Bash"
        );
        ensure!(
            (1..=128).contains(&self.minimum.cpu)
                && (1..=1_048_576).contains(&self.minimum.memory_mib)
                && (1..=4_194_304).contains(&self.minimum.disk_mib),
            "Invalid installation resource requirements"
        );
        ensure!(
            (1..=120).contains(&self.timeout_minutes) && script(&self.install),
            "Invalid installation script/timeout"
        );
        let capabilities: BTreeSet<_> = self
            .capabilities
            .iter()
            .map(|v| v.to_ascii_lowercase())
            .collect();
        let probes: BTreeSet<_> = self
            .verify
            .iter()
            .map(|v| v.capability.to_ascii_lowercase())
            .collect();
        ensure!(
            !capabilities.is_empty()
                && capabilities.len() == self.capabilities.len()
                && capabilities == probes
                && probes.len() == self.verify.len(),
            "Each capability needs exactly one verification probe"
        );
        ensure!(
            self.verify.iter().all(|v| script(&v.run)
                && !v.expected_stdout.trim().is_empty()
                && v.expected_stdout.len() <= 4096),
            "Invalid capability probe"
        );
        ensure!(
            self.paths.len() <= 16
                && self.paths.iter().all(|v| relative(v))
                && self.environment.len() <= 8,
            "Invalid toolchain paths"
        );
        ensure!(
            self.environment.iter().all(|(k, v)| [
                "JAVA_HOME",
                "ANDROID_HOME",
                "ANDROID_SDK_ROOT",
                "DEVELOPER_DIR",
                "SDKROOT",
                "CMAKE_PREFIX_PATH",
                "DOTNET_ROOT"
            ]
            .contains(&k.as_str())
                && relative(v)),
            "Only private toolchain location variables are allowed"
        );
        ensure!(self.assets.len() <= 8, "Too many assets");
        let mut names = BTreeSet::new();
        let mut bytes = 0u64;
        for asset in &self.assets {
            ensure!(
                identifier(&asset.name)
                    && names.insert(asset.name.to_ascii_lowercase())
                    && asset.sha256.len() == 64
                    && asset.sha256.bytes().all(|v| v.is_ascii_hexdigit())
                    && (1..=16 * 1024 * 1024 * 1024).contains(&asset.max_bytes),
                "Assets require unique names, SHA-256 and bounded size"
            );
            let url = url::Url::parse(&asset.url)?;
            ensure!(
                url.scheme() == "https"
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "Asset URL must use HTTPS without credentials/query/fragment"
            );
            bytes += asset.max_bytes;
        }
        ensure!(
            bytes <= self.minimum.disk_mib * 1024 * 1024,
            "Downloads exceed the declared disk requirement"
        );
        ensure!(
            serde_json::to_vec(self)?.len() <= 32768,
            "Template exceeds 32 KiB"
        );
        Ok(())
    }
    pub fn compatible(&self, node: &Node) -> bool {
        self.os == node.os && self.architecture == node.architecture
    }
    pub fn node(&self, node: &Node) -> Node {
        Node {
            image: self.image.clone(),
            capabilities: self.capabilities.clone(),
            ..node.clone()
        }
    }
    pub fn digest(&self) -> String {
        digest(self)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub schema_version: u32,
    pub node_binding: String,
    pub template_id: String,
    pub revision: u64,
    pub manifest_digest: String,
    pub installation: String,
    pub verified_at: u64,
    pub probes: BTreeMap<String, String>,
}
impl Receipt {
    pub fn validate(&self, node: &Node, template: &Template) -> Result<()> {
        ensure!(
            self.schema_version == 1
                && template.compatible(node)
                && self.node_binding == digest(node)
                && self.template_id == template.id
                && self.revision == template.revision
                && self.manifest_digest == template.digest()
                && self.installation.len() == 64
                && self.installation.bytes().all(|b| b.is_ascii_hexdigit()),
            "Installation receipt does not match this node/template"
        );
        ensure!(
            self.probes.len() == template.verify.len()
                && template
                    .verify
                    .iter()
                    .all(|v| self.probes.get(&v.capability).is_some_and(
                        |hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())
                    )),
            "Installation receipt lacks capability evidence"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub manifest: Template,
    pub receipt: Receipt,
}

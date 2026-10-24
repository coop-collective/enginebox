use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Source {
    pub schema: u32,
    pub project: String,
    pub generation: String,
    pub layerfs: Layerfs,
    pub tree_hash: String,
    pub entries: Vec<Entry>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Layerfs {
    pub revision: String,
    pub project: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub root: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum Entry {
    File {
        path: String,
        sha256: String,
        size: u64,
        executable: bool,
    },
    Symlink {
        path: String,
        target: String,
    },
}
impl Entry {
    pub fn path(&self) -> &str {
        match self {
            Self::File { path, .. } | Self::Symlink { path, .. } => path,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolFile {
    pub name: String,
    pub path: PathBuf,
    pub sha256: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Recipe {
    pub schema: u32,
    pub engine: String,
    pub version: String,
    pub target: String,
    pub architecture: String,
    pub executable: PathBuf,
    pub executable_sha256: String,
    #[serde(default)]
    pub inputs: Vec<ToolFile>,
    pub entrypoint: String,
    pub timeout_seconds: u64,
    #[serde(default = "default_log_limit")]
    pub max_log_bytes: usize,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default)]
    pub unity_method: Option<String>,
    #[serde(default)]
    pub unreal_project: Option<String>,
    #[serde(default = "default_cache_version")]
    pub cache_version: String,
}
fn default_log_limit() -> usize {
    8 * 1024 * 1024
}
fn default_cache_version() -> String {
    "1".into()
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Toolchain {
    pub engine: String,
    pub version: String,
    pub target: String,
    pub architecture: String,
    pub fingerprint: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactFile {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    pub executable: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    pub kind: String,
    pub project: String,
    pub generation: String,
    pub source_tree: String,
    pub recipe: String,
    pub toolchain: Toolchain,
    pub entrypoint: String,
    pub files: Vec<ArtifactFile>,
    pub total_bytes: u64,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildResult {
    pub artifact_id: String,
    pub manifest_path: PathBuf,
    pub files_root: PathBuf,
    pub cache_hit: bool,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LeaseRecord {
    pub attempt: String,
    pub expires_at_ms: u64,
    #[serde(default)]
    pub cancelled: bool,
}

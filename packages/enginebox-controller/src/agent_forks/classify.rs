//! Path classification and the secret scan for agent-fork submissions
//! (docs/ENGINE-SESSIONS.md, "Agent forks", "Checks on submit").
//!
//! The controller is authoritative: the node computes the same classes for a
//! preview, but every submission is classified again here from the changed
//! paths and modes the node reported. Paths are relative to the repository
//! root, `/`-separated; `project` is the session's Unity project directory.
//! The first matching rule wins:
//!
//! 1. **refused** (never merged): unsafe or Windows-hostile names, a `.git`
//!    component, symlinks and submodules, credential paths,
//!    `ProjectSettings/ProjectVersion.txt`, Unity's generated directories,
//!    LFS-untracked files over 100 MiB, and anything outside the project
//!    other than the protected root files, `README*` and `docs/**`.
//! 2. **protected** (owner approval): Git and LFS configuration
//!    (`.gitattributes`, `.lfsconfig` can repoint LFS and leak tokens,
//!    `.gitmodules`, `.gitignore`), `.github/**`, `package.json` and lockfiles
//!    outside the project, and files in the project root outside `Assets/`,
//!    `ProjectSettings/` and `Packages/`.
//! 3. **code** (owner approval): compiled or executed sources and binaries,
//!    anything under an `Editor/` or `Plugins/` directory, `Packages/**`
//!    (a manifest change installs packages), and the `.meta` of any of them.
//! 4. **config** (automatic): `ProjectSettings/**`, `*.inputactions`.
//! 5. **scene** (automatic, a textual three-way merge): `*.unity`,
//!    `*.prefab`, `*.scenetemplate`.
//! 6. **asset** (automatic): everything else under `Assets/`, root
//!    `README*` and `docs/**`.
//!
//! Findings name a rule, a path and a line, never the matched value.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Larger files must be LFS tracked.
pub const MAX_PLAIN_BYTES: u64 = 100 * 1024 * 1024;
/// Most changed paths one submission may carry.
pub const MAX_CHANGES: usize = 5000;
/// Most added bytes one submission may carry.
pub const MAX_ADDED_BYTES: u64 = 500 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    Refused,
    Protected,
    Code,
    Config,
    Scene,
    Asset,
}
impl Class {
    /// Classes a merge may take without the room owner (room policy may
    /// narrow this list, never widen it).
    pub fn automatic(self) -> bool {
        matches!(self, Self::Config | Self::Scene | Self::Asset)
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Refused => "refused",
            Self::Protected => "protected",
            Self::Code => "code",
            Self::Config => "config",
            Self::Scene => "scene",
            Self::Asset => "asset",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "refused" => Self::Refused,
            "protected" => Self::Protected,
            "code" => Self::Code,
            "config" => Self::Config,
            "scene" => Self::Scene,
            "asset" => Self::Asset,
            _ => return None,
        })
    }
}

/// One finding: why a path cannot merge (`line` for secret-scan hits).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub rule: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
}
impl Finding {
    pub fn new(rule: &str, path: &str, line: Option<u64>) -> Self {
        Self {
            rule: rule.to_owned(),
            path: path.to_owned(),
            line,
        }
    }
}

/// A Git-safe, Windows-safe relative path: no empty, `.` or `..`
/// components, no backslash, colon or control character, at most 1024 bytes.
pub fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1024
        && !path.starts_with('/')
        && !path.chars().any(|c| c.is_control() || c == '\\')
        && path
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != ".." && c.len() <= 255)
}

const RESERVED: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];
const PRIVATE_DIRS: [&str; 6] = [".ssh", ".aws", ".azure", ".claude", ".codex", ".cursor"];
const CODE_EXTENSIONS: [&str; 23] = [
    "cs", "asmdef", "asmref", "rsp", "dll", "so", "dylib", "jslib", "jspre", "mm", "m", "cpp", "c",
    "h", "java", "kt", "aar", "jar", "py", "ps1", "sh", "bat", "exe",
];
const LOCKFILES: [&str; 6] = [
    "package.json",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lockb",
];

fn extension(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

/// Why a single component is refused (Windows names, alternate streams,
/// credential files), if it is.
fn refused_component(component: &str, last: bool) -> Option<&'static str> {
    let lower = component.to_ascii_lowercase();
    if lower == ".git" {
        return Some("git_path");
    }
    if component.contains(':') {
        return Some("alternate_stream");
    }
    if component.ends_with('.') || component.ends_with(' ') || component.starts_with(' ') {
        return Some("trailing_dot_or_space");
    }
    let stem = lower.split('.').next().unwrap_or("");
    if RESERVED.contains(&stem) {
        return Some("reserved_name");
    }
    if PRIVATE_DIRS.contains(&lower.as_str()) {
        return Some("credential_path");
    }
    if !last {
        return None;
    }
    let env = lower == ".env"
        || (lower.starts_with(".env.")
            && ![".env.example", ".env.sample", ".env.template"].contains(&lower.as_str()));
    let key_file = lower.starts_with("id_")
        && (!lower[3..].contains('.') || lower.ends_with(".pub"))
        && lower.len() > 3;
    let credential = env
        || key_file
        || [".npmrc", ".pypirc", ".netrc", "auth.json"].contains(&lower.as_str())
        || ["pem", "key", "pfx", "p12"].contains(&extension(&lower).as_str())
        || (lower.contains("credentials") && lower.ends_with(".json"));
    credential.then_some("credential_path")
}

/// The class of `path` (with its Git `mode`) in a project at `project`, and
/// for refused paths the rule that refused it.
pub fn classify(path: &str, mode: u32, project: &str) -> (Class, Option<&'static str>) {
    let refused = |rule| (Class::Refused, Some(rule));
    if !valid_path(path) {
        return refused("invalid_path");
    }
    if mode == 0o120000 || mode == 0o160000 {
        return refused("link_or_submodule");
    }
    let parts: Vec<&str> = path.split('/').collect();
    for (i, component) in parts.iter().enumerate() {
        if let Some(rule) = refused_component(component, i + 1 == parts.len()) {
            return refused(rule);
        }
    }
    let lower = path.to_ascii_lowercase();
    let name = parts[parts.len() - 1].to_ascii_lowercase();
    let project = project.trim_matches('/');
    let project = if project == "." { "" } else { project };
    // The path inside the project, when it is inside.
    let inner: Option<&str> = if project.is_empty() {
        Some(path)
    } else {
        path.strip_prefix(project)
            .and_then(|rest| rest.strip_prefix('/'))
    };
    if let Some(inner) = inner {
        let first = inner.split('/').next().unwrap_or("");
        if inner == "ProjectSettings/ProjectVersion.txt" {
            return refused("engine_version");
        }
        if ["Library", "Temp", "Logs", "obj", "UserSettings"].contains(&first)
            && inner.contains('/')
        {
            return refused("generated_directory");
        }
    }
    // Git and LFS configuration anywhere; CI and JavaScript manifests at the
    // root (outside the project).
    if [".gitattributes", ".lfsconfig", ".gitmodules", ".gitignore"].contains(&name.as_str())
        || (inner.is_none() || project.is_empty()) && lower.starts_with(".github/")
        || (inner.is_none() && LOCKFILES.contains(&name.as_str()))
    {
        return (Class::Protected, None);
    }
    let docs = parts.len() == 1 && name.starts_with("readme") || lower.starts_with("docs/");
    if inner.is_none() && !docs {
        return refused("outside_project");
    }
    if code_path(path, project) {
        return (Class::Code, None);
    }
    if let Some(base) = path.strip_suffix(".meta").filter(|b| !b.is_empty())
        && code_path(base, project)
    {
        return (Class::Code, None);
    }
    let Some(inner) = inner else {
        return (Class::Asset, None);
    };
    if project.is_empty() && docs {
        return (Class::Asset, None);
    }
    let first = inner.split('/').next().unwrap_or("");
    if first == "ProjectSettings" || name.ends_with(".inputactions") {
        return (Class::Config, None);
    }
    if first != "Assets" {
        // The project root: .vsconfig, stray files, other directories.
        return (Class::Protected, None);
    }
    if [".unity", ".prefab", ".scenetemplate"]
        .iter()
        .any(|e| name.ends_with(e))
    {
        return (Class::Scene, None);
    }
    (Class::Asset, None)
}

/// Compiled or executed by Unity, the Editor or the host.
fn code_path(path: &str, project: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    let name = parts[parts.len() - 1];
    if CODE_EXTENSIONS.contains(&extension(name).as_str()) {
        return true;
    }
    if parts
        .iter()
        .any(|c| c.to_ascii_lowercase().ends_with(".bundle"))
    {
        return true;
    }
    if parts[..parts.len() - 1]
        .iter()
        .any(|c| c.eq_ignore_ascii_case("editor") || c.eq_ignore_ascii_case("plugins"))
    {
        return true;
    }
    let packages = if project.is_empty() {
        "Packages/".to_owned()
    } else {
        format!("{project}/Packages/")
    };
    path.starts_with(&packages)
}

// ------------------------------------------------------------------ secrets

/// Rules whose prefix must be followed by at least `min` token characters.
const PREFIXES: [(&str, usize, &str); 14] = [
    ("xk_live_", 16, "xinf_key"),
    ("xat_", 16, "xinf_token"),
    ("xrt_", 16, "xinf_token"),
    ("xak_", 16, "xinf_token"),
    ("ebr_", 32, "relay_token"),
    ("ebst_", 32, "enginebox_token"),
    ("ehp_", 32, "enginebox_token"),
    ("sk-ant-", 16, "anthropic_key"),
    ("sk-", 20, "openai_key"),
    ("ghp_", 30, "github_token"),
    ("ghs_", 30, "github_token"),
    ("gho_", 30, "github_token"),
    ("ghu_", 30, "github_token"),
    ("github_pat_", 20, "github_token"),
];

fn token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// The secret rules `line` matches (deduplicated), with `known` secrets the
/// controller handed out for this fork matched exactly. `entropy` enables
/// the high-entropy rule (`*.json`, `*.asset`, `*.txt`).
pub fn scan_line(line: &str, known: &[String], entropy: bool) -> Vec<&'static str> {
    let mut rules = Vec::new();
    let bytes = line.as_bytes();
    for (prefix, min, rule) in PREFIXES {
        let mut from = 0;
        while let Some(at) = line[from..].find(prefix) {
            let start = from + at;
            let before_ok = start == 0 || !token_char(bytes[start - 1]);
            let tail = bytes[start + prefix.len()..]
                .iter()
                .take_while(|b| token_char(**b))
                .count();
            // `sk-ant-` is reported once, as anthropic.
            let shadowed = prefix == "sk-" && line[start..].starts_with("sk-ant-");
            if before_ok && tail >= min && !shadowed && !rules.contains(&rule) {
                rules.push(rule);
            }
            from = start + prefix.len();
        }
    }
    // AWS access key ids.
    for (i, _) in line.match_indices("AKIA") {
        let rest = &bytes[i + 4..];
        if rest.len() >= 16
            && rest[..16]
                .iter()
                .all(|b| b.is_ascii_digit() || b.is_ascii_uppercase())
            && rest.get(16).is_none_or(|b| !b.is_ascii_alphanumeric())
            && !rules.contains(&"aws_key")
        {
            rules.push("aws_key");
        }
    }
    if line.contains("-----BEGIN ") && line.contains("PRIVATE KEY-----") {
        rules.push("private_key");
    }
    if jwt(line) {
        rules.push("jwt");
    }
    if entropy && high_entropy(line) {
        rules.push("high_entropy");
    }
    if known
        .iter()
        .any(|k| k.len() >= 12 && line.contains(k.as_str()))
    {
        rules.push("known_secret");
    }
    rules
}

/// Three base64url segments, the first a JSON header (`eyJ`).
fn jwt(line: &str) -> bool {
    line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'))
        .any(|word| {
            let parts: Vec<&str> = word.split('.').collect();
            parts.len() == 3 && parts[0].starts_with("eyJ") && parts.iter().all(|p| p.len() >= 10)
        })
}

/// A run of 40+ token characters with more than 4.2 bits of entropy per
/// character (hex digests and Unity GUIDs stay below it).
fn high_entropy(line: &str) -> bool {
    line.split(|c: char| !(c.is_ascii_alphanumeric() || "+/=_-".contains(c)))
        .filter(|w| w.len() >= 40)
        .any(|w| {
            let mut counts = [0usize; 256];
            for b in w.bytes() {
                counts[b as usize] += 1;
            }
            let n = w.len() as f64;
            let bits: f64 = counts
                .iter()
                .filter(|c| **c > 0)
                .map(|c| {
                    let p = *c as f64 / n;
                    -p * p.log2()
                })
                .sum();
            bits > 4.2
                && w.bytes().any(|b| b.is_ascii_digit())
                && w.bytes().any(|b| b.is_ascii_alphabetic())
        })
}

/// Secret-scan the added lines of a unified diff (`+` lines; file names from
/// `+++ b/<path>`, line numbers from hunk headers).
pub fn scan_diff(diff: &str, known: &[String]) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut path: Option<String> = None;
    let mut line_no: u64 = 0;
    for line in diff.lines() {
        if let Some(p) = line.strip_prefix("+++ ") {
            path = p.strip_prefix("b/").map(str::to_owned);
            continue;
        }
        if line.starts_with("--- ") {
            continue;
        }
        if let Some(header) = line.strip_prefix("@@ ") {
            // @@ -a,b +c,d @@
            line_no = header
                .split(' ')
                .find_map(|p| p.strip_prefix('+'))
                .and_then(|p| p.split(',').next())
                .and_then(|n| n.parse().ok())
                .unwrap_or(1);
            continue;
        }
        let Some(file) = &path else { continue };
        if let Some(added) = line.strip_prefix('+') {
            let ext = extension(file);
            let entropy = ["json", "asset", "txt"].contains(&ext.as_str());
            for rule in scan_line(added, known, entropy) {
                let finding = Finding::new(rule, file, Some(line_no));
                if !findings.contains(&finding) && findings.len() < 200 {
                    findings.push(finding);
                }
            }
            line_no += 1;
        } else if !line.starts_with('-') && !line.starts_with('\\') {
            line_no += 1;
        }
    }
    findings
}

/// `text` with known secrets and secret-shaped tokens replaced by
/// `[redacted]` (run events from the node are redacted there too).
pub fn redact(text: &str, known: &[String]) -> String {
    let mut out = text.to_owned();
    for k in known.iter().filter(|k| k.len() >= 12) {
        out = out.replace(k.as_str(), "[redacted]");
    }
    let mut result = String::with_capacity(out.len());
    let bytes = out.as_bytes();
    let mut i = 0;
    'outer: while i < bytes.len() {
        if i == 0 || !token_char(bytes[i - 1]) {
            for (prefix, min, _) in PREFIXES {
                if out[i..].starts_with(prefix) {
                    let tail = bytes[i + prefix.len()..]
                        .iter()
                        .take_while(|b| token_char(**b))
                        .count();
                    if tail >= min {
                        result.push_str("[redacted]");
                        i += prefix.len() + tail;
                        continue 'outer;
                    }
                }
            }
        }
        let ch = out[i..].chars().next().unwrap();
        result.push(ch);
        i += ch.len_utf8();
    }
    result
}

/// Counts per class, as `GET …/submissions` shows them.
pub fn counts(classes: &[Class]) -> Value {
    let n = |c: Class| classes.iter().filter(|x| **x == c).count();
    json!({
        "scene": n(Class::Scene), "asset": n(Class::Asset), "config": n(Class::Config),
        "code": n(Class::Code), "protected": n(Class::Protected), "refused": n(Class::Refused),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &str = "unity-project";
    fn class(path: &str) -> Class {
        classify(path, 0o100644, P).0
    }

    #[test]
    fn classifies_unity_paths_first_match_wins() {
        for (path, expected) in [
            ("unity-project/Assets/Scenes/Main.unity", Class::Scene),
            ("unity-project/Assets/Prefabs/Card.prefab", Class::Scene),
            ("unity-project/Assets/Art/card.png", Class::Asset),
            ("unity-project/Assets/Art/card.png.meta", Class::Asset),
            ("unity-project/Assets/Data/deck.asset", Class::Asset),
            ("unity-project/Assets/Shaders/glow.shader", Class::Asset),
            ("unity-project/Assets/Scripts/Deck.cs", Class::Code),
            ("unity-project/Assets/Scripts/Deck.cs.meta", Class::Code),
            ("unity-project/Assets/Scripts/Game.asmdef", Class::Code),
            ("unity-project/Assets/csc.rsp", Class::Code),
            ("unity-project/Assets/Tools/Editor/Menu.png", Class::Code),
            ("unity-project/Assets/Plugins/x.txt", Class::Code),
            (
                "unity-project/Assets/Plugins/mac.bundle/Contents/Info.plist",
                Class::Code,
            ),
            ("unity-project/Assets/Web/bridge.jslib", Class::Code),
            ("unity-project/Packages/manifest.json", Class::Code),
            ("unity-project/Packages/packages-lock.json", Class::Code),
            (
                "unity-project/ProjectSettings/TagManager.asset",
                Class::Config,
            ),
            (
                "unity-project/Assets/Input/Controls.inputactions",
                Class::Config,
            ),
            ("unity-project/.vsconfig", Class::Protected),
            (".gitattributes", Class::Protected),
            (".lfsconfig", Class::Protected),
            ("unity-project/Assets/.gitignore", Class::Protected),
            (".github/workflows/ci.yml", Class::Protected),
            ("package.json", Class::Protected),
            ("tools/pnpm-lock.yaml", Class::Protected),
            ("README.md", Class::Asset),
            ("docs/design.md", Class::Asset),
            ("docs/build.sh", Class::Code),
        ] {
            assert_eq!(class(path), expected, "{path}");
        }
    }

    #[test]
    fn refuses_dangerous_paths_with_a_rule() {
        for (path, rule) in [
            (
                "unity-project/ProjectSettings/ProjectVersion.txt",
                "engine_version",
            ),
            ("unity-project/Library/ArtifactDB", "generated_directory"),
            ("unity-project/Temp/x", "generated_directory"),
            ("unity-project/Assets/.git/config", "git_path"),
            ("unity-project/Assets/.GIT", "git_path"),
            ("unity-project/Assets/a:stream", "alternate_stream"),
            ("unity-project/Assets/CON.txt", "reserved_name"),
            ("unity-project/Assets/nul", "reserved_name"),
            ("unity-project/Assets/trail.", "trailing_dot_or_space"),
            ("unity-project/Assets/.env", "credential_path"),
            ("unity-project/Assets/.env.local", "credential_path"),
            ("unity-project/Assets/server.pem", "credential_path"),
            ("unity-project/Assets/id_ed25519", "credential_path"),
            (
                "unity-project/Assets/.claude/settings.json",
                "credential_path",
            ),
            ("unity-project/Assets/.codex/auth.json", "credential_path"),
            (
                "unity-project/Assets/gcp-credentials.json",
                "credential_path",
            ),
            ("unity-project/Assets/.npmrc", "credential_path"),
            ("scripts/build.cs", "outside_project"),
            ("/abs/path", "invalid_path"),
            ("unity-project/../x", "invalid_path"),
            ("unity-project\\Assets\\x", "invalid_path"),
            ("unity-project//x", "invalid_path"),
        ] {
            assert_eq!(
                classify(path, 0o100644, P),
                (Class::Refused, Some(rule)),
                "{path}"
            );
        }
        assert_eq!(
            classify("unity-project/Assets/link", 0o120000, P).0,
            Class::Refused
        );
        assert_eq!(
            classify("unity-project/Assets/sub", 0o160000, P).0,
            Class::Refused
        );
        // Harmless look-alikes stay assets.
        for path in [
            "unity-project/Assets/.env.example",
            "unity-project/Assets/id_card.png",
            "unity-project/Assets/Keys/keyboard.asset",
        ] {
            assert_eq!(class(path), Class::Asset, "{path}");
        }
    }

    #[test]
    fn a_root_project_has_no_outside() {
        assert_eq!(classify("Assets/Main.unity", 0o100644, "").0, Class::Scene);
        assert_eq!(
            classify("Packages/manifest.json", 0o100644, ".").0,
            Class::Code
        );
        assert_eq!(classify(".github/x.yml", 0o100644, "").0, Class::Protected);
        assert_eq!(classify("README.md", 0o100644, "").0, Class::Asset);
        assert_eq!(classify("tools/x.txt", 0o100644, "").0, Class::Protected);
    }

    #[test]
    fn scans_added_lines_and_names_only_rule_path_and_line() {
        let diff = "diff --git a/unity-project/Assets/a.json b/unity-project/Assets/a.json\n--- a/unity-project/Assets/a.json\n+++ b/unity-project/Assets/a.json\n@@ -1,2 +10,4 @@\n context\n+{\"key\": \"xk_live_abcdefghijklmnop1234\"}\n-removed xat_abcdefghijklmnopqrst\n+harmless line\n+token sk-ant-api03-abcdefghijklmnopqrstu and AKIAABCDEFGHIJKLMNOP\n+eyJhbGciOiJFUzI1NiJ9.eyJzdWIiOiJ4In0.c2lnbmF0dXJlLXZhbHVl\n+aB3dE5fG7hJ9kL1mN3pQ5rS7tU9vW1xY3zA5bC7dE9fG1hJ3kL5mN7p\n+the run token known-secret-value-123\n";
        let findings = scan_diff(diff, &["known-secret-value-123".into()]);
        let rules: Vec<(&str, u64)> = findings
            .iter()
            .map(|f| (f.rule.as_str(), f.line.unwrap()))
            .collect();
        assert_eq!(
            rules,
            vec![
                ("xinf_key", 11),
                ("anthropic_key", 13),
                ("aws_key", 13),
                ("jwt", 14),
                ("high_entropy", 15),
                ("known_secret", 16),
            ]
        );
        assert!(
            findings
                .iter()
                .all(|f| f.path == "unity-project/Assets/a.json")
        );
        let text = serde_json::to_string(&findings).unwrap();
        assert!(!text.contains("xk_live_") && !text.contains("known-secret"));
        // Hex digests and Unity GUIDs are not high entropy.
        assert!(scan_line(&"0123456789abcdef".repeat(4), &[], true).is_empty());
        assert!(scan_line("guid: 8f1e2d3c4b5a69788f1e2d3c4b5a6978", &[], true).is_empty());
        // Short look-alikes are not tokens.
        assert!(scan_line("task-runner sk-1 xat_short", &[], false).is_empty());
    }

    #[test]
    fn redacts_tokens_and_known_secrets() {
        let text = "Bearer ebr_0123456789abcdef0123456789abcdef0123 and xat_g_abcdefghijklmnopqrstuvwxyz plus the-known-secret-1";
        let out = redact(text, &["the-known-secret-1".into()]);
        assert_eq!(out, "Bearer [redacted] and [redacted] plus [redacted]");
        assert_eq!(redact("plain ünïcode text", &[]), "plain ünïcode text");
    }
}

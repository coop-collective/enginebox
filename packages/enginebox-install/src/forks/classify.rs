//! Path classes and the secret scan for fork submissions (contract §6).
//!
//! The controller is authoritative; the node computes the same classes for
//! the preview it reports and to decide what a live capture may commit.
//! Paths are repository-relative with `/` separators; `project` is the
//! session's `project_path` (`P`). Comparisons ignore ASCII case, as NTFS does.
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    /// Never merged.
    Refused,
    /// Owner approval (repository plumbing).
    Protected,
    /// Owner approval (code execution in the Editor).
    Code,
    /// Automatic.
    Config,
    /// Automatic (textual three-way; overlap is a conflict).
    Scene,
    /// Automatic.
    Asset,
}
impl Class {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Refused => "refused",
            Self::Protected => "protected",
            Self::Code => "code",
            Self::Config => "config",
            Self::Scene => "scene",
            Self::Asset => "asset",
        }
    }
    /// Merged without the room owner's approval.
    pub fn automatic(self) -> bool {
        matches!(self, Self::Config | Self::Scene | Self::Asset)
    }
}

/// A rule hit. Never carries the matched value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub rule: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}
impl Finding {
    pub fn new(rule: &str, path: &str) -> Self {
        Self {
            rule: rule.into(),
            path: path.into(),
            line: None,
        }
    }
}

const CODE_EXTENSIONS: &[&str] = &[
    "cs", "asmdef", "asmref", "rsp", "dll", "so", "dylib", "jslib", "jspre", "mm", "m", "cpp", "c",
    "h", "hpp", "cc", "java", "kt", "aar", "jar", "py", "ps1", "psm1", "sh", "bat", "cmd", "exe",
    "js", "mjs", "cjs", "ts",
];
const SCENE_EXTENSIONS: &[&str] = &["unity", "prefab", "scenetemplate"];
/// Names refused anywhere in a path (as EngineBox's LayerFS import refuses).
const PRIVATE_NAMES: &[&str] = &[
    ".git",
    ".ssh",
    ".aws",
    ".azure",
    ".claude",
    ".codex",
    ".cursor",
    ".npmrc",
    ".pypirc",
    ".netrc",
    ".git-credentials",
    "credentials.json",
    "auth.json",
];
const PROTECTED_ROOT: &[&str] = &[
    ".gitattributes",
    ".lfsconfig",
    ".gitmodules",
    ".gitignore",
    ".github",
];
const PACKAGE_FILES: &[&str] = &[
    "package.json",
    "package-lock.json",
    "pnpm-lock.yaml",
    "pnpm-workspace.yaml",
    "yarn.lock",
    "bun.lockb",
    ".yarnrc.yml",
];

fn extension(name: &str) -> &str {
    name.rsplit_once('.').map(|(_, e)| e).unwrap_or("")
}

/// The project-relative remainder of `path` when it is inside `project`.
fn inside<'a>(path: &'a str, project: &str) -> Option<&'a str> {
    let project = project.trim_matches('/');
    if project.is_empty() {
        return Some(path);
    }
    let lower = path.to_ascii_lowercase();
    let p = project.to_ascii_lowercase();
    (lower.len() > p.len() + 1 && lower.starts_with(&p) && lower.as_bytes()[p.len()] == b'/')
        .then(|| &path[p.len() + 1..])
}

fn credential_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    PRIVATE_NAMES.contains(&n.as_str())
        || (n == ".env" || n.starts_with(".env."))
            && ![".env.example", ".env.sample", ".env.template"].contains(&n.as_str())
        || [
            "id_rsa",
            "id_dsa",
            "id_ecdsa",
            "id_ed25519",
            "id_ecdsa_sk",
            "id_ed25519_sk",
        ]
        .iter()
        .any(|k| n == *k || n == format!("{k}.pub"))
        || matches!(extension(&n), "pem" | "key" | "pfx" | "p12")
        || n.contains("credentials") && n.ends_with(".json")
}

/// The class of one path with the rule that decided it. `mode` is the git
/// mode (`0o120000` symlink, `0o160000` submodule); pass `0o100644` when
/// unknown.
pub fn classify(path: &str, mode: u32, project: &str) -> (Class, &'static str) {
    if mode == 0o120000 {
        return (Class::Refused, "symlink");
    }
    if mode == 0o160000 {
        return (Class::Refused, "submodule");
    }
    if path.is_empty()
        || path.starts_with('/')
        || path.contains(['\\', '\0', '\n', '\r'])
        || path
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
    {
        return (Class::Refused, "unsafe-path");
    }
    let components: Vec<&str> = path.split('/').collect();
    let name = components[components.len() - 1];
    if components.iter().any(|c| c.eq_ignore_ascii_case(".git")) {
        return (Class::Refused, "git-dir");
    }
    if components.iter().any(|c| credential_name(c)) {
        return (Class::Refused, "credential-path");
    }
    let lower = path.to_ascii_lowercase();
    let in_project = inside(path, project);
    if let Some(rest) = in_project {
        let rest_lower = rest.to_ascii_lowercase();
        if rest_lower == "projectsettings/projectversion.txt"
            || rest_lower == "projectsettings/projectversion.txt.meta"
        {
            return (Class::Refused, "editor-version");
        }
        let top = rest_lower.split('/').next().unwrap_or("");
        if ["library", "temp", "logs", "obj", "usersettings"].contains(&top) {
            return (Class::Refused, "generated");
        }
    }
    // Repository plumbing at the root, and package manifests outside P.
    if !components.is_empty()
        && PROTECTED_ROOT.contains(&components[0].to_ascii_lowercase().as_str())
        || PROTECTED_ROOT.contains(&name.to_ascii_lowercase().as_str())
    {
        return (Class::Protected, "repository");
    }
    if in_project.is_none() && PACKAGE_FILES.contains(&name.to_ascii_lowercase().as_str()) {
        return (Class::Protected, "packages");
    }
    let Some(rest) = in_project else {
        // Outside P only the root README and docs are allowed.
        let top = components[0].to_ascii_lowercase();
        if components.len() == 1 && top.starts_with("readme") || top == "docs" {
            return (Class::Asset, "docs");
        }
        return (Class::Refused, "outside-project");
    };
    // A .meta has the class of what it describes.
    if lower.ends_with(".meta") && lower.len() > 5 {
        let (class, rule) = classify(&path[..path.len() - 5], 0o100644, project);
        return (class, rule);
    }
    let rest_lower = rest.to_ascii_lowercase();
    let rest_parts: Vec<&str> = rest_lower.split('/').collect();
    let ext = extension(rest_parts[rest_parts.len() - 1]).to_owned();
    if rest_parts[0] == "packages" {
        return (Class::Code, "packages");
    }
    if CODE_EXTENSIONS.contains(&ext.as_str()) {
        return (Class::Code, "code-file");
    }
    if rest_parts
        .iter()
        .any(|c| *c == "editor" || *c == "plugins" || c.ends_with(".bundle"))
    {
        return (Class::Code, "code-folder");
    }
    if rest_parts[0] == "projectsettings" {
        return (Class::Config, "project-settings");
    }
    if ext == "inputactions" {
        return (Class::Config, "input-actions");
    }
    if SCENE_EXTENSIONS.contains(&ext.as_str()) {
        return (Class::Scene, "scene");
    }
    if rest_parts[0] == "assets" && rest_parts.len() > 1 {
        return (Class::Asset, "asset");
    }
    // Anything else inside P (solution files, root-level project files).
    (Class::Protected, "project-root")
}

/// Per-class path counts for a submission.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Classes {
    pub scene: usize,
    pub asset: usize,
    pub config: usize,
    pub code: usize,
    pub protected: usize,
    pub refused: usize,
}
impl Classes {
    pub fn add(&mut self, class: Class) {
        match class {
            Class::Scene => self.scene += 1,
            Class::Asset => self.asset += 1,
            Class::Config => self.config += 1,
            Class::Code => self.code += 1,
            Class::Protected => self.protected += 1,
            Class::Refused => self.refused += 1,
        }
    }
    /// Needs the room owner's approval.
    pub fn needs_owner(&self) -> bool {
        self.code > 0 || self.protected > 0
    }
}

// ---------------------------------------------------------------- secret scan

struct Rule {
    name: &'static str,
    pattern: Regex,
}
static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    let rule = |name, pattern: &str| Rule {
        name,
        pattern: Regex::new(pattern).expect("secret pattern"),
    };
    vec![
        rule(
            "xinf-token",
            r"\b(?:xk_live|xat|xrt|xak)_[A-Za-z0-9_\-]{16,}",
        ),
        rule("enginebox-token", r"\b(?:ebr|ehp|ebst)_[A-Za-z0-9]{16,}"),
        rule("anthropic-key", r"\bsk-ant-[A-Za-z0-9_\-]{16,}"),
        rule("openai-key", r"\bsk-(?:proj-|svcacct-)?[A-Za-z0-9_\-]{20,}"),
        rule(
            "github-token",
            r"\b(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{30,})",
        ),
        rule("aws-access-key", r"\bAKIA[0-9A-Z]{16}\b"),
        rule("private-key", r"-----BEGIN [A-Z ]*PRIVATE KEY-----"),
        rule(
            "jwt",
            r"\beyJ[A-Za-z0-9_\-]{8,}\.eyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}",
        ),
        rule("slack-token", r"\bxox[abprs]-[A-Za-z0-9\-]{10,}"),
        rule("google-api-key", r"\bAIza[0-9A-Za-z_\-]{35}\b"),
    ]
});
static LONG_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z0-9+/=_\-]{40,}").expect("token pattern"));

/// Shannon entropy in bits per character.
fn entropy(s: &str) -> f64 {
    let mut counts = [0u32; 256];
    for b in s.bytes() {
        counts[b as usize] += 1;
    }
    let n = s.len() as f64;
    counts
        .iter()
        .filter(|c| **c > 0)
        .map(|c| {
            let p = f64::from(*c) / n;
            -p * p.log2()
        })
        .sum()
}

fn entropy_checked(path: &str) -> bool {
    matches!(
        extension(&path.to_ascii_lowercase()),
        "json" | "asset" | "txt" | "yaml" | "yml" | "env" | "ini" | "cfg" | "xml"
    )
}

/// Scan added text lines of `path`. `known` are exact secret values the node
/// holds for this session or run (only values of 8+ characters count).
/// Returns one finding per rule and line; never the value.
pub fn scan_lines<'a>(
    path: &str,
    lines: impl IntoIterator<Item = (u32, &'a str)>,
    known: &[&str],
) -> Vec<Finding> {
    let entropy_file = entropy_checked(path);
    let mut out = Vec::new();
    for (line, text) in lines {
        if text.len() > 64 * 1024 {
            // Minified or generated data: only exact secrets.
            for k in known.iter().filter(|k| k.len() >= 8) {
                if text.contains(*k) {
                    out.push(Finding {
                        rule: "known-secret".into(),
                        path: path.into(),
                        line: Some(line),
                    });
                }
            }
            continue;
        }
        let mut hit = |rule: &str| {
            if !out
                .iter()
                .any(|f: &Finding| f.rule == rule && f.line == Some(line))
            {
                out.push(Finding {
                    rule: rule.into(),
                    path: path.into(),
                    line: Some(line),
                });
            }
        };
        if known.iter().any(|k| k.len() >= 8 && text.contains(*k)) {
            hit("known-secret");
        }
        for rule in RULES.iter() {
            if rule.pattern.is_match(text) {
                hit(rule.name);
            }
        }
        if entropy_file
            && LONG_TOKEN.find_iter(text).any(|m| {
                let s = m.as_str();
                // Hex (GUIDs, hashes) tops out at 4 bits per character.
                !s.bytes().all(|b| b.is_ascii_hexdigit()) && entropy(s) > 4.5
            })
        {
            hit("high-entropy");
        }
        if out.len() > 200 {
            break;
        }
    }
    out
}

/// Replace every known secret (8+ characters) and every token-shaped string
/// the secret rules recognise with `[redacted]`.
pub fn redact(text: &str, known: &[&str]) -> String {
    let mut out = text.to_owned();
    let mut values: Vec<&str> = known.iter().copied().filter(|k| k.len() >= 8).collect();
    values.sort_by_key(|v| std::cmp::Reverse(v.len()));
    for v in values {
        if out.contains(v) {
            out = out.replace(v, "[redacted]");
        }
    }
    for rule in RULES.iter() {
        if rule.pattern.is_match(&out) {
            out = rule.pattern.replace_all(&out, "[redacted]").into_owned();
        }
    }
    out
}

/// Parse a `git diff -U0` text into (path, line number, added line) and scan
/// it. Binary files produce no lines.
pub fn scan_diff(diff: &str, known: &[&str]) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut path: Option<String> = None;
    let mut line_no: u32 = 0;
    let mut batch: Vec<(u32, String)> = Vec::new();
    let mut flush = |path: &Option<String>, batch: &mut Vec<(u32, String)>| {
        if let Some(p) = path {
            findings.extend(scan_lines(
                p,
                batch.iter().map(|(n, l)| (*n, l.as_str())),
                known,
            ));
        }
        batch.clear();
    };
    for raw in diff.lines() {
        if let Some(rest) = raw.strip_prefix("+++ ") {
            flush(&path, &mut batch);
            path = rest.strip_prefix("b/").map(str::to_owned);
            continue;
        }
        if raw.starts_with("diff --git ") {
            flush(&path, &mut batch);
            path = None;
            continue;
        }
        if let Some(h) = raw.strip_prefix("@@ ") {
            // @@ -a,b +c,d @@
            line_no = h
                .split_whitespace()
                .find_map(|t| t.strip_prefix('+'))
                .and_then(|t| t.split(',').next())
                .and_then(|t| t.parse().ok())
                .unwrap_or(0);
            continue;
        }
        if let Some(added) = raw.strip_prefix('+') {
            batch.push((line_no, added.to_owned()));
            line_no += 1;
        } else if raw.starts_with(' ') {
            line_no += 1;
        }
    }
    flush(&path, &mut batch);
    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &str = "unity-project";

    #[test]
    fn classes_follow_the_contract_table() {
        let c = |p: &str| classify(p, 0o100644, P).0;
        assert_eq!(c("unity-project/Assets/Scenes/Main.unity"), Class::Scene);
        assert_eq!(
            c("unity-project/Assets/Scenes/Main.unity.meta"),
            Class::Scene
        );
        assert_eq!(c("unity-project/Assets/Prefabs/Card.prefab"), Class::Scene);
        assert_eq!(c("unity-project/Assets/Art/card.png"), Class::Asset);
        assert_eq!(c("unity-project/Assets/Art/card.png.meta"), Class::Asset);
        assert_eq!(c("unity-project/Assets/Data/levels.json"), Class::Asset);
        assert_eq!(c("unity-project/Assets/Materials/a.mat"), Class::Asset);
        assert_eq!(c("unity-project/Assets/Scripts/Game.cs"), Class::Code);
        assert_eq!(c("unity-project/Assets/Scripts/Game.cs.meta"), Class::Code);
        assert_eq!(c("unity-project/Assets/Scripts/Game.asmdef"), Class::Code);
        assert_eq!(c("unity-project/Assets/csc.rsp"), Class::Code);
        assert_eq!(c("unity-project/Assets/Editor/Tool.txt"), Class::Code);
        assert_eq!(c("unity-project/Assets/Editor.meta"), Class::Code);
        assert_eq!(c("unity-project/Assets/Plugins/x/lib.png"), Class::Code);
        assert_eq!(
            c("unity-project/Assets/Native.bundle/Contents/x"),
            Class::Code
        );
        assert_eq!(c("unity-project/Assets/Web/plugin.jslib"), Class::Code);
        assert_eq!(c("unity-project/Packages/manifest.json"), Class::Code);
        assert_eq!(
            c("unity-project/ProjectSettings/TagManager.asset"),
            Class::Config
        );
        assert_eq!(
            c("unity-project/Assets/Input/Controls.inputactions"),
            Class::Config
        );
        assert_eq!(
            c("unity-project/ProjectSettings/ProjectVersion.txt"),
            Class::Refused
        );
        assert_eq!(c("unity-project/Library/x.asset"), Class::Refused);
        assert_eq!(
            c("unity-project/UserSettings/Layouts/a.dwlt"),
            Class::Refused
        );
        assert_eq!(c(".gitattributes"), Class::Protected);
        assert_eq!(c(".lfsconfig"), Class::Protected);
        assert_eq!(c(".github/workflows/ci.yml"), Class::Protected);
        assert_eq!(c("package.json"), Class::Protected);
        assert_eq!(c("tools/pnpm-lock.yaml"), Class::Protected);
        assert_eq!(c("README.md"), Class::Asset);
        assert_eq!(c("docs/plan.md"), Class::Asset);
        assert_eq!(c("server/index.ts"), Class::Refused);
        assert_eq!(c("unity-project/Assembly-CSharp.csproj"), Class::Protected);
    }

    #[test]
    fn refusals_beat_everything() {
        let c = |p: &str, m: u32| classify(p, m, P);
        assert_eq!(c("unity-project/Assets/a.png", 0o120000).1, "symlink");
        assert_eq!(c("unity-project/Assets/sub", 0o160000).1, "submodule");
        for p in [
            "unity-project/Assets/.git/config",
            "unity-project/Assets/.GIT",
            "unity-project/Assets/.env",
            "unity-project/Assets/.env.local",
            "unity-project/Assets/server.pem",
            "unity-project/Assets/tls.key",
            "unity-project/Assets/cert.pfx",
            ".npmrc",
            "unity-project/.claude/settings.json",
            "unity-project/Assets/.codex/auth.json",
            "unity-project/Assets/id_ed25519",
            "unity-project/Assets/id_rsa.pub",
            "unity-project/Assets/gcp-credentials.json",
            "unity-project/Assets/../x",
            "unity-project//Assets/x",
            "unity-project/Assets/a\\b",
        ] {
            assert_eq!(c(p, 0o100644).0, Class::Refused, "{p}");
        }
        // Allowed lookalikes.
        assert_eq!(
            c("unity-project/Assets/.env.example", 0o100644).0,
            Class::Asset
        );
        assert_eq!(
            c("unity-project/Assets/Keys/keyframes.anim", 0o100644).0,
            Class::Asset
        );
        assert_eq!(
            c("unity-project/Assets/id_card.png", 0o100644).0,
            Class::Asset
        );
        assert_eq!(c("unity-project/Assets/id_card", 0o100644).0, Class::Asset);
    }

    #[test]
    fn a_project_at_the_root_has_no_outside() {
        assert_eq!(classify("Assets/a.png", 0o100644, "").0, Class::Asset);
        assert_eq!(
            classify("Packages/manifest.json", 0o100644, "").0,
            Class::Code
        );
        assert_eq!(classify(".gitignore", 0o100644, "").0, Class::Protected);
        assert_eq!(classify("Notes.txt", 0o100644, "").0, Class::Protected);
        // Case-insensitive like NTFS.
        assert_eq!(classify("ASSETS/Scripts/A.CS", 0o100644, "").0, Class::Code);
    }

    #[test]
    fn secret_scan_reports_rule_and_line_never_value() {
        let diff = "diff --git a/unity-project/Assets/cfg.json b/unity-project/Assets/cfg.json\n\
--- a/unity-project/Assets/cfg.json\n+++ b/unity-project/Assets/cfg.json\n@@ -1,0 +3,4 @@\n\
+{\"key\": \"xk_live_abcdefghijklmnop1234\"}\n\
+ok line\n\
+-----BEGIN OPENSSH PRIVATE KEY-----\n\
+\"blob\": \"Zx9Qa7Lm2Pw4Rt6Yu8Io0Kj3Hg5Fd1Sa7Zx9Qb2Nc4Vm6Bq8We0Rt\"\n\
diff --git a/unity-project/Assets/a.cs b/unity-project/Assets/a.cs\n\
--- a/unity-project/Assets/a.cs\n+++ b/unity-project/Assets/a.cs\n@@ -10 +10 @@\n\
-old\n+var t = \"sk-ant-api03-ABCDEFGHIJKLMNOPQRST\"; // ebr_0123456789abcdef0123\n";
        let found = scan_diff(diff, &["supersecretvalue"]);
        let rules: Vec<(String, Option<u32>, String)> = found
            .iter()
            .map(|f| (f.rule.clone(), f.line, f.path.clone()))
            .collect();
        assert!(rules.contains(&(
            "xinf-token".into(),
            Some(3),
            "unity-project/Assets/cfg.json".into()
        )));
        assert!(rules.contains(&(
            "private-key".into(),
            Some(5),
            "unity-project/Assets/cfg.json".into()
        )));
        assert!(rules.contains(&(
            "high-entropy".into(),
            Some(6),
            "unity-project/Assets/cfg.json".into()
        )));
        assert!(rules.contains(&(
            "anthropic-key".into(),
            Some(10),
            "unity-project/Assets/a.cs".into()
        )));
        assert!(rules.contains(&(
            "enginebox-token".into(),
            Some(10),
            "unity-project/Assets/a.cs".into()
        )));
        assert!(!rules.iter().any(|r| r.1 == Some(4)));
        let text = serde_json::to_string(&found).unwrap();
        assert!(!text.contains("abcdefghijklmnop1234") && !text.contains("ABCDEFGHIJ"));
    }

    #[test]
    fn redaction_removes_known_and_token_shaped_values() {
        let text = "key xat_abcdefghijklmnopqrstu and pw hunter2hunter2 and ok";
        assert_eq!(
            redact(text, &["hunter2hunter2", "short"]),
            "key [redacted] and pw [redacted] and ok"
        );
    }

    #[test]
    fn known_secrets_and_hex_are_handled() {
        let f = scan_lines(
            "unity-project/Assets/x.asset",
            [
                (1, "guid: 0123456789abcdef0123456789abcdef01234567abcdef"),
                (2, "token=my-node-only-secret-value"),
            ],
            &["my-node-only-secret-value", "short"],
        );
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].rule, "known-secret");
        assert_eq!(f[0].line, Some(2));
    }
}

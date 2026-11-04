//! The node's pre-scan of a tree before git reads it (contract §2.2): links,
//! reparse points, hard links, unsafe names and case-only collisions. It runs
//! on the paths git would record (`ls-files --modified --others`), plus every
//! directory above them, so an agent cannot point git at node-private files.
use super::classify::Finding;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

/// More changed files than this are refused.
pub const MAX_CHANGED_FILES: usize = 5000;
/// More added content than this is refused.
pub const MAX_ADDED_BYTES: u64 = 500 << 20;
/// A file this large must be tracked by Git LFS.
pub const MAX_FILE_BYTES: u64 = 100 << 20;

const RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9", "conin$",
    "conout$",
];

/// A path component that is unsafe on some engine machine's filesystem.
pub fn bad_name(name: &str) -> Option<&'static str> {
    if name.is_empty() || name == "." || name == ".." {
        return Some("unsafe-path");
    }
    if name.eq_ignore_ascii_case(".git") {
        return Some("git-dir");
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Some("trailing-dot-or-space");
    }
    if name.contains(':') {
        return Some("alternate-data-stream");
    }
    if name
        .chars()
        .any(|c| c < ' ' || matches!(c, '<' | '>' | '"' | '|' | '?' | '*' | '\\'))
    {
        return Some("unsafe-name");
    }
    let stem = name.split('.').next().unwrap_or("").to_ascii_lowercase();
    if RESERVED.contains(&stem.trim_end().to_owned().as_str()) {
        return Some("reserved-name");
    }
    None
}

/// What one filesystem entry is, without following links.
#[derive(Debug, PartialEq, Eq)]
pub enum Kind {
    Missing,
    File {
        links: u64,
        bytes: u64,
    },
    Dir,
    /// Symlink, junction or any other reparse point.
    Link,
    Other,
}

#[cfg(unix)]
pub fn kind(path: &Path) -> Kind {
    use std::os::unix::fs::MetadataExt;
    match std::fs::symlink_metadata(path) {
        Err(_) => Kind::Missing,
        Ok(m) if m.file_type().is_symlink() => Kind::Link,
        Ok(m) if m.is_dir() => Kind::Dir,
        Ok(m) if m.is_file() => Kind::File {
            links: m.nlink(),
            bytes: m.len(),
        },
        Ok(_) => Kind::Other,
    }
}

#[cfg(windows)]
pub fn kind(path: &Path) -> Kind {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    match std::fs::symlink_metadata(path) {
        Err(_) => Kind::Missing,
        Ok(m) if m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 => Kind::Link,
        Ok(m) if m.file_type().is_symlink() => Kind::Link,
        Ok(m) if m.is_dir() => Kind::Dir,
        Ok(m) if m.is_file() => Kind::File {
            links: super::windows::link_count(path).unwrap_or(u64::MAX),
            bytes: m.len(),
        },
        Ok(_) => Kind::Other,
    }
}

/// Check `paths` (repository-relative, `/`-separated) under `root`. Every
/// path's components are name-checked and every ancestor directory must be a
/// real directory. Existing entries must be regular files with one link (or
/// directories, which git lists for an untracked tree). Missing entries are
/// deletions and are fine.
pub fn scan(root: &Path, paths: &[String]) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut dirs: BTreeMap<String, bool> = BTreeMap::new();
    for path in paths {
        let parts: Vec<&str> = path.trim_end_matches('/').split('/').collect();
        if let Some(rule) = parts.iter().find_map(|p| bad_name(p)) {
            findings.push(Finding::new(rule, path));
            continue;
        }
        let mut ok = true;
        let mut prefix = String::new();
        for part in &parts[..parts.len() - 1] {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            let good = *dirs
                .entry(prefix.clone())
                .or_insert_with(|| matches!(kind(&join(root, &prefix)), Kind::Dir | Kind::Missing));
            if !good {
                findings.push(Finding::new("link-in-path", path));
                ok = false;
                break;
            }
        }
        if !ok {
            continue;
        }
        match kind(&join(root, path)) {
            Kind::Missing | Kind::Dir => {}
            Kind::File { links: 1, .. } => {}
            Kind::File { .. } => findings.push(Finding::new("hard-link", path)),
            Kind::Link => findings.push(Finding::new("symlink-or-reparse-point", path)),
            Kind::Other => findings.push(Finding::new("special-file", path)),
        }
    }
    findings
}

/// Paths that differ only by case (NTFS and APFS would merge them).
pub fn case_collisions<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<Finding> {
    let mut seen: BTreeMap<String, &str> = BTreeMap::new();
    let mut out = BTreeSet::new();
    for p in paths {
        // Every prefix: a directory and a file may collide too.
        let mut prefix = String::new();
        for part in p.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            let lower = prefix.to_lowercase();
            let original = &p[..prefix.len()];
            match seen.get(&lower) {
                Some(other) if *other != original => {
                    out.insert(original.to_owned());
                }
                Some(_) => {}
                None => {
                    seen.insert(lower, original);
                }
            }
        }
    }
    out.into_iter()
        .map(|p| Finding::new("case-collision", &p))
        .collect()
}

pub fn join(root: &Path, path: &str) -> std::path::PathBuf {
    path.split('/')
        .filter(|c| !c.is_empty())
        .fold(root.to_path_buf(), |d, c| d.join(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_unsafe_on_windows_are_refused() {
        for (name, rule) in [
            ("CON", "reserved-name"),
            ("nul.txt", "reserved-name"),
            ("Com1.asset", "reserved-name"),
            ("a.", "trailing-dot-or-space"),
            ("a ", "trailing-dot-or-space"),
            ("a:stream", "alternate-data-stream"),
            ("a\\b", "unsafe-name"),
            ("a?b", "unsafe-name"),
            (".Git", "git-dir"),
        ] {
            assert_eq!(bad_name(name), Some(rule), "{name}");
        }
        for name in [
            "console.cs",
            "nullable.asset",
            "a.b",
            ".gitignore",
            "Card.prefab",
        ] {
            assert_eq!(bad_name(name), None, "{name}");
        }
    }

    #[test]
    fn case_only_collisions_are_found() {
        let found = case_collisions([
            "Assets/a.png",
            "Assets/A.png",
            "Assets/Dir/x",
            "assets/dir2/y",
            "Assets/ok",
        ]);
        let paths: Vec<_> = found.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["Assets/A.png", "assets"]);
    }

    #[cfg(unix)]
    #[test]
    fn links_and_hard_links_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("Assets")).unwrap();
        std::fs::write(root.join("Assets/ok.txt"), "x").unwrap();
        std::fs::write(root.join("outside.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(root.join("outside.txt"), root.join("Assets/link.txt")).unwrap();
        std::fs::hard_link(root.join("outside.txt"), root.join("Assets/hard.txt")).unwrap();
        std::os::unix::fs::symlink(root.join("Assets"), root.join("Linked")).unwrap();
        let found = scan(
            root,
            &[
                "Assets/ok.txt".into(),
                "Assets/link.txt".into(),
                "Assets/hard.txt".into(),
                "Linked/ok.txt".into(),
                "Assets/deleted.txt".into(),
            ],
        );
        let rules: Vec<_> = found
            .iter()
            .map(|f| (f.path.as_str(), f.rule.as_str()))
            .collect();
        assert_eq!(
            rules,
            [
                ("Assets/link.txt", "symlink-or-reparse-point"),
                ("Assets/hard.txt", "hard-link"),
                ("Linked/ok.txt", "link-in-path"),
            ]
        );
    }
}

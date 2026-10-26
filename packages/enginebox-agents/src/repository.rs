//! Hosted Git repositories for native runs. A run that names a repository
//! receives a short-lived, repository-scoped write token, minted by the
//! controller at admission. The guest writes it into a git config file in
//! this run's private directory under the native home, as an `extraHeader`
//! that git sends only to that repository's remote URL prefix, and points the
//! native CLI process at the file with git's `GIT_CONFIG_COUNT` variables.
//! The token never reaches run records, logs, callbacks or launch events; the
//! directory is removed when the run ends.
use crate::{native, profile, security};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// Longest repository token a client may request (`POST .../tokens`).
pub const MAX_TOKEN_TTL_SECONDS: u64 = 86_400;
/// Shortest token lifetime (the Artifacts API minimum).
pub const MIN_TOKEN_TTL_SECONDS: u64 = 60;
/// Added to a run's timeout for its repository token, so a run that queues
/// briefly or finishes late can still push.
pub const RUN_TOKEN_MARGIN_SECONDS: u64 = 600;
/// Longest run token: the longest run plus the margin.
pub const MAX_RUN_TOKEN_TTL_SECONDS: u64 = native::MAX_RUN_SECONDS + RUN_TOKEN_MARGIN_SECONDS;
/// Where a run's git config lives, relative to the native home.
pub const RUNS_DIR: &str = ".local/state/enginebox/agent-runs/repositories";
/// The guest helper command for runs with a repository. Helpers that do not
/// list it in `--help` are refused before the run starts.
pub const GUEST_MODE: &str = "run-repository";
/// Variables the CLI process receives (Engine Box controls the `ENGINEBOX_`
/// prefix, so a caller can never set them).
pub const ENV_REMOTE: &str = "ENGINEBOX_REPO_REMOTE";
pub const ENV_DEFAULT_BRANCH: &str = "ENGINEBOX_REPO_DEFAULT_BRANCH";
/// Largest repository envelope the guest accepts on stdin.
pub const MAX_ENVELOPE_BYTES: usize = profile::MAX_ENVELOPE_BYTES + 8192;

/// Repository names: lowercase letters, digits, `.`, `_` and `-`, starting
/// with a letter or digit, at most 63 characters, not ending in `.git`. A
/// subset of the Artifacts naming rules that is also a safe file name.
pub fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    (1..=63).contains(&b.len())
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(c))
        && !name.ends_with(".git")
        && !name.ends_with(".lock")
        && !name.contains("..")
}

/// Parse a run's `repository` field: `<project>/<name>`.
pub fn parse_ref(value: &str) -> Result<(String, String)> {
    let parsed = value
        .split_once('/')
        .filter(|(project, name)| crate::pools::valid_project(project) && valid_name(name));
    let Some((project, name)) = parsed else {
        anyhow::bail!("repository must be <project>/<name> naming a project repository.");
    };
    Ok((project.to_owned(), name.to_owned()))
}

/// The lifetime of a run's write token: its timeout plus a margin, capped.
pub fn run_token_ttl(timeout_seconds: u64) -> u64 {
    timeout_seconds
        .saturating_add(RUN_TOKEN_MARGIN_SECONDS)
        .clamp(MIN_TOKEN_TTL_SECONDS, MAX_RUN_TOKEN_TTL_SECONDS)
}

/// An HTTPS remote without credentials, query or fragment, ending in `.git`,
/// and without characters that would need quoting in a git config section.
pub fn valid_remote(remote: &str) -> bool {
    let Ok(url) = url::Url::parse(remote) else {
        return false;
    };
    remote.len() <= 512
        && url.scheme() == "https"
        && url.host_str().is_some_and(|h| !h.is_empty())
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && url.path().ends_with(".git")
        && url.as_str() == remote
        && remote
            .bytes()
            .all(|b| (33..=126).contains(&b) && !b"\"\\[]".contains(&b))
}

/// Repository tokens are opaque printable ASCII without spaces, quotes or
/// backslashes (Artifacts: `art_v1_<hex>?expires=<unix seconds>`).
pub fn valid_token(token: &str) -> bool {
    (16..=512).contains(&token.len())
        && token
            .bytes()
            .all(|b| (33..=126).contains(&b) && b != b'"' && b != b'\\')
}

/// Branch names a git config or environment variable can carry safely.
pub fn valid_branch(branch: &str) -> bool {
    let b = branch.as_bytes();
    (1..=200).contains(&b.len())
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || b"._/-".contains(c))
        && !branch.contains("..")
        && !branch.ends_with('/')
        && !branch.ends_with(".lock")
}

/// Caller variables a repository run may not set: git's own config
/// injection, which would override or extend the run's scoped config.
pub fn reserved_env_key(key: &str) -> bool {
    key.starts_with("GIT_CONFIG") || key == "GIT_ASKPASS"
}

/// A repository token minted for one run, as the controller holds it. The
/// token is a secret: `Debug` never prints it and it is never serialized to
/// a record; `token_id` lets the controller revoke it when the run ends.
#[derive(Clone)]
pub struct Credential {
    pub remote: String,
    pub default_branch: String,
    pub token: String,
    pub token_id: Option<String>,
    pub expires_at: String,
}
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("remote", &self.remote)
            .field("default_branch", &self.default_branch)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}
impl Credential {
    /// What the guest receives.
    pub fn access(&self) -> Access {
        Access {
            remote: self.remote.clone(),
            default_branch: self.default_branch.clone(),
            token: self.token.clone(),
        }
    }
}

/// The repository part of a guest envelope.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Access {
    pub remote: String,
    pub default_branch: String,
    pub token: String,
}
impl std::fmt::Debug for Access {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Access")
            .field("remote", &self.remote)
            .field("default_branch", &self.default_branch)
            .finish()
    }
}
impl Access {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            valid_remote(&self.remote),
            "Repository remote must be an https URL ending in .git."
        );
        ensure!(
            valid_branch(&self.default_branch),
            "Invalid repository default branch."
        );
        ensure!(valid_token(&self.token), "Invalid repository token.");
        Ok(())
    }
}

/// A profile carried inside a repository envelope.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfilePart {
    pub profile: String,
    pub block: profile::ProviderProfile,
    pub files: BTreeMap<String, String>,
}

/// What the guest receives for a repository run (`run-repository`): the
/// request, an optional profile, and the repository access.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub request: native::RunRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<ProfilePart>,
    pub repository: Access,
}
impl Envelope {
    /// The profile part as the profile module's own envelope, if any.
    pub fn profile_envelope(&self) -> Option<profile::Envelope> {
        self.profile.as_ref().map(|p| profile::Envelope {
            request: self.request.clone(),
            profile: p.profile.clone(),
            block: p.block.clone(),
            files: p.files.clone(),
        })
    }
    /// Re-validate everything in the guest. Returns the profile's decoded files.
    pub fn validate(&self) -> Result<Option<BTreeMap<String, Vec<u8>>>> {
        self.request.validate()?;
        self.repository.validate()?;
        ensure!(
            !self.request.env.keys().any(|k| reserved_env_key(k)),
            "env cannot set git configuration variables in a repository run."
        );
        match self.profile_envelope() {
            Some(envelope) => {
                let files = envelope.validate()?;
                ensure!(
                    !envelope.block.env.keys().any(|k| reserved_env_key(k)),
                    "env cannot set git configuration variables in a repository run."
                );
                Ok(Some(files))
            }
            None => Ok(None),
        }
    }
}

/// The generated git config: one `http.<remote>.extraHeader`, so git sends
/// the token only to URLs under this repository's remote.
pub fn git_config(access: &Access) -> Result<String> {
    access.validate()?;
    Ok(format!(
        "# Generated by Engine Box for one run. Sends this repository's token only to its remote.\n[http \"{}\"]\n\textraHeader = \"Authorization: Bearer {}\"\n",
        access.remote, access.token
    ))
}

pub fn run_dir(home: &Path, run_id: &str) -> Result<PathBuf> {
    native::validate_run_id(run_id)?;
    Ok(home.join(RUNS_DIR).join(run_id))
}

/// Write the run's git config into its private directory under the native
/// home and return the file's path.
pub fn materialize(home: &Path, run_id: &str, access: &Access) -> Result<PathBuf> {
    let config = git_config(access)?;
    let root = run_dir(home, run_id)?;
    security::safe_path(home, &format!("{RUNS_DIR}/{run_id}/placeholder"), true)?;
    remove_run_dir(home, run_id)?;
    let result = (|| -> Result<PathBuf> {
        security::private_dir(&root)?;
        security::write_bytes(&root, "gitconfig", config.as_bytes(), 8192)?;
        Ok(root.join("gitconfig"))
    })();
    if result.is_err() {
        let _ = remove_run_dir(home, run_id);
    }
    result
}

/// Remove a run's git config. Links are removed, never followed.
pub fn remove_run_dir(home: &Path, run_id: &str) -> Result<()> {
    let root = run_dir(home, run_id)?;
    security::check_components(&root)?;
    match std::fs::symlink_metadata(&root) {
        Ok(m) => {
            ensure!(
                m.is_dir() && !m.file_type().is_symlink(),
                "Invalid run repository directory."
            );
            std::fs::remove_dir_all(&root)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// The variables a repository run adds to the CLI process: git includes the
/// run's config file (on top of the user's own), and the remote and default
/// branch are named for the agent.
pub fn environment(access: &Access, config: &Path) -> Result<BTreeMap<String, String>> {
    let path = config
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Invalid repository config path."))?;
    Ok([
        ("GIT_CONFIG_COUNT", "1".to_owned()),
        ("GIT_CONFIG_KEY_0", "include.path".to_owned()),
        ("GIT_CONFIG_VALUE_0", path.to_owned()),
        (ENV_REMOTE, access.remote.clone()),
        (ENV_DEFAULT_BRANCH, access.default_branch.clone()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect())
}

/// True when a guest helper's `--help` lists the repository command.
pub fn guest_supports(help: &str) -> bool {
    help.lines().any(|line| {
        line.split_whitespace()
            .next()
            .is_some_and(|word| word == GUEST_MODE)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn access() -> Access {
        Access {
            remote: "https://0123456789abcdef0123456789abcdef.artifacts.cloudflare.net/git/eb-alpha/game.git".into(),
            default_branch: "main".into(),
            token: "art_v1_0123456789abcdef0123456789abcdef01234567?expires=1760000000".into(),
        }
    }
    #[test]
    fn names_refs_and_ttls_are_bounded() {
        assert!(valid_name("game") && valid_name("my.repo_2-x"));
        for bad in [
            "",
            "Game",
            "-x",
            ".x",
            "a/b",
            "x.git",
            "a..b",
            &"a".repeat(64),
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
        assert_eq!(
            parse_ref("alpha/game").unwrap(),
            ("alpha".into(), "game".into())
        );
        for bad in [
            "alpha",
            "alpha/",
            "/game",
            "Alpha/game",
            "alpha/game/x",
            "alpha/x.git",
        ] {
            assert!(parse_ref(bad).is_err(), "{bad}");
        }
        assert_eq!(run_token_ttl(120), 720);
        assert_eq!(
            run_token_ttl(native::MAX_RUN_SECONDS),
            MAX_RUN_TOKEN_TTL_SECONDS
        );
        assert_eq!(run_token_ttl(u64::MAX), MAX_RUN_TOKEN_TTL_SECONDS);
    }
    #[test]
    fn the_git_config_scopes_the_header_to_the_remote() {
        let config = git_config(&access()).unwrap();
        assert_eq!(
            config,
            "# Generated by Engine Box for one run. Sends this repository's token only to its remote.\n[http \"https://0123456789abcdef0123456789abcdef.artifacts.cloudflare.net/git/eb-alpha/game.git\"]\n\textraHeader = \"Authorization: Bearer art_v1_0123456789abcdef0123456789abcdef01234567?expires=1760000000\"\n"
        );
        assert!(
            !config.contains("[http]"),
            "never a global http.extraHeader"
        );
        for remote in [
            "http://host/git/ns/r.git",
            "https://user:pw@host/git/ns/r.git",
            "https://host/git/ns/r.git?x=1",
            "https://host/git/ns/r",
            "https://host/git/\"ns/r.git",
        ] {
            let mut a = access();
            a.remote = remote.into();
            assert!(git_config(&a).is_err(), "{remote}");
        }
        let mut a = access();
        a.token = "abc\"\ninjected = 1 xxxxxxxxxxxxxx".into();
        assert!(git_config(&a).is_err());
        assert!(!format!("{a:?}").contains("injected"));
        assert!(!format!("{:?}", access()).contains("art_v1"));
    }
    #[test]
    fn materialize_writes_one_private_file_and_cleanup_removes_it() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().canonicalize().unwrap();
        let id = "a".repeat(32);
        let path = materialize(&home, &id, &access()).unwrap();
        assert_eq!(path, run_dir(&home, &id).unwrap().join("gitconfig"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            git_config(&access()).unwrap()
        );
        let env = environment(&access(), &path).unwrap();
        assert_eq!(env["GIT_CONFIG_COUNT"], "1");
        assert_eq!(env["GIT_CONFIG_KEY_0"], "include.path");
        assert_eq!(env["GIT_CONFIG_VALUE_0"], path.to_str().unwrap());
        assert_eq!(env[ENV_REMOTE], access().remote);
        assert_eq!(env[ENV_DEFAULT_BRANCH], "main");
        assert!(!env.values().any(|v| v.contains("art_v1")));
        remove_run_dir(&home, &id).unwrap();
        assert!(!run_dir(&home, &id).unwrap().exists());
        assert!(materialize(&home, "bad", &access()).is_err());
    }
    #[test]
    fn envelopes_refuse_git_config_injection_and_old_helpers_are_detected() {
        let request = native::RunRequest::parse(serde_json::json!({
            "provider":"claude","prompt":"hi","env":{"GIT_CONFIG_COUNT":"0"}
        }))
        .unwrap();
        let envelope = Envelope {
            request,
            profile: None,
            repository: access(),
        };
        assert!(envelope.validate().is_err());
        let mut ok = envelope.clone();
        ok.request.env.clear();
        assert!(ok.validate().unwrap().is_none());
        assert!(guest_supports(
            "Commands:\n  run WORKSPACE [ID]  x\n  run-repository WORKSPACE ID  y\n"
        ));
        assert!(!guest_supports(
            "Commands:\n  run WORKSPACE [ID]  x\n  run-profile WORKSPACE ID  y\n"
        ));
    }
}

//! Reads a repository's `.enginebox/config.json` at the head of its default
//! branch (docs/PROJECT-CONFIG.md, "Where the controller reads the config"),
//! so a run's repository profile follows the code without anyone registering
//! it. The read uses a short-lived read token presented through git's
//! environment config, never argv, and a blobless shallow fetch, so a large
//! repository costs one commit, its trees and one blob.

use anyhow::{Context, Result, bail, ensure};
use enginebox_agents::{process, repository};
use serde_json::Value;
use std::{collections::BTreeMap, path::Path, time::Duration};

/// The config file, relative to the repository root.
pub const CONFIG_PATH: &str = ".enginebox/config.json";
const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OUTPUT: usize = 128 * 1024;

/// What the default branch head holds.
#[derive(Debug, PartialEq)]
pub enum Head {
    /// The head is `commit`, which the caller already has.
    Unchanged,
    /// The head is `commit`; `config` is the file, or None without one.
    Fetched {
        commit: String,
        config: Option<Value>,
    },
}

fn auth_header(credential: &repository::Credential) -> String {
    use base64::Engine;
    match credential.auth {
        repository::Auth::Bearer => format!("Authorization: Bearer {}", credential.token),
        repository::Auth::GithubToken => format!(
            "Authorization: Basic {}",
            base64::engine::general_purpose::STANDARD
                .encode(format!("x-access-token:{}", credential.token))
        ),
    }
}

/// A clean git environment: no system or global config, no prompts, and the
/// token as an extra header scoped to the remote's URL.
fn git_env(credential: &repository::Credential, home: &Path) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert(
        "PATH".into(),
        std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into()),
    );
    env.insert("HOME".into(), home.display().to_string());
    env.insert("GIT_TERMINAL_PROMPT".into(), "0".into());
    env.insert("GIT_CONFIG_NOSYSTEM".into(), "1".into());
    env.insert("GIT_CONFIG_GLOBAL".into(), "/dev/null".into());
    env.insert("GIT_CONFIG_COUNT".into(), "1".into());
    env.insert(
        "GIT_CONFIG_KEY_0".into(),
        format!("http.{}.extraHeader", credential.remote),
    );
    env.insert("GIT_CONFIG_VALUE_0".into(), auth_header(credential));
    env
}

fn git(env: &BTreeMap<String, String>, cwd: &Path, args: &[&str]) -> Result<process::Output> {
    let mut argv = vec!["git".to_owned()];
    argv.extend(args.iter().map(|a| (*a).to_owned()));
    process::bounded(&argv, Some(env), Some(cwd), b"", TIMEOUT, MAX_OUTPUT, None)
}

fn ok(output: process::Output, what: &str) -> Result<String> {
    // git's own messages may echo the remote, never the header.
    ensure!(
        output.exit_code == 0 && !output.timed_out && !output.output_truncated,
        "git {what} failed"
    );
    Ok(output.output)
}

/// The default branch head and, when it differs from `known`, the config
/// file at that commit.
pub fn read(credential: &repository::Credential, known: Option<&str>) -> Result<Head> {
    let dir = tempfile::tempdir().context("Could not create a scratch directory")?;
    let env = git_env(credential, dir.path());
    let branch = format!("refs/heads/{}", credential.default_branch);
    let listed = ok(
        git(
            &env,
            dir.path(),
            &["ls-remote", "--", &credential.remote, &branch],
        )?,
        "ls-remote",
    )?;
    let Some(head) = listed.lines().find_map(|l| {
        l.split_once('\t')
            .filter(|(_, r)| *r == branch)
            .map(|(s, _)| s)
    }) else {
        bail!("The default branch has no commits");
    };
    ensure!(
        (40..=64).contains(&head.len()) && head.bytes().all(|b| b.is_ascii_hexdigit()),
        "git ls-remote returned no commit"
    );
    if known == Some(head) {
        return Ok(Head::Unchanged);
    }
    ok(git(&env, dir.path(), &["init", "-q"])?, "init")?;
    ok(
        git(
            &env,
            dir.path(),
            &["remote", "add", "origin", &credential.remote],
        )?,
        "remote add",
    )?;
    ok(
        git(
            &env,
            dir.path(),
            &[
                "fetch",
                "-q",
                "--depth=1",
                "--filter=blob:none",
                "--no-tags",
                "origin",
                &format!("+{branch}:refs/remotes/origin/head"),
            ],
        )?,
        "fetch",
    )?;
    // The branch may have moved since ls-remote: report what was fetched.
    let commit = ok(
        git(&env, dir.path(), &["rev-parse", "refs/remotes/origin/head"])?,
        "rev-parse",
    )?
    .trim()
    .to_owned();
    let object = format!("{commit}:{CONFIG_PATH}");
    if git(&env, dir.path(), &["cat-file", "-e", &object])?.exit_code != 0 {
        return Ok(Head::Fetched {
            commit,
            config: None,
        });
    }
    let text = ok(git(&env, dir.path(), &["show", &object])?, "show")?;
    let config = serde_json::from_str(&text)
        .with_context(|| format!("{CONFIG_PATH} at {commit} is not JSON"))?;
    Ok(Head::Fetched {
        commit,
        config: Some(config),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn sh(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn credential(remote: &Path) -> repository::Credential {
        repository::Credential {
            remote: format!("file://{}", remote.display()),
            default_branch: "main".into(),
            token: "tok".into(),
            token_id: None,
            expires_at: String::new(),
            auth: repository::Auth::Bearer,
        }
    }

    #[test]
    fn reads_the_config_at_the_head_then_reports_unchanged() {
        let t = tempfile::tempdir().unwrap();
        let work = t.path().join("work");
        std::fs::create_dir_all(work.join(".enginebox")).unwrap();
        sh(&work, &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("README"), "x").unwrap();
        sh(&work, &["add", "."]);
        sh(&work, &["commit", "-q", "-m", "one"]);
        sh(t.path(), &["clone", "-q", "--bare", "work", "remote.git"]);
        let remote = t.path().join("remote.git");
        let cred = credential(&remote);

        let Head::Fetched { commit, config } = read(&cred, None).unwrap() else {
            panic!("expected a fetch");
        };
        assert_eq!(config, None);
        assert_eq!(read(&cred, Some(&commit)).unwrap(), Head::Unchanged);

        std::fs::write(
            work.join(CONFIG_PATH),
            r#"{"schema_version":2,"agents":{"profiles":{"default":{"default":true}}}}"#,
        )
        .unwrap();
        sh(&work, &["add", "."]);
        sh(&work, &["commit", "-q", "-m", "two"]);
        sh(&work, &["push", "-q", remote.to_str().unwrap(), "main"]);
        let Head::Fetched {
            commit: next,
            config,
        } = read(&cred, Some(&commit)).unwrap()
        else {
            panic!("expected a fetch");
        };
        assert_ne!(next, commit);
        assert_eq!(config.unwrap()["schema_version"], 2);
    }

    #[test]
    fn the_token_is_a_header_scoped_to_the_remote() {
        let mut cred = credential(Path::new("/r.git"));
        cred.remote = "https://github.com/o/r.git".into();
        cred.auth = repository::Auth::GithubToken;
        let env = git_env(&cred, Path::new("/tmp"));
        assert_eq!(
            env["GIT_CONFIG_KEY_0"],
            "http.https://github.com/o/r.git.extraHeader"
        );
        assert!(env["GIT_CONFIG_VALUE_0"].starts_with("Authorization: Basic "));
    }
}

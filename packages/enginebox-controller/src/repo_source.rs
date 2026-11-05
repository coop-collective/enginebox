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

/// The automation files at a default branch head and what changed since the
/// commit the caller last read (docs/PROJECT-CONFIG.md, "Automations").
#[derive(Debug, PartialEq)]
pub struct Automations {
    pub commit: String,
    /// `(path, contents)` of each `.enginebox/automations/*.yml|yaml`.
    pub files: Vec<(String, String)>,
    /// Paths changed between the known commit and `commit`; None when there
    /// was no known commit or it could not be fetched (a force push).
    pub changed: Option<Vec<String>>,
}

/// Most changed paths read for push filters; more counts as "unknown".
const MAX_CHANGED: usize = 5000;

/// Read the default branch head's automation files when the head differs
/// from `known` (None when it does not).
pub fn read_automations(
    credential: &repository::Credential,
    known: Option<&str>,
) -> Result<Option<Automations>> {
    use crate::automations::{DIR, MAX_FILE_BYTES, MAX_FILES, id_of};
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
            .map(|(s, _)| s.to_owned())
    }) else {
        bail!("The default branch has no commits");
    };
    if known == Some(head.as_str()) {
        return Ok(None);
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
    let commit = ok(
        git(&env, dir.path(), &["rev-parse", "refs/remotes/origin/head"])?,
        "rev-parse",
    )?
    .trim()
    .to_owned();
    let tree = git(
        &env,
        dir.path(),
        &["ls-tree", "-z", "--full-tree", &commit, &format!("{DIR}/")],
    )?;
    let mut files = Vec::new();
    if tree.exit_code == 0 {
        for entry in tree.output.split('\0').filter(|e| !e.is_empty()) {
            // `<mode> <type> <object>\t<path>`: regular files only.
            let Some((meta, path)) = entry.split_once('\t') else {
                continue;
            };
            if !meta.starts_with("100644 blob") || id_of(path).is_none() {
                continue;
            }
            ensure!(
                files.len() < MAX_FILES,
                "More than {MAX_FILES} automation files in {DIR}"
            );
            let object = format!("{commit}:{path}");
            let size = ok(
                git(&env, dir.path(), &["cat-file", "-s", &object])?,
                "cat-file",
            )?;
            let text = if size.trim().parse::<usize>().unwrap_or(usize::MAX) > MAX_FILE_BYTES {
                // Parsed as an error by the caller, without reading it.
                format!(
                    "# larger than {MAX_FILE_BYTES} bytes\n{}",
                    "#".repeat(MAX_FILE_BYTES)
                )
            } else {
                ok(git(&env, dir.path(), &["show", &object])?, "show")?
            };
            files.push((path.to_owned(), text));
        }
    }
    let changed = known.and_then(|old| {
        let valid = (40..=64).contains(&old.len()) && old.bytes().all(|b| b.is_ascii_hexdigit());
        if !valid {
            return None;
        }
        let fetched = git(
            &env,
            dir.path(),
            &[
                "fetch",
                "-q",
                "--depth=1",
                "--filter=blob:none",
                "--no-tags",
                "origin",
                old,
            ],
        )
        .ok()?;
        if fetched.exit_code != 0 {
            return None;
        }
        let diff = git(
            &env,
            dir.path(),
            &["diff", "--name-only", "-z", "--no-renames", old, &commit],
        )
        .ok()?;
        if diff.exit_code != 0 || diff.output_truncated {
            return None;
        }
        let paths: Vec<String> = diff
            .output
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect();
        (paths.len() <= MAX_CHANGED).then_some(paths)
    });
    Ok(Some(Automations {
        commit,
        files,
        changed,
    }))
}

/// Whether `branch` exists on the remote.
pub fn branch_exists(credential: &repository::Credential, branch: &str) -> Result<bool> {
    ensure!(repository::valid_branch(branch), "Invalid branch");
    let dir = tempfile::tempdir().context("Could not create a scratch directory")?;
    let env = git_env(credential, dir.path());
    let reference = format!("refs/heads/{branch}");
    let listed = ok(
        git(
            &env,
            dir.path(),
            &["ls-remote", "--", &credential.remote, &reference],
        )?,
        "ls-remote",
    )?;
    Ok(listed
        .lines()
        .any(|l| l.split_once('\t').is_some_and(|(_, r)| r == reference)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn reads_automation_files_and_the_paths_a_push_changed() {
        let t = tempfile::tempdir().unwrap();
        let work = t.path().join("work");
        std::fs::create_dir_all(work.join(".enginebox/automations")).unwrap();
        std::fs::create_dir_all(work.join("docs")).unwrap();
        sh(&work, &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("README"), "x").unwrap();
        std::fs::write(
            work.join(".enginebox/automations/tidy.yml"),
            "prompt: tidy\non: {manual: true}\n",
        )
        .unwrap();
        std::fs::write(work.join(".enginebox/automations/notes.txt"), "ignored").unwrap();
        sh(&work, &["add", "."]);
        sh(&work, &["commit", "-q", "-m", "one"]);
        sh(t.path(), &["clone", "-q", "--bare", "work", "remote.git"]);
        let remote = t.path().join("remote.git");
        let cred = credential(&remote);

        let first = read_automations(&cred, None).unwrap().unwrap();
        assert_eq!(first.changed, None);
        assert_eq!(
            first.files,
            vec![(
                ".enginebox/automations/tidy.yml".to_owned(),
                "prompt: tidy\non: {manual: true}\n".to_owned()
            )]
        );
        assert_eq!(read_automations(&cred, Some(&first.commit)).unwrap(), None);

        std::fs::write(work.join("docs/a.md"), "doc").unwrap();
        sh(&work, &["add", "."]);
        sh(&work, &["commit", "-q", "-m", "two"]);
        sh(&work, &["push", "-q", remote.to_str().unwrap(), "main"]);
        let second = read_automations(&cred, Some(&first.commit))
            .unwrap()
            .unwrap();
        assert_eq!(second.changed, Some(vec!["docs/a.md".to_owned()]));
        assert_eq!(second.files.len(), 1);
        // An unknown previous commit (history rewritten) leaves the paths unknown.
        let third = read_automations(&cred, Some(&"0".repeat(40)))
            .unwrap()
            .unwrap();
        assert_eq!(third.changed, None);

        assert!(branch_exists(&cred, "main").unwrap());
        assert!(!branch_exists(&cred, "automation/x/2026-11-09").unwrap());
    }

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

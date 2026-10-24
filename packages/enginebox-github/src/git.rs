//! Explicit, single-branch Git transfer. Never auto-commit, merge or force push.
use crate::{Repository, full_name};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use enginebox_core::state;
use serde_json::{Value, json};
use std::{path::Path, process::Command, time::Duration};

pub fn branch(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 200
            && !value.starts_with(['-', '.', '/'])
            && !value.ends_with(['.', '/'])
            && !value.contains(['\\', '~', '^', ':', '?', '*', '[', ' ', '\x7f'])
            && !value.chars().any(char::is_control)
            && !value.contains("..")
            && !value.contains("@{")
            && value != "@"
            && value
                .split('/')
                .all(|p| !p.is_empty() && !p.starts_with('.') && !p.ends_with(".lock")),
        "Invalid Git branch"
    );
    Ok(())
}
fn run(path: &Path, token: &str, args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    command.env_clear();
    for key in [
        "PATH",
        "SystemRoot",
        "SYSTEMROOT",
        "WINDIR",
        "TMPDIR",
        "TEMP",
        "TMP",
    ] {
        if let Some(v) = std::env::var_os(key) {
            command.env(key, v);
        }
    }
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    command
        .current_dir(path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "http.https://github.com/.extraHeader")
        .env(
            "GIT_CONFIG_VALUE_0",
            format!(
                "Authorization: Basic {}",
                STANDARD.encode(format!("x-access-token:{token}"))
            ),
        )
        .args([
            "-c",
            "core.hooksPath=",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "credential.helper=",
            "-c",
            "protocol.allow=never",
            "-c",
            "protocol.https.allow=always",
            "-c",
            "http.followRedirects=false",
            "-c",
            "submodule.recurse=false",
            "-c",
            "init.templateDir=",
        ]);
    // HooksPath must be an actual empty directory, not the empty config value.
    let hooks = tempfile::tempdir()?;
    command
        .arg("-c")
        .arg(format!("core.hooksPath={}", hooks.path().display()));
    #[cfg(test)]
    if token == "fixture" {
        command.args(["-c", "protocol.file.allow=always"]);
    }
    command.args(args);
    let bytes = enginebox_core::transport::run_command(command, None, Duration::from_secs(120))
        .map_err(|_| anyhow::anyhow!("Git transfer refused or interrupted; check access, branch, dirty files and diverged history before retrying"))?;
    ensure!(bytes.len() <= 1024 * 1024, "Git result too large");
    Ok(String::from_utf8(bytes)?.trim().into())
}
pub fn sync(path: &Path, repo: &Repository, token: &str, direction: &str) -> Result<Value> {
    full_name(&repo.full_name)?;
    branch(&repo.branch)?;
    transfer(
        path,
        repo,
        token,
        direction,
        &format!("https://github.com/{}.git", repo.full_name),
    )
}
fn transfer(
    path: &Path,
    repo: &Repository,
    token: &str,
    direction: &str,
    remote: &str,
) -> Result<Value> {
    ensure!(["pull", "push"].contains(&direction), "Use pull or push");
    if !path.exists() {
        ensure!(direction == "pull", "Pull this repository before pushing");
        let parent = path.parent().context("Missing checkout parent")?;
        let staging = tempfile::tempdir_in(parent)?;
        run(
            staging.path(),
            token,
            &[
                "clone",
                "--single-branch",
                "--branch",
                &repo.branch,
                "--",
                remote,
                "checkout",
            ],
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                staging.path().join("checkout"),
                std::fs::Permissions::from_mode(0o700),
            )?;
        }
        std::fs::rename(staging.path().join("checkout"), path)?;
    } else {
        state::private_dir(path)?;
        let metadata = std::fs::symlink_metadata(path)?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "Invalid checkout directory"
        );
        ensure!(
            std::fs::symlink_metadata(path.join(".git"))?.is_dir(),
            "Managed checkout must have its own Git directory"
        );
        ensure!(
            run(path, token, &["symbolic-ref", "--short", "HEAD"])? == repo.branch,
            "Checkout branch changed"
        );
        ensure!(
            run(
                path,
                token,
                &["status", "--porcelain", "--untracked-files=normal"]
            )?
            .is_empty(),
            "Commit or stash local changes before syncing"
        );
        if direction == "pull" {
            run(
                path,
                token,
                &[
                    "fetch",
                    "--no-tags",
                    "--no-recurse-submodules",
                    "--",
                    remote,
                    &format!("refs/heads/{}", repo.branch),
                ],
            )?;
            run(
                path,
                token,
                &[
                    "merge",
                    "--ff-only",
                    "--no-edit",
                    "--no-overwrite-ignore",
                    "FETCH_HEAD",
                ],
            )?;
        } else {
            run(
                path,
                token,
                &[
                    "push",
                    "--no-follow-tags",
                    "--",
                    remote,
                    &format!("HEAD:refs/heads/{}", repo.branch),
                ],
            )?;
        }
    }
    state::private_dir(path)?;
    Ok(
        json!({"repository_id":repo.id,"direction":direction,"branch":repo.branch,"commit":run(path,token,&["rev-parse","HEAD"])?,"checkout":path}),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    fn command(path: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .current_dir(path)
                .args(args)
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    #[test]
    fn invalid_refs_are_rejected() {
        for b in [
            "-option",
            "../x",
            "main:evil",
            "a.lock/x",
            "x@{y",
            "a//b",
            "a\nb",
        ] {
            assert!(branch(b).is_err(), "{b}");
        }
        branch("feature/safe").unwrap();
    }
    #[test]
    fn real_git_roundtrip_and_conflict_preservation() {
        let root = tempfile::tempdir().unwrap();
        let remote = root.path().join("remote");
        std::fs::create_dir(&remote).unwrap();
        command(&remote, &["init", "--bare", "-b", "main"]);
        let source = root.path().join("source");
        std::fs::create_dir(&source).unwrap();
        command(&source, &["init", "-b", "main"]);
        command(
            &source,
            &["config", "user.email", "fixture@example.invalid"],
        );
        command(&source, &["config", "user.name", "Fixture"]);
        std::fs::write(source.join("a"), "one").unwrap();
        command(&source, &["add", "a"]);
        command(&source, &["commit", "-m", "initial"]);
        command(&source, &["push", remote.to_str().unwrap(), "main"]);
        let repo = Repository {
            id: 1,
            installation_id: 1,
            full_name: "fixture/repo".into(),
            branch: "main".into(),
            private: true,
        };
        let fresh = root.path().join("fresh");
        transfer(&fresh, &repo, "fixture", "pull", remote.to_str().unwrap()).unwrap();
        let config = std::fs::read_to_string(fresh.join(".git/config")).unwrap();
        assert!(!config.contains("Authorization") && !config.contains("x-access-token"));
        let checkout = root.path().join("checkout");
        // Git respects a private umask in deployment; set explicit mode for this fixture.
        std::fs::create_dir(&checkout).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&checkout, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        command(&checkout, &["clone", remote.to_str().unwrap(), "."]);
        transfer(
            &checkout,
            &repo,
            "fixture",
            "pull",
            remote.to_str().unwrap(),
        )
        .unwrap();
        command(
            &checkout,
            &["config", "user.email", "fixture@example.invalid"],
        );
        command(&checkout, &["config", "user.name", "Fixture"]);
        // A pull must not silently overwrite local ignored configuration.
        std::fs::write(checkout.join(".git/info/exclude"), "private-local\n").unwrap();
        std::fs::write(checkout.join("private-local"), "local-only").unwrap();
        std::fs::write(source.join("private-local"), "remote").unwrap();
        command(&source, &["add", "private-local"]);
        command(&source, &["commit", "-m", "tracked collision"]);
        command(&source, &["push", remote.to_str().unwrap(), "main"]);
        assert!(
            transfer(
                &checkout,
                &repo,
                "fixture",
                "pull",
                remote.to_str().unwrap()
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(checkout.join("private-local")).unwrap(),
            "local-only"
        );
        std::fs::remove_file(checkout.join("private-local")).unwrap();
        transfer(
            &checkout,
            &repo,
            "fixture",
            "pull",
            remote.to_str().unwrap(),
        )
        .unwrap();
        std::fs::write(checkout.join("b"), "two").unwrap();
        assert!(
            transfer(
                &checkout,
                &repo,
                "fixture",
                "push",
                remote.to_str().unwrap()
            )
            .is_err()
        );
        command(&checkout, &["add", "b"]);
        command(&checkout, &["commit", "-m", "local"]);
        transfer(
            &checkout,
            &repo,
            "fixture",
            "push",
            remote.to_str().unwrap(),
        )
        .unwrap();
        std::fs::write(source.join("c"), "diverged").unwrap();
        command(&source, &["add", "c"]);
        command(&source, &["commit", "-m", "diverged"]);
        command(
            &source,
            &["push", "--force", remote.to_str().unwrap(), "main"],
        );
        assert!(
            transfer(
                &checkout,
                &repo,
                "fixture",
                "pull",
                remote.to_str().unwrap()
            )
            .is_err()
        );
        assert!(checkout.join("b").exists());
        assert!(
            transfer(
                &checkout,
                &repo,
                "fixture",
                "push",
                remote.to_str().unwrap()
            )
            .is_err()
        );
    }
}

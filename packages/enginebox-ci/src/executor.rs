use crate::model::{Job, Node, Request, Shell};
use anyhow::{Context, Result, ensure};
use enginebox_core::{
    state,
    transport::{run_command, run_command_bounded, run_process},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Execution {
    pub node: Node,
    pub request: Request,
}

/// Remote transport and the executable entry point share one versioned request.
pub fn execute_on(node: &Node, request: &Request) -> Result<Value> {
    node.validate()?;
    request.validate()?;
    if node.target == "local" {
        return execute(node, request);
    }
    let Job::Automation {
        timeout_minutes, ..
    } = &request.job
    else {
        anyhow::bail!("Only automations execute over SSH");
    };
    let argv = vec![
        "ssh".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "StrictHostKeyChecking=yes".into(),
        "-o".into(),
        "ConnectTimeout=10".into(),
        "--".into(),
        node.target.clone(),
        "ebox ci execute".into(),
    ];
    let bytes = run_process(
        &argv,
        Some(&serde_json::to_vec(&Execution {
            node: node.clone(),
            request: request.clone(),
        })?),
        Duration::from_secs(timeout_minutes * 60 + 30),
    )?;
    let value: Value = serde_json::from_slice(&bytes)?;
    ensure!(
        value["ok"] == true && value["request_id"] == request.request_id,
        "Invalid CI worker result"
    );
    Ok(value)
}

pub fn root() -> Result<PathBuf> {
    let path = PathBuf::from(
        std::env::var_os("ENGINEBOX_CI_ROOT")
            .context("Set ENGINEBOX_CI_ROOT to a dedicated private worker directory")?,
    );
    ensure!(path.is_absolute(), "CI worker directory must be absolute");
    state::private_dir(&path)?;
    Ok(path.canonicalize()?)
}

/// Deliberately excludes controller, GitHub and provider credentials. This is a
/// dedicated trusted worker, not a sandbox against hostile native build scripts.
pub fn command(program: &str, cwd: &Path, home: &Path) -> Command {
    let mut command = Command::new(program);
    command.env_clear().current_dir(cwd);
    for key in [
        "PATH",
        "SystemRoot",
        "WINDIR",
        "COMSPEC",
        "PATHEXT",
        "JAVA_HOME",
        "ANDROID_HOME",
        "ANDROID_SDK_ROOT",
        "DEVELOPER_DIR",
        "SDKROOT",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("TMPDIR", home)
        .env("TMP", home)
        .env("TEMP", home)
        .env("CI", "true")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    command
}

pub fn execute(node: &Node, request: &Request) -> Result<Value> {
    execute_at(node, request, &root()?)
}
pub fn execute_at(node: &Node, request: &Request, root: &Path) -> Result<Value> {
    node.require_local()?;
    request.validate()?;
    let Job::Automation {
        runs_on,
        node: selected,
        checkout,
        shell,
        run,
        artifacts,
        timeout_minutes,
    } = &request.job
    else {
        anyhow::bail!("Expected automation");
    };
    ensure!(
        selected.as_deref().is_none_or(|id| id == node.id) && node.matches(runs_on),
        "Worker does not match all requested capabilities"
    );
    state::private_dir(root)?;
    let _lock = state::lock(&root.join("worker.lock"), false)?;
    let tmp = tempfile::Builder::new().prefix("job-").tempdir_in(root)?;
    let home = tmp.path().join("home");
    let workspace = tmp.path().join("workspace");
    state::private_dir(&home)?;
    state::private_dir(&workspace)?;
    let deadline = Instant::now() + Duration::from_secs(timeout_minutes * 60);
    let remaining = || {
        deadline
            .checked_duration_since(Instant::now())
            .context("CI job timed out")
    };
    if let Some(checkout) = checkout {
        for args in [
            vec!["init", "--quiet"],
            vec!["remote", "add", "origin", &checkout.url],
            vec![
                "-c",
                "http.followRedirects=false",
                "fetch",
                "--depth=1",
                "origin",
                &request.sha,
            ],
            vec!["checkout", "--detach", "--quiet", "FETCH_HEAD"],
        ] {
            let mut cmd = command("git", &workspace, &home);
            cmd.args(args);
            run_command(cmd, None, remaining()?)?;
        }
        let mut cmd = command("git", &workspace, &home);
        cmd.args(["rev-parse", "HEAD"]);
        let actual = run_command(cmd, None, remaining()?)?;
        ensure!(
            String::from_utf8_lossy(&actual)
                .trim()
                .eq_ignore_ascii_case(&request.sha),
            "Checkout commit differs from trigger"
        );
    }
    let script = tmp.path().join(match shell {
        Shell::Bash => "job.sh",
        Shell::Pwsh => "job.ps1",
    });
    let source = match shell {
        Shell::Bash => format!("set -euo pipefail\n{run}\n"),
        Shell::Pwsh => format!(
            "$ErrorActionPreference = 'Stop'\n$PSNativeCommandUseErrorActionPreference = $true\n{run}\nif ($LASTEXITCODE) {{ exit $LASTEXITCODE }}\n"
        ),
    };
    std::fs::write(&script, source)?;
    let mut cmd = match shell {
        Shell::Bash => {
            let mut cmd = command("bash", &workspace, &home);
            cmd.args(["--noprofile", "--norc"]);
            cmd
        }
        Shell::Pwsh => {
            let mut cmd = command("pwsh", &workspace, &home);
            cmd.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-File"]);
            cmd
        }
    };
    // Windows canonical paths use the \\?\ prefix, which PowerShell's
    // RemoteSigned authorization can misclassify as a remote script. Resolve
    // our fixed sibling script from the workspace without changing host policy.
    let script_argument = match shell {
        Shell::Pwsh => Path::new("..").join("job.ps1"),
        Shell::Bash => script,
    };
    cmd.arg(script_argument)
        .env("ENGINEBOX_SHA", &request.sha)
        .env("ENGINEBOX_REQUEST_ID", &request.request_id)
        .env(
            "ENGINEBOX_EVENT",
            serde_json::to_value(&request.event)?.as_str().unwrap(),
        );
    let output = run_command_bounded(cmd, None, remaining()?, 16 * 1024 * 1024, 16 * 1024 * 1024)?;
    let artifact_root = retain_artifacts(root, &workspace, &request.request_id, artifacts)?;
    // Keep durable job records bounded. Failure output stays out of controller errors.
    let output = String::from_utf8_lossy(&output);
    Ok(
        json!({"ok":true,"runner":"automation","request_id":request.request_id,"node_id":node.id,"sha":request.sha,"stdout":output.chars().take(16384).collect::<String>(),"output_truncated":output.chars().count()>16384,"artifacts":artifacts,"artifact_root":artifact_root}),
    )
}

fn retain_artifacts(
    root: &Path,
    workspace: &Path,
    request_id: &str,
    paths: &[String],
) -> Result<Option<String>> {
    if paths.is_empty() {
        return Ok(None);
    }
    let mut files = 0;
    let mut bytes = 0;
    fn inspect(path: &Path, files: &mut u64, bytes: &mut u64) -> Result<()> {
        let meta =
            std::fs::symlink_metadata(path).context("A required build artifact is missing")?;
        ensure!(
            !meta.file_type().is_symlink() && (meta.is_dir() || meta.is_file()),
            "Artifacts must be regular files/directories, without symlinks"
        );
        *files += 1;
        *bytes = bytes.saturating_add(meta.len());
        ensure!(
            *files <= 100_000 && *bytes <= 10 * 1024 * 1024 * 1024,
            "Artifacts exceed 100,000 entries or 10 GiB"
        );
        if meta.is_dir() {
            for entry in std::fs::read_dir(path)? {
                inspect(&entry?.path(), files, bytes)?;
            }
        }
        Ok(())
    }
    for path in paths {
        // Check every ancestor: a regular leaf under a symlink is still outside the checkout.
        let mut current = workspace.to_owned();
        for component in Path::new(path).components() {
            current.push(component);
            ensure!(
                !std::fs::symlink_metadata(&current)?
                    .file_type()
                    .is_symlink(),
                "Artifact path traverses a symlink"
            );
        }
        inspect(&current, &mut files, &mut bytes)?;
        ensure!(
            !paths
                .iter()
                .any(|other| other != path && path.starts_with(&format!("{other}/"))),
            "Artifact paths must not overlap"
        );
    }
    let parent = root.join("artifacts");
    state::private_dir(&parent)?;
    let target = parent.join(request_id);
    ensure!(
        !target.try_exists()?,
        "Artifacts for this request already exist; never overwrite a previous build"
    );
    let staging = tempfile::Builder::new()
        .prefix(".staging-")
        .tempdir_in(&parent)?;
    for path in paths {
        let destination = staging.path().join(path);
        std::fs::create_dir_all(destination.parent().unwrap())?;
        std::fs::rename(workspace.join(path), &destination)?;
    }
    std::fs::rename(staging.path(), target)?;
    Ok(Some(format!("artifacts/{request_id}")))
}

/// Configures one ephemeral official runner, then serves one GitHub job. Both
/// runner modes use the same node-local lock to avoid overlapping allocations.
pub fn github_runner(node: &Node, directory: &Path, repository: &str) -> Result<Value> {
    node.require_local()?;
    ensure!(crate::model::repository(repository), "Use owner/repository");
    let root = root()?;
    let _lock = state::lock(&root.join("worker.lock"), false)?;
    let directory = directory.canonicalize()?;
    state::private_dir(&directory)?;
    ensure!(
        !directory.join(".runner").exists() && !directory.join(".credentials").exists(),
        "Use a fresh official runner installation; existing registration is never replaced"
    );
    let binary = directory.join(if cfg!(windows) {
        "bin/Runner.Listener.exe"
    } else {
        "bin/Runner.Listener"
    });
    ensure!(
        binary.is_file(),
        "Install the official GitHub Actions runner archive first"
    );
    let token = std::env::var("ENGINEBOX_CI_REGISTRATION_TOKEN")
        .context("Set a transient ENGINEBOX_CI_REGISTRATION_TOKEN")?;
    ensure!(
        !token.is_empty() && !token.chars().any(char::is_control),
        "Invalid registration token"
    );
    let home = tempfile::Builder::new()
        .prefix("runner-home-")
        .tempdir_in(&root)?;
    let mut config = command(
        binary.to_str().context("Invalid runner path")?,
        &directory,
        home.path(),
    );
    config
        .args([
            "configure",
            "--unattended",
            "--ephemeral",
            "--disableupdate",
            "--url",
            &format!("https://github.com/{repository}"),
            "--name",
            &format!("enginebox-{}", node.id),
            "--labels",
            &node.labels().into_iter().collect::<Vec<_>>().join(","),
            "--work",
            "_work",
        ])
        .env("ACTIONS_RUNNER_INPUT_TOKEN", token);
    run_command(config, None, Duration::from_secs(120)).map_err(|_| {
        anyhow::anyhow!("GitHub runner registration failed; inspect its private diagnostics")
    })?;
    let mut listener = command(binary.to_str().unwrap(), &directory, home.path());
    listener.arg("run");
    run_command(listener, None, Duration::from_secs(7 * 3600)).map_err(|_| anyhow::anyhow!("GitHub runner stopped unexpectedly; inspect its registration and private diagnostics before retrying"))?;
    Ok(
        json!({"ok":true,"runner":"github-actions","node_id":node.id,"ephemeral":true,"build_conclusion":"Read the workflow run from GitHub; listener exit is not a build result"}),
    )
}

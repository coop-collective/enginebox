//! Opt-in only, inside a new disposable Linux VM. A prepared immutable pnpm
//! generation and pinned SDK/image must already exist. No provider sign-in.
#![cfg(target_os = "linux")]
use anyhow::{Context, Result, ensure};
use enginebox_core::{state, transport::run_process};
use enginebox_storage_layerfs::{config::Config, source};
use serde_json::{Value, json};
use std::{
    fs,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone)]
struct Harness {
    binary: String,
    config: String,
    receipts: Arc<Mutex<Vec<Value>>>,
}
impl Harness {
    fn call(&self, args: &[&str]) -> Result<Value> {
        let mut command = vec![
            self.binary.clone(),
            "storage".into(),
            "layerfs".into(),
            "--config".into(),
            self.config.clone(),
        ];
        command.extend(args.iter().map(|s| (*s).into()));
        let start = Instant::now();
        let result = run_process(&command, None, Duration::from_secs(400));
        match result {
            Ok(bytes) => {
                let value: Value = serde_json::from_slice(&bytes)?;
                self.receipts.lock().unwrap().push(
                    json!({"argv":args,"seconds":start.elapsed().as_secs_f64(),"result":value}),
                );
                Ok(value)
            }
            Err(error) => {
                self.receipts.lock().unwrap().push(json!({"argv":args,"seconds":start.elapsed().as_secs_f64(),"error":format!("{error:#}")}));
                Err(error)
            }
        }
    }
}
#[test]
#[ignore = "requires explicit disposable VM opt-in, prepared SDK/cache and root"]
fn real_checkout_lifecycle() -> Result<()> {
    ensure!(
        std::env::var("ENGINEBOX_LAYERFS_LIVE").as_deref() == Ok("1"),
        "Explicit opt-in required"
    );
    let marker = std::env::var("ENGINEBOX_LAYERFS_MARKER")
        .unwrap_or_else(|_| "/etc/enginebox-layerfs-validation".into());
    ensure!(
        std::path::Path::new(&marker).is_absolute() && std::path::Path::new(&marker).is_file(),
        "Disposable fixture marker required"
    );
    if std::env::var_os("ENGINEBOX_LAYERFS_MARKER").is_some() {
        let expected = std::env::var("ENGINEBOX_LAYERFS_MARKER_CONTENT")?;
        ensure!(
            !expected.is_empty() && fs::read_to_string(&marker)?.trim() == expected,
            "Disposable fixture marker identity mismatch"
        );
    }
    let config_path = std::env::var("ENGINEBOX_LAYERFS_LIVE_CONFIG")?;
    let config = Config::load(std::path::Path::new(&config_path))?;
    ensure!(
        !config.root.exists() && config.max_workspaces == 2,
        "Use a fresh test root and exactly two workspace slots"
    );
    let report = PathBuf::from(std::env::var("ENGINEBOX_LAYERFS_REPORT")?);
    ensure!(
        report.is_absolute() && !report.exists(),
        "Fresh absolute private report path required"
    );
    let seed = PathBuf::from(std::env::var("ENGINEBOX_LAYERFS_SEED")?);
    let directory = report.parent().context("Report directory")?;
    state::private_dir(directory)?;
    let repo = directory.join("fixture");
    ensure!(!repo.exists(), "Fresh fixture required");
    fs::create_dir(&repo)?;
    for file in ["package.json", "pnpm-lock.yaml"] {
        fs::copy(seed.join(file), repo.join(file))?;
    }
    fs::write(repo.join(".gitignore"), "node_modules/\ndist/\n")?;
    fs::write(repo.join("hello.txt"), "base\n")?;
    source::git(&repo, &["init", "-b", "main"])?;
    source::git(&repo, &["add", "."])?;
    source::git(&repo, &["commit", "-m", "seed"])?;
    fs::write(repo.join(".env"), "synthetic-untracked-secret")?;
    source::git(
        &repo,
        &[
            "config",
            "http.extraHeader",
            "Authorization: synthetic-test-canary",
        ],
    )?;
    let h = Harness {
        binary: std::env::var("ENGINEBOX_LAYERFS_EBOX")?,
        config: config_path,
        receipts: Arc::new(Mutex::new(vec![])),
    };
    let result = (|| {
        h.call(&["doctor"])?;
        let preview = h.call(&["preview", "--repo", repo.to_str().unwrap()])?;
        let sha = preview["commit"].as_str().context("Commit")?;
        let approval = preview["approval"].as_str().context("Approval")?;
        ensure!(
            h.call(&[
                "import",
                "bad",
                "--repo",
                repo.to_str().unwrap(),
                "--revision",
                sha,
                "--approval",
                "wrong",
                "--credential-free"
            ])
            .is_err(),
            "Mismatched approval accepted"
        );
        h.call(&[
            "import",
            "fixture",
            "--repo",
            repo.to_str().unwrap(),
            "--revision",
            sha,
            "--approval",
            approval,
            "--credential-free",
        ])?;
        h.call(&["fork", "fixture", "a"])?;
        h.call(&["fork", "fixture", "b"])?;
        h.call(&["exec", "a", "--", "sh", "-c", "test ! -e /project/.git"])?;
        ensure!(
            h.call(&["fork", "fixture", "over-budget"]).is_err(),
            "Capacity exceeded"
        );
        let content=h.call(&["exec","a","--","sh","-c","set -eu; test \"$(pwd -P)\" = /project; test ! -e .env; test ! -e .git; test ! -r /layerfs-store/global/store.sqlite; test ! -S /var/run/docker.sock; cat hello.txt; printf 'agent-a\\n' > only-a.txt"])?;
        ensure!(
            content["result"]["stdout"]
                .as_str()
                .unwrap_or("")
                .contains("base"),
            "Fixture content mismatch"
        );
        h.call(&[
            "exec",
            "b",
            "--",
            "sh",
            "-c",
            "set -eu; test ! -e only-a.txt; printf 'agent-b\\n' > only-b.txt",
        ])?;
        ensure!(
            h.call(&["close", "a"]).is_err(),
            "Dirty close discarded changes"
        );
        let deps = h.call(&["deps", "a"])?;
        let output = format!(
            "{}{}",
            deps["result"]["stdout"].as_str().unwrap_or(""),
            deps["result"]["stderr"].as_str().unwrap_or("")
        );
        ensure!(
            output.contains("reused 1, downloaded 0"),
            "Offline dependency reuse not established: {output}"
        );
        let a = h.clone();
        let started = Instant::now();
        let thread = std::thread::spawn(move || {
            a.call(&["exec", "a", "--", "sh", "-c", "sleep 3; printf parallel-a"])
        });
        let deadline = Instant::now() + Duration::from_secs(20);
        while state::read_json(&config.root.join("workspaces/a.json"))?["status"] != "exec" {
            ensure!(Instant::now() < deadline, "Exec did not acquire lease");
            std::thread::sleep(Duration::from_millis(20));
        }
        ensure!(
            h.call(&["checkpoint", "a"]).is_err(),
            "Concurrent mutation of busy workspace accepted"
        );
        let b = h.clone();
        let second = std::thread::spawn(move || {
            b.call(&["exec", "b", "--", "sh", "-c", "sleep 3; printf parallel-b"])
        });
        thread.join().unwrap()?;
        second.join().unwrap()?;
        h.receipts.lock().unwrap().push(
            json!({"parallel_two_three_second_commands_seconds":started.elapsed().as_secs_f64()}),
        );
        h.call(&["checkpoint", "a"])?;
        h.call(&["checkpoint", "b"])?;
        h.call(&["remount", "a"])?;
        let remounted=h.call(&["exec","a","--","sh","-c","set -eu; test \"$(pwd -P)\" = /project; test ! -e only-b.txt; test \"$(cat only-a.txt)\" = agent-a; PNPM_CONFIG_CACHE_DIR=/tmp/enginebox-pnpm-cache PNPM_CONFIG_STORE_DIR=/pnpm-store PNPM_CONFIG_FROZEN_STORE=true PNPM_CONFIG_OFFLINE=true pnpm exec tsc --version"])?;
        ensure!(
            remounted["result"]["stdout"].as_str().unwrap_or("").trim() == "Version 5.9.3",
            "Compiler did not execute"
        );
        ensure!(
            !remounted["result"]["stderr"]
                .as_str()
                .unwrap_or("")
                .contains("Packages:"),
            "Remount triggered reinstall"
        );
        h.call(&["checkpoint", "a"])?;
        h.call(&["close", "a"])?;
        h.call(&["close", "b"])?;
        let list = h.call(&["list"])?;
        ensure!(
            list["workspaces"]
                .as_array()
                .context("Workspace list")?
                .iter()
                .all(|v| v["status"] == "closed"),
            "Workspace not closed"
        );
        // A prepared checkpoint can seed a new checkout with installed files;
        // no package install is needed for direct compiler execution.
        h.call(&["fork", "fixture", "cached", "--from", "a"])?;
        let cached = h.call(&[
            "exec",
            "cached",
            "--",
            "node",
            "node_modules/typescript/bin/tsc",
            "--version",
        ])?;
        ensure!(
            cached["result"]["stdout"].as_str().unwrap_or("").trim() == "Version 5.9.3",
            "Prepared dependency files were not reused"
        );
        h.call(&["close", "cached"])?;
        let before = h.call(&["main", "fixture"])?;
        let expected = before["revision"].as_str().context("Main revision")?;
        let published = h.call(&[
            "promote",
            "fixture",
            "a",
            "--expected-main",
            expected,
            "--merge",
        ])?;
        ensure!(
            published["status"] == "published",
            "LayerFS promotion did not complete"
        );
        ensure!(
            h.call(&[
                "promote",
                "fixture",
                "b",
                "--expected-main",
                expected,
                "--merge"
            ])
            .is_err(),
            "Stale main promotion was accepted"
        );
        h.call(&["fork", "fixture", "promoted"])?;
        let view = h.call(&[
            "exec",
            "promoted",
            "--",
            "sh",
            "-c",
            "set -eu; test -f only-a.txt; test -d node_modules; test ! -e .git; cat only-a.txt",
        ])?;
        ensure!(view["ok"] == true, "New fork did not inherit LayerFS main");
        h.call(&["close", "promoted"])?;

        // A real operator-process interruption must not turn into an automatic
        // replay, even after the original command finishes independently.
        h.call(&["fork", "fixture", "interrupted", "--from", "a"])?;
        let mut child = Command::new(&h.binary)
            .args([
                "storage",
                "layerfs",
                "--config",
                &h.config,
                "exec",
                "interrupted",
                "--timeout",
                "30",
                "--",
                "sh",
                "-c",
                "printf 'once\\n' >> once.txt; sleep 5; touch done.txt",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let receipt_path = config.root.join("workspaces/interrupted.json");
        let workspace = state::read_json(&receipt_path)?;
        let container = workspace["container"]
            .as_str()
            .context("Interrupted container")?;
        let observe = |command: &str| {
            run_process(
                &[
                    "docker".into(),
                    format!("--host=unix://{}", config.docker_socket.display()),
                    "exec".into(),
                    container.into(),
                    "sh".into(),
                    "-c".into(),
                    command.into(),
                ],
                None,
                Duration::from_secs(10),
            )
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        while observe("test -f /project/once.txt").is_err() {
            ensure!(
                Instant::now() < deadline,
                "Interrupted command did not begin"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        child.kill()?;
        child.wait()?;
        ensure!(
            h.call(&["recover", "interrupted"]).is_err(),
            "Recovery accepted an original command that was still running"
        );
        ensure!(
            h.call(&[
                "exec",
                "interrupted",
                "--",
                "sh",
                "-c",
                "printf duplicate >> once.txt"
            ])
            .is_err(),
            "Interrupted operation was replayed"
        );
        while observe("test -f /project/done.txt").is_err() {
            ensure!(Instant::now() < deadline, "Original command did not finish");
            std::thread::sleep(Duration::from_millis(50));
        }
        ensure!(
            observe("cat /project/once.txt")? == b"once\n",
            "Command ran more than once"
        );
        let retained = state::read_json(&receipt_path)?;
        ensure!(
            retained["status"] == "exec",
            "Interrupted receipt was silently made ready"
        );
        h.receipts.lock().unwrap().push(json!({"interrupted_operator":"killed_after_first_write","original_command_finished":true,"replay_refused":true,"retained_status":retained["status"]}));
        // A completed original command can be reconciled without replaying it.
        // Wait only for the already bounded helper to exit, never kill/retry it.
        let project = state::read_json(&config.root.join("projects/fixture/project.json"))?;
        let owner = project["owner"].as_str().context("Project owner")?;
        loop {
            let helpers = run_process(
                &[
                    "docker".into(),
                    format!("--host=unix://{}", config.docker_socket.display()),
                    "ps".into(),
                    "--all".into(),
                    "--filter".into(),
                    format!("label=enginebox.layerfs.owner={owner}"),
                    "--format={{.ID}}".into(),
                ],
                None,
                Duration::from_secs(10),
            )?;
            if helpers.iter().all(|b| b.is_ascii_whitespace()) {
                break;
            }
            ensure!(Instant::now() < deadline, "Original helper did not finish");
            std::thread::sleep(Duration::from_millis(50));
        }
        let recovered = h.call(&["recover", "interrupted"])?;
        ensure!(
            recovered["recovery"]["command_outcome"] == "original returned; not replayed"
                && recovered["recovery"]["settled_call"]["exit_code"] == 0,
            "Recovery did not retain the exact helper result"
        );
        h.call(&["exec","interrupted","--","sh","-c","set -eu; test \"$(cat once.txt)\" = once; test -f done.txt; printf checkpointed > recovered.txt"])?;
        h.call(&["checkpoint", "interrupted"])?;
        h.call(&[
            "exec",
            "interrupted",
            "--",
            "sh",
            "-c",
            "printf uncheckpointed > unsaved.txt",
        ])?;
        let alive_refusal = h
            .call(&[
                "restore-checkpoint",
                "interrupted",
                "--acknowledge-uncheckpointed",
            ])
            .expect_err("Checkpoint restore stopped a running sandbox");
        ensure!(
            format!("{alive_refusal:#}").contains("must already be stopped"),
            "Live-sandbox refusal had an unexpected cause: {alive_refusal:#}"
        );
        // Exact fixture container only: simulate loss of its daemon, not a
        // power failure or whole-node durability guarantee.
        run_process(
            &[
                "docker".into(),
                format!("--host=unix://{}", config.docker_socket.display()),
                "kill".into(),
                "--signal=KILL".into(),
                container.into(),
            ],
            None,
            Duration::from_secs(15),
        )?;
        ensure!(
            h.call(&["recover", "interrupted"]).is_err(),
            "Dead daemon was marked ready"
        );
        let restored = h.call(&[
            "restore-checkpoint",
            "interrupted",
            "--acknowledge-uncheckpointed",
        ])?;
        ensure!(
            restored["recovery"]["old_container_retained"] == true
                && restored["recovery"]["uncheckpointed_edits_recovered"] == false,
            "Restoration misstated data custody"
        );
        let replacement = state::read_json(&receipt_path)?;
        ensure!(
            replacement["container"] != container && replacement["status"] == "ready",
            "Stopped sandbox was reused"
        );
        let inspected: Value = serde_json::from_slice(&run_process(
            &[
                "docker".into(),
                format!("--host=unix://{}", config.docker_socket.display()),
                "inspect".into(),
                container.into(),
            ],
            None,
            Duration::from_secs(10),
        )?)?;
        ensure!(
            inspected[0]["State"]["Running"] == false,
            "Old container was deleted or restarted"
        );
        let overlay = directory.join("retained-overlay.sqlite");
        run_process(
            &[
                "docker".into(),
                format!("--host=unix://{}", config.docker_socket.display()),
                "cp".into(),
                format!("{container}:/layerfs-local/overlay/overlay.sqlite"),
                overlay.to_string_lossy().into_owned(),
            ],
            None,
            Duration::from_secs(15),
        )?;
        ensure!(
            fs::metadata(&overlay)?.len() > 0,
            "Original overlay not retained"
        );
        let content=h.call(&["exec","interrupted","--","sh","-c","set -eu; test \"$(cat once.txt)\" = once; test \"$(cat recovered.txt)\" = checkpointed; test ! -e unsaved.txt; node node_modules/typescript/bin/tsc --version"])?;
        ensure!(
            content["result"]["stdout"].as_str().unwrap_or("").trim() == "Version 5.9.3",
            "Checkpoint restoration lost prepared dependencies"
        );
        h.call(&["close", "interrupted"])?;
        h.receipts.lock().unwrap().push(json!({"daemon_loss":"SIGKILL exact disposable sandbox","checkpoint_restored":true,"original_command_replayed":false,"old_container_retained":true,"old_overlay_bytes":fs::metadata(&overlay)?.len(),"uncheckpointed_edits_recovered":false}));
        Ok::<_, anyhow::Error>(())
    })();
    state::write_json(
        &report,
        &json!({"ok":result.is_ok(),"error":result.as_ref().err().map(|e|format!("{e:#}")),"receipts":*h.receipts.lock().unwrap()}),
    )?;
    result
}

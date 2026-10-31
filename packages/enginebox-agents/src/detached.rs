//! Detached native runs (docs/UPDATES.md, "Work that outlives a controller"). The
//! guest helper starts a run in its own session and returns at once; the run
//! writes an append-only spool under the agent's home: the request (removed as
//! soon as the worker reads it, since it can carry run env), a receipt with the
//! worker's pid, and finally the result. A controller polls `collect` and
//! never holds the run's process, so a controller handoff neither waits for
//! nor interrupts it, and the next owner adopts it by run ID.
use crate::{native, security};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Spool root, relative to the guest user's home.
pub const DIR: &str = ".local/state/enginebox/detached";
/// Guest modes a detached run may use (the synchronous run modes).
pub const MODES: &[&str] = &["run", "run-profile", "run-repository"];
const INPUT: &str = "input.json";
const RECEIPT: &str = "receipt.json";
const RESULT: &str = "result.json";
const FAILED: &str = "failed.json";
/// The argument the worker process is started with; `collect` checks a live
/// pid really is this run's worker before calling the run running.
pub const WORKER_ARG: &str = "detached-worker";

fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn spool(home: &Path, id: &str) -> Result<PathBuf> {
    ensure!(valid_id(id), "Invalid run ID.");
    let root = home.join(DIR);
    security::private_dir(&root)?;
    let dir = root.join(id);
    security::private_dir(&dir)?;
    Ok(dir)
}

/// Spool the request and start `worker` (followed by [`WORKER_ARG`] and the
/// run ID) in a new session with no inherited stdio. A run ID is started at
/// most once: an existing receipt is refused, never replayed.
pub fn start(
    home: &Path,
    id: &str,
    mode: &str,
    workspace: &str,
    input: &[u8],
    worker: &[String],
) -> Result<Value> {
    ensure!(MODES.contains(&mode), "Unsupported detached mode.");
    ensure!(!worker.is_empty(), "Missing worker command.");
    let dir = spool(home, id)?;
    ensure!(
        !dir.join(RECEIPT).exists(),
        "Run already started; collect it instead of starting it again."
    );
    security::write_bytes(&dir, INPUT, input, native::MAX_OUTPUT * 7)?;
    let mut receipt =
        json!({"schema":1,"id":id,"mode":mode,"workspace":workspace,"started_at":crate::now()});
    security::write_json(&dir, RECEIPT, &receipt)?;
    let mut command = std::process::Command::new(&worker[0]);
    command
        .args(&worker[1..])
        .args([WORKER_ARG, id])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own session: the exec channel that started it can close.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let child = command
        .spawn()
        .context("Could not start the detached run.")?;
    receipt["pid"] = json!(child.id());
    security::write_json(&dir, RECEIPT, &receipt)?;
    // Not waited for: the worker outlives this helper.
    std::mem::forget(child);
    Ok(json!({"detached":true,"id":id,"pid":receipt["pid"]}))
}

/// The worker side: read and delete the spooled request, run it, and write
/// the result (or a failure marker) atomically.
pub fn run_worker(
    home: &Path,
    id: &str,
    exec: impl FnOnce(&str, &str, Value) -> Result<Value>,
) -> Result<()> {
    let dir = spool(home, id)?;
    let receipt = security::read_json(&dir, RECEIPT)?.context("Missing run receipt.")?;
    let raw = security::read_bytes(&dir, INPUT, native::MAX_OUTPUT * 7, true)?
        .context("Missing run request.")?;
    // The request can carry run env: gone before the run starts.
    std::fs::remove_file(dir.join(INPUT))?;
    let input: Value = serde_json::from_slice(&raw)?;
    let mode = receipt["mode"].as_str().unwrap_or("");
    let workspace = receipt["workspace"].as_str().unwrap_or("");
    match exec(mode, workspace, input) {
        Ok(result) => security::write_bytes(
            &dir,
            RESULT,
            &serde_json::to_vec(&result)?,
            native::MAX_OUTPUT * 7,
        ),
        Err(_) => security::write_json(
            &dir,
            FAILED,
            &json!({"error":"The detached run failed before producing a result."}),
        ),
    }
}

/// `{"state": "finished", "result": ...}`, `failed`, `running`, `lost` (a
/// receipt whose worker is gone without a result) or `unknown` (never started).
pub fn collect(home: &Path, id: &str) -> Result<Value> {
    let dir = spool(home, id)?;
    if let Some(raw) = security::read_bytes(&dir, RESULT, native::MAX_OUTPUT * 7, true)? {
        let result: Value = serde_json::from_slice(&raw)?;
        return Ok(json!({"state":"finished","result":result}));
    }
    if security::read_json(&dir, FAILED)?.is_some() {
        return Ok(json!({"state":"failed"}));
    }
    let Some(receipt) = security::read_json(&dir, RECEIPT)? else {
        return Ok(json!({"state":"unknown"}));
    };
    let alive = receipt["pid"]
        .as_u64()
        .and_then(|p| i32::try_from(p).ok())
        .is_some_and(|pid| worker_alive(pid, id));
    // The worker may have finished between the reads above.
    if !alive && dir.join(RESULT).exists() {
        return collect(home, id);
    }
    Ok(json!({"state": if alive { "running" } else { "lost" }}))
}

/// Remove a collected run's spool.
pub fn forget(home: &Path, id: &str) -> Result<()> {
    let dir = spool(home, id)?;
    std::fs::remove_dir_all(dir)?;
    Ok(())
}

fn worker_alive(pid: i32, id: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        // The pid must still be this run's worker, not a reused pid.
        std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|raw| {
            let args: Vec<&[u8]> = raw.split(|b| *b == 0).collect();
            args.windows(2)
                .any(|w| w[0] == WORKER_ARG.as_bytes() && w[1] == id.as_bytes())
        })
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let _ = id;
        unsafe { libc::kill(pid, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, id);
        false
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const ID: &str = "0123456789abcdef0123456789abcdef";

    struct Home(#[allow(dead_code)] tempfile::TempDir, PathBuf);
    impl Home {
        fn path(&self) -> &Path {
            &self.1
        }
    }
    fn home() -> Home {
        let t = tempfile::tempdir().unwrap();
        std::fs::set_permissions(t.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = t.path().canonicalize().unwrap();
        Home(t, path)
    }

    #[test]
    fn a_detached_run_outlives_its_starter_and_is_collected_once() {
        let home = home();
        // A worker that takes a moment, then runs the worker side in-process
        // via a tiny shell stand-in: it just waits; the result is written below.
        let script = home.path().join("worker.sh");
        std::fs::write(&script, "#!/bin/sh\nsleep 1\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let worker = vec![script.display().to_string()];
        let started = start(home.path(), ID, "run", "/workspace", br#"{"x":1}"#, &worker).unwrap();
        assert_eq!(started["detached"], true);
        // Started once: a second start is refused.
        assert!(start(home.path(), ID, "run", "/workspace", b"{}", &worker).is_err());
        // The shell stand-in is not a real worker, so a live pid is not "running"
        // on Linux (cmdline check); elsewhere it is. Either way no result yet.
        let state = collect(home.path(), ID).unwrap();
        assert!(["running", "lost"].contains(&state["state"].as_str().unwrap()));
        // The worker side: reads (and deletes) the request, writes the result.
        run_worker(home.path(), ID, |mode, workspace, input| {
            assert_eq!((mode, workspace), ("run", "/workspace"));
            assert_eq!(input["x"], 1);
            Ok(json!({"exit_code":0,"timed_out":false,"output":"done"}))
        })
        .unwrap();
        assert!(!home.path().join(DIR).join(ID).join(INPUT).exists());
        let done = collect(home.path(), ID).unwrap();
        assert_eq!(done["state"], "finished");
        assert_eq!(done["result"]["output"], "done");
        forget(home.path(), ID).unwrap();
        assert_eq!(collect(home.path(), ID).unwrap()["state"], "unknown");
    }

    #[test]
    fn a_failed_worker_and_a_vanished_worker_are_told_apart() {
        let home = home();
        let worker = vec!["/bin/sh".to_owned(), "-c".to_owned(), "exit 0".to_owned()];
        start(home.path(), ID, "run", "/w", b"{}", &worker).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        // The worker exits without a result: lost, never "running". (Linux only:
        // elsewhere the unreaped test child still answers kill(pid, 0); in a
        // guest the starter exits and init reaps the worker.)
        #[cfg(target_os = "linux")]
        assert_eq!(collect(home.path(), ID).unwrap()["state"], "lost");
        run_worker(home.path(), ID, |_, _, _| anyhow::bail!("boom")).unwrap();
        assert_eq!(collect(home.path(), ID).unwrap()["state"], "failed");
        assert!(start(home.path(), "not-a-run-id", "run", "/w", b"{}", &worker).is_err());
        assert!(
            start(
                home.path(),
                &"a".repeat(32),
                "transfer",
                "/w",
                b"{}",
                &worker
            )
            .is_err()
        );
    }
}

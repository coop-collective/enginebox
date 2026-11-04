//! Slot accounts and the agent run spawner (contract §3).
//!
//! - **Windows** (`isolation: "accounts"`): the installer creates
//!   `eb-agent-01..N` and `enginebox-editor` with random passwords in
//!   `private\accounts.pending.json`; the node seals them with user-scope
//!   DPAPI into `private\accounts.dpapi` at start. A run starts with
//!   `CreateProcessWithLogonW` (no profile) into its own Job Object (4 GiB,
//!   25 % CPU, 64 processes, UI limits, no breakaway) on the node's private
//!   desktop `WinSta0\eb-agents`, and the fork tree and home are ACL'd to the
//!   slot for the run only.
//! - **macOS / Linux without slot users** (`isolation: "none"`): the run is a
//!   child of the node in its own process group with an explicit environment
//!   and a private home; Codex keeps its own sandbox on. The controller may
//!   only place forks of the session starter on such a node. Per-slot uids
//!   (`_ebagent1..N` behind a root `enginebox-spawn` helper) are WS-11.
use super::{SlotAccount, harness::Plan};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;

pub const SLOT_PREFIX: &str = "eb-agent-";
pub const EDITOR_ACCOUNT: &str = "enginebox-editor";
/// Sealed with user-scope DPAPI (Windows).
pub const ACCOUNTS_FILE: &str = "accounts.dpapi";
/// Written by the installer for the node to seal.
pub const PENDING_ACCOUNTS_FILE: &str = "accounts.pending.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Isolation {
    /// Slot accounts (Windows).
    Accounts,
    /// The node's own account: forks of the session starter only.
    None,
}

/// A local account's sign-in. Only the node holds these, in memory.
#[derive(Clone, Deserialize, Serialize)]
pub struct Credential {
    pub user: String,
    pub password: String,
}
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Credential({})", self.user)
    }
}

/// The installer's account file.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Accounts {
    pub schema: u32,
    #[serde(default)]
    pub editor: Option<Credential>,
    #[serde(default)]
    pub slots: Vec<Credential>,
}
impl Accounts {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.schema == 1, "accounts schema must be 1");
        if let Some(e) = &self.editor {
            anyhow::ensure!(e.user == EDITOR_ACCOUNT, "unexpected editor account");
        }
        for (i, s) in self.slots.iter().enumerate() {
            anyhow::ensure!(
                s.user == format!("{SLOT_PREFIX}{:02}", i + 1),
                "unexpected slot account"
            );
        }
        anyhow::ensure!(self.slots.len() <= 8, "too many slot accounts");
        Ok(())
    }
    /// Every password, for redaction.
    pub fn secrets(&self) -> Vec<String> {
        self.editor
            .iter()
            .chain(&self.slots)
            .map(|c| c.password.clone())
            .collect()
    }
}

/// Seal a pending account file (Windows) and read the sealed accounts.
/// Returns `None` when the installer created no accounts (or off Windows).
pub fn load_accounts(private: &Path) -> Result<Option<Accounts>> {
    #[cfg(windows)]
    {
        let pending = private.join(PENDING_ACCOUNTS_FILE);
        let sealed = private.join(ACCOUNTS_FILE);
        if pending.is_file() {
            let bytes = std::fs::read(&pending)?;
            let accounts: Accounts = serde_json::from_slice(&bytes)
                .context("accounts.pending.json has the wrong shape")?;
            accounts.validate()?;
            let blob = super::windows::protect(&bytes)?;
            let mut tmp = tempfile::NamedTempFile::new_in(private)?;
            tmp.write_all(&blob)?;
            tmp.persist(&sealed).map_err(|e| e.error)?;
            std::fs::remove_file(&pending)?;
        }
        if !sealed.is_file() {
            return Ok(None);
        }
        let bytes = super::windows::unprotect(&std::fs::read(&sealed)?)?;
        let accounts: Accounts = serde_json::from_slice(&bytes)?;
        accounts.validate()?;
        Ok(Some(accounts))
    }
    #[cfg(not(windows))]
    {
        let _ = private;
        Ok(None)
    }
}

/// A slot taken from the pool for one run.
#[derive(Clone, Debug)]
pub struct Slot {
    pub account: SlotAccount,
    pub credential: Option<Credential>,
}

pub struct SlotPool {
    slots: Vec<Slot>,
    busy: BTreeSet<String>,
    isolation: Isolation,
}
impl SlotPool {
    /// `capacity`: `engine.json` `agents.slots` (at most 3).
    pub fn new(accounts: Option<&Accounts>, capacity: usize) -> Self {
        let capacity = capacity.clamp(1, 3);
        match accounts.filter(|a| !a.slots.is_empty()) {
            Some(a) => Self {
                slots: a
                    .slots
                    .iter()
                    .take(capacity)
                    .map(|c| Slot {
                        account: SlotAccount {
                            name: c.user.clone(),
                            sid: account_sid(&c.user),
                            uid: None,
                        },
                        credential: Some(c.clone()),
                    })
                    .collect(),
                busy: BTreeSet::new(),
                isolation: Isolation::Accounts,
            },
            None => Self {
                slots: vec![Slot {
                    account: SlotAccount::node(),
                    credential: None,
                }],
                busy: BTreeSet::new(),
                isolation: Isolation::None,
            },
        }
    }
    pub fn isolation(&self) -> Isolation {
        self.isolation
    }
    pub fn acquire(&mut self) -> Option<Slot> {
        let slot = self
            .slots
            .iter()
            .find(|s| !self.busy.contains(&s.account.name))?
            .clone();
        self.busy.insert(slot.account.name.clone());
        Some(slot)
    }
    pub fn release(&mut self, name: &str) {
        self.busy.remove(name);
    }
    pub fn free(&self) -> usize {
        self.slots.len() - self.busy.len()
    }
    /// `tools.engine.forks` for the heartbeat.
    pub fn inventory(&self, provider: &str) -> Value {
        json!({"provider": provider, "slots": self.slots.len(), "free_slots": self.free(),
            "isolation": self.isolation})
    }
}

fn account_sid(user: &str) -> Option<String> {
    #[cfg(windows)]
    {
        super::windows::account_sid(user).ok()
    }
    #[cfg(not(windows))]
    {
        let _ = user;
        None
    }
}

/// Give a slot modify access to these directories (and take it back).
pub fn grant_dirs(dirs: &[&Path], slot: &SlotAccount) -> Result<()> {
    if slot.is_node() {
        return Ok(());
    }
    #[cfg(windows)]
    {
        let sid = slot.sid.as_deref().context("slot account has no SID")?;
        for d in dirs {
            super::windows::grant_dir(d, sid)?;
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = dirs;
        bail!("per-slot accounts are not supported on this platform yet")
    }
}
pub fn revoke_dirs(dirs: &[&Path]) -> Result<()> {
    #[cfg(windows)]
    for d in dirs {
        if d.exists() {
            super::windows::revoke_dir(d)?;
        }
    }
    #[cfg(not(windows))]
    let _ = dirs;
    Ok(())
}

/// Resource limits for one run.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub memory_bytes: u64,
    pub cpu_percent: u32,
    pub processes: u32,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_bytes: 4 << 30,
            cpu_percent: 25,
            processes: 64,
        }
    }
}

/// A running harness: stdout lines, a stderr tail, kill and wait.
pub struct RunChild {
    pub lines: mpsc::UnboundedReceiver<String>,
    pub stderr: Arc<Mutex<String>>,
    pub pid: u32,
    inner: Inner,
}
enum Inner {
    #[cfg(unix)]
    Unix(Arc<Mutex<std::process::Child>>),
    #[cfg(windows)]
    Windows(super::windows::LogonChild),
}

/// Lines from a pipe, one per message (bounded to 1 MiB each).
fn pump_lines(
    pipe: impl Read + Send + 'static,
    tx: mpsc::UnboundedSender<String>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::with_capacity(64 * 1024, pipe);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.by_ref().take(1 << 20).read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx
                        .send(String::from_utf8_lossy(&buf).trim_end().to_owned())
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    })
}
fn pump_tail(pipe: impl Read + Send + 'static, tail: Arc<Mutex<String>>) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(pipe);
        let mut chunk = [0u8; 4096];
        while let Ok(n) = reader.read(&mut chunk) {
            if n == 0 {
                break;
            }
            let mut t = tail.lock().unwrap_or_else(|e| e.into_inner());
            t.push_str(&String::from_utf8_lossy(&chunk[..n]));
            if t.len() > 16 * 1024 {
                let cut = t.len() - 8 * 1024;
                let cut = (cut..t.len()).find(|i| t.is_char_boundary(*i)).unwrap_or(0);
                t.drain(..cut);
            }
        }
    });
}
fn feed(mut stdin: impl Write + Send + 'static, text: String) {
    std::thread::spawn(move || {
        let _ = stdin.write_all(text.as_bytes());
    });
}

/// Start `plan` as `slot`.
pub fn spawn(plan: &Plan, slot: &Slot, limits: &Limits) -> Result<RunChild> {
    let (tx, lines) = mpsc::unbounded_channel();
    let stderr = Arc::new(Mutex::new(String::new()));
    if slot.account.is_node() {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let _ = limits;
            let mut c = std::process::Command::new(&plan.program);
            c.args(&plan.args)
                .env_clear()
                .envs(plan.env.iter().map(|(k, v)| (k, v)))
                .current_dir(&plan.cwd)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .process_group(0);
            let mut child = c
                .spawn()
                .with_context(|| format!("Cannot start {}", plan.program.display()))?;
            feed(child.stdin.take().context("stdin")?, plan.stdin.clone());
            pump_lines(child.stdout.take().context("stdout")?, tx);
            pump_tail(child.stderr.take().context("stderr")?, stderr.clone());
            let pid = child.id();
            return Ok(RunChild {
                lines,
                stderr,
                pid,
                inner: Inner::Unix(Arc::new(Mutex::new(child))),
            });
        }
        #[cfg(windows)]
        bail!("agent runs on Windows need slot accounts; run the installer with -EngineAccount");
    }
    #[cfg(windows)]
    {
        let cred = slot.credential.as_ref().context("slot has no credential")?;
        let child = super::windows::spawn_logon(
            cred,
            &super::windows::LogonSpec {
                program: &plan.program,
                args: &plan.args,
                env: &plan.env,
                cwd: &plan.cwd,
                profile: false,
                desktop: Some(super::windows::AGENT_DESKTOP),
                limits: Some(*limits),
                pipes: true,
            },
        )?;
        let (stdin, stdout, err) = child.take_pipes()?;
        feed(stdin, plan.stdin.clone());
        pump_lines(stdout, tx);
        pump_tail(err, stderr.clone());
        let pid = child.pid;
        Ok(RunChild {
            lines,
            stderr,
            pid,
            inner: Inner::Windows(child),
        })
    }
    #[cfg(not(windows))]
    {
        let _ = (tx, limits);
        bail!("per-slot accounts are not supported on this platform yet")
    }
}

impl RunChild {
    /// End the run's whole process tree.
    pub fn kill(&self) {
        match &self.inner {
            #[cfg(unix)]
            Inner::Unix(_) => unsafe {
                // The child leads its own process group.
                libc::killpg(self.pid as libc::pid_t, libc::SIGKILL);
            },
            #[cfg(windows)]
            Inner::Windows(c) => c.terminate(),
        }
    }
    /// Wait for the harness to exit (its exit code when it has one).
    pub async fn wait(&self) -> Option<i32> {
        match &self.inner {
            #[cfg(unix)]
            Inner::Unix(child) => {
                let child = child.clone();
                loop {
                    let done = child
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .try_wait()
                        .ok()
                        .flatten();
                    if let Some(status) = done {
                        // Its process group may outlive the leader.
                        unsafe { libc::killpg(self.pid as libc::pid_t, libc::SIGKILL) };
                        return status.code();
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
            #[cfg(windows)]
            Inner::Windows(c) => loop {
                if let Some(code) = c.exit_code() {
                    c.terminate();
                    return Some(code as i32);
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_without_accounts_runs_as_the_node_only() {
        let mut pool = SlotPool::new(None, 3);
        assert_eq!(pool.isolation(), Isolation::None);
        assert_eq!(pool.inventory("git")["slots"], 1);
        let s = pool.acquire().unwrap();
        assert!(s.account.is_node());
        assert!(pool.acquire().is_none());
        pool.release(&s.account.name);
        assert_eq!(pool.free(), 1);
    }

    #[test]
    fn pool_with_accounts_hands_out_distinct_slots_up_to_three() {
        let accounts = Accounts {
            schema: 1,
            editor: None,
            slots: (1..=4)
                .map(|i| Credential {
                    user: format!("eb-agent-{i:02}"),
                    password: format!("pw{i}"),
                })
                .collect(),
        };
        accounts.validate().unwrap();
        assert_eq!(accounts.secrets().len(), 4);
        assert_eq!(
            format!("{:?}", accounts.slots[0]),
            "Credential(eb-agent-01)"
        );
        let mut pool = SlotPool::new(Some(&accounts), 8);
        assert_eq!(pool.isolation(), Isolation::Accounts);
        let a = pool.acquire().unwrap();
        let b = pool.acquire().unwrap();
        let c = pool.acquire().unwrap();
        assert!(pool.acquire().is_none());
        assert_eq!(
            [a.account.name, b.account.name, c.account.name.clone()],
            ["eb-agent-01", "eb-agent-02", "eb-agent-03"]
        );
        pool.release(&c.account.name);
        assert_eq!(pool.inventory("git")["free_slots"], 1);
        let bad = Accounts {
            schema: 1,
            editor: None,
            slots: vec![Credential {
                user: "Administrator".into(),
                password: "x".into(),
            }],
        };
        assert!(bad.validate().is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn node_runs_get_only_the_plan_environment_and_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let plan = Plan {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "read line; echo \"got:$line\"; echo \"home:$HOME\"; env | grep -c . ; echo oops >&2".into(),
            ],
            env: vec![("HOME".into(), dir.path().to_string_lossy().into())],
            cwd: dir.path().to_owned(),
            stdin: "hello\n".into(),
        };
        let slot = Slot {
            account: SlotAccount::node(),
            credential: None,
        };
        let mut child = spawn(&plan, &slot, &Limits::default()).unwrap();
        let code = child.wait().await;
        assert_eq!(code, Some(0));
        let mut out = Vec::new();
        while let Some(l) = child.lines.recv().await {
            out.push(l);
        }
        assert_eq!(out[0], "got:hello");
        assert_eq!(out[1], format!("home:{}", dir.path().display()));
        // HOME, plus what sh itself sets (PWD, SHLVL, _).
        assert!(out[2].trim().parse::<u32>().unwrap() <= 4, "{out:?}");
        assert!(child.stderr.lock().unwrap().contains("oops"));
    }
}

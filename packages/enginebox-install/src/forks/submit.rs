//! The node's submission pipeline (contract §5.3, §6, owner decision 3):
//! re-check the fork (pre-scan, classes, secret scan, owner approval for
//! code), capture live edits, three-way merge into the dev head, apply the
//! result under the Editor bridge's source lock, ask the bridge to refresh,
//! read the compile result, and revert automatically when a merge breaks a
//! compiling project.
//!
//! Bridge fields used (Gamejam `packages/unity-editor-bridge`, PR #52):
//! `POST /refresh {nonce}` → 200 `{refresh:"done"}` / 202 `{refresh:"queued"|
//! "running", deferred?}` / 503 busy; `/status` `refresh`, `refreshNonce`,
//! `refreshDeferred`, `compiling`, `lastCompileOk`, `lastCompileAt`,
//! `compileErrors`, `dirtyScenes`, `playing`, `instance`, `saveSeq`,
//! `savedPaths`. The source-lock handshake files sit beside `bridge.json`.
use super::{
    Change, Commit, DevTree, ForkError, ForkProvider, ForkTree, Principal, Snapshot,
    classify::{self, Class, Classes, Finding},
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

/// How long the Editor may take to grant the source lock.
const LOCK_WAIT: Duration = Duration::from_secs(20);
/// How long a refresh and compile may take after a merge.
const COMPILE_WAIT: Duration = Duration::from_secs(600);
/// Diff text scanned for secrets.
const SCAN_LIMIT: usize = 8 << 20;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
fn nonce() -> String {
    let mut s = uuid::Uuid::new_v4().simple().to_string();
    s.truncate(32);
    s
}

/// The Editor bridge of the session (absent for world sessions).
#[derive(Clone, Debug)]
pub struct Bridge {
    /// `bridge.json` (its `port` moves when the node moves the bridge).
    pub config: PathBuf,
    pub token: String,
}
impl Bridge {
    fn port(&self) -> Option<u16> {
        let v: Value = serde_json::from_slice(&std::fs::read(&self.config).ok()?).ok()?;
        v["port"].as_u64().and_then(|p| u16::try_from(p).ok())
    }
    fn client() -> Option<reqwest::Client> {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(8))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .ok()
    }
    pub async fn status(&self) -> Option<Value> {
        let r = Self::client()?
            .get(format!("http://127.0.0.1:{}/status", self.port()?))
            .bearer_auth(&self.token)
            .send()
            .await
            .ok()?;
        if !r.status().is_success() {
            return None;
        }
        r.json().await.ok()
    }
    /// `POST /refresh {nonce}`: `Some(state)` (`done`, `queued`, `running`),
    /// `None` when the bridge is busy, absent or too old to know the route.
    pub async fn refresh(&self, nonce: &str) -> Option<String> {
        let r = Self::client()?
            .post(format!("http://127.0.0.1:{}/refresh", self.port()?))
            .bearer_auth(&self.token)
            .json(&json!({"nonce": nonce}))
            .send()
            .await
            .ok()?;
        if !matches!(r.status().as_u16(), 200 | 202) {
            return None;
        }
        let v: Value = r.json().await.ok()?;
        v["refresh"].as_str().map(str::to_owned)
    }
    fn sync_file(&self, suffix: &str) -> PathBuf {
        let mut name = self.config.as_os_str().to_owned();
        name.push(".refresh.sync");
        name.push(suffix);
        PathBuf::from(name)
    }
    /// The source-lock handshake: ask the Editor to pause imports and
    /// reloads, run `apply` once it confirms, then let it go. `Err` is the
    /// held reason.
    pub async fn locked<T: Send + 'static>(
        &self,
        apply: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, String> {
        let n = nonce();
        let request = self.sync_file("");
        let ready = self.sync_file("-ready");
        let body = json!({"nonce": n, "expires": now_ms() + 30_000}).to_string();
        let tmp = self.sync_file(".tmp");
        std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &request).map_err(|e| e.to_string())?;
        let t = Instant::now();
        let outcome = loop {
            if std::fs::read_to_string(&ready).is_ok_and(|s| s.trim() == n) {
                break Ok(());
            }
            if t.elapsed() > LOCK_WAIT {
                let status = self.status().await.unwrap_or(Value::Null);
                break Err(held_reason(&status));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let result = match outcome {
            Ok(()) => tokio::task::spawn_blocking(apply)
                .await
                .map_err(|e| e.to_string()),
            Err(reason) => Err(reason),
        };
        // Releasing the request ends the lock; the Editor then refreshes.
        let _ = std::fs::remove_file(&request);
        result
    }
}
fn held_reason(status: &Value) -> String {
    if status["playing"] == true {
        "Play mode".into()
    } else if status["dirty"] == true
        || status["dirtyScenes"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
    {
        "unsaved live scene edits".into()
    } else if status["compiling"] == true {
        "the Editor is compiling".into()
    } else {
        "the Editor did not pause imports".into()
    }
}

/// The compile state reported in `dev`.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Compile {
    /// `ok`, `errors`, `compiling` or `unknown`.
    pub state: String,
    pub errors: Vec<Value>,
    /// The applied head this result is for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<Commit>,
}
impl Compile {
    pub fn from_status(status: &Value) -> Self {
        let state = if status["compiling"] == true {
            "compiling"
        } else {
            match status["lastCompileOk"].as_bool() {
                Some(true) => "ok",
                Some(false) => "errors",
                None => "unknown",
            }
        };
        Self {
            state: state.into(),
            errors: status["compileErrors"]
                .as_array()
                .map(|a| a.iter().take(50).cloned().collect())
                .unwrap_or_default(),
            head: None,
        }
    }
}

/// Node-side state of a session's dev tree.
#[derive(Debug)]
pub struct DevState {
    /// The commit whose files are on disk.
    pub applied: Commit,
    pub held: Option<String>,
    pub compile: Compile,
    pub live_code_pending: Vec<String>,
    /// The last `(instance, saveSeq)` captured.
    pub saves: Option<(String, u64)>,
    /// The last publish op's result (`engine-report` `dev.publish`).
    pub publish: Option<Value>,
}

/// One session's dev tree with everything the pipeline needs.
pub struct Dev {
    pub tree: DevTree,
    pub bridge: Option<Bridge>,
    pub provider: Arc<dyn ForkProvider>,
    /// Secrets of the session, for the secret scan and redaction.
    pub known: Vec<String>,
    pub state: tokio::sync::Mutex<DevState>,
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> super::Result<T> + Send + 'static,
) -> super::Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ForkError::Other(e.into()))?
}

/// `Op.merge` from `engine-poll`.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct MergeOp {
    pub fork_head: String,
    pub expected_dev_head: String,
    #[serde(default)]
    pub message: String,
    /// The room owner who approved code-class paths.
    #[serde(default)]
    pub approved_by: Option<String>,
}

/// What a submission's checks found.
#[derive(Clone, Debug, Serialize)]
pub struct Checks {
    pub snapshot: Snapshot,
    pub classes: Classes,
    pub changed: Vec<(Change, Class)>,
    pub findings: Vec<Finding>,
}

/// Pre-scan (by snapshotting), classify and secret-scan a fork's head.
pub async fn check(
    provider: Arc<dyn ForkProvider>,
    fork: ForkTree,
    project: String,
    known: Vec<String>,
) -> super::Result<Checks> {
    blocking(move || {
        let snapshot = provider.snapshot(&fork, "Submission")?;
        let mut classes = Classes::default();
        let mut findings = Vec::new();
        let mut changed = Vec::new();
        for c in &snapshot.changed {
            let (class, rule) = classify::classify(&c.path, c.mode, &project);
            classes.add(class);
            if class == Class::Refused {
                findings.push(Finding::new(rule, &c.path));
            }
            changed.push((c.clone(), class));
        }
        let diff = provider.diff(&fork, None, SCAN_LIMIT)?;
        let known: Vec<&str> = known.iter().map(String::as_str).collect();
        findings.extend(classify::scan_diff(&diff, &known));
        Ok(Checks {
            snapshot,
            classes,
            changed,
            findings,
        })
    })
    .await
}

/// The result of a merge op, for `fork-report` `detail.merge`/`apply`.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Outcome {
    /// `applied`, `held`, `conflict`, `refused`, `stale` or `failed`.
    pub state: String,
    pub dev_head_before: Option<Commit>,
    pub dev_head: Option<Commit>,
    pub applied: Option<Commit>,
    pub held_reason: Option<String>,
    pub conflict_paths: Vec<String>,
    pub findings: Vec<Finding>,
    pub classes: Option<Classes>,
    /// Code or protected paths changed (the Editor recompiles).
    pub code_changed: bool,
    /// The merge lost a compare-and-swap; `dev_head` is the current head.
    pub stale: bool,
    pub error: Option<String>,
}

impl Dev {
    /// The `dev` block of `engine-report`.
    pub async fn report(&self) -> Value {
        let head = {
            let p = self.provider.clone();
            let t = self.tree.clone();
            blocking(move || p.dev_head(&t)).await.ok()
        };
        let s = self.state.lock().await;
        let mut compile = json!(s.compile);
        if s.compile.state == "unknown" {
            compile = Value::Null;
        }
        let mut v = json!({"head": head, "applied": s.applied, "held": s.held,
            "live_code_pending": s.live_code_pending});
        if !compile.is_null() {
            v["compile"] = compile;
        }
        if let Some(p) = &s.publish {
            v["publish"] = p.clone();
        }
        v
    }

    /// Capture live saves when the bridge's `(instance, saveSeq)` moved (or
    /// `force`). Returns whether the dev head changed.
    pub async fn capture(&self, force: bool, attribution: Vec<Principal>) -> super::Result<bool> {
        let status = match &self.bridge {
            Some(b) => b.status().await,
            None => None,
        };
        let mut state = self.state.lock().await;
        if let Some(st) = &status
            && st["compiling"] != true
        {
            state.compile = Compile::from_status(st);
        }
        let saves = status.as_ref().and_then(|st| {
            Some((
                st["instance"].as_str().unwrap_or("").to_owned(),
                st["saveSeq"].as_u64()?,
            ))
        });
        if !force && (saves.is_none() || saves == state.saves) {
            return Ok(false);
        }
        self.capture_locked(&mut state, attribution).await?;
        if saves.is_some() {
            state.saves = saves;
        }
        Ok(true)
    }
    async fn capture_locked(
        &self,
        state: &mut DevState,
        attribution: Vec<Principal>,
    ) -> super::Result<Commit> {
        let (p, t) = (self.provider.clone(), self.tree.clone());
        let applied = state.applied.clone();
        let capture = blocking(move || {
            // Never capture over files that are not the dev head's yet.
            let head = p.dev_head(&t)?;
            if head != applied {
                return Err(ForkError::Held("an earlier merge is not on disk".into()));
            }
            p.capture_live(&t, &attribution)
        })
        .await?;
        state.live_code_pending = capture.live_code_pending.clone();
        state.applied = capture.head.clone();
        Ok(capture.head)
    }

    /// Apply `from → to` under the source lock (directly without a bridge).
    async fn apply_locked(&self, from: &Commit, to: &Commit) -> super::Result<Vec<Change>> {
        let (p, t, f, x) = (
            self.provider.clone(),
            self.tree.clone(),
            from.clone(),
            to.clone(),
        );
        let apply = move || p.apply(&t, &f, &x);
        match &self.bridge {
            Some(b) => match b.locked(apply).await {
                Ok(r) => r,
                Err(reason) => Err(ForkError::Held(reason)),
            },
            None => tokio::task::spawn_blocking(apply)
                .await
                .map_err(|e| ForkError::Other(e.into()))?,
        }
    }

    /// Ask the Editor to refresh and wait for the compile result after
    /// `started` (Unix ms). Without a bridge the result is `unknown`.
    pub async fn refresh_and_compile(&self, code_changed: bool) -> Compile {
        let Some(bridge) = &self.bridge else {
            return Compile {
                state: "unknown".into(),
                errors: Vec::new(),
                head: None,
            };
        };
        let n = nonce();
        let t = Instant::now();
        loop {
            // Re-posting the same nonce reports that request's state.
            let state = bridge.refresh(&n).await;
            let status = bridge.status().await.unwrap_or(Value::Null);
            let mine = status["refreshNonce"].as_str() == Some(n.as_str());
            if (state.as_deref() == Some("done") || mine && status["refresh"] == "done")
                && status["compiling"] != true
            {
                let compile = Compile::from_status(&status);
                // A content-only merge needs no compile.
                if code_changed || compile.state != "unknown" {
                    return compile;
                }
                return Compile {
                    state: "ok".into(),
                    errors: Vec::new(),
                    head: None,
                };
            }
            if t.elapsed() > COMPILE_WAIT {
                return Compile {
                    state: if status["compiling"] == true {
                        "compiling".into()
                    } else {
                        "unknown".into()
                    },
                    errors: Vec::new(),
                    head: None,
                };
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    /// Merge `op` into the dev head and apply it (contract §2.2, §5.3).
    pub async fn submit(&self, fork: ForkTree, op: &MergeOp) -> Outcome {
        let mut out = Outcome::default();
        let fail = |out: &mut Outcome, e: ForkError| {
            out.state = e.code().into();
            match &e {
                ForkError::Conflict { paths } => out.conflict_paths = paths.clone(),
                ForkError::Refused { findings } => out.findings = findings.clone(),
                ForkError::Held(r) => out.held_reason = Some(r.clone()),
                ForkError::Stale { current } => {
                    out.stale = true;
                    out.dev_head = current.clone();
                }
                _ => {}
            }
            out.error = Some(classify::redact(
                &e.to_string(),
                &self.known.iter().map(String::as_str).collect::<Vec<_>>(),
            ));
        };
        let (Ok(fork_head), Ok(_)) = (
            Commit::parse(&op.fork_head),
            Commit::parse(&op.expected_dev_head),
        ) else {
            fail(
                &mut out,
                ForkError::Invalid("merge op has bad commit ids".into()),
            );
            return out;
        };
        let checks = match check(
            self.provider.clone(),
            fork.clone(),
            self.tree.project_path.clone(),
            self.known.clone(),
        )
        .await
        {
            Ok(c) => c,
            Err(e) => {
                fail(&mut out, e);
                return out;
            }
        };
        out.classes = Some(checks.classes.clone());
        if checks.snapshot.head != fork_head {
            fail(
                &mut out,
                ForkError::Invalid("the fork changed since it was submitted".into()),
            );
            return out;
        }
        let mut findings = checks.findings.clone();
        if checks.classes.needs_owner() && op.approved_by.is_none() {
            findings.push(Finding::new("owner-approval-required", ""));
        }
        if !findings.is_empty() {
            fail(&mut out, ForkError::Refused { findings });
            return out;
        }
        let code_changed = checks.classes.code > 0 || checks.classes.protected > 0;
        let message = if op.message.trim().is_empty() {
            format!("Merge fork {}", fork.id.as_str())
        } else {
            op.message.clone()
        };
        let mut state = self.state.lock().await;
        // Twice: a live edit in the way gets captured and the merge redone.
        for attempt in 0..2 {
            let before = match self.capture_locked(&mut state, Vec::new()).await {
                Ok(h) => h,
                Err(e) => {
                    fail(&mut out, e);
                    return out;
                }
            };
            out.dev_head_before = Some(before.clone());
            let (p, t, fh, b, m) = (
                self.provider.clone(),
                self.tree.clone(),
                fork_head.clone(),
                before.clone(),
                message.clone(),
            );
            let merged = match blocking(move || p.merge(&t, &b, &fh, &m)).await {
                Ok(c) => c,
                Err(e) => {
                    fail(&mut out, e);
                    return out;
                }
            };
            out.dev_head = Some(merged.clone());
            match self.apply_locked(&before, &merged).await {
                Ok(_) => {
                    state.applied = merged.clone();
                    state.held = None;
                    out.applied = Some(merged.clone());
                    out.state = "applied".into();
                    break;
                }
                Err(e) => {
                    // Rewind: the dev head never points past the files on disk.
                    let (p, t, b, m) = (
                        self.provider.clone(),
                        self.tree.clone(),
                        before.clone(),
                        merged.clone(),
                    );
                    let _ = blocking(move || p.set_dev_head(&t, &m, &b)).await;
                    out.dev_head = Some(before);
                    if attempt == 1 || !matches!(e, ForkError::Held(_)) {
                        if let ForkError::Held(r) = &e {
                            state.held = Some(r.clone());
                        }
                        fail(&mut out, e);
                        return out;
                    }
                }
            }
        }
        out.code_changed = code_changed;
        out
    }

    /// After an apply: ask the Editor to refresh, wait for the compile
    /// result and record it (with the head it is for) in the dev state. The
    /// controller reverts a submission whose merge broke compilation (owner
    /// decision 3) by sending a `revert` op.
    pub async fn settle(&self, code_changed: bool) -> Compile {
        let head = self.state.lock().await.applied.clone();
        let mut compile = self.refresh_and_compile(code_changed).await;
        compile.head = Some(head);
        self.state.lock().await.compile = compile.clone();
        compile
    }

    /// `revert` op: undo a merged submission on the dev head and apply it.
    pub async fn revert(&self, commit: &str, message: &str) -> Outcome {
        let mut out = Outcome::default();
        let Ok(bad) = Commit::parse(commit) else {
            out.state = "invalid".into();
            out.error = Some("revert op names no commit".into());
            return out;
        };
        let mut state = self.state.lock().await;
        let before = match self.capture_locked(&mut state, Vec::new()).await {
            Ok(h) => h,
            Err(e) => {
                out.state = e.code().into();
                out.error = Some(e.to_string());
                return out;
            }
        };
        out.dev_head_before = Some(before.clone());
        let (p, t, b, x, m) = (
            self.provider.clone(),
            self.tree.clone(),
            before.clone(),
            bad,
            if message.trim().is_empty() {
                "Revert a submission".to_owned()
            } else {
                message.to_owned()
            },
        );
        let reverted = match blocking(move || p.revert(&t, &b, &x, &m)).await {
            Ok(c) => c,
            Err(e) => {
                out.state = e.code().into();
                if let ForkError::Conflict { paths } = &e {
                    out.conflict_paths = paths.clone();
                }
                out.error = Some(e.to_string());
                return out;
            }
        };
        match self.apply_locked(&before, &reverted).await {
            Ok(_) => {
                state.applied = reverted.clone();
                out.state = "applied".into();
                out.applied = Some(reverted.clone());
                out.dev_head = Some(reverted);
            }
            Err(e) => {
                let (p, t, r, b) = (
                    self.provider.clone(),
                    self.tree.clone(),
                    reverted,
                    before.clone(),
                );
                let _ = blocking(move || p.set_dev_head(&t, &r, &b)).await;
                out.state = e.code().into();
                out.dev_head = Some(before);
                out.error = Some(e.to_string());
                return out;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forks::{ForkId, SlotAccount, git::tests::fixture};
    use axum::{Json, Router, extract::State, routing::get, routing::post};
    use std::sync::Mutex as StdMutex;

    fn write(root: &std::path::Path, path: &str, text: &str) {
        let f = crate::forks::prescan::join(root, path);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, text).unwrap();
    }

    /// A fake Editor bridge: grants the source lock, answers `/refresh` and
    /// reports `lastCompileOk` from whether `Game.cs` contains "broken".
    async fn fake_bridge(dir: &std::path::Path, tree: PathBuf) -> Bridge {
        #[derive(Clone)]
        struct S {
            tree: PathBuf,
            nonce: Arc<StdMutex<String>>,
        }
        async fn status(State(s): State<S>) -> Json<Value> {
            let game = std::fs::read_to_string(s.tree.join("unity-project/Assets/Scripts/Game.cs"))
                .unwrap_or_default();
            let ok = !game.contains("broken");
            Json(
                json!({"type": "unity-editor", "compiling": false, "playing": false,
                "refresh": "done", "refreshNonce": *s.nonce.lock().unwrap(), "instance": "i1",
                "saveSeq": 0, "lastCompileOk": ok, "lastCompileAt": 1,
                "compileErrors": if ok { json!([]) } else { json!([{"file": "Assets/Scripts/Game.cs", "line": 1, "message": "error CS1002"}]) }}),
            )
        }
        async fn refresh(State(s): State<S>, Json(body): Json<Value>) -> Json<Value> {
            *s.nonce.lock().unwrap() = body["nonce"].as_str().unwrap_or("").into();
            Json(json!({"refresh": "done"}))
        }
        let s = S {
            tree,
            nonce: Arc::new(StdMutex::new(String::new())),
        };
        let app = Router::new()
            .route("/status", get(status))
            .route("/refresh", post(refresh))
            .with_state(s);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await });
        let config = dir.join("bridge.json");
        std::fs::write(&config, json!({"port": port}).to_string()).unwrap();
        // The Editor's side of the handshake.
        let b = Bridge {
            config: config.clone(),
            token: "t".repeat(64),
        };
        let (req, ready) = (b.sync_file(""), b.sync_file("-ready"));
        tokio::spawn(async move {
            loop {
                if let Ok(text) = std::fs::read_to_string(&req) {
                    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                    if let Some(n) = v["nonce"].as_str() {
                        let _ = std::fs::write(&ready, n);
                    }
                } else {
                    let _ = std::fs::remove_file(&ready);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        b
    }

    async fn dev() -> (crate::forks::git::tests::Fixture, Arc<Dev>) {
        let f = fixture();
        let p: Arc<dyn ForkProvider> = Arc::new(
            crate::forks::git::GitTreeProvider::new(
                "git".into(),
                &f.s.git_dir.parent().unwrap().join(".node"),
                false,
            )
            .unwrap(),
        );
        let head = p.dev_head(&f.s).unwrap();
        let bridge = fake_bridge(f.s.forks_root.parent().unwrap(), f.s.work_tree.clone()).await;
        let d = Arc::new(Dev {
            tree: f.s.clone(),
            bridge: Some(bridge),
            provider: p,
            known: vec!["node-session-secret-value".into()],
            state: tokio::sync::Mutex::new(DevState {
                applied: head,
                held: None,
                compile: Compile {
                    state: "ok".into(),
                    errors: vec![],
                    head: None,
                },
                live_code_pending: vec![],
                saves: None,
                publish: None,
            }),
        });
        (f, d)
    }
    fn new_fork(f: &crate::forks::git::tests::Fixture, n: u8) -> ForkTree {
        let id = ForkId::parse(&format!("{:032x}", 0xf00 + u128::from(n))).unwrap();
        f.p.create(&f.s, &id, None, &SlotAccount::node()).unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn content_merges_apply_and_code_needs_approval() {
        let (f, d) = dev().await;
        let k = new_fork(&f, 1);
        write(&k.tree, "unity-project/Assets/Art/a.txt", "a\n");
        let head = f.p.snapshot(&k, "x").unwrap().head;
        let before = f.p.dev_head(&f.s).unwrap();
        let op = MergeOp {
            fork_head: head.0.clone(),
            expected_dev_head: before.0.clone(),
            message: "Add art".into(),
            approved_by: None,
        };
        let out = d.submit(k.clone(), &op).await;
        assert_eq!(out.state, "applied", "{out:?}");
        assert_eq!(d.settle(out.code_changed).await.state, "ok");
        assert!(
            f.s.work_tree
                .join("unity-project/Assets/Art/a.txt")
                .is_file()
        );
        // Code without the owner's approval is refused.
        let c = new_fork(&f, 2);
        write(
            &c.tree,
            "unity-project/Assets/Scripts/Game.cs",
            "class Game { int z; }\n",
        );
        let head = f.p.snapshot(&c, "x").unwrap().head;
        let mut op = MergeOp {
            fork_head: head.0.clone(),
            expected_dev_head: out.applied.unwrap().0,
            message: String::new(),
            approved_by: None,
        };
        let refused = d.submit(c.clone(), &op).await;
        assert_eq!(refused.state, "refused");
        assert_eq!(refused.findings[0].rule, "owner-approval-required");
        op.approved_by = Some("usr_owner".into());
        assert_eq!(d.submit(c, &op).await.state, "applied");
        let report = d.report().await;
        assert_eq!(report["compile"]["state"], "ok");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn secrets_and_credential_files_are_refused() {
        let (f, d) = dev().await;
        let k = new_fork(&f, 3);
        write(
            &k.tree,
            "unity-project/Assets/Data/cfg.json",
            "{\"t\": \"node-session-secret-value\"}\n",
        );
        write(&k.tree, "unity-project/Assets/.env", "A=1\n");
        let head = f.p.snapshot(&k, "x").unwrap().head;
        let out = d
            .submit(
                k,
                &MergeOp {
                    fork_head: head.0,
                    expected_dev_head: f.p.dev_head(&f.s).unwrap().0,
                    message: String::new(),
                    approved_by: Some("usr_owner".into()),
                },
            )
            .await;
        assert_eq!(out.state, "refused");
        let rules: Vec<_> = out.findings.iter().map(|x| x.rule.as_str()).collect();
        assert!(
            rules.contains(&"credential-path") && rules.contains(&"known-secret"),
            "{rules:?}"
        );
        assert!(
            !serde_json::to_string(&out)
                .unwrap()
                .contains("node-session-secret-value")
        );
    }

    /// A broken merge reports its compile errors for the applied head; the
    /// controller's `revert` op then restores the last compiling tree.
    #[tokio::test(flavor = "multi_thread")]
    async fn compile_errors_are_reported_and_a_revert_op_restores_the_tree() {
        let (f, d) = dev().await;
        let k = new_fork(&f, 4);
        write(
            &k.tree,
            "unity-project/Assets/Scripts/Game.cs",
            "class Game { broken }\n",
        );
        let head = f.p.snapshot(&k, "x").unwrap().head;
        let out = d
            .submit(
                k,
                &MergeOp {
                    fork_head: head.0,
                    expected_dev_head: f.p.dev_head(&f.s).unwrap().0,
                    message: String::new(),
                    approved_by: Some("usr_owner".into()),
                },
            )
            .await;
        assert_eq!(out.state, "applied", "{out:?}");
        assert!(out.code_changed);
        let merged = out.applied.clone().unwrap();
        let compile = d.settle(true).await;
        assert_eq!(compile.state, "errors");
        assert_eq!(compile.head.as_ref(), Some(&merged));
        assert_eq!(compile.errors[0]["line"], 1);
        assert_eq!(d.report().await["compile"]["head"], json!(merged));
        let reverted = d
            .revert(merged.as_str(), "Revert: it does not compile")
            .await;
        assert_eq!(reverted.state, "applied", "{reverted:?}");
        assert_eq!(f.p.dev_head(&f.s).unwrap(), reverted.applied.unwrap());
        assert_eq!(
            std::fs::read_to_string(f.s.work_tree.join("unity-project/Assets/Scripts/Game.cs"))
                .unwrap(),
            "class Game {}\n"
        );
        assert_eq!(d.settle(true).await.state, "ok");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn saves_are_captured_when_the_counter_moves() {
        let (f, d) = dev().await;
        let head0 = f.p.dev_head(&f.s).unwrap();
        write(
            &f.s.work_tree,
            "unity-project/Assets/Art/live.txt",
            "live\n",
        );
        // First status sets the baseline (and captures).
        assert!(d.capture(false, vec![]).await.unwrap());
        assert!(!d.capture(false, vec![]).await.unwrap());
        let head = f.p.dev_head(&f.s).unwrap();
        assert_eq!(d.state.lock().await.applied, head);
        assert_ne!(head, head0);
    }
}

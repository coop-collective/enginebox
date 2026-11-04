//! One agent run in a fork (contract §4, §5.2 `run` ops): register a relay
//! token, write the harness configuration into the fork's home, hand the
//! tree to the slot, start Claude Code or Codex, stream its transcript as
//! redacted events, enforce the timeout and cancellation, then take the tree
//! back and snapshot it.
use super::{
    ForkProvider, ForkTree, Snapshot,
    classify::redact,
    harness::{self, AgentTools},
    relay::{Relay, RunGrant, RunUsage},
    slots::{self, Limits, Slot},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::{Duration, SystemTime};
use tokio::sync::watch;

/// The default run timeout (owner decision) and the controller's maximum.
pub const DEFAULT_TIMEOUT: u64 = 30 * 60;
pub const MAX_TIMEOUT: u64 = 2 * 3600;
/// Text in one event is clipped to this many bytes.
const EVENT_TEXT: usize = 4096;

/// `Op.run` from `engine-poll`.
#[derive(Clone, Debug, Deserialize)]
pub struct RunOp {
    pub run_id: String,
    pub provider: String,
    #[serde(default)]
    pub model: Option<String>,
    pub prompt: String,
    #[serde(default)]
    pub resume_id: Option<String>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    pub budget_usd: f64,
}
impl RunOp {
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(
            self.timeout_seconds
                .unwrap_or(DEFAULT_TIMEOUT)
                .clamp(60, MAX_TIMEOUT),
        )
    }
}

/// `secrets.gateway`: the run's xinf access token (memory only).
#[derive(Clone, Deserialize)]
pub struct Gateway {
    pub access_token: String,
    /// RFC 3339 or Unix seconds.
    pub expires_at: Value,
    pub base_url: String,
}
impl std::fmt::Debug for Gateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Gateway({})", self.base_url)
    }
}
impl Gateway {
    pub fn expiry(&self) -> SystemTime {
        let secs = match &self.expires_at {
            Value::Number(n) => n.as_u64(),
            Value::String(s) => chrono::DateTime::parse_from_rfc3339(s)
                .ok()
                .and_then(|t| u64::try_from(t.timestamp()).ok()),
            _ => None,
        };
        // 0: an API key, which does not expire (the run deadline still applies).
        secs.map(|s| {
            if s == 0 {
                SystemTime::now() + Duration::from_secs(MAX_TIMEOUT + 3600)
            } else {
                SystemTime::UNIX_EPOCH + Duration::from_secs(s)
            }
        })
        // Unknown: treat as an hour (xinf access tokens' lifetime).
        .unwrap_or_else(|| SystemTime::now() + Duration::from_secs(3600))
    }
}

/// One transcript event (`fork-events`).
#[derive(Clone, Debug, Serialize)]
pub struct Event {
    pub seq: u64,
    pub at: String,
    /// Unix seconds (`fork-events` `at`).
    pub unix: u64,
    pub kind: &'static str,
    pub data: Value,
}

/// What a run ended with.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Outcome {
    pub native_session_id: Option<String>,
    pub spent_usd: f64,
    pub requests: u64,
    pub exit: Option<i32>,
    /// `completed`, `failed`, `timeout` or `cancelled`.
    pub state: String,
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Snapshot>,
}

fn clip(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_owned();
    }
    let mut end = n;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Turn one harness output line into transcript events (and the native
/// session id when it shows up).
pub fn parse_line(
    provider: &str,
    line: &str,
    session: &mut Option<String>,
) -> Vec<(&'static str, Value)> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    match provider {
        "claude" => match v["type"].as_str() {
            Some("system") if v["subtype"] == "init" => {
                if let Some(id) = v["session_id"].as_str() {
                    *session = Some(id.to_owned());
                }
                out.push(("status", json!({"state": "started", "model": v["model"]})));
            }
            Some("assistant") => {
                for c in v["message"]["content"].as_array().into_iter().flatten() {
                    match c["type"].as_str() {
                        Some("text") => out.push((
                            "text",
                            json!({"text": clip(c["text"].as_str().unwrap_or(""), EVENT_TEXT)}),
                        )),
                        Some("tool_use") => out.push((
                            "tool",
                            json!({"name": c["name"], "input": clip(&c["input"].to_string(), 1024)}),
                        )),
                        _ => {}
                    }
                }
            }
            Some("result") => {
                if let Some(id) = v["session_id"].as_str() {
                    *session = Some(id.to_owned());
                }
                out.push((
                    "usage",
                    json!({"usage": v["usage"], "turns": v["num_turns"], "duration_ms": v["duration_ms"]}),
                ));
                out.push((
                    "status",
                    json!({"state": if v["is_error"] == true { "error" } else { "done" },
                        "result": clip(v["result"].as_str().unwrap_or(""), EVENT_TEXT)}),
                ));
            }
            _ => {}
        },
        "codex" => match v["type"].as_str() {
            Some("thread.started") => {
                if let Some(id) = v["thread_id"].as_str() {
                    *session = Some(id.to_owned());
                }
                out.push(("status", json!({"state": "started"})));
            }
            Some("item.completed") => {
                let item = &v["item"];
                match item["type"].as_str() {
                    Some("agent_message") => out.push((
                        "text",
                        json!({"text": clip(item["text"].as_str().unwrap_or(""), EVENT_TEXT)}),
                    )),
                    Some("command_execution") => out.push((
                        "tool",
                        json!({"name": "shell", "input": clip(item["command"].as_str().unwrap_or(""), 1024),
                            "exit_code": item["exit_code"],
                            "output": clip(item["aggregated_output"].as_str().unwrap_or(""), 1024)}),
                    )),
                    Some("file_change") => out.push((
                        "tool",
                        json!({"name": "edit", "input": clip(&item["changes"].to_string(), 1024)}),
                    )),
                    _ => {}
                }
            }
            Some("turn.completed") => out.push(("usage", json!({"usage": v["usage"]}))),
            Some("turn.failed") | Some("error") => out.push((
                "status",
                json!({"state": "error", "message": clip(v["error"]["message"].as_str().or(v["message"].as_str()).unwrap_or(""), 1024)}),
            )),
            _ => {}
        },
        _ => {}
    }
    out
}

/// Everything a run needs from the node.
pub struct Context<'a> {
    pub provider: &'a dyn ForkProvider,
    pub relay: &'a Relay,
    pub tools: &'a AgentTools,
    pub limits: Limits,
    /// Values to redact from events and errors (session and node secrets).
    pub known: Vec<String>,
}

/// Run `op` in `fork` as `slot`. `emit` receives events in order; the
/// outcome carries the spend and the fork snapshot afterwards.
pub async fn run(
    ctx: &Context<'_>,
    fork: &ForkTree,
    slot: &Slot,
    op: &RunOp,
    gateway: &Gateway,
    mut cancel: watch::Receiver<bool>,
    mut emit: impl FnMut(Event),
) -> Outcome {
    let mut seq = 0u64;
    let started = std::time::Instant::now();
    let deadline = SystemTime::now() + op.timeout();
    let mut known = ctx.known.clone();
    known.push(gateway.access_token.clone());
    if let Some(c) = &slot.credential {
        known.push(c.password.clone());
    }
    let mut outcome = Outcome {
        state: "failed".into(),
        ..Default::default()
    };
    let token = match ctx.relay.register(RunGrant {
        run_id: op.run_id.clone(),
        access_token: gateway.access_token.clone(),
        base_url: gateway.base_url.clone(),
        budget_usd: op.budget_usd,
        expires_at: deadline,
    }) {
        Ok(t) => t,
        Err(e) => {
            outcome.error = Some(redact(&format!("{e:#}"), &refs(&known)));
            return outcome;
        }
    };
    ctx.relay
        .refresh(&op.run_id, gateway.access_token.clone(), gateway.expiry());
    known.push(token.clone());
    let sandboxed = slot.account.is_node();
    let mut emit_one = |kind: &'static str, data: Value, known: &[String]| {
        seq += 1;
        let text = redact(&data.to_string(), &refs(known));
        emit(Event {
            seq,
            at: chrono::Utc::now().to_rfc3339(),
            unix: chrono::Utc::now().timestamp().max(0) as u64,
            kind,
            data: serde_json::from_str(&text).unwrap_or(Value::Null),
        });
    };
    let result: anyhow::Result<Option<i32>> = async {
        let plan = harness::prepare(
            ctx.tools,
            &harness::Request {
                provider: &op.provider,
                model: op.model.as_deref(),
                prompt: &op.prompt,
                resume_id: op.resume_id.as_deref(),
            },
            &fork.home,
            &fork.cwd(),
            &ctx.relay.base_url(),
            &token,
            sandboxed,
        )?;
        ctx.provider
            .grant(fork, &slot.account)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut child = slots::spawn(&plan, slot, &ctx.limits)?;
        let mut lines = std::mem::replace(
            &mut child.lines,
            tokio::sync::mpsc::unbounded_channel().1,
        );
        emit_one("status", json!({"state": "running", "slot": slot.account.name}), &known);
        let mut session = op.resume_id.clone();
        let mut lines_open = true;
        let timeout = tokio::time::sleep(op.timeout());
        tokio::pin!(timeout);
        loop {
            tokio::select! {
                line = lines.recv(), if lines_open => match line {
                    Some(line) => {
                        for (kind, data) in parse_line(&op.provider, &line, &mut session) {
                            emit_one(kind, data, &known);
                        }
                    }
                    None => lines_open = false,
                },
                code = child.wait(), if !lines_open => {
                    outcome.native_session_id = session;
                    outcome.state = if code == Some(0) { "completed" } else { "failed" }.into();
                    if code != Some(0) {
                        let tail = child.stderr.lock().map(|t| t.clone()).unwrap_or_default();
                        let last = tail.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
                        outcome.error = Some(format!("The agent exited with {}: {}",
                            code.map_or("a signal".into(), |c| c.to_string()), clip(last, 300)));
                    }
                    return Ok(code);
                }
                _ = &mut timeout => {
                    child.kill();
                    outcome.native_session_id = session;
                    outcome.state = "timeout".into();
                    outcome.error = Some(format!("The run reached its {} minute limit", op.timeout().as_secs() / 60));
                    return Ok(None);
                }
                _ = cancel.changed() => if *cancel.borrow() {
                    child.kill();
                    outcome.native_session_id = session;
                    outcome.state = "cancelled".into();
                    return Ok(None);
                },
            }
        }
    }
    .await;
    match result {
        Ok(exit) => outcome.exit = exit,
        Err(e) => {
            outcome.state = "failed".into();
            outcome.error = Some(format!("{e:#}"));
        }
    }
    let usage: RunUsage = ctx.relay.revoke(&op.run_id).unwrap_or_default();
    outcome.spent_usd = usage.spent_usd;
    outcome.requests = usage.requests;
    harness::scrub(&fork.home);
    if let Err(e) = ctx.provider.revoke(fork) {
        outcome.state = "failed".into();
        outcome.error = Some(format!("Could not take the fork back from the slot: {e}"));
    }
    // The tree after the run is the fork's new snapshot.
    match ctx
        .provider
        .snapshot(fork, &format!("Agent run {}", op.run_id))
    {
        Ok(s) => outcome.snapshot = Some(s),
        Err(e) => {
            if outcome.error.is_none() {
                outcome.error = Some(format!("The fork could not be recorded: {e}"));
            }
            if outcome.state == "completed" {
                outcome.state = "failed".into();
            }
        }
    }
    outcome.error = outcome.error.map(|e| clip(&redact(&e, &refs(&known)), 600));
    emit_one(
        "status",
        json!({"state": outcome.state, "seconds": started.elapsed().as_secs(),
            "spent_usd": outcome.spent_usd, "requests": outcome.requests}),
        &known,
    );
    outcome
}

fn refs(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::forks::{SlotAccount, git::tests::fixture, relay::RelayConfig};

    #[test]
    fn transcripts_become_bounded_events() {
        let mut s = None;
        let ev = parse_line(
            "claude",
            r#"{"type":"system","subtype":"init","session_id":"abc-1","model":"claude-x"}"#,
            &mut s,
        );
        assert_eq!(s.as_deref(), Some("abc-1"));
        assert_eq!(ev[0].0, "status");
        let ev = parse_line(
            "claude",
            &json!({"type":"assistant","message":{"content":[{"type":"text","text":"x".repeat(9000)},
                {"type":"tool_use","name":"Edit","input":{"file_path":"a.cs"}}]}})
            .to_string(),
            &mut s,
        );
        assert_eq!(ev.len(), 2);
        assert!(ev[0].1["text"].as_str().unwrap().len() <= EVENT_TEXT + 3);
        assert_eq!(ev[1].1["name"], "Edit");
        let mut c = None;
        let ev = parse_line(
            "codex",
            r#"{"type":"thread.started","thread_id":"t-9"}"#,
            &mut c,
        );
        assert_eq!((c.as_deref(), ev.len()), (Some("t-9"), 1));
        let ev = parse_line(
            "codex",
            r#"{"type":"item.completed","item":{"type":"command_execution","command":"ls","exit_code":0,"aggregated_output":"a"}}"#,
            &mut c,
        );
        assert_eq!(ev[0].1["name"], "shell");
        assert!(parse_line("codex", "not json", &mut c).is_empty());
    }

    /// A fake harness (a shell script standing in for `claude`) runs in a
    /// fork through the real relay: it reads its settings, edits the tree and
    /// prints stream-json; secrets never reach the events.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_run_edits_the_fork_and_reports_redacted_events() {
        let f = fixture();
        let id = crate::forks::ForkId::parse(&"c".repeat(32)).unwrap();
        let fork = f.p.create(&f.s, &id, None, &SlotAccount::node()).unwrap();
        let bin = f.s.forks_root.parent().unwrap().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let claude = bin.join("claude");
        std::fs::write(
            &claude,
            "#!/bin/sh\ncat >/dev/null\n\
             tok=$(sed -n 's/.*\"ANTHROPIC_AUTH_TOKEN\": \"\\(ebr_[0-9a-f]*\\)\".*/\\1/p' \"$CLAUDE_CONFIG_DIR/settings.json\")\n\
             echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s-1\"}'\n\
             echo \"{\\\"type\\\":\\\"assistant\\\",\\\"message\\\":{\\\"content\\\":[{\\\"type\\\":\\\"text\\\",\\\"text\\\":\\\"my token is $tok\\\"}]}}\"\n\
             mkdir -p Assets/Art && echo red > Assets/Art/card.txt\n\
             echo '{\"type\":\"result\",\"session_id\":\"s-1\",\"is_error\":false,\"result\":\"done\"}'\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        let tools = AgentTools {
            claude: Some(claude),
            ..Default::default()
        };
        let relay = Relay::start(RelayConfig::default()).await.unwrap();
        let ctx = Context {
            provider: &f.p,
            relay: &relay,
            tools: &tools,
            limits: Limits::default(),
            known: vec![],
        };
        let slot = Slot {
            account: SlotAccount::node(),
            credential: None,
        };
        let op = RunOp {
            run_id: "r1".into(),
            provider: "claude".into(),
            model: None,
            prompt: "Make the card red".into(),
            resume_id: None,
            timeout_seconds: Some(60),
            budget_usd: 2.0,
        };
        let gateway = Gateway {
            access_token: "xat_live_access_token_for_member".into(),
            expires_at: json!(4102444800u64),
            base_url: "https://xavainf.com".into(),
        };
        let (_tx, rx) = watch::channel(false);
        let mut events = Vec::new();
        let out = run(&ctx, &fork, &slot, &op, &gateway, rx, |e| events.push(e)).await;
        assert_eq!(out.state, "completed", "{out:?}");
        assert_eq!(out.native_session_id.as_deref(), Some("s-1"));
        let snap = out.snapshot.unwrap();
        assert_eq!(snap.changed[0].path, "unity-project/Assets/Art/card.txt");
        let text = serde_json::to_string(&events).unwrap();
        assert!(text.contains("my token is [redacted]"), "{text}");
        assert!(!text.contains("ebr_") && !text.contains("xat_"));
        // The token left the home and the relay with the run.
        assert!(!fork.home.join(".claude/settings.json").exists());
        assert!(relay.usage("r1").is_none());
        assert_eq!(events.first().unwrap().seq, 1);
        assert_eq!(events.last().unwrap().data["state"], "completed");
    }
}

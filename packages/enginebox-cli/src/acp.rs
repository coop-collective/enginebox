//! ACP v1 transport for existing controller-owned native runs.
//! Provider loops, tools, credentials and memory stay in the assigned guest.
use crate::FleetClient;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{BufRead, Write},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const MAX_FRAME: usize = 64 * 1024;
const MAX_SESSIONS: usize = 64;

#[derive(Clone, clap::Args)]
pub struct Options {
    /// Assigned private workspace; ACP does not create or recycle workspaces.
    #[arg(long)]
    pub workspace: String,
    #[arg(long, value_parser = ["codex", "claude", "cursor"])]
    pub provider: String,
    /// Absolute workspace root inside the guest.
    #[arg(long, default_value = "/workspace")]
    pub workspace_root: String,
    /// Map client cwd beneath this local directory to the remote workspace root.
    /// This maps paths only; it does not synchronize local files.
    #[arg(long)]
    pub local_root: Option<PathBuf>,
    #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..=300))]
    pub timeout: u64,
    #[arg(long)]
    pub model: Option<String>,
}

#[derive(Clone)]
struct Target {
    host: String,
    workspace: String,
    provider: String,
}
impl Target {
    fn validate(&self) -> Result<()> {
        enginebox_core::identifier(&self.host, false)?;
        enginebox_core::identifier(&self.workspace, true)?;
        ensure!(
            ["codex", "claude", "cursor"].contains(&self.provider.as_str()),
            "Unsupported provider"
        );
        Ok(())
    }
    fn path(&self) -> String {
        format!(
            "/v1/hosts/{}/instances/{}/agent-runs",
            self.host, self.workspace
        )
    }
}

struct Session {
    target: Target,
    workdir: String,
    native_id: Option<String>,
    busy: bool,
    continuation_lost: bool,
    cancel: Arc<AtomicBool>,
}

type Sessions = Arc<Mutex<BTreeMap<String, Session>>>;
type Output = Arc<Mutex<Box<dyn Write + Send>>>;

fn emit(output: &Output, value: Value) -> Result<()> {
    let mut output = output.lock().unwrap();
    serde_json::to_writer(&mut *output, &value)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}
fn reply(output: &Output, id: &Value, result: Value) -> Result<()> {
    emit(output, json!({"jsonrpc":"2.0","id":id,"result":result}))
}
fn error(output: &Output, id: &Value, code: i64, message: &str) -> Result<()> {
    emit(
        output,
        json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}),
    )
}
fn update(output: &Output, session: &str, value: Value) -> Result<()> {
    emit(
        output,
        json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session,"update":value}}),
    )
}

fn remote_parts(path: &str) -> Result<Vec<&str>> {
    ensure!(
        path.starts_with('/') && !path.contains('\\') && !path.chars().any(char::is_control),
        "cwd must be an absolute guest path"
    );
    let parts = path
        .split('/')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    ensure!(
        !parts.iter().any(|s| [".", ".."].contains(s)),
        "cwd must not contain traversal components"
    );
    Ok(parts)
}

impl Options {
    fn workdir(&self, cwd: &str) -> Result<String> {
        let relative = if let Some(root) = &self.local_root {
            let path = Path::new(cwd);
            ensure!(
                path.is_absolute() && !path.components().any(|c| matches!(c, Component::ParentDir)),
                "cwd must be an absolute local path without traversal"
            );
            let path = path.canonicalize().context("Local cwd does not exist")?;
            let relative = path
                .strip_prefix(root)
                .context("cwd is outside --local-root")?;
            relative
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/")
        } else {
            let root = remote_parts(&self.workspace_root)?;
            let cwd = remote_parts(cwd)?;
            ensure!(
                cwd.starts_with(&root),
                "cwd is outside --workspace-root; use --local-root to map an editor checkout"
            );
            cwd[root.len()..].join("/")
        };
        ensure!(
            relative.len() <= 512
                && !relative.contains('\\')
                && !relative.chars().any(char::is_control),
            "Invalid workspace path"
        );
        Ok(if relative.is_empty() {
            ".".into()
        } else {
            relative
        })
    }
}

fn text_prompt(params: &Value) -> Result<String> {
    let blocks = params["prompt"]
        .as_array()
        .context("prompt must be an array")?;
    ensure!(!blocks.is_empty(), "prompt must contain text");
    let texts = blocks
        .iter()
        .map(|block| {
            ensure!(
                block["type"] == "text",
                "Only text prompts are supported; attach context as text"
            );
            block["text"].as_str().context("text must be a string")
        })
        .collect::<Result<Vec<_>>>()?;
    let text = texts.join("\n");
    ensure!(
        !text.trim().is_empty() && text.len() <= 12000 && !text.contains('\0'),
        "Prompt must contain 1–12000 UTF-8 bytes without NUL"
    );
    Ok(text)
}

fn native_text(output: &str) -> String {
    let mut chunks = Vec::new();
    let mut final_text = None;
    for event in output
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
    {
        if event["type"] == "item.completed" && event["item"]["type"] == "agent_message" {
            if let Some(text) = event["item"]["text"].as_str() {
                chunks.push(text.to_owned());
            }
        } else if event["type"] == "assistant" {
            if let Some(content) = event["message"]["content"].as_array() {
                for block in content {
                    if block["type"] == "text"
                        && let Some(text) = block["text"].as_str()
                    {
                        chunks.push(text.to_owned());
                    }
                }
            }
        } else if event["type"] == "result"
            && let Some(text) = event["result"].as_str()
        {
            final_text = Some(text.to_owned());
        }
    }
    if chunks.is_empty() {
        final_text.unwrap_or_default()
    } else {
        chunks.join("\n")
    }
}

fn exact_session(id: &str) -> bool {
    uuid::Uuid::parse_str(id)
        .is_ok_and(|uuid| uuid.hyphenated().to_string().eq_ignore_ascii_case(id))
}

struct Turn {
    request_id: Value,
    session_id: String,
    target: Target,
    workdir: String,
    prompt: String,
    resume_id: Option<String>,
    cancel: Arc<AtomicBool>,
}

struct RunGuard<'a> {
    client: &'a FleetClient,
    path: String,
    confirmed: bool,
}
impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        if !self.confirmed {
            // A broken output pipe or failed poll must still request remote stop.
            // Failure is not reported as cancellation: the session becomes uncertain.
            let _ = self
                .client
                .request(&format!("{}/cancel", self.path), Some(json!({})));
        }
    }
}

fn prompt_turn(
    client: &FleetClient,
    options: &Options,
    output: &Output,
    turn: &Turn,
) -> Result<(Option<String>, bool, Value)> {
    if turn.cancel.load(Ordering::SeqCst) {
        return Ok((
            turn.resume_id.clone(),
            false,
            json!({"result":{"stopReason":"cancelled"}}),
        ));
    }
    let submitted = client.request(
        &turn.target.path(),
        Some(json!({
            "provider":turn.target.provider,"prompt":turn.prompt,"workdir":turn.workdir,
            "resume_id":turn.resume_id,"timeout_seconds":options.timeout,"model":options.model
        })),
    )?;
    let id = submitted["run"]["id"]
        .as_str()
        .context("Controller did not return a run ID")?;
    ensure!(
        id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "Invalid controller run ID"
    );
    let path = format!("{}/{id}", turn.target.path());
    let mut guard = RunGuard {
        client,
        path: path.clone(),
        confirmed: false,
    };
    update(
        output,
        &turn.session_id,
        json!({
            "sessionUpdate":"tool_call","toolCallId":id,"title":format!("{} native run",turn.target.provider),
            "kind":"other","status":"pending", "_meta":{"enginebox":{"runId":id,"hostId":turn.target.host,"workspace":turn.target.workspace,"provider":turn.target.provider}}
        }),
    )?;
    let deadline = Instant::now() + Duration::from_secs(options.timeout + 90);
    let mut cancel_sent = false;
    let mut last_status = String::new();
    loop {
        if turn.cancel.load(Ordering::SeqCst) && !cancel_sent {
            client.request(&format!("{path}/cancel"), Some(json!({})))?;
            cancel_sent = true;
        }
        let value = client.request(&path, None)?;
        let run = &value["run"];
        let status = run["status"]
            .as_str()
            .context("Controller did not return run status")?;
        if status != last_status {
            let state = match status {
                "queued" => "pending",
                "running" => "in_progress",
                "succeeded" => "completed",
                _ => "failed",
            };
            update(
                output,
                &turn.session_id,
                json!({"sessionUpdate":"tool_call_update","toolCallId":id,"status":state,"_meta":{"enginebox":{"status":status}}}),
            )?;
            last_status = status.into();
        }
        if !["queued", "running"].contains(&status) {
            guard.confirmed = true;
            if let Some(expected) = &turn.resume_id
                && let Some(reported) = run["native_session_id"].as_str()
                && reported != expected
            {
                return Ok((
                    None,
                    true,
                    json!({"error":{"code":-32603,"message":"Native provider returned a different conversation; continuation stopped"}}),
                ));
            }
            let text = native_text(run["result"]["output"].as_str().unwrap_or(""));
            if !text.is_empty() {
                update(
                    output,
                    &turn.session_id,
                    json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text}}),
                )?;
            }
            let native = run["native_session_id"]
                .as_str()
                .filter(|s| exact_session(s))
                .map(str::to_owned)
                .or_else(|| turn.resume_id.clone());
            let metadata = json!({"enginebox":{"runId":id,"provider":turn.target.provider,"elapsedSeconds":run["elapsed_seconds"],"usage":run["usage"],"nativeSessionId":native}});
            let response = match status {
                "succeeded" => json!({"result":{"stopReason":"end_turn","_meta":metadata}}),
                "cancelled" => json!({"result":{"stopReason":"cancelled","_meta":metadata}}),
                _ => {
                    json!({"error":{"code":-32603,"message":"Native run failed; inspect ebox tasks and the run record"}})
                }
            };
            let lost = native.is_none() || !["succeeded", "cancelled"].contains(&status);
            return Ok((native, lost, response));
        }
        ensure!(
            Instant::now() < deadline,
            "Native run did not reach a confirmed terminal state; inspect ebox tasks before retrying"
        );
        thread::sleep(Duration::from_millis(250));
    }
}

fn frame(input: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    let mut frame = Vec::new();
    loop {
        let bytes = input.fill_buf()?;
        if bytes.is_empty() {
            return Ok((!frame.is_empty()).then_some(frame));
        }
        let end = bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(bytes.len(), |n| n + 1);
        ensure!(frame.len() + end <= MAX_FRAME, "ACP input exceeds 64 KiB");
        let complete = bytes[end - 1] == b'\n';
        frame.extend_from_slice(&bytes[..end]);
        input.consume(end);
        if complete {
            return Ok(Some(frame));
        }
    }
}

/// The controller credentials are supplied out of band, never through ACP frames.
pub fn serve(
    client: FleetClient,
    host: String,
    mut options: Options,
    mut input: impl BufRead,
    output: Box<dyn Write + Send>,
) -> Result<()> {
    let default_target = Target {
        host,
        workspace: options.workspace.clone(),
        provider: options.provider.clone(),
    };
    default_target.validate()?;
    remote_parts(&options.workspace_root)?;
    if let Some(root) = &options.local_root {
        ensure!(root.is_absolute(), "--local-root must be absolute");
        options.local_root = Some(root.canonicalize()?);
    }
    let output = Arc::new(Mutex::new(output));
    let sessions: Sessions = Arc::new(Mutex::new(BTreeMap::new()));
    let mut workers: Vec<thread::JoinHandle<()>> = Vec::new();
    let mut initialized = false;
    let outcome = (|| -> Result<()> {
        while let Some(bytes) = frame(&mut input)? {
            // Reap finished turns so a long editor connection does not retain threads.
            let mut remaining = Vec::new();
            for worker in workers.drain(..) {
                if worker.is_finished() {
                    let _ = worker.join();
                } else {
                    remaining.push(worker);
                }
            }
            workers = remaining;
            let request: Value = match serde_json::from_slice(&bytes) {
                Ok(value) => value,
                Err(_) => {
                    error(&output, &Value::Null, -32700, "Parse error")?;
                    continue;
                }
            };
            let id = request.get("id").cloned();
            let method = request["method"].as_str();
            if request["jsonrpc"] != "2.0"
                || method.is_none()
                || id
                    .as_ref()
                    .is_some_and(|id| !id.is_string() && !id.is_number())
            {
                error(&output, &Value::Null, -32600, "Invalid Request")?;
                continue;
            }
            let method = method.unwrap();
            let params = &request["params"];
            if method == "session/cancel" && id.is_none() {
                if let Some(session) = params["sessionId"]
                    .as_str()
                    .and_then(|id| sessions.lock().unwrap().get(id).map(|s| s.cancel.clone()))
                {
                    session.store(true, Ordering::SeqCst);
                }
                continue;
            }
            let Some(id) = id else {
                continue;
            };
            if method == "initialize" {
                if initialized || params["protocolVersion"].as_u64().is_none() {
                    error(
                        &output,
                        &id,
                        -32602,
                        "Expected a single initialize request with protocolVersion",
                    )?;
                    continue;
                }
                // ACP negotiates the latest supported version, even if the client proposed another.
                initialized = true;
                reply(
                    &output,
                    &id,
                    json!({
                        "protocolVersion":1,
                        "agentInfo":{"name":"enginebox","version":env!("CARGO_PKG_VERSION"),"title":"Engine Box fleet"},
                        "agentCapabilities":{"loadSession":false,"promptCapabilities":{"image":false,"audio":false,"embeddedContext":false},"mcpCapabilities":{"http":false,"sse":false},"_meta":{"enginebox":{"fleetRuns":true,"sessionTarget":true}}},
                        "authMethods":[]
                    }),
                )?;
                continue;
            }
            if !initialized {
                error(&output, &id, -32600, "Initialize before creating sessions")?;
                continue;
            }
            match method {
                "session/new" => {
                    let result = (|| -> Result<Value> {
                        let servers = params["mcpServers"]
                            .as_array()
                            .context("mcpServers must be an array")?;
                        ensure!(
                            servers.is_empty(),
                            "Client MCP servers are not forwarded; configure native guest MCP servers on the controller"
                        );
                        let workdir =
                            options.workdir(params["cwd"].as_str().context("cwd is required")?)?;
                        let metadata = &params["_meta"]["enginebox"];
                        let mut target = default_target.clone();
                        if !metadata.is_null() {
                            let meta = metadata
                                .as_object()
                                .context("enginebox metadata must be an object")?;
                            for (key, value) in meta {
                                let value =
                                    value.as_str().context("Target fields must be strings")?;
                                match key.as_str() {
                                    "hostId" => target.host = value.into(),
                                    "workspace" => target.workspace = value.into(),
                                    "provider" => target.provider = value.into(),
                                    _ => bail!("Unsupported enginebox session target field"),
                                }
                            }
                        }
                        target.validate()?;
                        let mut sessions = sessions.lock().unwrap();
                        ensure!(
                            sessions.len() < MAX_SESSIONS,
                            "Session limit reached; restart this ACP connection"
                        );
                        let session_id = uuid::Uuid::new_v4().to_string();
                        let target_meta = json!({"enginebox":{"hostId":target.host,"workspace":target.workspace,"provider":target.provider,"workdir":workdir}});
                        sessions.insert(
                            session_id.clone(),
                            Session {
                                target,
                                workdir,
                                native_id: None,
                                busy: false,
                                continuation_lost: false,
                                cancel: Arc::new(AtomicBool::new(false)),
                            },
                        );
                        Ok(json!({"sessionId":session_id,"_meta":target_meta}))
                    })();
                    match result {
                        Ok(value) => reply(&output, &id, value)?,
                        Err(e) => error(&output, &id, -32602, &e.to_string())?,
                    }
                }
                "session/prompt" => {
                    let result = (|| -> Result<Turn> {
                        ensure!(workers.len() < 16, "Too many active ACP requests");
                        let prompt = text_prompt(params)?;
                        let session_id = params["sessionId"]
                            .as_str()
                            .context("sessionId is required")?;
                        let mut sessions = sessions.lock().unwrap();
                        let session = sessions.get_mut(session_id).context("Unknown session")?;
                        ensure!(!session.busy, "A prompt is already active for this session");
                        ensure!(
                            !session.continuation_lost,
                            "Native continuation is unavailable; inspect the previous run and create a new session"
                        );
                        session.cancel.store(false, Ordering::SeqCst);
                        session.busy = true;
                        Ok(Turn {
                            request_id: id.clone(),
                            session_id: session_id.into(),
                            target: session.target.clone(),
                            workdir: session.workdir.clone(),
                            prompt,
                            resume_id: session.native_id.clone(),
                            cancel: session.cancel.clone(),
                        })
                    })();
                    match result {
                        Err(e) => error(&output, &id, -32602, &e.to_string())?,
                        Ok(turn) => {
                            let (client, options, output, sessions) = (
                                client.clone(),
                                options.clone(),
                                output.clone(),
                                sessions.clone(),
                            );
                            workers.push(thread::spawn(move || {
                                let outcome = prompt_turn(&client, &options, &output, &turn);
                                let mut sessions = sessions.lock().unwrap();
                                if let Some(session) = sessions.get_mut(&turn.session_id) {
                                    session.busy = false;
                                    match outcome {
                                        Ok((native, lost, mut response)) => {
                                            session.native_id = native;
                                            session.continuation_lost = lost;
                                            response["jsonrpc"] = json!("2.0");
                                            response["id"] = turn.request_id;
                                            // Commit continuation before acknowledging the turn;
                                            // clients may immediately send their next prompt.
                                            let _ = emit(&output, response);
                                        }
                                        Err(_) => {
                                            session.continuation_lost = true;
                                            let _ = error(&output, &turn.request_id, -32603, "Native run could not be confirmed; inspect ebox tasks before retrying");
                                        }
                                    }
                                }
                            }));
                        }
                    }
                }
                "_enginebox/agent_runs" => {
                    if workers.len() >= 16 {
                        error(&output, &id, -32603, "Too many active ACP requests")?;
                        continue;
                    }
                    let (client, output) = (client.clone(), output.clone());
                    workers.push(thread::spawn(move || {
                        let _ = match client.request("/v1/agent-runs", None) {
                            Ok(value) => reply(&output, &id, value),
                            Err(_) => error(&output, &id, -32603, "Fleet task list is unavailable"),
                        };
                    }));
                }
                _ => error(&output, &id, -32601, "Method not found")?,
            }
        }
        Ok(())
    })();
    // EOF is a disconnect, not permission to leave an unobserved remote turn running.
    for session in sessions.lock().unwrap().values() {
        if session.busy {
            session.cancel.store(true, Ordering::SeqCst);
        }
    }
    for worker in workers {
        let _ = worker.join();
    }
    outcome
}

pub fn run(client: FleetClient, host: String, options: Options) -> Result<()> {
    serve(
        client,
        host,
        options,
        std::io::stdin().lock(),
        Box::new(std::io::stdout()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn options() -> Options {
        Options {
            workspace: "ebox-test".into(),
            provider: "codex".into(),
            workspace_root: "/workspace".into(),
            local_root: None,
            timeout: 120,
            model: None,
        }
    }
    #[test]
    fn paths_stay_within_the_configured_guest_root() {
        let options = options();
        assert_eq!(options.workdir("/workspace").unwrap(), ".");
        assert_eq!(options.workdir("/workspace/pkg/src").unwrap(), "pkg/src");
        for path in [
            "/workspace-other",
            "/workspace/../secret",
            "relative",
            "/workspace\\secret",
        ] {
            assert!(options.workdir(path).is_err());
        }
    }
    #[test]
    fn unsupported_content_is_rejected_instead_of_silently_dropped() {
        assert!(text_prompt(&json!({"prompt":[{"type":"image","data":"x"}]})).is_err());
        assert_eq!(
            text_prompt(
                &json!({"prompt":[{"type":"text","text":"one"},{"type":"text","text":"two"}]})
            )
            .unwrap(),
            "one\ntwo"
        );
    }
    #[test]
    fn native_text_excludes_reasoning_and_does_not_duplicate_final_results() {
        let output = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"thinking\",\"thinking\":\"private\"},{\"type\":\"text\",\"text\":\"Done\"}]}}\n{\"type\":\"result\",\"result\":\"Done\"}";
        assert_eq!(native_text(output), "Done");
        assert_eq!(
            native_text(
                "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"Hello\"}}"
            ),
            "Hello"
        );
    }
}

//! Exercise the shipped stdio binary against an authenticated fake fleet API.
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const NATIVE: &str = "c31c0bac-9a1c-4c26-84c1-730f1b9b3b93";

#[derive(Default)]
struct State {
    launches: Vec<Value>,
    runs: BTreeMap<String, Value>,
    cancelled: Vec<String>,
}
struct Fleet {
    url: String,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Fleet {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (s, stopped) = (state.clone(), stop.clone());
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                // macOS may inherit the listener's nonblocking flag. The fixture
                // polls accept, but each accepted HTTP request uses bounded reads.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let parts = line.split_whitespace().collect::<Vec<_>>();
                let (method, path) = (parts[0].to_owned(), parts[1].to_owned());
                let mut length = 0;
                let mut authorized = false;
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(':') {
                        match key.to_lowercase().as_str() {
                            "content-length" => length = value.trim().parse::<usize>().unwrap(),
                            "authorization" => {
                                authorized = value.trim() == format!("Bearer {}", "t".repeat(32))
                            }
                            _ => {}
                        }
                    }
                }
                assert!(authorized, "Every fleet request must authenticate");
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let body: Value = if body.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&body).unwrap()
                };
                let mut state = s.lock().unwrap();
                let result = if method == "POST" && path.ends_with("/agent-runs") {
                    state.launches.push(body.clone());
                    let id = format!("{:032x}", state.launches.len());
                    let run = json!({"id":id,"provider":body["provider"],"status":"running","task":body["prompt"]});
                    state.runs.insert(id, run.clone());
                    json!({"run":run})
                } else if method == "POST" && path.ends_with("/cancel") {
                    let id = path.split('/').rev().nth(1).unwrap();
                    state.cancelled.push(id.into());
                    let run = state.runs.get_mut(id).unwrap();
                    run["status"] = json!("cancelled");
                    json!({"run":run})
                } else if path == "/v1/agent-runs" {
                    json!({"runs":state.runs.values().collect::<Vec<_>>()})
                } else {
                    json!({"run":state.runs[path.rsplit('/').next().unwrap()]})
                };
                drop(state);
                let result = serde_json::to_vec(&result).unwrap();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", result.len()).unwrap();
                stream.write_all(&result).unwrap();
            }
        });
        Self {
            url,
            state,
            stop,
            worker: Some(worker),
        }
    }
    fn wait_launches(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.state.lock().unwrap().launches.len() < count {
            assert!(
                Instant::now() < deadline,
                "ACP did not launch the expected native runs"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn complete(&self, provider: &str) {
        let mut state = self.state.lock().unwrap();
        for run in state
            .runs
            .values_mut()
            .filter(|r| r["provider"] == provider && r["status"] == "running")
        {
            run["status"] = json!("succeeded");
            run["native_session_id"] = json!(NATIVE);
            run["result"] = json!({"output":"{\"type\":\"result\",\"result\":\"Task done\"}\n"});
            run["elapsed_seconds"] = json!(0.5);
            run["usage"] = json!({"scope":"run","input_tokens":123,"output_tokens":7,"cached_input_tokens":100});
        }
    }
}
impl Drop for Fleet {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
    }
}

struct Client {
    process: Child,
    input: Option<ChildStdin>,
    messages: mpsc::Receiver<Value>,
    pending: Vec<Value>,
}

fn ebox_command(fleet: &Fleet) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ebox"));
    command.env_clear();
    // Winsock expands its provider DLL paths through the Windows directory.
    // Preserve OS paths, but not user config, proxy settings or credentials.
    #[cfg(windows)]
    for name in ["SystemRoot", "WINDIR"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .env("ENGINEBOX_API_URL", &fleet.url)
        .env("ENGINEBOX_API_TOKEN", "t".repeat(32));
    command
}

impl Client {
    fn new(fleet: &Fleet) -> Self {
        let mut process = ebox_command(fleet)
            .args([
                "--fleet-host",
                "node",
                "acp",
                "--workspace",
                "ebox-one",
                "--provider",
                "codex",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let output = process.stdout.take().unwrap();
        let (sender, messages) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let value: Value = serde_json::from_str(&line.unwrap())
                    .expect("stdout must contain only ACP frames");
                if sender.send(value).is_err() {
                    break;
                }
            }
        });
        let input = process.stdin.take();
        Self {
            process,
            input,
            messages,
            pending: Vec::new(),
        }
    }
    fn send(&mut self, value: Value) {
        writeln!(self.input.as_mut().unwrap(), "{value}").unwrap();
        self.input.as_mut().unwrap().flush().unwrap();
    }
    fn response(&mut self, id: i64) -> Value {
        if let Some(index) = self.pending.iter().position(|v| v["id"] == id) {
            return self.pending.remove(index);
        }
        loop {
            let value = self
                .messages
                .recv_timeout(Duration::from_secs(5))
                .expect("ACP response timed out");
            if value["id"] == id {
                return value;
            }
            self.pending.push(value);
        }
    }
    fn initialize(&mut self) {
        self.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}));
        let result = self.response(1);
        assert_eq!(result["result"]["protocolVersion"], 1);
        assert_eq!(result["result"]["agentCapabilities"]["loadSession"], false);
    }
    fn session(&mut self, id: i64, provider: &str) -> String {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":"session/new","params":{"cwd":"/workspace","mcpServers":[],"_meta":{"enginebox":{"provider":provider}}}}));
        self.response(id)["result"]["sessionId"]
            .as_str()
            .unwrap()
            .into()
    }
    fn prompt(&mut self, id: i64, session: &str, text: &str) {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":text}]}}));
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        self.input.take();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if self.process.try_wait().unwrap().is_some() {
                break;
            }
            if Instant::now() > deadline {
                let _ = self.process.kill();
                let _ = self.process.wait();
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

#[test]
fn parallel_types_cancel_independently_and_continue_the_exact_native_session() {
    let fleet = Fleet::new();
    let mut client = Client::new(&fleet);
    client.initialize();
    let codex = client.session(2, "codex");
    let claude = client.session(3, "claude");
    client.prompt(10, &codex, "Wait for cancellation");
    client.prompt(11, &claude, "Complete the task");
    fleet.wait_launches(2);
    client.prompt(12, &claude, "Overlapping same-session turn");
    assert_eq!(client.response(12)["error"]["code"], -32602);
    client.send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":codex}}));
    assert_eq!(client.response(10)["result"]["stopReason"], "cancelled");
    {
        let state = fleet.state.lock().unwrap();
        assert_eq!(state.cancelled.len(), 1);
        assert_eq!(state.runs[&state.cancelled[0]]["provider"], "codex");
        assert!(
            state
                .runs
                .values()
                .any(|run| run["provider"] == "claude" && run["status"] == "running")
        );
    }
    fleet.complete("claude");
    let completed = client.response(11);
    assert_eq!(completed["result"]["stopReason"], "end_turn");
    assert_eq!(
        completed["result"]["_meta"]["enginebox"]["usage"]["input_tokens"],
        123
    );
    assert!(
        client
            .pending
            .iter()
            .any(|v| v["params"]["update"]["content"]["text"] == "Task done")
    );
    client.prompt(13, &claude, "Remember the first turn");
    fleet.wait_launches(3);
    assert_eq!(fleet.state.lock().unwrap().launches[2]["resume_id"], NATIVE);
    fleet.complete("claude");
    assert_eq!(client.response(13)["result"]["stopReason"], "end_turn");
    client.prompt(14, &claude, "Detect a provider conversation mismatch");
    fleet.wait_launches(4);
    {
        let mut state = fleet.state.lock().unwrap();
        let run = state.runs.get_mut(&format!("{:032x}", 4)).unwrap();
        run["status"] = json!("succeeded");
        run["native_session_id"] = json!("1b3f2dca-d32d-44a2-9fd1-1c47673b3715");
    }
    assert_eq!(client.response(14)["error"]["code"], -32603);
    client.prompt(15, &claude, "Must not silently switch conversation");
    assert_eq!(client.response(15)["error"]["code"], -32602);
    assert_eq!(fleet.state.lock().unwrap().launches.len(), 4);
}

#[test]
fn invalid_session_inputs_never_launch_a_native_agent() {
    let fleet = Fleet::new();
    let mut client = Client::new(&fleet);
    client.send(json!({"jsonrpc":"2.0","id":9,"method":"session/new","params":{"cwd":"/workspace","mcpServers":[]}}));
    assert_eq!(client.response(9)["error"]["code"], -32600);
    client.initialize();
    for (id, params) in [
        (20, json!({"cwd":"/workspace/../private","mcpServers":[]})),
        (
            21,
            json!({"cwd":"/workspace","mcpServers":[{"name":"injected","command":"sh"}]}),
        ),
        (
            22,
            json!({"cwd":"/workspace","mcpServers":[],"_meta":{"enginebox":{"hostId":"../other"}}}),
        ),
    ] {
        client.send(json!({"jsonrpc":"2.0","id":id,"method":"session/new","params":params}));
        assert_eq!(client.response(id)["error"]["code"], -32602);
    }
    let session = client.session(23, "cursor");
    client.send(json!({"jsonrpc":"2.0","id":24,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"image","data":"x"}]}}));
    assert_eq!(client.response(24)["error"]["code"], -32602);
    client.send(
        json!({"jsonrpc":"2.0","id":25,"method":"session/load","params":{"sessionId":session}}),
    );
    assert_eq!(client.response(25)["error"]["code"], -32601);
    assert!(fleet.state.lock().unwrap().launches.is_empty());
}

#[test]
fn disconnect_requests_remote_cancellation_and_task_cli_uses_the_same_authenticated_list() {
    let fleet = Fleet::new();
    let mut client = Client::new(&fleet);
    client.initialize();
    let session = client.session(2, "cursor");
    client.prompt(3, &session, "Long task");
    fleet.wait_launches(1);
    drop(client);
    assert_eq!(fleet.state.lock().unwrap().cancelled.len(), 1);
    let output = ebox_command(&fleet)
        .args(["tasks", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let tasks: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(tasks["runs"][0]["provider"], "cursor");
    assert_eq!(tasks["runs"][0]["status"], "cancelled");
}

#[test]
fn native_timeout_is_an_error_and_cannot_silently_start_a_new_conversation() {
    let fleet = Fleet::new();
    let mut client = Client::new(&fleet);
    client.initialize();
    let session = client.session(2, "codex");
    client.prompt(3, &session, "Timeout fixture");
    fleet.wait_launches(1);
    {
        let mut state = fleet.state.lock().unwrap();
        let run = state.runs.values_mut().next().unwrap();
        run["status"] = json!("failed");
        run["result"] = json!({"timed_out":true,"exit_code":-1,"output":""});
    }
    assert_eq!(client.response(3)["error"]["code"], -32603);
    client.prompt(4, &session, "Follow up");
    assert_eq!(client.response(4)["error"]["code"], -32602);
    assert_eq!(fleet.state.lock().unwrap().launches.len(), 1);
}

#[test]
fn malformed_frames_receive_protocol_errors_and_watch_json_stays_machine_readable() {
    let fleet = Fleet::new();
    let mut client = Client::new(&fleet);
    writeln!(client.input.as_mut().unwrap(), "not-json").unwrap();
    let parse_error = client
        .messages
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    assert_eq!(parse_error["error"]["code"], -32700);
    assert!(parse_error["id"].is_null());
    client.initialize();
    let mut watch = ebox_command(&fleet)
        .args(["tasks", "--watch", "--json", "--interval", "1"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let (sender, receiver) = mpsc::channel();
    let output = watch.stdout.take().unwrap();
    thread::spawn(move || {
        for line in BufReader::new(output).lines().take(2) {
            sender.send(line.unwrap()).unwrap();
        }
    });
    for _ in 0..2 {
        let line = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!line.contains('\u{001b}'));
        assert!(serde_json::from_str::<Value>(&line).unwrap()["runs"].is_array());
    }
    watch.kill().unwrap();
    watch.wait().unwrap();
}

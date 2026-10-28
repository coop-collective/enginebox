use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    process::{Command, Stdio},
    time::Duration,
};

#[test]
fn installed_stdio_interface_discovers_workspaces_and_uses_authenticated_controller() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "MCP did not call the controller"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("{e}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut header = String::new();
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            if line == "\r\n" {
                break;
            }
            header.push_str(&line);
            assert!(header.len() < 8192);
        }
        assert!(header.starts_with("GET /v1/workspaces HTTP/1.1\r\n"));
        assert!(
            header
                .to_ascii_lowercase()
                .contains(&format!("authorization: bearer {}", "x".repeat(32)))
        );
        let body = json!({"workspaces":[{"id":"fixture","repositories":[]}]}).to_string();
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
    });
    let token = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(token.path(), "x".repeat(32)).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_enginebox-fleet-mcp"));
    command.env_clear();
    for key in ["PATH", "SystemRoot", "WINDIR", "COMSPEC", "PATHEXT"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    let mut child = command
        .env("ENGINEBOX_API_URL", origin)
        .env("ENGINEBOX_API_TOKEN_FILE", token.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for message in [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"workspace_catalog","arguments":{}}}),
    ] {
        writeln!(input, "{message}").unwrap();
    }
    drop(input);
    let output = child.wait_with_output().unwrap();
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let replies = String::from_utf8(output.stdout).unwrap();
    assert!(!replies.contains(&"x".repeat(32)));
    let values = replies
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(values.len(), 3);
    for name in [
        "workspace_catalog",
        "repository_create",
        "repository_link_github",
        "pool_agent_launch",
    ] {
        assert!(
            values[1]["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == name)
        );
    }
    let result: Value =
        serde_json::from_str(values[2]["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(result["workspaces"][0]["id"], "fixture");
}

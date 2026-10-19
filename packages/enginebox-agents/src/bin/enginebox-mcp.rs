//! Stdio MCP desktop tools. Browser tools remain the unmodified Playwright MCP.
use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use enginebox_agents::process;
use serde_json::{Value, json};
use std::io::{BufRead, Read, Write};
use std::time::Duration;
fn tools() -> Value {
    let mut result = json!([
    {"name":"desktop_screenshot","description":"Capture the private workspace desktop as PNG.","inputSchema":{"type":"object","properties":{"name":{"type":"string","default":"ebox-dev"}}}},
    {"name":"desktop_click","description":"Click a coordinate on the 1440 by 900 workspace desktop.","inputSchema":{"type":"object","properties":{"x":{"type":"integer","minimum":0,"maximum":1439},"y":{"type":"integer","minimum":0,"maximum":899},"button":{"type":"integer","enum":[1,2,3],"default":1},"name":{"type":"string","default":"ebox-dev"}},"required":["x","y"]}},
    {"name":"desktop_type","description":"Type literal text into the focused application.","inputSchema":{"type":"object","properties":{"text":{"type":"string","maxLength":10000},"name":{"type":"string","default":"ebox-dev"}},"required":["text"]}},
    {"name":"desktop_key","description":"Send an X11 key or chord, such as Return or ctrl+l.","inputSchema":{"type":"object","properties":{"key":{"type":"string","maxLength":100},"name":{"type":"string","default":"ebox-dev"}},"required":["key"]}}
    ]);
    if !guest_mode() {
        let extra = json!([
            {"name":"environments","description":"List Engine Box environments on the configured host.","inputSchema":{"type":"object","properties":{}}},
            {"name":"environment_state","description":"Pause, resume, hibernate or stop a private environment.","inputSchema":{"type":"object","properties":{"name":{"type":"string"},"action":{"type":"string","enum":["pause","resume","hibernate","stop"]}},"required":["name","action"]}},
            {"name":"environment_exec","description":"Execute literal argv as the private workspace user.","inputSchema":{"type":"object","properties":{"name":{"type":"string"},"argv":{"type":"array","items":{"type":"string"}},"timeout":{"type":"integer","minimum":1,"maximum":300,"default":60}},"required":["name","argv"]}},
            {"name":"environment_snapshot","description":"Snapshot private disk and running RAM; shared mounts are excluded.","inputSchema":{"type":"object","properties":{"name":{"type":"string"},"snapshot":{"type":"string"}},"required":["name","snapshot"]}},
            {"name":"environment_fork","description":"Create a stopped private snapshot clone, including credentials present in the snapshot.","inputSchema":{"type":"object","properties":{"name":{"type":"string"},"snapshot":{"type":"string"},"target":{"type":"string"}},"required":["name","snapshot","target"]}},
            {"name":"environment_restore","description":"Replace stopped private disk with its snapshot, optionally restoring saved RAM.","inputSchema":{"type":"object","properties":{"name":{"type":"string"},"snapshot":{"type":"string"},"stateful":{"type":"boolean","default":true}},"required":["name","snapshot"]}}
        ]);
        result
            .as_array_mut()
            .unwrap()
            .extend(extra.as_array().unwrap().iter().cloned());
    }
    result
}
fn guest_mode() -> bool {
    std::env::var("ENGINEBOX_GUEST").is_ok_and(|v| v == "1")
}
fn argv_for(name: &str, argv: &[String]) -> Result<Vec<String>> {
    if guest_mode() {
        return Ok(argv.to_vec());
    }
    let engine = enginebox_core::EngineBox::from_env()?;
    Ok(engine.command(&engine.exec_args(name, argv)?))
}
fn computer(name: &str, argv: &[String]) -> Result<()> {
    let mut environment = std::env::vars().collect::<std::collections::BTreeMap<_, _>>();
    environment.insert(
        "DISPLAY".into(),
        std::env::var("ENGINEBOX_GUEST_DISPLAY")
            .unwrap_or_else(|_| std::env::var("DISPLAY").unwrap_or(":99".into())),
    );
    process::checked(
        &argv_for(name, argv)?,
        Some(&environment),
        None,
        b"",
        30,
        65536,
    )?;
    Ok(())
}
fn screenshot(name: &str) -> Result<Value> {
    if guest_mode() {
        return Ok(
            json!({"content":[{"type":"image","data":STANDARD.encode(enginebox_agents::browser::screenshot()?),"mimeType":"image/png"}]}),
        );
    }
    let command = argv_for(name, &["enginebox-guest".into(), "screenshot".into()])?;
    let mut cmd = process::command(&command, None, None)?;
    let tempfile = tempfile::tempfile()?;
    cmd.stdin(std::process::Stdio::null())
        .stdout(tempfile.try_clone()?)
        .stderr(std::process::Stdio::null());
    let mut child = cmd.spawn()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let outcome = (|| -> Result<Vec<u8>> {
        loop {
            ensure!(
                std::time::Instant::now() < deadline,
                "Screenshot timed out."
            );
            ensure!(
                tempfile.metadata()?.len() <= 20 * 1024 * 1024,
                "Screenshot exceeds bound."
            );
            if let Some(status) = child.try_wait()? {
                ensure!(status.success(), "Screenshot failed.");
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        use std::io::{Seek, SeekFrom};
        let mut file = tempfile;
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        file.take(20 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 20 * 1024 * 1024 && bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            "Invalid screenshot."
        );
        Ok(bytes)
    })();
    process::stop(&mut child);
    Ok(
        json!({"content":[{"type":"image","data":STANDARD.encode(outcome?),"mimeType":"image/png"}]}),
    )
}
fn call(name: &str, args: &Value) -> Result<Value> {
    let target = args["name"].as_str().unwrap_or("ebox-dev");
    if name.starts_with("environment") {
        ensure!(!guest_mode(), "Host control is unavailable in guest mode.");
        let engine = enginebox_core::EngineBox::from_env()?;
        let snapshot = args["snapshot"].as_str().unwrap_or("");
        let value = match name {
            "environments" => engine.list()?,
            "environment_state" => {
                engine.lifecycle(target, args["action"].as_str().unwrap_or(""))?
            }
            "environment_exec" => {
                let argv: Vec<String> = serde_json::from_value(args["argv"].clone())?;
                json!(String::from_utf8_lossy(&engine.execute(
                    target,
                    &argv,
                    Duration::from_secs(args["timeout"].as_u64().unwrap_or(60))
                )?))
            }
            "environment_snapshot" => engine.snapshot(target, snapshot)?,
            "environment_fork" => {
                engine.fork(target, snapshot, args["target"].as_str().unwrap_or(""))?
            }
            "environment_restore" => {
                engine.restore(target, snapshot, args["stateful"].as_bool().unwrap_or(true))?
            }
            _ => anyhow::bail!("Unknown environment tool."),
        };
        return Ok(json!({"content":[{"type":"text","text":value.to_string()}]}));
    }
    match name {
        "desktop_screenshot" => return screenshot(target),
        "desktop_click" => {
            let x = args["x"].as_i64().unwrap_or(-1);
            let y = args["y"].as_i64().unwrap_or(-1);
            let button = args["button"].as_u64().unwrap_or(1);
            ensure!(
                (0..1440).contains(&x) && (0..900).contains(&y) && (1..=3).contains(&button),
                "Coordinates are outside the desktop."
            );
            computer(
                target,
                &[
                    "xdotool".into(),
                    "mousemove".into(),
                    "--sync".into(),
                    x.to_string(),
                    y.to_string(),
                    "click".into(),
                    button.to_string(),
                ],
            )?;
        }
        "desktop_type" => {
            let text = args["text"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing text."))?;
            ensure!(
                text.chars().count() <= 10000 && !text.contains('\0'),
                "Text exceeds bounds."
            );
            computer(
                target,
                &[
                    "xdotool".into(),
                    "type".into(),
                    "--clearmodifiers".into(),
                    "--".into(),
                    text.into(),
                ],
            )?;
        }
        "desktop_key" => {
            let key = args["key"].as_str().unwrap_or("");
            ensure!(
                !key.is_empty() && !key.starts_with('-') && key.len() <= 100 && !key.contains('\0'),
                "Invalid key chord."
            );
            computer(
                target,
                &[
                    "xdotool".into(),
                    "key".into(),
                    "--clearmodifiers".into(),
                    key.into(),
                ],
            )?;
        }
        _ => anyhow::bail!("Unknown desktop tool."),
    }
    Ok(json!({"content":[{"type":"text","text":"completed"}]}))
}
fn main() -> Result<()> {
    if std::env::args().any(|a| a == "--version") {
        println!("enginebox-mcp {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if std::env::args().any(|a| a == "--help") {
        println!(
            "enginebox-mcp {}\nStdio MCP private desktop tools. Set ENGINEBOX_GUEST=1 inside a workspace.",
            env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    }
    ensure!(
        ["0", "1"].contains(
            &std::env::var("ENGINEBOX_GUEST")
                .unwrap_or("0".into())
                .as_str()
        ),
        "ENGINEBOX_GUEST must be 0 or 1."
    );
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut stdout = std::io::stdout();
    loop {
        let mut line = String::new();
        let n = reader.by_ref().take(1048577).read_line(&mut line)?;
        if n == 0 {
            break;
        }
        ensure!(
            n <= 1048576 && line.ends_with('\n'),
            "MCP message exceeds limit."
        );
        let value: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                writeln!(
                    stdout,
                    "{}",
                    json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}})
                )?;
                stdout.flush()?;
                continue;
            }
        };
        let Some(id) = value.get("id") else { continue };
        let result=match value["method"].as_str(){
            Some("initialize")=>{
                let requested=value["params"]["protocolVersion"].as_str().unwrap_or("");
                let negotiated=if ["2024-11-05","2025-03-26","2025-06-18","2025-11-25"].contains(&requested){requested}else{"2025-11-25"};
                Ok(json!({"protocolVersion":negotiated,"capabilities":{"tools":{}},"serverInfo":{"name":"Engine Box desktop","version":env!("CARGO_PKG_VERSION")},"instructions":"Native harnesses run unchanged; these tools control the assigned environment desktop."}))
            },
            Some("ping")=>Ok(json!({})),
            Some("tools/list")=>Ok(json!({"tools":tools()})),
            Some("tools/call")=>call(value["params"]["name"].as_str().unwrap_or(""),&value["params"]["arguments"]).or_else(|_|Ok::<_,anyhow::Error>(json!({"isError":true,"content":[{"type":"text","text":"Desktop operation failed; check its arguments, display and workspace access."}]}))),
            _=>Err(anyhow::anyhow!("Unknown method"))
        };
        let response = match result {
            Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            Err(_) => {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not found"}})
            }
        };
        writeln!(stdout, "{response}")?;
        stdout.flush()?;
    }
    Ok(())
}

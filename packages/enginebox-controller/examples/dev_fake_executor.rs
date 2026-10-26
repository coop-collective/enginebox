//! Development-only controller with a stand-in run executor, for exercising the
//! HTTP API, the run queue, pools, service tokens, OAuth and callbacks on a
//! machine without Incus (for example macOS). Never use it in production: no
//! workspace is prepared and no native CLI runs. Each admitted run sleeps
//! `ENGINEBOX_DEV_RUN_SECONDS` (default 3) and succeeds with a synthetic native
//! result that reports a session ID (the run's `resume_id` when it has one).
//!
//! Callbacks are signed and retried by the real delivery code, but every
//! delivery goes to `ENGINEBOX_DEV_CALLBACK_SINK` (a plain-HTTP loopback URL)
//! instead of the run's public callback URL, which the start request still
//! validates as usual.
//!
//! ```sh
//! ENGINEBOX_API_TOKEN=<32+ characters> \
//! ENGINEBOX_DEV_CALLBACK_SINK=http://127.0.0.1:8790/callback \
//! cargo run -p enginebox-controller --example dev_fake_executor -- \
//!   --config dev-fleet.toml --state-dir /tmp/ebox-dev --listen 127.0.0.1:8765
//! ```
use anyhow::{Context, Result, ensure};
use enginebox_agents::queue::HostLoad;
use enginebox_controller::callbacks;
use serde_json::json;
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let config = PathBuf::from(arg(&args, "--config").context("--config is required")?);
    let root = PathBuf::from(arg(&args, "--state-dir").context("--state-dir is required")?);
    let listen: SocketAddr = arg(&args, "--listen")
        .unwrap_or_else(|| "127.0.0.1:8765".into())
        .parse()?;
    ensure!(
        listen.ip().is_loopback(),
        "The dev controller binds loopback only"
    );
    let token = std::env::var("ENGINEBOX_API_TOKEN").context("Set ENGINEBOX_API_TOKEN")?;
    let run_seconds: u64 = std::env::var("ENGINEBOX_DEV_RUN_SECONDS")
        .ok()
        .map(|v| v.parse())
        .transpose()?
        .unwrap_or(3);
    let sink = std::env::var("ENGINEBOX_DEV_CALLBACK_SINK").ok();
    if let Some(sink) = &sink {
        let url = url::Url::parse(sink)?;
        ensure!(
            url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")),
            "ENGINEBOX_DEV_CALLBACK_SINK must be a loopback http URL"
        );
    }

    std::fs::create_dir_all(&root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    }
    let controller = enginebox_controller::Controller::new(&config, &root, token)?;
    let active = controller.clone();
    tokio::task::spawn_blocking(move || active.activate()).await??;
    let agents = controller.dev_agent_service()?;

    // A connected controller account for every provider, so runs are delegable.
    let root = root.canonicalize()?;
    for provider in ["claude", "codex", "cursor"] {
        let home = root.join(format!("agents/homes/{provider}"));
        enginebox_agents::security::private_dir(&home)?;
        if provider == "claude" {
            enginebox_agents::security::write_json(
                &home,
                ".config/enginebox/controller-token.json",
                &json!({"kind":"setup_token","value":"dev-fake-controller-token","expires_at":null}),
            )?;
        }
    }
    // Every host measures as roomy.
    agents.set_measure_hook(Arc::new(|_| {
        Ok(HostLoad {
            cpu_total: 16,
            reserve_cpu: 2,
            memory_available_bytes: 64 << 30,
            reserve_memory_bytes: 4 << 30,
        })
    }));
    // The stand-in for workspace preparation and the guest helper.
    agents.set_run_executor(Arc::new(move |run| {
        std::thread::sleep(Duration::from_secs(run_seconds));
        let session = run
            .request
            .resume_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().hyphenated().to_string());
        let mut env_keys: Vec<&String> = run.request.env.keys().collect();
        env_keys.sort();
        let text = format!(
            "dev fake executor: run {} on {}/{} (pool {}), profile {}, env keys {:?}",
            run.id,
            run.host_id,
            run.name,
            run.pool.as_deref().unwrap_or("-"),
            run.profile.as_deref().unwrap_or("-"),
            env_keys
        );
        Ok(json!({
            "exit_code":0,
            "output":json!({"type":"result","session_id":session,"result":text}).to_string(),
            "timed_out":false,
            "output_truncated":false
        }))
    }));
    // Real signing and retry, delivered to the loopback sink.
    if let Some(sink) = sink {
        let recorder = agents.clone();
        agents.set_finish_hook(Arc::new(move |finished| {
            let sink = sink.clone();
            let recorder = recorder.clone();
            std::thread::spawn(move || {
                let transport = move |_: &url::Url,
                                      _: &[SocketAddr],
                                      headers: &[(String, String)],
                                      body: &[u8]|
                      -> Result<u16> {
                    let client = reqwest::blocking::Client::builder()
                        .timeout(Duration::from_secs(10))
                        .redirect(reqwest::redirect::Policy::none())
                        .no_proxy()
                        .build()?;
                    let mut request = client.post(&sink).body(body.to_vec());
                    for (name, value) in headers {
                        request = request.header(name, value);
                    }
                    Ok(request.send()?.status().as_u16())
                };
                let id = finished.id.clone();
                let attempts = recorder.clone();
                let outcome = callbacks::deliver(
                    &finished,
                    &transport,
                    &|_| Ok(vec![SocketAddr::from(([192, 0, 2, 1], 443))]),
                    &std::thread::sleep,
                    &move |n, status| {
                        let _ = attempts.record_callback(&id, "delivering", n, status);
                    },
                );
                let _ = recorder.record_callback(
                    &finished.id,
                    if outcome.delivered {
                        "delivered"
                    } else {
                        "failed"
                    },
                    outcome.attempts,
                    outcome.last_status,
                );
            });
        }));
    }

    let background = controller.start_background();
    let listener = tokio::net::TcpListener::bind(listen).await?;
    eprintln!(
        "Engine Box DEV controller (fake executor, {run_seconds}s runs) listening on {}",
        listener.local_addr()?
    );
    axum::serve(listener, enginebox_controller::router(controller))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    background.abort();
    Ok(())
}

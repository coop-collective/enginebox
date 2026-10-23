//! Operator task view. Missing provider measurements remain unknown.
use crate::FleetClient;
use anyhow::{Context, Result};
use serde_json::Value;
use std::io::{IsTerminal, Write};

#[derive(clap::Args)]
pub struct Options {
    /// Refresh until interrupted; redirected output appends snapshots.
    #[arg(long)]
    pub watch: bool,
    /// Emit one JSON object per snapshot, suitable for piping.
    #[arg(long)]
    pub json: bool,
    /// Show only queued or running tasks.
    #[arg(long)]
    pub active: bool,
    #[arg(long, value_parser = ["codex", "claude", "cursor"])]
    pub provider: Option<String>,
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u64).range(1..=60))]
    pub interval: u64,
}

fn active(run: &Value) -> bool {
    matches!(run["status"].as_str(), Some("queued" | "running"))
}

/// Strip controls, including terminal escape sequences, from untrusted task text.
fn cell(value: &Value, limit: usize) -> String {
    let raw = value.as_str().unwrap_or("—");
    let mut chars = raw.chars().map(|c| if c.is_control() { ' ' } else { c });
    let mut text: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        text.push('…');
    }
    text
}

fn tokens(value: &Value) -> String {
    value.as_u64().map_or_else(|| "—".into(), |n| n.to_string())
}

pub fn render(runs: &[Value]) -> String {
    let mut result = format!(
        "Engine Box tasks · {} active / {} retained\n\n{:<12} {:<7} {:<24} {:<12} {:>8} {:<7} {:>10} {:>10} {:>10}  TASK\n",
        runs.iter().filter(|r| active(r)).count(),
        runs.len(),
        "RUN",
        "AGENT",
        "HOST / WORKSPACE",
        "STATE",
        "ELAPSED",
        "SCOPE",
        "INPUT",
        "OUTPUT",
        "CACHED"
    );
    for run in runs {
        let location = Value::String(format!(
            "{} / {}",
            run["host_id"].as_str().unwrap_or("?"),
            run["name"].as_str().unwrap_or("?")
        ));
        let elapsed = run["elapsed_seconds"]
            .as_f64()
            .filter(|n| n.is_finite() && *n >= 0.)
            .map_or_else(|| "—".into(), |n| format!("{n:.1}s"));
        let state = if run["cancel_requested"] == true && active(run) {
            "cancelling".into()
        } else {
            cell(&run["status"], 12)
        };
        let scope = match run["usage"]["scope"].as_str() {
            Some("run") => "run",
            Some("session_total") => "session",
            _ => "—",
        };
        result.push_str(&format!(
            "{:<12} {:<7} {:<24} {:<12} {:>8} {:<7} {:>10} {:>10} {:>10}  {}\n",
            cell(&run["id"], 10),
            cell(&run["provider"], 7),
            cell(&location, 24),
            state,
            elapsed,
            scope,
            tokens(&run["usage"]["input_tokens"]),
            tokens(&run["usage"]["output_tokens"]),
            tokens(&run["usage"]["cached_input_tokens"]),
            cell(&run["task"], 90)
        ));
    }
    result.push_str("\n— = not reported. Token scope: run or cumulative session; not subscription balance. Elapsed starts at execution.\n");
    result
}

pub fn run(client: FleetClient, options: Options) -> Result<()> {
    let terminal = std::io::stdout().is_terminal();
    loop {
        let mut snapshot = client.request("/v1/agent-runs", None)?;
        let runs = snapshot["runs"]
            .as_array_mut()
            .context("Controller did not return a task list")?;
        runs.retain(|run| {
            (!options.active || active(run))
                && options
                    .provider
                    .as_deref()
                    .is_none_or(|p| run["provider"] == p)
        });
        runs.sort_by(|a, b| {
            active(b)
                .cmp(&active(a))
                .then_with(|| b["created_at"].as_str().cmp(&a["created_at"].as_str()))
        });
        let mut rendered = if options.json {
            serde_json::to_string(&snapshot)? + "\n"
        } else {
            render(runs)
        };
        if !options.json && snapshot["truncated"] == true {
            rendered.push_str(&format!("Showing a bounded window of {} retained records. Use individual run IDs for older details.\n", snapshot["total"]));
        }
        let mut output = std::io::stdout().lock();
        if options.watch && terminal && !options.json {
            output.write_all(b"\x1b[2J\x1b[H")?;
        }
        output.write_all(rendered.as_bytes())?;
        output.flush()?;
        if !options.watch {
            return Ok(());
        }
        drop(output);
        std::thread::sleep(std::time::Duration::from_secs(options.interval));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn concurrent_agents_and_unknown_usage_are_visible_without_terminal_controls() {
        let output = render(&[
            json!({"id":"a", "provider":"claude", "host_id":"node", "name":"ebox-one", "task":"Inspect\u{001b}[31m\ncode", "status":"running", "elapsed_seconds":1.25, "usage":{"scope":"run","input_tokens":42,"output_tokens":0}}),
            json!({"id":"b", "provider":"codex", "status":"queued", "cancel_requested":true,"usage":{"scope":"session_total","input_tokens":60824}}),
            json!({"id":"c", "provider":"cursor", "status":"succeeded", "usage":null}),
        ]);
        assert!(output.contains("2 active / 3 retained"));
        assert!(output.contains("claude") && output.contains("codex") && output.contains("cursor"));
        assert!(output.contains("cancelling") && output.contains("1.2s"));
        assert!(output.contains("42") && output.contains("—"));
        assert!(output.lines().any(|line| line.contains("codex")
            && line.contains("session")
            && line.contains("60824")));
        assert!(!output.contains('\u{001b}'));
    }
}

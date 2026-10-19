use anyhow::{Result, ensure};
use std::io::{BufRead, Write};
fn main() -> Result<()> {
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        println!(
            "enginebox-fleet-mcp: stdio MCP server. Configure ENGINEBOX_API_URL and ENGINEBOX_API_TOKEN_FILE. Never install operator tokens in guests."
        );
        return Ok(());
    }
    let client = enginebox_cli::FleetClient::from_env()?;
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    loop {
        let mut line = Vec::new();
        loop {
            let data = input.fill_buf()?;
            if data.is_empty() {
                break;
            }
            let end = data
                .iter()
                .position(|b| *b == b'\n')
                .map(|p| p + 1)
                .unwrap_or(data.len());
            ensure!(line.len() + end <= 1024 * 1024, "MCP input exceeds 1 MiB");
            let complete = data[end - 1] == b'\n';
            line.extend_from_slice(&data[..end]);
            input.consume(end);
            if complete {
                break;
            }
        }
        if line.is_empty() {
            break;
        }
        let response = match serde_json::from_slice(&line) {
            Ok(request) => enginebox_cli::rpc(request, &client),
            Err(_) => Some(
                serde_json::json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}}),
            ),
        };
        if let Some(response) = response {
            serde_json::to_writer(&mut output, &response)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
    Ok(())
}

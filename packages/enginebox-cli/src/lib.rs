use anyhow::{Context, Result, ensure};
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::io::Read;
use std::{path::PathBuf, time::Duration};

#[derive(Clone)]
pub struct FleetClient {
    client: Client,
    base: String,
    token: String,
}
impl FleetClient {
    pub fn new(base: &str, token: String) -> Result<Self> {
        let url = url::Url::parse(base)?;
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url.path() == "/",
            "Use a fleet server origin without credentials, path, query or fragment"
        );
        let local = url.host_str().is_some_and(|host| {
            host == "localhost"
                || host == "[::1]"
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        ensure!(
            url.scheme() == "https" || (url.scheme() == "http" && local),
            "Remote fleet API requires HTTPS"
        );
        ensure!(
            token.len() >= 32 && token.bytes().all(|b| (33..=126).contains(&b)),
            "Set a valid API token"
        );
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(45))
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            client,
            base: base.trim_end_matches('/').to_owned(),
            token,
        })
    }
    pub fn from_env() -> Result<Self> {
        ensure!(
            std::env::var("ENGINEBOX_GUEST").unwrap_or_default() != "1",
            "Operator fleet access is unavailable in guest mode"
        );
        let base =
            std::env::var("ENGINEBOX_API_URL").unwrap_or_else(|_| "http://127.0.0.1:8765".into());
        let token = match std::env::var_os("ENGINEBOX_API_TOKEN_FILE") {
            Some(path) => std::fs::read_to_string(PathBuf::from(path))?
                .trim()
                .to_owned(),
            None => std::env::var("ENGINEBOX_API_TOKEN")
                .context("Set ENGINEBOX_API_TOKEN_FILE or ENGINEBOX_API_TOKEN")?,
        };
        Self::new(&base, token)
    }
    pub fn request(&self, path: &str, body: Option<Value>) -> Result<Value> {
        ensure!(
            path.starts_with("/v1/")
                && !path.contains(['?', '#', '%', '\\'])
                && !path.split('/').any(|s| s == ".." || s == "."),
            "Invalid API path"
        );
        let url = format!("{}{path}", self.base);
        let req = if let Some(body) = body {
            self.client.post(url).json(&body)
        } else {
            self.client.get(url)
        };
        let response = req
            .bearer_auth(&self.token)
            .send()
            .context("Fleet API request failed")?;
        let status = response.status();
        let mut bytes = Vec::new();
        std::io::Read::take(response, 4 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 4 * 1024 * 1024,
            "Fleet response exceeded the size limit"
        );
        let v: Value = serde_json::from_slice(&bytes).context("Fleet did not return JSON")?;
        ensure!(
            status.is_success(),
            "{}",
            v["error"].as_str().unwrap_or("Fleet request failed")
        );
        Ok(v)
    }
}

fn property(kind: &str, description: &str) -> Value {
    json!({"type":kind,"description":description})
}
fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false}})
}
pub fn tools() -> Vec<Value> {
    vec![
        tool(
            "fleet_inventory",
            "List configured hosts, capacity budgets, prepared templates and workspaces.",
            json!({}),
            &[],
        ),
        tool(
            "fleet_create",
            "Queue a clean VM clone after admission; poll fleet_job.",
            json!({"host_id":property("string","Host ID"),"name":property("string","New ebox- workspace name"),"template":property("string","Prepared template ID"),"size":{"type":"string","enum":["small","standard","large"],"default":"standard"}}),
            &["host_id", "name", "template"],
        ),
        tool(
            "fleet_allocate",
            "Assign a fresh warm workspace, otherwise clone. Always use result.name for subsequent calls.",
            json!({"host_id":property("string","Host ID"),"name":property("string","New ebox- workspace label"),"template":property("string","Prepared template ID"),"size":{"type":"string","enum":["minimum","recommended","small","standard","large"],"default":"recommended"},"resources":{"type":"object","properties":{"cpu":{"type":"integer","minimum":1},"memory_gib":{"type":"integer","minimum":1},"disk_gib":{"type":"integer","minimum":1}},"required":["cpu","memory_gib","disk_gib"],"additionalProperties":false}}),
            &["host_id", "name", "template"],
        ),
        tool(
            "template_catalog",
            "List controller templates and installation status across every node.",
            json!({}),
            &[],
        ),
        tool(
            "template_register",
            "Register a JSON manifest or public HTTPS URL; synchronizes every node.",
            json!({"manifest":{"type":"object"},"url":{"type":"string"}}),
            &[],
        ),
        tool(
            "template_install",
            "Retry a template on one node; controller registration already synchronizes all nodes.",
            json!({"host_id":{"type":"string"},"template":{"type":"string"}}),
            &["host_id", "template"],
        ),
        tool(
            "template_sync",
            "Retry synchronization of a registered template to all nodes.",
            json!({"template":{"type":"string"}}),
            &["template"],
        ),
        tool(
            "agent_connections",
            "Read native agent connection status; never returns credentials.",
            json!({}),
            &[],
        ),
        tool(
            "agent_launch",
            "Run an official native CLI in a private workspace using its existing home/memory. May consume provider allowance. Resume only an exact native_session_id returned by agent_run.",
            json!({"host_id":{"type":"string"},"name":{"type":"string"},"provider":{"type":"string","enum":["codex","claude","cursor"]},"prompt":{"type":"string","minLength":1,"maxLength":12000},"resume_id":{"type":["string","null"]},"workdir":{"type":"string","default":"."},"timeout_seconds":{"type":"integer","minimum":1,"maximum":300,"default":120},"model":{"type":["string","null"]}}),
            &["host_id", "name", "provider", "prompt"],
        ),
        tool(
            "agent_run",
            "Read a private run's bounded output and native_session_id for exact resume.",
            json!({"host_id":{"type":"string"},"name":{"type":"string"},"run_id":{"type":"string","pattern":"^[a-f0-9]{32}$"}}),
            &["host_id", "name", "run_id"],
        ),
        tool(
            "fleet_action",
            "Queue pause, resume, hibernate or stop. Stop discards RAM; hibernate preserves it.",
            json!({"host_id":{"type":"string"},"name":{"type":"string"},"action":{"type":"string","enum":["pause","resume","hibernate","stop"]}}),
            &["host_id", "name", "action"],
        ),
        tool(
            "fleet_job",
            "Poll queued/running/succeeded/failed fleet job status.",
            json!({"job_id":{"type":"string","pattern":"^[a-f0-9]{32}$"}}),
            &["job_id"],
        ),
    ]
}
fn field<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key].as_str().context("Required string field missing")
}
fn id(v: &Value, key: &str, instance: bool) -> Result<String> {
    Ok(enginebox_core::identifier(field(v, key)?, instance)?.to_owned())
}
fn record_id(v: &Value, key: &str) -> Result<String> {
    let value = field(v, key)?;
    ensure!(
        value.len() == 32
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "Invalid record ID"
    );
    Ok(value.to_owned())
}

pub fn tool_request(name: &str, args: &Value) -> Result<(String, Option<Value>)> {
    let spec = tools()
        .into_iter()
        .find(|t| t["name"] == name)
        .context("Unknown tool")?;
    let object = args.as_object().context("Arguments must be an object")?;
    ensure!(
        object
            .keys()
            .all(|k| spec["inputSchema"]["properties"].get(k).is_some()),
        "Unsupported tool argument"
    );
    for key in spec["inputSchema"]["required"].as_array().unwrap() {
        ensure!(
            object.contains_key(key.as_str().unwrap()),
            "Required argument missing"
        );
    }
    let result = match name {
        "fleet_inventory" => ("/v1/fleet".into(), None),
        "template_catalog" => ("/v1/templates".into(), None),
        "agent_connections" => ("/v1/agents".into(), None),
        "fleet_job" => (format!("/v1/jobs/{}", record_id(args, "job_id")?), None),
        "template_register" => {
            ensure!(
                object.len() == 1 && (args["manifest"].is_object() || args["url"].is_string()),
                "Provide exactly one manifest object or URL"
            );
            ("/v1/templates".into(), Some(args.clone()))
        }
        "template_sync" => (
            format!("/v1/templates/{}/sync", id(args, "template", false)?),
            Some(json!({})),
        ),
        "template_install" => (
            format!(
                "/v1/hosts/{}/templates/{}/install",
                id(args, "host_id", false)?,
                id(args, "template", false)?
            ),
            Some(json!({})),
        ),
        "fleet_create" | "fleet_allocate" => {
            let mut body =
                json!({"name":id(args,"name",true)?,"template":id(args,"template",false)?});
            if let Some(resources) = args.get("resources") {
                ensure!(!object.contains_key("size"), "Choose resources or size");
                enginebox_core::Resources::from_value(resources, None)?;
                body["resources"] = resources.clone();
            } else {
                let size =
                    args.get("size")
                        .and_then(Value::as_str)
                        .unwrap_or(if name == "fleet_create" {
                            "standard"
                        } else {
                            "recommended"
                        });
                ensure!(
                    ["minimum", "recommended", "small", "standard", "large"].contains(&size),
                    "Unknown size"
                );
                body["size"] = json!(size);
            }
            (
                format!(
                    "/v1/hosts/{}/{}",
                    id(args, "host_id", false)?,
                    if name == "fleet_create" {
                        "instances"
                    } else {
                        "allocations"
                    }
                ),
                Some(body),
            )
        }
        "fleet_action" => {
            let action = field(args, "action")?;
            ensure!(
                ["pause", "resume", "hibernate", "stop"].contains(&action),
                "Unknown action"
            );
            (
                format!(
                    "/v1/hosts/{}/instances/{}/actions",
                    id(args, "host_id", false)?,
                    id(args, "name", true)?
                ),
                Some(json!({"action":action})),
            )
        }
        "agent_run" => (
            format!(
                "/v1/hosts/{}/instances/{}/agent-runs/{}",
                id(args, "host_id", false)?,
                id(args, "name", true)?,
                record_id(args, "run_id")?
            ),
            None,
        ),
        "agent_launch" => {
            let provider = field(args, "provider")?;
            ensure!(
                ["codex", "claude", "cursor"].contains(&provider),
                "Unknown agent"
            );
            let prompt = field(args, "prompt")?;
            ensure!(
                !prompt.trim().is_empty() && prompt.len() <= 12000 && !prompt.contains('\0'),
                "Prompt must contain 1–12000 bytes"
            );
            let mut body = args.clone();
            body.as_object_mut().unwrap().remove("host_id");
            body.as_object_mut().unwrap().remove("name");
            (
                format!(
                    "/v1/hosts/{}/instances/{}/agent-runs",
                    id(args, "host_id", false)?,
                    id(args, "name", true)?
                ),
                Some(body),
            )
        }
        _ => unreachable!(),
    };
    Ok(result)
}

pub fn rpc(request: Value, client: &FleetClient) -> Option<Value> {
    let id = request.get("id")?.clone();
    let error = |code, message: &str| json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}});
    if request["jsonrpc"] != "2.0" || (!id.is_string() && !id.is_number() && !id.is_null()) {
        return Some(error(-32600, "Invalid request"));
    }
    let result = match request["method"].as_str() {
        Some("initialize") => {
            let offered = request["params"]["protocolVersion"].as_str().unwrap_or("");
            let supported = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
            json!({"protocolVersion":if supported.contains(&offered){offered}else{"2025-11-25"},"capabilities":{"tools":{}},"serverInfo":{"name":"enginebox-fleet","version":env!("CARGO_PKG_VERSION")},"instructions":"Operate authorized private workspaces using native provider harnesses."})
        }
        Some("ping") => json!({}),
        Some("tools/list") => json!({"tools":tools()}),
        Some("tools/call") => {
            let result = (|| {
                let name = request["params"]["name"]
                    .as_str()
                    .context("Missing tool name")?;
                let args = request["params"]
                    .get("arguments")
                    .cloned()
                    .unwrap_or(json!({}));
                let (path, body) = tool_request(name, &args)?;
                client.request(&path, body)
            })();
            match result {
                Ok(value) => {
                    json!({"content":[{"type":"text","text":value.to_string()}],"structuredContent":value,"isError":false})
                }
                Err(e) => json!({"content":[{"type":"text","text":e.to_string()}],"isError":true}),
            }
        }
        _ => return Some(error(-32601, "Method not found")),
    };
    Some(json!({"jsonrpc":"2.0","id":id,"result":result}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_token_exfiltration_and_path_injection() {
        let token = "x".repeat(32);
        assert!(FleetClient::new("http://example.com", token.clone()).is_err());
        assert!(FleetClient::new("https://user:pass@example.com", token.clone()).is_err());
        assert!(FleetClient::new("https://example.com/path", token).is_err());
        assert!(
            tool_request(
                "fleet_action",
                &json!({"host_id":"a/../b","name":"ebox-test","action":"resume"})
            )
            .is_err()
        );
        assert!(
            tool_request(
                "template_register",
                &json!({"manifest":{},"url":"https://example.com"})
            )
            .is_err()
        );
    }
    #[test]
    fn tools_preserve_native_resume_and_allocation_contracts() {
        let (_,body)=tool_request("agent_launch",&json!({"host_id":"worker","name":"ebox-test","provider":"codex","prompt":"test","resume_id":"12345678-1234-1234-1234-123456789012"})).unwrap();
        assert_eq!(
            body.unwrap()["resume_id"],
            "12345678-1234-1234-1234-123456789012"
        );
        let (path,body)=tool_request("fleet_allocate",&json!({"host_id":"worker","name":"ebox-test","template":"native-dev","resources":{"cpu":2,"memory_gib":4,"disk_gib":40}})).unwrap();
        assert_eq!(path, "/v1/hosts/worker/allocations");
        assert!(body.unwrap().get("size").is_none());
    }
    #[test]
    fn mcp_handshake_has_tools_and_notifications_have_no_reply() {
        let client = FleetClient::new("http://127.0.0.1:1", "x".repeat(32)).unwrap();
        assert_eq!(rpc(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}),&client).unwrap()["result"]["capabilities"],json!({"tools":{}}));
        assert!(
            rpc(
                json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
                &client
            )
            .is_none()
        );
        assert_eq!(
            rpc(
                json!({"jsonrpc":"2.0","id":"list","method":"tools/list"}),
                &client
            )
            .unwrap()["result"]["tools"]
                .as_array()
                .unwrap()
                .len(),
            12
        );
    }
}

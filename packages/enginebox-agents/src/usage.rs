use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Mutex};
pub(crate) fn iso(n: f64) -> Option<String> {
    chrono::DateTime::from_timestamp(n as i64, 0).map(|d| d.to_rfc3339())
}
pub fn unavailable(p: &str, detail: Option<&str>) -> Value {
    let d = match p {
        "codex" => "Connect a ChatGPT account to read its reported allowance.",
        "claude" => {
            "Claude does not provide an on-demand allowance through this integration. Check provider usage."
        }
        _ => {
            "Cursor does not expose personal allowance through the supported CLI. Check provider usage."
        }
    };
    json!({"provider":p,"scope":"controller_account","status":"unavailable","detail":detail.unwrap_or(d),"source":null,"observed_at":null,"freshness":"unknown","plan":null,"billing_cycle":null,"allowance_windows":[],"credits":[],"refresh_after_seconds":60})
}
fn num(v: &Value, min: f64, max: f64) -> Option<f64> {
    v.as_f64()
        .filter(|n| n.is_finite() && *n >= min && *n <= max)
}
fn label(v: &Value, fallback: &str) -> String {
    v.as_str()
        .filter(|s| {
            regex::Regex::new(r"^[a-zA-Z0-9][a-zA-Z0-9_. /+\-]{0,79}$")
                .unwrap()
                .is_match(s)
        })
        .unwrap_or(fallback)
        .into()
}
pub fn normalize_codex(value: &Value, now: f64) -> Result<Value> {
    ensure!(value.is_object(), "Unsupported native usage response.");
    let mut fallback = serde_json::Map::new();
    fallback.insert("codex".into(), value["rateLimits"].clone());
    let buckets = value["rateLimitsByLimitId"]
        .as_object()
        .filter(|m| !m.is_empty())
        .unwrap_or(&fallback);
    ensure!(
        !buckets.is_empty() && buckets.len() <= 16 && buckets.values().any(Value::is_object),
        "Unsupported native usage response."
    );
    let mut result = unavailable("codex", None);
    result["source"] = json!("Codex native app-server");
    result["observed_at"] = json!(iso(now));
    result["freshness"] = json!("current");
    let plan_pattern = regex::Regex::new(r"^[a-zA-Z][a-zA-Z0-9_-]{0,39}$")?;
    let balance_pattern = regex::Regex::new(r"^\d{1,15}(\.\d{1,8})?$")?;
    for (key, b) in buckets {
        if !b.is_object() {
            continue;
        }
        let id = label(&b["limitId"], &label(&json!(key), "codex"));
        let name = label(&b["limitName"], &id);
        if result["plan"].is_null()
            && let Some(p) = b["planType"].as_str().filter(|p| plan_pattern.is_match(p))
        {
            result["plan"] = json!(p);
        }
        for position in ["primary", "secondary"] {
            let window = &b[position];
            if let (Some(used), Some(reset)) = (
                num(&window["usedPercent"], 0., 100.),
                num(&window["resetsAt"], 1., 253402300799.),
            ) {
                result["allowance_windows"].as_array_mut().unwrap().push(json!({"id":format!("{id}:{position}"),"label":name,"used_percentage":used,"resets_at":iso(reset),"window_minutes":num(&window["windowDurationMins"],1.,525600.)}));
            }
        }
        let credit = &b["credits"];
        let balance = if credit["balance"].is_number() {
            num(&credit["balance"], 0., 1e15).map(|n| n.to_string())
        } else {
            credit["balance"].as_str().map(str::to_owned)
        }
        .filter(|s| balance_pattern.is_match(s));
        let unlimited = credit["unlimited"] == true;
        if balance.is_some() || unlimited {
            result["credits"].as_array_mut().unwrap().push(json!({"label":name,"balance":balance,"unlimited":unlimited,"unit":"provider credits"}));
        }
    }
    if !result["allowance_windows"].as_array().unwrap().is_empty()
        || !result["credits"].as_array().unwrap().is_empty()
    {
        result["status"] = json!("available");
        result["detail"] = json!("Reported by the provider for this connected account.");
    } else {
        result["detail"] =
            json!("The provider did not report allowance windows or a credit balance.");
    }
    Ok(result)
}
pub fn read_codex(exe: &str, env: &BTreeMap<String, String>) -> Result<Value> {
    read_codex_with_lease(exe, env, None)
}
pub fn read_codex_with_lease(
    exe: &str,
    env: &BTreeMap<String, String>,
    lease: Option<&crate::security::Lease>,
) -> Result<Value> {
    let value = crate::process::native_rpc_with_lease(
        &[
            exe.into(),
            "-c".into(),
            "cli_auth_credentials_store=\"file\"".into(),
            "app-server".into(),
        ],
        env,
        "account/rateLimits/read",
        None,
        12,
        262144,
        lease,
    )?;
    normalize_codex(&value, chrono::Utc::now().timestamp() as f64)
}
/// Render a cached controller observation conservatively during a handoff.
/// Accepts either a raw usage object or the HTTP {usage: ...} envelope.
pub fn handoff_snapshot(value: &Value) -> Value {
    let mut value = value.clone();
    let usage = if value["usage"].is_object() {
        &mut value["usage"]
    } else {
        &mut value
    };
    if usage["allowance_windows"].is_array() {
        usage["freshness"] = json!(if usage["observed_at"].is_null() {
            "unknown"
        } else {
            "stale"
        });
        usage["detail"] = json!("This allowance is a previous observation; refresh is pending.");
        apply_freshness(
            usage,
            chrono::Utc::now().timestamp(),
            chrono::Utc::now().timestamp() - 59,
        );
    }
    value
}
#[derive(Default)]
pub struct Cache {
    entries: Mutex<BTreeMap<String, (String, i64, Value)>>,
}
impl Cache {
    pub fn read<F: FnOnce() -> Result<Value>>(
        &self,
        p: &str,
        fingerprint: &str,
        loader: F,
    ) -> Value {
        let mut entries = self.entries.lock().unwrap();
        let now = chrono::Utc::now().timestamp();
        if entries.get(p).is_some_and(|e| e.0 != fingerprint) {
            entries.remove(p);
        }
        if entries.get(p).is_none_or(|e| now - e.1 >= 60) {
            let snapshot = loader().unwrap_or_else(|_| {
                let mut v = entries
                    .get(p)
                    .map(|e| e.2.clone())
                    .unwrap_or_else(|| unavailable(p, None));
                v["status"] = json!("unavailable");
                v["freshness"] = json!(if v["observed_at"].is_null() {
                    "unknown"
                } else {
                    "stale"
                });
                v["detail"] =
                    json!("Usage could not be refreshed. Check the provider or try again shortly.");
                v
            });
            entries.insert(p.into(), (fingerprint.into(), now, snapshot));
        }
        let (_, attempted, snapshot) = &entries[p];
        let mut result = snapshot.clone();
        apply_freshness(&mut result, chrono::Utc::now().timestamp(), *attempted);
        result
    }
}
pub fn apply_freshness(result: &mut Value, now: i64, attempted: i64) {
    let observed = result["observed_at"]
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
    if observed.is_some_and(|d| now - d.timestamp() >= 300) {
        result["freshness"] = json!("stale");
    }
    result["refresh_after_seconds"] = json!((60 - (now - attempted)).max(1));
    let current = result["freshness"] == "current";
    if let Some(windows) = result["allowance_windows"].as_array_mut() {
        for w in windows {
            let reset = w["resets_at"]
                .as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
            let fresh = current && reset.is_some_and(|d| d.timestamp() > now);
            w["freshness"] = json!(if fresh { "current" } else { "stale" });
            w["remaining_percentage"] = if fresh {
                json!((100. - w["used_percentage"].as_f64().unwrap_or(100.)).max(0.))
            } else {
                Value::Null
            };
        }
    }
}

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
/// The credential a live usage read needs for one controller account, read
/// from the provider's own native credential file (the same one its CLI
/// uses). The secret never leaves the process: it is not serialized, and
/// `Debug` redacts it. Non-secret facts the file carries (Claude's
/// `subscriptionType` / `rateLimitTier`, ChatGPT's plan claim) are in `meta`.
pub struct Credential {
    pub provider: String,
    /// `oauth` (Claude's subscription OAuth), `session` (Cursor's native
    /// session JWT), `chatgpt` (Codex's ChatGPT tokens), `api_key` (a provider
    /// key; GLM's coding plan key, or a key that cannot read a personal plan).
    pub kind: &'static str,
    secret: String,
    /// ChatGPT's account id (the `ChatGPT-Account-Id` header) or Cursor's
    /// user id (the dashboard session cookie's prefix); neither is a secret.
    pub account_id: Option<String>,
    /// Unix seconds when the secret expires, when the file says.
    pub expires_at: Option<f64>,
    pub meta: Value,
}
impl Credential {
    pub fn new(provider: &str, kind: &'static str, secret: String) -> Self {
        Self {
            provider: provider.into(),
            kind,
            secret,
            account_id: None,
            expires_at: None,
            meta: Value::Null,
        }
    }
    pub fn secret(&self) -> &str {
        &self.secret
    }
    /// True when the secret is known to expire within `margin` seconds.
    pub fn expired(&self, now: f64, margin: f64) -> bool {
        self.expires_at.is_some_and(|e| e <= now + margin)
    }
}
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("provider", &self.provider)
            .field("kind", &self.kind)
            .field("secret", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}
/// Claims of a JWT payload, unverified: read only for non-secret facts
/// (expiry, ChatGPT plan type, the Cursor user id its dashboard session needs).
pub fn jwt_claims(token: &str) -> Option<Value> {
    use base64::Engine;
    let part = token.split('.').nth(1)?;
    serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(part.trim_end_matches('='))
            .ok()?,
    )
    .ok()
}
/// The usage credential in a provider home (pure). `native` is the CLI's own
/// credential file ([`crate::delegation::native_file`]); `saved` is the
/// controller-token file (`CLAUDE_CODE_OAUTH_TOKEN` / `CURSOR_API_KEY`).
pub fn credential_from_files(p: &str, native: &Value, saved: &Value) -> Option<Credential> {
    let is_token = crate::security::token;
    match p {
        "claude" => {
            let t = &native["claudeAiOauth"];
            if is_token(&t["accessToken"]) {
                let mut c = Credential::new(p, "oauth", t["accessToken"].as_str()?.into());
                c.expires_at = t["expiresAt"].as_f64().map(|ms| ms / 1000.);
                c.meta = json!({
                    "subscription_type": t["subscriptionType"].as_str(),
                    "rate_limit_tier": t["rateLimitTier"].as_str(),
                    "scopes": t["scopes"].as_array().map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>()),
                });
                return Some(c);
            }
            // A `claude setup-token` token (inference scope only).
            is_token(&saved["value"]).then(|| {
                let mut c = Credential::new(p, "oauth", saved["value"].as_str().unwrap_or_default().into());
                c.expires_at = saved["expires_at"].as_f64();
                c.meta = json!({"setup_token": true});
                c
            })
        }
        "codex" => {
            if !native["OPENAI_API_KEY"].as_str().is_none_or(str::is_empty) {
                return Some(Credential::new(p, "api_key", String::new()));
            }
            let t = &native["tokens"];
            if !is_token(&t["access_token"]) {
                return None;
            }
            let token = t["access_token"].as_str()?;
            let mut c = Credential::new(p, "chatgpt", token.into());
            c.account_id = t["account_id"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
                .map(str::to_owned);
            let access = jwt_claims(token).unwrap_or(Value::Null);
            c.expires_at = access["exp"].as_f64();
            let id = t["id_token"].as_str().and_then(jwt_claims).unwrap_or(Value::Null);
            let claim = "https://api.openai.com/auth";
            let auth = if id[claim].is_object() { &id[claim] } else { &access[claim] };
            c.meta = json!({"plan_type": auth["chatgpt_plan_type"].as_str()});
            Some(c)
        }
        _ => {
            if is_token(&native["accessToken"]) {
                let token = native["accessToken"].as_str()?;
                let claims = jwt_claims(token).unwrap_or(Value::Null);
                let mut c = Credential::new(p, "session", token.into());
                c.expires_at = claims["exp"].as_f64();
                // The dashboard's session cookie is `<user id>::<session JWT>`;
                // the user id is the JWT subject after its `<idp>|` prefix.
                let user = regex::Regex::new(r"^[A-Za-z0-9_\-]{1,128}$").ok()?;
                c.account_id = claims["sub"]
                    .as_str()
                    .and_then(|s| s.rsplit('|').next())
                    .filter(|s| user.is_match(s))
                    .map(str::to_owned);
                return Some(c);
            }
            is_token(&saved["value"]).then(|| Credential::new(p, "api_key", String::new()))
        }
    }
}
/// The Codex app-server's raw `account/rateLimits/read` reply (the source of
/// the CLI's own `/status`), for the controller's usage poller.
pub fn read_codex_raw_with_lease(
    exe: &str,
    env: &BTreeMap<String, String>,
    lease: Option<&crate::security::Lease>,
) -> Result<Value> {
    crate::process::native_rpc_with_lease(
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
    )
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

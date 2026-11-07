//! Live subscription usage for every provider account the controller runs:
//! plan, usage windows, resets, credits and extra usage, read from the
//! providers with the same credentials their own CLIs use, normalized to one
//! shape, stored, and served by `GET /v1/usage/accounts` to the owner and to
//! service tokens that may start runs (they run on these accounts; usage
//! numbers are not secrets). Nothing here is typed in by a person: a field the
//! provider did not report is `null`, never guessed.
//!
//! # Endpoints (each checked against a live account on 2026-11-07; the
//! redacted replies are the fixtures in `tests/fixtures/usage/`)
//!
//! - **Claude** (subscription OAuth token from `~/.claude/.credentials.json`,
//!   which needs the `user:profile` scope; `claude setup-token` tokens lack it):
//!   - `GET https://api.anthropic.com/api/oauth/usage` with
//!     `Authorization: Bearer`, `anthropic-beta: oauth-2025-04-20`: the data
//!     behind Claude Code's `/usage` (`five_hour` / `seven_day` / per-model
//!     `seven_day_*` `{utilization 0-100, resets_at}`, `limits[]`,
//!     `extra_usage {is_enabled, monthly_limit, used_credits}` in minor units,
//!     `spend {used {amount_minor, exponent, currency}, auto_reload, balance}`).
//!     Claude Code's source is not public; the endpoint and its units are
//!     cited from CodexBar's fetcher, which reads it the same way:
//!     <https://github.com/steipete/CodexBar/blob/main/Sources/CodexBarCore/Providers/Claude/ClaudeOAuth/ClaudeOAuthUsageFetcher.swift>
//!     and <https://github.com/steipete/CodexBar/blob/main/docs/claude.md>.
//!   - `GET https://api.anthropic.com/api/oauth/profile` (same headers):
//!     `organization.{organization_type, rate_limit_tier, subscription_status,
//!     has_extra_usage_enabled}`. Read at most every [`PROFILE_TTL`].
//! - **Codex** (ChatGPT tokens from `~/.codex/auth.json`):
//!   - `GET https://chatgpt.com/backend-api/wham/usage` with
//!     `Authorization: Bearer` and `ChatGPT-Account-Id`: the Codex CLI's own
//!     `/status` source, openai/codex
//!     `codex-rs/backend-client/src/client/rate_limit_resets.rs`
//!     (`rate_limit_status_url`) and `codex-rs/backend-client/src/client.rs`
//!     (`headers_with`); models in
//!     `codex-rs/codex-backend-openapi-models/src/models/rate_limit_status_payload.rs`
//!     (<https://github.com/openai/codex>). `credits.balance` is a string.
//!   - When the access token has expired or is refused, the native app-server's
//!     `account/rateLimits/read` (`codex-rs/app-server-protocol/src/protocol/v2/account.rs`),
//!     which refreshes the CLI's own tokens the way it always does.
//!   - The plan also comes from the `id_token` claim
//!     `https://api.openai.com/auth`.`chatgpt_plan_type`
//!     (`codex-rs/login/src/token_data.rs`).
//! - **Cursor** (the cursor-agent session JWT from `~/.config/cursor/auth.json`,
//!   sent as the dashboard's cookie `WorkosCursorSessionToken=<user id>%3A%3A<jwt>`,
//!   the user id being the JWT `sub` after `|`; POSTs need `Origin`):
//!   - `POST https://cursor.com/api/dashboard/get-current-period-usage` `{}`:
//!     the dashboard's current allowance (`billingCycleStart/End` in epoch ms,
//!     `planUsage.totalPercentUsed`). Seen live as the weekly team allowance.
//!   - `GET https://cursor.com/api/usage-summary`: plan and on-demand money in
//!     cents (`individualUsage.plan|onDemand {used, limit, remaining, enabled}`,
//!     `teamUsage.onDemand`), as CodexBar reads it:
//!     <https://github.com/steipete/CodexBar/blob/main/Sources/CodexBarCore/Providers/Cursor/CursorStatusProbe.swift>,
//!     cookie form in `.../Cursor/CursorAppAuth.swift`.
//!   - `GET https://cursor.com/api/auth/stripe`: `membershipType`,
//!     `subscriptionStatus`, `customerBalance` (Stripe cents; negative is a
//!     credit). Grok in Cursor draws on this same allowance.
//! - **GLM** (Z.ai Coding Plan API key, saved write-only with
//!   `POST /v1/usage/accounts/glm/key`; no GLM account is connected otherwise):
//!   - `GET https://api.z.ai/api/monitor/usage/quota/limit` (China:
//!     `https://open.bigmodel.cn/...`) with `Authorization: <key>`: the
//!     official `glm-plan-usage` plugin's call,
//!     <https://github.com/zai-org/zai-coding-plugins/blob/main/plugins/glm-plan-usage/skills/usage-query-skill/scripts/query-usage.mjs>.
//!     `data.limits[] {type TOKENS_LIMIT|TIME_LIMIT, unit, number, usage (the
//!     limit), currentValue, percentage, nextResetTime (epoch ms)}`; unit 3 is
//!     hours (5-hour window), 6 weekly, 5 on TIME_LIMIT the monthly tool quota
//!     (<https://github.com/steipete/CodexBar/blob/main/docs/zai.md>).
//!
//! # Schedule
//! Every [`ACTIVE_INTERVAL`] while the account has queued or running runs,
//! every [`IDLE_INTERVAL`] otherwise, and right after one of its runs ends (at
//! most every [`MIN_GAP`]). Failures back off exponentially up to
//! [`MAX_BACKOFF`] and honor `Retry-After`. Polls run on their own thread so
//! the reconcile loop never waits on a provider.
//!
//! # Secrets
//! Tokens are read under the provider lease, used for the request and dropped.
//! They are never logged, stored here or returned; request `Debug` prints only
//! the method and URL, and provider error bodies are never copied out.
use anyhow::{Context, Result, ensure};
use enginebox_agents::usage::Credential;
use enginebox_core::state;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

pub const PROVIDERS: [&str; 4] = ["claude", "codex", "cursor", "glm"];
/// Seconds between polls while the account has active runs.
pub const ACTIVE_INTERVAL: i64 = 300;
/// Seconds between polls while it has none.
pub const IDLE_INTERVAL: i64 = 1800;
/// The shortest gap between two polls of one account (a run just ended).
pub const MIN_GAP: i64 = 60;
/// The longest error backoff.
pub const MAX_BACKOFF: i64 = 7200;
/// Claude's profile (plan, subscription status) changes rarely.
pub const PROFILE_TTL: i64 = 6 * 3600;

pub const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
pub const CLAUDE_PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
pub const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
pub const CURSOR_PERIOD_URL: &str = "https://cursor.com/api/dashboard/get-current-period-usage";
pub const CURSOR_SUMMARY_URL: &str = "https://cursor.com/api/usage-summary";
pub const CURSOR_STRIPE_URL: &str = "https://cursor.com/api/auth/stripe";
pub const GLM_QUOTA_PATH: &str = "/api/monitor/usage/quota/limit";
const GLM_GLOBAL: &str = "https://api.z.ai";
const GLM_CHINA: &str = "https://open.bigmodel.cn";

// ------------------------------------------------------------------ transport

/// One provider request. `Debug` never shows headers or body (they carry the
/// credential).
pub struct Request {
    pub method: &'static str,
    pub url: String,
    headers: Vec<(&'static str, String)>,
    pub body: Option<Value>,
}
impl Request {
    fn get(url: &str) -> Self {
        Self {
            method: "GET",
            url: url.into(),
            headers: vec![("accept", "application/json".into())],
            body: None,
        }
    }
    fn post(url: &str, body: Value) -> Self {
        let mut r = Self::get(url);
        r.method = "POST";
        r.headers.push(("content-type", "application/json".into()));
        r.body = Some(body);
        r
    }
    fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }
    /// Header names only (tests check what is sent without seeing values).
    pub fn header_names(&self) -> Vec<&'static str> {
        self.headers.iter().map(|(n, _)| *n).collect()
    }
    /// A header's value, for test doubles that must check the auth form.
    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}
impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.method, self.url)
    }
}
pub struct Response {
    pub status: u16,
    /// The JSON body (Null when it is not JSON).
    pub body: Value,
    /// `Retry-After` in seconds, when sent.
    pub retry_after: Option<i64>,
}
pub trait Http: Send + Sync {
    fn send(&self, request: &Request) -> Result<Response>;
}
/// The real transport (blocking reqwest, used only on the poll thread).
pub struct Reqwest;
impl Http for Reqwest {
    fn send(&self, request: &Request) -> Result<Response> {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let mut builder = match request.method {
            "POST" => client.post(&request.url),
            _ => client.get(&request.url),
        };
        for (name, value) in &request.headers {
            builder = builder.header(*name, value);
        }
        if let Some(body) = &request.body {
            builder = builder.body(body.to_string());
        }
        // The error never carries the request (it would name the URL only,
        // but a provider's reply could echo headers).
        let response = builder.send().map_err(|_| anyhow::anyhow!("request failed"))?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|n| (0..=86400).contains(n));
        let bytes = response.bytes().map_err(|_| anyhow::anyhow!("read failed"))?;
        ensure!(bytes.len() <= 1024 * 1024, "reply too large");
        Ok(Response {
            status,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            retry_after,
        })
    }
}

/// What the poller needs from the agent service (a trait so tests need no
/// native clients).
pub trait Accounts: Send + Sync {
    fn credential(&self, provider: &str) -> Result<Option<Credential>>;
    fn codex_rate_limits(&self) -> Result<Value>;
    /// Per provider: (has queued or running runs, latest run `finished_at`).
    fn activity(&self) -> BTreeMap<String, (bool, Option<String>)>;
}
impl Accounts for enginebox_agents::service::AgentService {
    fn credential(&self, provider: &str) -> Result<Option<Credential>> {
        self.usage_credential(provider)
    }
    fn codex_rate_limits(&self) -> Result<Value> {
        enginebox_agents::service::AgentService::codex_rate_limits(self)
    }
    fn activity(&self) -> BTreeMap<String, (bool, Option<String>)> {
        activity_of(&self.list_runs())
    }
}
/// Activity per provider from `list_runs()` (pure).
pub fn activity_of(runs: &Value) -> BTreeMap<String, (bool, Option<String>)> {
    let mut out: BTreeMap<String, (bool, Option<String>)> = BTreeMap::new();
    for run in runs["runs"].as_array().into_iter().flatten() {
        let Some(p) = run["provider"].as_str() else {
            continue;
        };
        let entry = out.entry(p.to_owned()).or_default();
        if ["queued", "running"].contains(&run["status"].as_str().unwrap_or("")) {
            entry.0 = true;
        }
        if let Some(f) = run["finished_at"].as_str()
            && entry.1.as_deref().is_none_or(|e| f > e)
        {
            entry.1 = Some(f.to_owned());
        }
    }
    out
}

// ------------------------------------------------------------------ normalized shape (pure)

fn iso(secs: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(secs, 0).map(|d| d.to_rfc3339())
}
/// Epoch seconds or milliseconds (number or digit string), or an RFC 3339
/// time, to RFC 3339.
fn time(v: &Value) -> Option<String> {
    let n = match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) => s.parse().ok(),
        Value::String(s) => {
            return chrono::DateTime::parse_from_rfc3339(s)
                .ok()
                .and_then(|d| iso(d.timestamp()));
        }
        _ => None,
    }?;
    if !n.is_finite() || n <= 0. {
        return None;
    }
    let secs = if n >= 1e12 { n / 1000. } else { n };
    (secs < 253402300799.).then(|| iso(secs as i64)).flatten()
}
fn seconds(v: &Value) -> Option<i64> {
    time(v).and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok()).map(|d| d.timestamp())
}
fn number(v: &Value) -> Option<f64> {
    let n = match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }?;
    (n.is_finite() && n.abs() < 1e15).then_some(n)
}
fn pct(v: &Value) -> Option<f64> {
    number(v).filter(|n| (0. ..=1000.).contains(n)).map(round2)
}
fn round2(n: f64) -> f64 {
    (n * 100.).round() / 100.
}
fn minor(v: &Value, exponent: i32) -> Option<f64> {
    number(v).map(|n| round2(n / 10f64.powi(exponent)))
}
fn word(v: &Value) -> Option<String> {
    let s = v.as_str()?.trim();
    let ok = !s.is_empty()
        && s.len() <= 64
        && s.chars().all(|c| c.is_ascii_alphanumeric() || " _-.:/+".contains(c));
    ok.then(|| s.to_owned())
}
fn kind_of_minutes(m: f64) -> &'static str {
    match m.round() as i64 {
        300 => "5h",
        1440 => "daily",
        10080 => "weekly",
        40320..=44640 => "monthly",
        _ => "other",
    }
}
/// The empty normalized account: every field unknown.
pub fn blank(provider: &str, source: Option<&str>, now: i64) -> Value {
    json!({
        "provider": provider,
        "account": format!("controller:{provider}"),
        "scope": "controller",
        "plan": {"name": null, "tier": null, "price_usd_month": null, "status": null},
        "windows": [],
        "credits": {"balance": null, "currency": null, "auto_topup": null, "extra_usage_enabled": null, "overage_spend": null, "overage_limit": null},
        "status": "unavailable",
        "detail": null,
        "fetched_at": iso(now),
        "source": source,
    })
}
#[allow(clippy::too_many_arguments)]
fn window(
    kind: &str,
    label: Option<String>,
    used_pct: Option<f64>,
    used: Option<f64>,
    limit: Option<f64>,
    unit: &str,
    resets_at: Option<String>,
    window_minutes: Option<f64>,
) -> Value {
    json!({"kind": kind, "label": label, "used_pct": used_pct, "used": used, "limit": limit, "unit": unit, "resets_at": resets_at, "window_minutes": window_minutes})
}
/// `ok`, or `limited` when a provider-reported window is used up.
fn settle(mut v: Value, limited: bool) -> Value {
    let full = v["windows"]
        .as_array()
        .is_some_and(|w| w.iter().any(|w| w["used_pct"].as_f64().is_some_and(|p| p >= 100.)));
    v["status"] = json!(if limited || full { "limited" } else { "ok" });
    v
}

/// Claude's `/api/oauth/usage` (+ `/api/oauth/profile`, + the non-secret facts
/// in its credentials file) to the normalized shape.
pub fn normalize_claude(usage: &Value, profile: Option<&Value>, meta: &Value, now: i64) -> Value {
    let mut v = blank("claude", Some(CLAUDE_USAGE_URL), now);
    let org = profile.map(|p| &p["organization"]).unwrap_or(&Value::Null);
    let plan_word = word(&org["organization_type"]).or_else(|| word(&meta["subscription_type"]));
    v["plan"]["name"] = json!(plan_word.map(|p| p.trim_start_matches("claude_").to_owned()));
    v["plan"]["tier"] = json!(word(&org["rate_limit_tier"]).or_else(|| word(&meta["rate_limit_tier"])));
    v["plan"]["status"] = json!(word(&org["subscription_status"]));
    let mut windows = Vec::new();
    let mut models = 0;
    for (key, kind, minutes) in [
        ("five_hour", "5h", 300.),
        ("seven_day", "weekly", 10080.),
        ("seven_day_opus", "weekly", 10080.),
        ("seven_day_sonnet", "weekly", 10080.),
        ("seven_day_oauth_apps", "weekly", 10080.),
        ("seven_day_cowork", "weekly", 10080.),
    ] {
        let w = &usage[key];
        if !w.is_object() {
            continue;
        }
        let used_pct = pct(&w["utilization"]);
        let resets = time(&w["resets_at"]);
        if used_pct.is_none() && resets.is_none() {
            continue;
        }
        if key.starts_with("seven_day_") {
            models += 1;
        }
        let (used, limit) = (number(&w["used_dollars"]), number(&w["limit_dollars"]));
        let money = used.is_some() || limit.is_some();
        windows.push(window(
            kind,
            Some(key.into()),
            used_pct,
            used.map(round2),
            limit.map(round2),
            if money { "usd" } else { "percent" },
            resets,
            Some(minutes),
        ));
    }
    // Newer replies list per-model weekly limits in `limits[]` instead.
    if models == 0 {
        for l in usage["limits"].as_array().into_iter().flatten() {
            if l["kind"] != "weekly_scoped" {
                continue;
            }
            let model = word(&l["scope"]["model"]["display_name"])
                .or_else(|| word(&l["scope"]["model"]["id"]))
                .unwrap_or_else(|| "model".into());
            windows.push(window(
                "weekly",
                Some(format!("weekly_scoped:{model}")),
                pct(&l["percent"]),
                None,
                None,
                "percent",
                time(&l["resets_at"]),
                Some(10080.),
            ));
        }
    }
    let extra = &usage["extra_usage"];
    let spend = &usage["spend"];
    let exponent = number(&spend["used"]["exponent"])
        .or_else(|| number(&extra["decimal_places"]))
        .map(|n| n as i32)
        .filter(|n| (0..=4).contains(n))
        .unwrap_or(2); // CodexBar: "OAuth extra usage is always in cents"
    let currency = word(&spend["used"]["currency"]).or_else(|| word(&extra["currency"]));
    let overage_spend = minor(&spend["used"]["amount_minor"], exponent)
        .or_else(|| minor(&extra["used_credits"], exponent));
    let overage_limit = minor(&extra["monthly_limit"], exponent);
    let enabled = extra["is_enabled"]
        .as_bool()
        .or(spend["enabled"].as_bool())
        .or(org["has_extra_usage_enabled"].as_bool());
    v["credits"] = json!({
        "balance": minor(&spend["balance"]["amount_minor"], exponent).or_else(|| number(&spend["balance"]).map(round2)),
        "currency": if overage_spend.is_some() || overage_limit.is_some() { currency.or(Some("USD".into())) } else { currency },
        "auto_topup": spend["auto_reload"].as_bool().or(spend["auto_reload"]["enabled"].as_bool()),
        "extra_usage_enabled": enabled,
        "overage_spend": overage_spend,
        "overage_limit": overage_limit,
    });
    if overage_limit.is_some() {
        windows.push(window(
            "monthly",
            Some("extra_usage".into()),
            pct(&extra["utilization"]),
            overage_spend,
            overage_limit,
            "usd",
            None,
            None,
        ));
    }
    v["windows"] = json!(windows);
    settle(v, extra["spend_limit_reached"] == true && enabled == Some(true))
}

/// Codex's `wham/usage` reply (+ the plan claim) to the normalized shape.
pub fn normalize_codex(usage: &Value, meta: &Value, now: i64) -> Value {
    let mut v = blank("codex", Some(CODEX_USAGE_URL), now);
    v["plan"]["name"] = json!(word(&usage["plan_type"]).or_else(|| word(&meta["plan_type"])));
    let mut windows = Vec::new();
    let mut add = |label: Option<String>, rl: &Value| {
        for position in ["primary_window", "secondary_window"] {
            let w = &rl[position];
            if !w.is_object() {
                continue;
            }
            let minutes = number(&w["limit_window_seconds"]).filter(|s| *s > 0.).map(|s| s / 60.);
            let resets = time(&w["reset_at"]).or_else(|| {
                number(&w["reset_after_seconds"]).and_then(|s| iso(now + s as i64))
            });
            windows.push(window(
                minutes.map(kind_of_minutes).unwrap_or("other"),
                label.clone(),
                pct(&w["used_percent"]),
                None,
                None,
                "percent",
                resets,
                minutes,
            ));
        }
    };
    add(None, &usage["rate_limit"]);
    for extra in usage["additional_rate_limits"].as_array().into_iter().flatten().take(8) {
        add(word(&extra["limit_name"]).or_else(|| word(&extra["metered_feature"])), &extra["rate_limit"]);
    }
    let credits = &usage["credits"];
    v["credits"] = json!({
        "balance": if credits["unlimited"] == true { None } else { number(&credits["balance"]).map(round2) },
        "currency": credits.is_object().then_some("credits"),
        "auto_topup": null,
        "extra_usage_enabled": credits["has_credits"].as_bool(),
        "overage_spend": null,
        "overage_limit": number(&usage["spend_control"]["individual_limit"]).map(round2),
    });
    if credits["unlimited"] == true {
        v["credits"]["unlimited"] = json!(true);
    }
    v["windows"] = json!(windows);
    let limited = usage["rate_limit"]["limit_reached"] == true || usage["rate_limit"]["allowed"] == false;
    settle(v, limited)
}

/// The Codex app-server's `account/rateLimits/read` reply (the fallback).
pub fn normalize_codex_app_server(raw: &Value, meta: &Value, now: i64) -> Value {
    let mut v = blank("codex", Some("codex app-server account/rateLimits/read"), now);
    let mut buckets: Vec<(String, &Value)> = raw["rateLimitsByLimitId"]
        .as_object()
        .map(|m| m.iter().map(|(k, b)| (k.clone(), b)).collect())
        .unwrap_or_default();
    if buckets.is_empty() && raw["rateLimits"].is_object() {
        buckets.push(("codex".into(), &raw["rateLimits"]));
    }
    let mut windows = Vec::new();
    for (key, b) in buckets.iter().take(16) {
        if v["plan"]["name"].is_null() {
            v["plan"]["name"] = json!(word(&b["planType"]));
        }
        let label = (key != "codex").then(|| word(&b["limitName"]).unwrap_or_else(|| key.clone()));
        for position in ["primary", "secondary"] {
            let w = &b[position];
            if !w.is_object() {
                continue;
            }
            let minutes = number(&w["windowDurationMins"]).filter(|m| *m > 0.);
            windows.push(window(
                minutes.map(kind_of_minutes).unwrap_or("other"),
                label.clone(),
                pct(&w["usedPercent"]),
                None,
                None,
                "percent",
                time(&w["resetsAt"]),
                minutes,
            ));
        }
        let c = &b["credits"];
        if key == "codex" && c.is_object() {
            v["credits"]["balance"] = json!(if c["unlimited"] == true { None } else { number(&c["balance"]).map(round2) });
            v["credits"]["currency"] = json!("credits");
            v["credits"]["extra_usage_enabled"] = json!(c["hasCredits"].as_bool());
            v["credits"]["overage_limit"] = json!(number(&b["individualLimit"]).map(round2));
        }
    }
    if v["plan"]["name"].is_null() {
        v["plan"]["name"] = json!(word(&meta["plan_type"]));
    }
    v["windows"] = json!(windows);
    settle(v, false)
}

/// Cursor's dashboard replies to the normalized shape. Any may be missing.
pub fn normalize_cursor(period: Option<&Value>, summary: Option<&Value>, stripe: Option<&Value>, now: i64) -> Value {
    let mut v = blank("cursor", Some(CURSOR_PERIOD_URL), now);
    let null = Value::Null;
    let (period, summary, stripe) = (period.unwrap_or(&null), summary.unwrap_or(&null), stripe.unwrap_or(&null));
    v["plan"]["name"] = json!(word(&stripe["membershipType"]).or_else(|| word(&summary["membershipType"])));
    v["plan"]["tier"] = json!(if stripe["isTeamMember"] == true {
        word(&stripe["teamMembershipType"]).map(|t| format!("team:{}", t.to_lowercase()))
    } else {
        word(&stripe["individualMembershipType"])
    });
    v["plan"]["status"] = json!(word(&stripe["subscriptionStatus"]));
    let mut windows = Vec::new();
    // The current allowance (live: the weekly team allowance).
    let (start, end) = (seconds(&period["billingCycleStart"]), seconds(&period["billingCycleEnd"]));
    if period["planUsage"].is_object() {
        let minutes = start.zip(end).filter(|(s, e)| e > s).map(|(s, e)| (e - s) as f64 / 60.);
        let plan = &period["planUsage"];
        let (used, limit) = (minor(&plan["totalSpend"], 2), minor(&plan["limit"], 2));
        windows.push(window(
            minutes.map(kind_of_minutes).unwrap_or("other"),
            None,
            pct(&plan["totalPercentUsed"]),
            used,
            limit,
            if used.is_some() || limit.is_some() { "usd" } else { "percent" },
            time(&period["billingCycleEnd"]),
            minutes,
        ));
        for (key, label) in [("autoPercentUsed", "auto"), ("apiPercentUsed", "api")] {
            if let Some(p) = pct(&plan[key]) {
                windows.push(window(
                    minutes.map(kind_of_minutes).unwrap_or("other"),
                    Some(label.into()),
                    Some(p),
                    None,
                    None,
                    "percent",
                    time(&period["billingCycleEnd"]),
                    minutes,
                ));
            }
        }
    }
    // The billing cycle's included money (cents), when the summary reports it.
    let plan = &summary["individualUsage"]["plan"];
    if plan.is_object() && number(&plan["limit"]).is_some() {
        let (s, e) = (seconds(&summary["billingCycleStart"]), seconds(&summary["billingCycleEnd"]));
        let minutes = s.zip(e).filter(|(s, e)| e > s).map(|(s, e)| (e - s) as f64 / 60.);
        windows.push(window(
            minutes.map(kind_of_minutes).unwrap_or("other"),
            Some("included_usd".into()),
            pct(&plan["totalPercentUsed"]),
            minor(&plan["used"], 2),
            minor(&plan["limit"], 2),
            "usd",
            time(&summary["billingCycleEnd"]),
            minutes,
        ));
    }
    // On-demand (extra) usage: the member's own, else the team's.
    let own = &summary["individualUsage"]["onDemand"];
    let team = &summary["teamUsage"]["onDemand"];
    let on_demand = if own["enabled"] == true || !team.is_object() { own } else { team };
    let customer = number(&stripe["customerBalance"]);
    v["credits"] = json!({
        "balance": customer.map(|c| round2((-c).max(0.) / 100.)),
        "currency": if on_demand.is_object() || customer.is_some() { Some("USD") } else { None },
        "auto_topup": null,
        "extra_usage_enabled": on_demand["enabled"].as_bool(),
        "overage_spend": minor(&on_demand["used"], 2),
        "overage_limit": minor(&on_demand["limit"], 2),
    });
    v["windows"] = json!(windows);
    settle(v, false)
}

/// Z.ai's quota reply to the normalized shape.
pub fn normalize_glm(quota: &Value, source: &str, now: i64) -> Value {
    let mut v = blank("glm", Some(source), now);
    let data = &quota["data"];
    v["plan"]["name"] = json!(["level", "planName", "plan", "plan_type", "packageName"]
        .iter()
        .find_map(|k| word(&data[*k])));
    let mut windows = Vec::new();
    for l in data["limits"].as_array().into_iter().flatten().take(16) {
        let ty = l["type"].as_str().unwrap_or("");
        let (unit_code, n) = (number(&l["unit"]).unwrap_or(0.) as i64, number(&l["number"]).unwrap_or(1.).max(1.));
        let (kind, minutes) = match (ty, unit_code) {
            (_, 3) if n == 5. => ("5h", Some(300.)),
            (_, 3) => ("other", Some(n * 60.)),
            (_, 1) => ("daily", Some(1440. * n)),
            (_, 6) => ("weekly", Some(10080.)),
            ("TIME_LIMIT", 5) => ("monthly", None),
            _ => ("other", None),
        };
        let unit = match ty {
            "TOKENS_LIMIT" => "tokens",
            "TIME_LIMIT" => "tool_calls",
            "CREDIT_LIMIT" => "credits",
            _ => "percent",
        };
        windows.push(window(
            kind,
            word(&json!(ty.to_lowercase())),
            pct(&l["percentage"]),
            number(&l["currentValue"]),
            number(&l["usage"]),
            unit,
            time(&l["nextResetTime"]),
            minutes,
        ));
    }
    v["windows"] = json!(windows);
    settle(v, false)
}

// ------------------------------------------------------------------ fetching

/// A failed read: a reason for people, and when to try again.
#[derive(Debug)]
pub struct Failure {
    pub detail: String,
    pub retry_after: Option<i64>,
}
fn fail(detail: impl Into<String>) -> Failure {
    Failure { detail: detail.into(), retry_after: None }
}
/// The poll outcome: a normalized account, or "not readable" (no credential
/// or an unsupported kind: not an error), or a failure.
pub enum Outcome {
    Read(Value),
    Unavailable(String),
    Failed(Failure),
}
fn check(r: Result<Response>, what: &str) -> std::result::Result<Value, Failure> {
    let r = r.map_err(|_| fail(format!("{what} could not be reached")))?;
    match r.status {
        200 => Ok(r.body),
        401 | 403 => Err(fail(format!("{what} refused the account's credential ({})", r.status))),
        429 => Err(Failure {
            detail: format!("{what} rate-limited the usage read (429)"),
            retry_after: Some(r.retry_after.unwrap_or(900)),
        }),
        s => Err(fail(format!("{what} answered {s}"))),
    }
}

pub struct Poller<'a> {
    pub http: &'a dyn Http,
    pub accounts: &'a dyn Accounts,
    pub root: &'a Path,
    pub profile: &'a Mutex<Option<(i64, Value)>>,
}
impl Poller<'_> {
    pub fn read(&self, provider: &str, now: i64) -> Outcome {
        match provider {
            "glm" => self.glm(now),
            p => match self.accounts.credential(p) {
                Ok(Some(c)) => match p {
                    "claude" => self.claude(&c, now),
                    "codex" => self.codex(&c, now),
                    _ => self.cursor(&c, now),
                },
                Ok(None) => Outcome::Unavailable(format!("No {p} account is connected on this controller.")),
                Err(_) => Outcome::Failed(fail("The account's credential could not be read")),
            },
        }
    }
    fn claude(&self, c: &Credential, now: i64) -> Outcome {
        let scopes = c.meta["scopes"].as_array();
        if c.meta["setup_token"] == true
            || scopes.is_some_and(|s| !s.iter().any(|x| x == "user:profile"))
        {
            return Outcome::Unavailable(
                "This Claude credential is an inference-only token (no user:profile scope); sign in with the full Claude login to read usage.".into(),
            );
        }
        if c.expired(now as f64, 60.) {
            return Outcome::Failed(fail("The Claude access token has expired; usage is read again after the native refresh"));
        }
        let auth = |r: Request| {
            r.header("authorization", format!("Bearer {}", c.secret()))
                .header("anthropic-beta", "oauth-2025-04-20")
                .header("user-agent", format!("claude-code/{}", enginebox_core::native_versions::CLAUDE_VERSION))
        };
        let usage = match check(self.http.send(&auth(Request::get(CLAUDE_USAGE_URL))), "Anthropic") {
            Ok(u) if u.is_object() => u,
            Ok(_) => return Outcome::Failed(fail("Anthropic's usage reply was not understood")),
            Err(f) => return Outcome::Failed(f),
        };
        let cached = self.profile.lock().unwrap().clone().filter(|(at, _)| now - at < PROFILE_TTL);
        let profile = cached.map(|(_, p)| p).or_else(|| {
            let p = check(self.http.send(&auth(Request::get(CLAUDE_PROFILE_URL))), "Anthropic").ok()?;
            // Keep only the plan facts (the reply names the person).
            let keep = json!({"organization": {
                "organization_type": p["organization"]["organization_type"],
                "rate_limit_tier": p["organization"]["rate_limit_tier"],
                "subscription_status": p["organization"]["subscription_status"],
                "has_extra_usage_enabled": p["organization"]["has_extra_usage_enabled"],
            }});
            *self.profile.lock().unwrap() = Some((now, keep.clone()));
            Some(keep)
        });
        Outcome::Read(normalize_claude(&usage, profile.as_ref(), &c.meta, now))
    }
    fn codex(&self, c: &Credential, now: i64) -> Outcome {
        if c.kind == "api_key" {
            return Outcome::Unavailable("Codex runs on an API key here, which has no ChatGPT plan usage.".into());
        }
        if let Some(account) = c.account_id.as_deref().filter(|_| !c.expired(now as f64, 60.)) {
            let request = Request::get(CODEX_USAGE_URL)
                .header("authorization", format!("Bearer {}", c.secret()))
                .header("chatgpt-account-id", account)
                .header("user-agent", format!("codex_cli_rs/{}", enginebox_core::native_versions::CODEX_VERSION))
                .header("originator", "codex_cli_rs");
            match check(self.http.send(&request), "ChatGPT") {
                Ok(u) if u.is_object() => return Outcome::Read(normalize_codex(&u, &c.meta, now)),
                Err(f) if f.retry_after.is_some() => return Outcome::Failed(f),
                _ => {}
            }
        }
        // Expired or refused: the native client refreshes and answers itself.
        match self.accounts.codex_rate_limits() {
            Ok(raw) if raw.is_object() => Outcome::Read(normalize_codex_app_server(&raw, &c.meta, now)),
            _ => Outcome::Failed(fail("Codex usage could not be read (ChatGPT and the native app-server both failed)")),
        }
    }
    fn cursor(&self, c: &Credential, now: i64) -> Outcome {
        if c.kind == "api_key" {
            return Outcome::Unavailable("Cursor runs on an API key here, which cannot read the personal dashboard's usage.".into());
        }
        let Some(user) = c.account_id.as_deref() else {
            return Outcome::Unavailable("The Cursor session does not name its user, so the dashboard cannot be read.".into());
        };
        if c.expired(now as f64, 60.) {
            return Outcome::Failed(fail("The Cursor session has expired; sign in to Cursor again on the controller"));
        }
        let cookie = format!("WorkosCursorSessionToken={user}%3A%3A{}", c.secret());
        let auth = |r: Request| {
            r.header("cookie", cookie.clone())
                .header("origin", "https://cursor.com")
                .header("referer", "https://cursor.com/dashboard")
        };
        let period = check(self.http.send(&auth(Request::post(CURSOR_PERIOD_URL, json!({})))), "Cursor");
        if let Err(f) = &period
            && f.retry_after.is_some()
        {
            return Outcome::Failed(fail(&f.detail));
        }
        let summary = check(self.http.send(&auth(Request::get(CURSOR_SUMMARY_URL))), "Cursor");
        let (period, summary) = (period.ok().filter(Value::is_object), summary.ok().filter(Value::is_object));
        if period.is_none() && summary.is_none() {
            return Outcome::Failed(fail("Cursor's dashboard usage could not be read"));
        }
        let stripe = check(self.http.send(&auth(Request::get(CURSOR_STRIPE_URL))), "Cursor").ok();
        let mut v = normalize_cursor(period.as_ref(), summary.as_ref(), stripe.as_ref(), now);
        if period.is_none() {
            v["source"] = json!(CURSOR_SUMMARY_URL);
        }
        Outcome::Read(v)
    }
    fn glm(&self, now: i64) -> Outcome {
        let Ok(saved) = state::read_json(&glm_key_path(self.root)) else {
            return Outcome::Unavailable("No GLM Coding Plan key is saved on this controller (POST /v1/usage/accounts/glm/key).".into());
        };
        let Some(key) = saved["key"].as_str().filter(|k| !k.is_empty()) else {
            return Outcome::Unavailable("No GLM Coding Plan key is saved on this controller.".into());
        };
        let base = if saved["region"] == "china" { GLM_CHINA } else { GLM_GLOBAL };
        let url = format!("{base}{GLM_QUOTA_PATH}");
        let request = Request::get(&url)
            .header("authorization", key)
            .header("accept-language", "en-US,en")
            .header("content-type", "application/json");
        match check(self.http.send(&request), "Z.ai") {
            Ok(q) if q["success"] == true && number(&q["code"]) == Some(200.) => Outcome::Read(normalize_glm(&q, &url, now)),
            Ok(_) => Outcome::Unavailable("Z.ai reports no Coding Plan for this key.".into()),
            Err(f) => Outcome::Failed(f),
        }
    }
}

// ------------------------------------------------------------------ the store and schedule

fn glm_key_path(root: &Path) -> PathBuf {
    root.join("usage-accounts").join("keys").join("glm.json")
}

#[derive(Clone, Default)]
struct Slot {
    account: Option<Value>,
    last_attempt: i64,
    failures: u32,
    backoff_until: i64,
}

#[derive(Clone)]
pub struct UsageAccounts {
    root: PathBuf,
    slots: Arc<Mutex<BTreeMap<String, Slot>>>,
    busy: Arc<AtomicBool>,
    http: Arc<dyn Http>,
    profile: Arc<Mutex<Option<(i64, Value)>>>,
}
impl UsageAccounts {
    pub fn open(root: &Path) -> Result<Self> {
        let dir = root.join("usage-accounts");
        let mut slots = BTreeMap::new();
        for p in PROVIDERS {
            let mut slot = Slot::default();
            if let Ok(saved) = state::read_json(&dir.join(format!("{p}.json"))) {
                slot.account = saved["account"].is_object().then(|| saved["account"].clone());
                slot.last_attempt = saved["last_attempt"].as_i64().unwrap_or(0);
                slot.failures = saved["failures"].as_u64().unwrap_or(0).min(16) as u32;
                slot.backoff_until = saved["backoff_until"].as_i64().unwrap_or(0);
            }
            slots.insert(p.to_owned(), slot);
        }
        Ok(Self {
            root: root.to_owned(),
            slots: Arc::new(Mutex::new(slots)),
            busy: Arc::new(AtomicBool::new(false)),
            http: Arc::new(Reqwest),
            profile: Arc::new(Mutex::new(None)),
        })
    }
    pub fn with_http(mut self, http: Arc<dyn Http>) -> Self {
        self.http = http;
        self
    }
    fn view(&self, p: &str, slot: &Slot) -> Value {
        let mut v = slot.account.clone().unwrap_or_else(|| {
            let mut b = blank(p, None, 0);
            b["fetched_at"] = Value::Null;
            b["detail"] = json!("Not read yet.");
            b
        });
        v["checked_at"] = json!((slot.last_attempt > 0).then(|| iso(slot.last_attempt)).flatten());
        v
    }
    /// Every account's last observation (no provider is contacted).
    pub fn list(&self) -> Value {
        let slots = self.slots.lock().unwrap();
        let accounts: Vec<Value> = PROVIDERS.iter().map(|p| self.view(p, &slots[*p])).collect();
        json!({"accounts": accounts, "schedule": {"active_seconds": ACTIVE_INTERVAL, "idle_seconds": IDLE_INTERVAL}})
    }
    pub fn get(&self, p: &str) -> Result<Value> {
        let slots = self.slots.lock().unwrap();
        let slot = slots.get(p).context("Unknown provider")?;
        Ok(json!({"account": self.view(p, slot)}))
    }
    /// The providers due now (pure apart from the clock it is given).
    fn due(&self, activity: &BTreeMap<String, (bool, Option<String>)>, now: i64, force: bool) -> Vec<String> {
        let slots = self.slots.lock().unwrap();
        PROVIDERS
            .iter()
            .filter(|p| {
                let s = &slots[**p];
                let since = now - s.last_attempt;
                if since < MIN_GAP || (now < s.backoff_until && !force) {
                    return false;
                }
                if force {
                    return true;
                }
                let (active, finished) = activity.get(**p).cloned().unwrap_or_default();
                let ended_since = finished
                    .and_then(|f| chrono::DateTime::parse_from_rfc3339(&f).ok())
                    .is_some_and(|f| f.timestamp() >= s.last_attempt);
                ended_since || since >= if active { ACTIVE_INTERVAL } else { IDLE_INTERVAL }
            })
            .map(|p| (*p).to_owned())
            .collect()
    }
    /// Poll the due accounts now, on this thread. Returns the providers read.
    pub fn poll(&self, accounts: &dyn Accounts, now: i64, force: bool) -> Vec<String> {
        let due = self.due(&accounts.activity(), now, force);
        for p in &due {
            self.slots.lock().unwrap().get_mut(p).unwrap().last_attempt = now;
            let poller = Poller { http: self.http.as_ref(), accounts, root: &self.root, profile: &self.profile };
            let outcome = poller.read(p, now);
            self.record(p, outcome, now);
        }
        due
    }
    fn record(&self, p: &str, outcome: Outcome, now: i64) {
        let mut slots = self.slots.lock().unwrap();
        let slot = slots.get_mut(p).unwrap();
        match outcome {
            Outcome::Read(v) => {
                slot.account = Some(v);
                slot.failures = 0;
                slot.backoff_until = 0;
            }
            Outcome::Unavailable(detail) => {
                let mut v = blank(p, None, now);
                v["detail"] = json!(detail);
                slot.account = Some(v);
                slot.failures = 0;
                slot.backoff_until = 0;
            }
            Outcome::Failed(f) => {
                slot.failures = (slot.failures + 1).min(16);
                let backoff = (ACTIVE_INTERVAL << (slot.failures - 1).min(10)).min(MAX_BACKOFF);
                slot.backoff_until = now + backoff.max(f.retry_after.unwrap_or(0));
                // The last good observation stays, marked stale; never a guess.
                let mut v = match slot.account.take() {
                    Some(prev) if prev["fetched_at"].is_string() && prev["status"] != "unavailable" => {
                        let mut prev = prev;
                        prev["status"] = json!("stale");
                        prev
                    }
                    _ => {
                        let mut b = blank(p, None, now);
                        b["status"] = json!("error");
                        b["fetched_at"] = Value::Null;
                        b
                    }
                };
                v["detail"] = json!(f.detail);
                v["retry_at"] = json!(iso(slot.backoff_until));
                slot.account = Some(v);
            }
        }
        let saved = json!({"account": slot.account, "last_attempt": slot.last_attempt, "failures": slot.failures, "backoff_until": slot.backoff_until});
        if let Err(e) = state::write_json(&self.root.join("usage-accounts").join(format!("{p}.json")), &saved) {
            eprintln!("{}", json!({"event":"enginebox.usage_accounts.store_failed","provider":p,"reason":e.to_string()}));
        }
    }
    /// The background step: start a poll thread when an account is due and
    /// none is running. Never blocks on a provider.
    pub fn tick(&self, accounts: Arc<dyn Accounts>) {
        let now = chrono::Utc::now().timestamp();
        if self.due(&accounts.activity(), now, false).is_empty() || self.busy.swap(true, Ordering::SeqCst) {
            return;
        }
        let this = self.clone();
        std::thread::spawn(move || {
            let read = this.poll(accounts.as_ref(), chrono::Utc::now().timestamp(), false);
            if !read.is_empty() {
                eprintln!("{}", json!({"event":"enginebox.usage_accounts.polled","providers":read}));
            }
            this.busy.store(false, Ordering::SeqCst);
        });
    }
    /// Save (or replace) the GLM Coding Plan key, write-only.
    pub fn set_glm_key(&self, body: &Value) -> Result<Value> {
        let key = body["key"].as_str().unwrap_or("").trim();
        ensure!(
            (16..=512).contains(&key.len()) && key.chars().all(|c| c.is_ascii_graphic()),
            "Provide the GLM Coding Plan API key as key."
        );
        let region = match body["region"].as_str().unwrap_or("global") {
            "global" => "global",
            "china" => "china",
            _ => anyhow::bail!("region is global (api.z.ai) or china (open.bigmodel.cn)."),
        };
        state::write_json(&glm_key_path(&self.root), &json!({"key": key, "region": region}))?;
        // Read it on the next tick.
        let mut slots = self.slots.lock().unwrap();
        let slot = slots.get_mut("glm").unwrap();
        slot.last_attempt = 0;
        slot.backoff_until = 0;
        Ok(json!({"provider": "glm", "saved": true, "region": region}))
    }
    pub fn delete_glm_key(&self) -> Result<Value> {
        let path = glm_key_path(&self.root);
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        let mut slots = self.slots.lock().unwrap();
        let slot = slots.get_mut("glm").unwrap();
        slot.last_attempt = 0;
        slot.account = None;
        Ok(json!({"provider": "glm", "deleted": true}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const NOW: i64 = 1_794_384_000; // 2026-11-11T08:00:00Z

    fn fixture(name: &str) -> Value {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/usage").join(name);
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }
    fn win<'a>(v: &'a Value, kind: &str, label: Option<&str>) -> &'a Value {
        v["windows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["kind"] == kind && w["label"].as_str() == label)
            .unwrap_or_else(|| panic!("no {kind} {label:?} window in {v}"))
    }

    #[test]
    fn claude_live_reply_normalizes_windows_plan_and_extra_usage() {
        let meta = json!({"subscription_type": "team", "rate_limit_tier": "default_claude_max_5x", "scopes": ["user:inference", "user:profile"]});
        let v = normalize_claude(&fixture("claude-oauth-usage.json"), Some(&fixture("claude-oauth-profile.json")), &meta, NOW);
        assert_eq!(v["provider"], "claude");
        assert_eq!(v["plan"]["name"], "team");
        assert_eq!(v["plan"]["tier"], "default_claude_max_5x");
        assert_eq!(v["plan"]["status"], "active");
        assert!(v["plan"]["price_usd_month"].is_null(), "no price is reported, none is guessed");
        let five = win(&v, "5h", Some("five_hour"));
        assert_eq!(five["used_pct"], 100.0);
        assert_eq!(five["resets_at"], "2026-11-07T09:50:00+00:00");
        assert_eq!(five["unit"], "percent");
        let week = win(&v, "weekly", Some("seven_day"));
        assert_eq!(week["used_pct"], 49.0);
        assert_eq!(week["resets_at"], "2026-11-13T03:00:00+00:00");
        // the per-model weekly limit from limits[] (no seven_day_* object)
        let scoped = win(&v, "weekly", Some("weekly_scoped:REDACTED"));
        assert_eq!(scoped["used_pct"], 2.0);
        assert_eq!(v["credits"]["extra_usage_enabled"], true);
        assert_eq!(v["credits"]["overage_spend"], 881.99);
        assert_eq!(v["credits"]["currency"], "USD");
        assert!(v["credits"]["overage_limit"].is_null());
        assert!(v["credits"]["balance"].is_null());
        assert_eq!(v["status"], "limited", "the 5-hour window is used up");
        // no profile: the credentials file's facts
        let bare = normalize_claude(&fixture("claude-oauth-usage.json"), None, &meta, NOW);
        assert_eq!(bare["plan"]["name"], "team");
        assert!(bare["plan"]["status"].is_null());
    }

    #[test]
    fn claude_legacy_shape_with_monthly_limit() {
        let usage = json!({"five_hour":{"utilization":42.0,"resets_at":"2026-11-07T18:00:00Z"},"seven_day":{"utilization":13.0,"resets_at":"2026-11-10T00:00:00Z"},"seven_day_opus":null,"seven_day_sonnet":{"utilization":5.0,"resets_at":"2026-11-10T00:00:00Z"},"extra_usage":{"is_enabled":true,"monthly_limit":5000,"used_credits":1234,"utilization":24.68}});
        let v = normalize_claude(&usage, None, &json!({}), NOW);
        assert_eq!(win(&v, "weekly", Some("seven_day_sonnet"))["used_pct"], 5.0);
        let m = win(&v, "monthly", Some("extra_usage"));
        assert_eq!((m["used"].as_f64(), m["limit"].as_f64()), (Some(12.34), Some(50.0)));
        assert_eq!(v["credits"]["overage_limit"], 50.0);
        assert_eq!(v["status"], "ok");
    }

    #[test]
    fn codex_live_reply_normalizes_weekly_window_plan_and_credits() {
        let v = normalize_codex(&fixture("codex-wham-usage.json"), &json!({"plan_type": "pro"}), NOW);
        assert_eq!(v["plan"]["name"], "pro");
        let week = win(&v, "weekly", None);
        assert_eq!(week["used_pct"], 27.0);
        assert_eq!(week["window_minutes"], 10080.0);
        assert_eq!(week["resets_at"], "2026-11-12T07:01:21+00:00");
        assert_eq!(v["windows"].as_array().unwrap().len(), 1, "secondary_window is null");
        assert_eq!(v["credits"]["balance"], 19093.36);
        assert_eq!(v["credits"]["currency"], "credits");
        assert_eq!(v["credits"]["extra_usage_enabled"], true);
        assert!(v["credits"]["overage_limit"].is_null());
        assert_eq!(v["status"], "ok");
        let limited = normalize_codex(&json!({"plan_type":"plus","rate_limit":{"allowed":false,"limit_reached":true,"primary_window":{"used_percent":100,"limit_window_seconds":18000,"reset_at":1794390000}}}), &json!({}), NOW);
        assert_eq!(win(&limited, "5h", None)["used_pct"], 100.0);
        assert_eq!(limited["status"], "limited");
    }

    #[test]
    fn codex_app_server_fallback_normalizes() {
        // account/rateLimits/read, codex-rs/app-server-protocol v2/account.rs
        let raw = json!({"rateLimits":{"limitId":"codex","primary":{"usedPercent":15,"windowDurationMins":300,"resetsAt":1794390000},"secondary":{"usedPercent":5,"windowDurationMins":10080,"resetsAt":1794466881},"credits":{"hasCredits":true,"unlimited":false,"balance":"150.0"},"planType":"pro"}});
        let v = normalize_codex_app_server(&raw, &json!({}), NOW);
        assert_eq!(v["plan"]["name"], "pro");
        assert_eq!(win(&v, "5h", None)["used_pct"], 15.0);
        assert_eq!(win(&v, "weekly", None)["resets_at"], "2026-11-12T07:01:21+00:00");
        assert_eq!(v["credits"]["balance"], 150.0);
        assert_eq!(v["credits"]["extra_usage_enabled"], true);
    }

    #[test]
    fn cursor_live_replies_normalize_allowance_plan_and_on_demand() {
        let v = normalize_cursor(
            Some(&fixture("cursor-current-period-usage.json")),
            Some(&fixture("cursor-usage-summary.json")),
            Some(&fixture("cursor-auth-stripe.json")),
            NOW,
        );
        assert_eq!(v["plan"]["name"], "enterprise");
        assert_eq!(v["plan"]["tier"], "team:self_serve");
        assert_eq!(v["plan"]["status"], "active");
        let week = win(&v, "weekly", None);
        assert_eq!(week["used_pct"], 8.33);
        assert_eq!(week["resets_at"], "2026-11-13T14:59:39+00:00");
        assert_eq!(win(&v, "weekly", Some("auto"))["used_pct"], 8.33);
        let month = win(&v, "monthly", Some("included_usd"));
        assert_eq!((month["used"].as_f64(), month["limit"].as_f64()), (Some(0.0), Some(553.95)));
        assert_eq!(month["resets_at"], "2026-12-01T00:00:00+00:00");
        assert_eq!(v["credits"]["extra_usage_enabled"], false);
        assert_eq!(v["credits"]["overage_spend"], 0.0);
        assert!(v["credits"]["overage_limit"].is_null());
        assert_eq!(v["credits"]["balance"], 0.0);
        assert_eq!(v["status"], "ok");
        // only the summary
        let s = normalize_cursor(None, Some(&fixture("cursor-usage-summary.json")), None, NOW);
        assert!(s["plan"]["tier"].is_null());
        assert_eq!(s["plan"]["name"], "enterprise");
    }

    #[test]
    fn glm_quota_reply_normalizes() {
        // The official glm-plan-usage plugin's endpoint; no GLM account is
        // connected to capture from, so this follows its documented shape.
        let q = json!({"code":200,"msg":"Operation successful","success":true,"data":{"level":"pro","limits":[
            {"type":"TOKENS_LIMIT","unit":3,"number":5,"usage":800000000,"currentValue":96000000,"remaining":704000000,"percentage":12,"nextResetTime":1794402000000_i64},
            {"type":"TOKENS_LIMIT","unit":6,"number":1,"percentage":4,"nextResetTime":1794816000000_i64},
            {"type":"TIME_LIMIT","unit":5,"number":1,"usage":1000,"currentValue":37,"remaining":963,"percentage":3,"usageDetails":[{"modelCode":"search-prime","usage":20}]}]}});
        let v = normalize_glm(&q, "https://api.z.ai/api/monitor/usage/quota/limit", NOW);
        assert_eq!(v["plan"]["name"], "pro");
        let five = win(&v, "5h", Some("tokens_limit"));
        assert_eq!((five["used_pct"].as_f64(), five["used"].as_f64(), five["limit"].as_f64()), (Some(12.0), Some(96e6), Some(8e8)));
        assert_eq!(five["resets_at"], "2026-11-11T13:00:00+00:00");
        assert_eq!(win(&v, "weekly", Some("tokens_limit"))["used_pct"], 4.0);
        let tools = win(&v, "monthly", Some("time_limit"));
        assert_eq!((tools["used"].as_f64(), tools["limit"].as_f64(), tools["unit"].as_str()), (Some(37.0), Some(1000.0), Some("tool_calls")));
        assert!(tools["resets_at"].is_null());
        assert!(v["credits"]["balance"].is_null() && v["credits"]["extra_usage_enabled"].is_null());
    }

    #[test]
    fn unknown_input_stays_null() {
        for v in [
            normalize_claude(&json!({}), None, &json!({}), NOW),
            normalize_codex(&json!({}), &json!({}), NOW),
            normalize_cursor(None, None, None, NOW),
            normalize_glm(&json!({}), "x", NOW),
        ] {
            assert!(v["plan"]["name"].is_null() && v["plan"]["tier"].is_null() && v["plan"]["price_usd_month"].is_null(), "{v}");
            assert_eq!(v["windows"], json!([]));
            assert!(v["credits"].as_object().unwrap().values().all(Value::is_null), "{v}");
        }
        // out-of-range and malformed numbers are dropped, not clamped
        let v = normalize_codex(&json!({"rate_limit":{"primary_window":{"used_percent":-3,"limit_window_seconds":"x","reset_at":"soon"}}}), &json!({}), NOW);
        assert!(v["windows"][0]["used_pct"].is_null() && v["windows"][0]["resets_at"].is_null());
    }

    // ---------------------------------------------------------------- poller

    struct Fake {
        replies: Mutex<BTreeMap<String, (u16, Value)>>,
        seen: Mutex<Vec<(String, Vec<&'static str>, Option<String>)>>,
    }
    impl Fake {
        fn new(replies: &[(&str, u16, Value)]) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies.iter().map(|(u, s, b)| ((*u).to_owned(), (*s, b.clone()))).collect()),
                seen: Mutex::new(Vec::new()),
            })
        }
    }
    impl Http for Fake {
        fn send(&self, r: &Request) -> Result<Response> {
            let auth = r.header_value("authorization").or(r.header_value("cookie")).map(str::to_owned);
            self.seen.lock().unwrap().push((r.url.clone(), r.header_names(), auth));
            let (status, body) = self.replies.lock().unwrap().get(&r.url).cloned().unwrap_or((404, Value::Null));
            Ok(Response { status, body, retry_after: (status == 429).then_some(120) })
        }
    }
    struct FakeAccounts {
        active: bool,
        finished: Option<String>,
        codex_raw: Option<Value>,
    }
    const SECRET: &str = "sk-ant-oat01-SECRETSECRETSECRET";
    impl Accounts for FakeAccounts {
        fn credential(&self, p: &str) -> Result<Option<Credential>> {
            let mut c = match p {
                "claude" => Credential::new(p, "oauth", SECRET.into()),
                "codex" => {
                    let mut c = Credential::new(p, "chatgpt", SECRET.into());
                    c.account_id = Some("acct-1".into());
                    c
                }
                _ => {
                    let mut c = Credential::new(p, "session", SECRET.into());
                    c.account_id = Some("user_1".into());
                    c
                }
            };
            c.expires_at = Some((NOW + 3600) as f64);
            c.meta = json!({"scopes": ["user:inference", "user:profile"], "plan_type": "pro"});
            Ok(Some(c))
        }
        fn codex_rate_limits(&self) -> Result<Value> {
            self.codex_raw.clone().context("no app-server")
        }
        fn activity(&self) -> BTreeMap<String, (bool, Option<String>)> {
            ["claude", "codex", "cursor"].iter().map(|p| ((*p).to_owned(), (self.active, self.finished.clone()))).collect()
        }
    }

    #[test]
    fn poller_reads_every_provider_stores_and_never_leaks_the_token() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new(&[
            (CLAUDE_USAGE_URL, 200, fixture("claude-oauth-usage.json")),
            (CLAUDE_PROFILE_URL, 200, fixture("claude-oauth-profile.json")),
            (CODEX_USAGE_URL, 200, fixture("codex-wham-usage.json")),
            (CURSOR_PERIOD_URL, 200, fixture("cursor-current-period-usage.json")),
            (CURSOR_SUMMARY_URL, 200, fixture("cursor-usage-summary.json")),
            (CURSOR_STRIPE_URL, 200, fixture("cursor-auth-stripe.json")),
        ]);
        let store = UsageAccounts::open(dir.path()).unwrap().with_http(fake.clone());
        let accounts = FakeAccounts { active: false, finished: None, codex_raw: None };
        let read = store.poll(&accounts, NOW, false);
        assert_eq!(read, vec!["claude", "codex", "cursor", "glm"]);
        let list = store.list();
        let by = |p: &str| list["accounts"].as_array().unwrap().iter().find(|a| a["provider"] == p).unwrap().clone();
        assert_eq!(by("claude")["status"], "limited");
        assert_eq!(by("codex")["credits"]["balance"], 19093.36);
        assert_eq!(by("cursor")["plan"]["name"], "enterprise");
        assert_eq!(by("glm")["status"], "unavailable");
        assert!(by("glm")["detail"].as_str().unwrap().contains("No GLM"));
        // the auth forms the CLIs use
        let seen = fake.seen.lock().unwrap();
        let auth_of = |u: &str| seen.iter().find(|s| s.0 == u).unwrap().2.clone().unwrap();
        assert_eq!(auth_of(CLAUDE_USAGE_URL), format!("Bearer {SECRET}"));
        assert!(seen.iter().find(|s| s.0 == CLAUDE_USAGE_URL).unwrap().1.contains(&"anthropic-beta"));
        assert!(seen.iter().find(|s| s.0 == CODEX_USAGE_URL).unwrap().1.contains(&"chatgpt-account-id"));
        assert_eq!(auth_of(CURSOR_PERIOD_URL), format!("WorkosCursorSessionToken=user_1%3A%3A{SECRET}"));
        assert!(seen.iter().find(|s| s.0 == CURSOR_PERIOD_URL).unwrap().1.contains(&"origin"));
        drop(seen);
        // nothing stored or served carries the token
        assert!(!list.to_string().contains("SECRET"));
        for p in PROVIDERS {
            let raw = std::fs::read_to_string(dir.path().join("usage-accounts").join(format!("{p}.json"))).unwrap();
            assert!(!raw.contains("SECRET"), "{p}");
        }
        assert!(!format!("{:?}", accounts.credential("claude").unwrap().unwrap()).contains("SECRET"));
        assert!(!format!("{:?}", Request::get(CLAUDE_USAGE_URL).header("authorization", SECRET)).contains("SECRET"));
        // a restart serves the stored observation
        let reopened = UsageAccounts::open(dir.path()).unwrap();
        assert_eq!(reopened.get("codex").unwrap()["account"]["plan"]["name"], "pro");
    }

    #[test]
    fn schedule_active_idle_run_end_and_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new(&[(CODEX_USAGE_URL, 200, fixture("codex-wham-usage.json"))]);
        let store = UsageAccounts::open(dir.path()).unwrap().with_http(fake.clone());
        let idle = FakeAccounts { active: false, finished: None, codex_raw: None };
        store.poll(&idle, NOW, false);
        // idle: not again before 30 minutes
        assert!(!store.due(&idle.activity(), NOW + ACTIVE_INTERVAL, false).contains(&"codex".to_owned()));
        assert!(store.due(&idle.activity(), NOW + IDLE_INTERVAL, false).contains(&"codex".to_owned()));
        // active runs: every 5 minutes
        let active = FakeAccounts { active: true, finished: None, codex_raw: None };
        assert!(store.due(&active.activity(), NOW + ACTIVE_INTERVAL, false).contains(&"codex".to_owned()));
        // a run that ended after the last poll: right away (after the minimum gap)
        let ended = FakeAccounts { active: false, finished: iso(NOW + 90), codex_raw: None };
        assert!(!store.due(&ended.activity(), NOW + 30, false).contains(&"codex".to_owned()));
        assert!(store.due(&ended.activity(), NOW + 120, false).contains(&"codex".to_owned()));
        // claude failed (no fixture: 404) -> error with backoff; cursor too
        let claude = store.get("claude").unwrap()["account"].clone();
        assert_eq!(claude["status"], "error");
        assert!(claude["fetched_at"].is_null());
        assert!(!store.due(&active.activity(), NOW + ACTIVE_INTERVAL - 1, false).contains(&"claude".to_owned()));
        // a second failure doubles the backoff
        store.poll(&active, NOW + ACTIVE_INTERVAL, false);
        assert!(!store.due(&active.activity(), NOW + ACTIVE_INTERVAL + 2 * ACTIVE_INTERVAL - 1, false).contains(&"claude".to_owned()));
        assert!(store.due(&active.activity(), NOW + 3 * ACTIVE_INTERVAL, false).contains(&"claude".to_owned()));
    }

    #[test]
    fn a_failure_keeps_the_last_good_reading_as_stale_and_honors_retry_after() {
        let dir = tempfile::tempdir().unwrap();
        let ok = Fake::new(&[(CODEX_USAGE_URL, 200, fixture("codex-wham-usage.json"))]);
        let store = UsageAccounts::open(dir.path()).unwrap().with_http(ok);
        let a = FakeAccounts { active: true, finished: None, codex_raw: None };
        store.poll(&a, NOW, false);
        let limited = Fake::new(&[(CODEX_USAGE_URL, 429, Value::Null)]);
        let store = store.with_http(limited);
        store.poll(&a, NOW + ACTIVE_INTERVAL, false);
        let codex = store.get("codex").unwrap()["account"].clone();
        assert_eq!(codex["status"], "stale");
        assert_eq!(codex["windows"][0]["used_pct"], 27.0, "the last good reading, with its fetched_at");
        assert!(codex["detail"].as_str().unwrap().contains("429"));
        // 429 without Retry-After in the fake -> 120 s, under the 300 s backoff floor
        assert!(!store.due(&a.activity(), NOW + 2 * ACTIVE_INTERVAL - 1, false).contains(&"codex".to_owned()));
    }

    #[test]
    fn codex_falls_back_to_the_app_server_when_chatgpt_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let refused = Fake::new(&[(CODEX_USAGE_URL, 401, Value::Null)]);
        let store = UsageAccounts::open(dir.path()).unwrap().with_http(refused);
        let a = FakeAccounts { active: false, finished: None, codex_raw: Some(json!({"rateLimits":{"primary":{"usedPercent":40,"windowDurationMins":10080,"resetsAt":1794466881},"planType":"plus"}})) };
        store.poll(&a, NOW, false);
        let codex = store.get("codex").unwrap()["account"].clone();
        assert_eq!(codex["plan"]["name"], "plus");
        assert_eq!(codex["windows"][0]["used_pct"], 40.0);
        assert!(codex["source"].as_str().unwrap().contains("app-server"));
    }

    #[test]
    fn inference_only_claude_token_is_unavailable_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new(&[]);
        struct Setup;
        impl Accounts for Setup {
            fn credential(&self, p: &str) -> Result<Option<Credential>> {
                let mut c = Credential::new(p, "oauth", SECRET.into());
                c.meta = json!({"setup_token": true});
                Ok((p == "claude").then_some(c))
            }
            fn codex_rate_limits(&self) -> Result<Value> {
                anyhow::bail!("none")
            }
            fn activity(&self) -> BTreeMap<String, (bool, Option<String>)> {
                BTreeMap::new()
            }
        }
        let store = UsageAccounts::open(dir.path()).unwrap().with_http(fake.clone());
        store.poll(&Setup, NOW, false);
        assert!(fake.seen.lock().unwrap().is_empty(), "no provider request without a usable credential");
        let c = store.get("claude").unwrap()["account"].clone();
        assert_eq!(c["status"], "unavailable");
        assert!(c["detail"].as_str().unwrap().contains("user:profile"));
        assert_eq!(store.get("codex").unwrap()["account"]["status"], "unavailable");
    }

    #[test]
    fn glm_key_is_write_only_and_read_with_the_plugin_auth_form() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("{GLM_GLOBAL}{GLM_QUOTA_PATH}");
        let fake = Fake::new(&[(url.as_str(), 200, json!({"code":200,"success":true,"data":{"level":"lite","limits":[{"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":100,"nextResetTime":1794402000000_i64}]}}))]);
        let store = UsageAccounts::open(dir.path()).unwrap().with_http(fake.clone());
        assert!(store.set_glm_key(&json!({"key":"short"})).is_err());
        let saved = store.set_glm_key(&json!({"key":"zai-0123456789abcdef.SECRETKEY"})).unwrap();
        assert!(!saved.to_string().contains("SECRET"));
        let none = FakeAccounts { active: false, finished: None, codex_raw: None };
        store.poll(&none, NOW, false);
        let g = store.get("glm").unwrap();
        assert!(!g.to_string().contains("SECRET"));
        assert_eq!(g["account"]["plan"]["name"], "lite");
        assert_eq!(g["account"]["status"], "limited");
        let seen = fake.seen.lock().unwrap();
        let glm = seen.iter().find(|s| s.0 == url).unwrap();
        assert_eq!(glm.2.as_deref(), Some("zai-0123456789abcdef.SECRETKEY"), "no Bearer prefix, as the official plugin sends it");
        drop(seen);
        store.delete_glm_key().unwrap();
        assert!(!glm_key_path(dir.path()).exists());
    }

    #[test]
    fn activity_from_runs() {
        let runs = json!({"runs":[
            {"provider":"claude","status":"running","finished_at":null},
            {"provider":"codex","status":"succeeded","finished_at":"2026-11-07T07:00:00+00:00"},
            {"provider":"codex","status":"failed","finished_at":"2026-11-07T07:30:00+00:00"}]});
        let a = activity_of(&runs);
        assert_eq!(a["claude"], (true, None));
        assert_eq!(a["codex"], (false, Some("2026-11-07T07:30:00+00:00".into())));
    }
}

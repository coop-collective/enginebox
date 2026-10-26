//! Additive operator telemetry. Sidecars keep legacy run records rollback-readable.
use anyhow::{Result, ensure};
use serde_json::{Value, json};

pub fn task_preview(prompt: &str) -> String {
    prompt
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .take(240)
        .collect()
}

/// Principal recorded for runs started with the controller owner credential.
pub const OWNER: &str = "owner";
const SERVICE_TOKEN: &str = "service-token:";

/// `owner`, or `service-token:` followed by a 32-character lowercase hex token ID.
pub fn validate_principal(principal: &str) -> Result<()> {
    ensure!(
        principal == OWNER
            || principal.strip_prefix(SERVICE_TOKEN).is_some_and(|id| {
                id.len() == 32
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            }),
        "Invalid native run principal."
    );
    Ok(())
}

pub fn validate(detail: &Value) -> Result<()> {
    ensure!(
        detail["schema"] == 1
            && detail["task"]
                .as_str()
                .is_some_and(|s| s.chars().count() <= 240 && !s.chars().any(char::is_control))
            && detail["cancel_requested"].is_boolean()
            && detail["cancelled"].is_boolean(),
        "Invalid native run details."
    );
    // Optional: sidecars written before scoped tokens and run env omit these.
    if let Some(principal) = detail.get("started_by") {
        validate_principal(principal.as_str().unwrap_or(""))?;
    }
    if let Some(keys) = detail.get("env_keys") {
        ensure!(
            keys.as_array()
                .is_some_and(|keys| keys.len() <= crate::native::MAX_ENV_VARS
                    && keys
                        .iter()
                        .all(|k| k.as_str().is_some_and(crate::native::env_key_allowed))),
            "Invalid native run env keys."
        );
    }
    if let Some(profile) = detail.get("profile") {
        ensure!(
            profile["name"]
                .as_str()
                .is_some_and(crate::profile::valid_name)
                && profile["revision"].is_u64(),
            "Invalid native run profile."
        );
    }
    if let Some(repository) = detail.get("repository") {
        crate::repository::parse_ref(repository.as_str().unwrap_or(""))?;
    }
    if let Some(idempotency) = detail.get("idempotency") {
        ensure!(
            ["key_digest", "request_digest"]
                .iter()
                .all(|k| idempotency[*k]
                    .as_str()
                    .is_some_and(crate::profile::valid_sha256)),
            "Invalid native run idempotency record."
        );
    }
    if let Some(callback) = detail.get("callback") {
        ensure!(
            callback["origin"]
                .as_str()
                .is_some_and(|o| o.len() <= 2048 && o.starts_with("https://"))
                && ["pending", "delivering", "delivered", "failed"]
                    .contains(&callback["state"].as_str().unwrap_or(""))
                && callback["attempts"].is_u64()
                && (callback["last_status"].is_null() || callback["last_status"].is_u64()),
            "Invalid native run callback record."
        );
    }
    if let Some(failure) = detail.get("failure") {
        ensure!(
            failure["stage"]
                .as_str()
                .is_some_and(|s| crate::launch_events::Stage::parse(s).is_some())
                && failure["reason"]
                    .as_str()
                    .is_some_and(crate::launch_events::valid_reason)
                && failure.as_object().is_some_and(|m| m.len() == 2),
            "Invalid native run failure."
        );
    }
    if !detail["started_at"].is_null() {
        chrono::DateTime::parse_from_rfc3339(
            detail["started_at"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Invalid run start time."))?,
        )?;
    }
    Ok(())
}

/// Input tokens include cache reads/writes, while cached_input_tokens reports reads.
/// Codex 0.163.0 reports cumulative session totals on resume; Claude result.usage
/// is invocation-scoped. Never silently compare those scopes or infer deltas.
pub fn token_usage(provider: &str, output: &str) -> Value {
    let mut input: Option<u64> = None;
    let mut output_count: Option<u64> = None;
    let mut cached: Option<u64> = None;
    let mut scope = "unknown";
    let events = output
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok());
    for event in events {
        if provider == "codex" && event["type"] == "turn.completed" {
            let usage = &event["usage"];
            // Each completed turn is a snapshot. Adding snapshots double-counts.
            input = usage["input_tokens"].as_u64();
            output_count = usage["output_tokens"].as_u64();
            cached = usage["cached_input_tokens"].as_u64();
            scope = "session_total";
        } else if provider == "claude" && event["type"] == "result" {
            let usage = &event["usage"];
            input = sum(&[
                usage["input_tokens"].as_u64(),
                usage["cache_read_input_tokens"].as_u64(),
                usage["cache_creation_input_tokens"].as_u64(),
            ]);
            output_count = usage["output_tokens"].as_u64();
            cached = usage["cache_read_input_tokens"].as_u64();
            scope = "run";
            // modelUsage is session-cumulative on resume, so it is not a fallback
            // for a missing invocation result.usage.
        }
        // Cursor's published terminal result has no token counters. Do not infer
        // usage from duration, transcript size or similarly named arbitrary fields.
    }
    if input.is_none() && output_count.is_none() && cached.is_none() {
        scope = "unknown";
    }
    json!({"input_tokens":input,"output_tokens":output_count,"cached_input_tokens":cached,"scope":scope})
}
fn sum(values: &[Option<u64>]) -> Option<u64> {
    values
        .iter()
        .try_fold(0_u64, |total, value| total.checked_add((*value)?))
}

pub fn enrich(record: &Value, detail: Option<&Value>, now: chrono::DateTime<chrono::Utc>) -> Value {
    let mut value = record.clone();
    let start = detail.and_then(|d| d["started_at"].as_str());
    value["task"] = detail.map(|d| d["task"].clone()).unwrap_or(Value::Null);
    value["started_at"] = json!(start);
    value["cancel_requested"] = json!(detail.is_some_and(|d| d["cancel_requested"] == true));
    // Legacy runs predate principals; only the owner credential existed then.
    value["started_by"] = detail
        .and_then(|d| d.get("started_by").cloned())
        .unwrap_or_else(|| json!(OWNER));
    value["env_keys"] = detail
        .and_then(|d| d.get("env_keys").cloned())
        .unwrap_or_else(|| json!([]));
    value["profile"] = detail
        .and_then(|d| d.get("profile").cloned())
        .unwrap_or(Value::Null);
    value["callback"] = detail
        .and_then(|d| d.get("callback").cloned())
        .unwrap_or(Value::Null);
    // `<project>/<name>` only; a repository token never reaches a record.
    value["repository"] = detail
        .and_then(|d| d.get("repository").cloned())
        .unwrap_or(Value::Null);
    // Why a launch stopped: `{stage, reason}` codes, null otherwise.
    value["failure"] = detail
        .and_then(|d| d.get("failure").cloned())
        .unwrap_or(Value::Null);
    if detail.is_some_and(|d| d["cancelled"] == true) {
        value["status"] = json!("cancelled");
    }
    let terminal = !["queued", "running"].contains(&value["status"].as_str().unwrap_or(""));
    let end = record["finished_at"]
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&chrono::Utc));
    let end = end.or_else(|| (!terminal).then_some(now));
    let elapsed = start
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .zip(end)
        .map(|(start, end)| {
            (end.signed_duration_since(start).num_milliseconds().max(0) as f64) / 1000.0
        });
    value["elapsed_seconds"] = json!(elapsed);
    value["usage"] = token_usage(
        record["provider"].as_str().unwrap_or(""),
        record["result"]["output"].as_str().unwrap_or(""),
    );
    value
}

pub fn summary(record: &Value, detail: Option<&Value>) -> Value {
    let value = enrich(record, detail, chrono::Utc::now());
    let mut out = serde_json::Map::new();
    for key in [
        "id",
        "host_id",
        "name",
        "provider",
        "scope",
        "state_scope",
        "status",
        "task",
        "created_at",
        "started_at",
        "finished_at",
        "elapsed_seconds",
        "usage",
        "native_session_id",
        "cancel_requested",
        "execution_uncertain",
        "started_by",
        "env_keys",
        "profile",
        "repository",
        "failure",
    ] {
        out.insert(key.into(), value[key].clone());
    }
    Value::Object(out)
}

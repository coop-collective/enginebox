//! GLM via ZCode (Z.ai Coding Plan). No inference proxy: Engine Box only
//! locates the official app, checks a local coding-plan identity key, and
//! launches `zcode.cjs` with stream-json.
use crate::security;
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub const APP_ENV: &str = "ENGINEBOX_ZCODE_APP";
pub const DATA_DIR_ENV: &str = "ZCODE_DATA_BASE_DIR";
pub const BUILTIN_CONFIG_ENV: &str = "ZCODE_BUILTIN_PROVIDER_CONFIG_FILE";
pub const PERSONAL_CONFIG_ENV: &str = "ZCODE_PERSONAL_PROVIDER_CONFIG_FILE";
const IDENTITY_KEYS: &[&str] = &[
    "account-provider:account:zai-individual-coding-plan:identity",
    "account-provider:account:bigmodel-individual-coding-plan:identity",
];
const STRIP_PREFIXES: &[&str] = &["ANTHROPIC_", "CLAUDE_CODE_", "GLM_"];

pub fn unavailable_message() -> &'static str {
    "GLM (Z.ai Coding Plan) is unavailable on this host because ZCode.app was not found. Install ZCode on macOS or set ENGINEBOX_ZCODE_APP."
}
pub fn login_message() -> &'static str {
    "GLM is not signed in. Run zcode login for the Z.ai Coding Plan, then retry."
}

fn app_layout(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
    (
        root.join("Contents/MacOS/ZCode"),
        root.join("Contents/Resources/glm/zcode.cjs"),
        root.join("Contents/Resources/config/provider/zcode-builtin.json"),
    )
}

pub fn is_zcode_app(root: &Path) -> bool {
    let (bin, script, builtin) = app_layout(root);
    bin.is_file() && script.is_file() && builtin.is_file()
}

/// Locate ZCode.app: `ENGINEBOX_ZCODE_APP`, then `/Applications`, then `~/Applications`.
pub fn locate_app() -> Result<PathBuf> {
    if let Ok(path) = std::env::var(APP_ENV) {
        let path = PathBuf::from(path);
        ensure!(
            path.is_absolute() && is_zcode_app(&path),
            "ENGINEBOX_ZCODE_APP must be an absolute ZCode.app bundle."
        );
        return Ok(path);
    }
    let mut candidates = vec![PathBuf::from("/Applications/ZCode.app")];
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join("Applications/ZCode.app"));
    }
    candidates
        .into_iter()
        .find(|p| is_zcode_app(p))
        .ok_or_else(|| anyhow::Error::msg(unavailable_message()))
}

pub fn credentials_path(home: &Path) -> PathBuf {
    if let Ok(dir) = std::env::var(DATA_DIR_ENV) {
        return PathBuf::from(dir).join("v2/credentials.json");
    }
    home.join(".zcode/v2/credentials.json")
}

/// Readiness is the presence of a coding-plan identity key. Values are never inspected.
pub fn coding_plan_ready(credentials: &Value) -> bool {
    credentials
        .as_object()
        .is_some_and(|map| IDENTITY_KEYS.iter().any(|k| map.contains_key(*k)))
}

pub fn require_ready(home: &Path) -> Result<()> {
    let path = credentials_path(home);
    let relative = path
        .strip_prefix(home)
        .ok()
        .and_then(|p| p.to_str())
        .unwrap_or("");
    let value = if !relative.is_empty() && path.starts_with(home) {
        security::read_json(home, relative)?.unwrap_or(Value::Null)
    } else {
        let bytes = std::fs::read(&path).unwrap_or_default();
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    ensure!(coding_plan_ready(&value), "{}", login_message());
    Ok(())
}

pub fn preflight(home: &Path) -> Result<PathBuf> {
    let app = locate_app()?;
    require_ready(home)?;
    Ok(app)
}

pub fn strip_provider_env(env: &mut BTreeMap<String, String>) {
    env.retain(|key, _| !STRIP_PREFIXES.iter().any(|prefix| key.starts_with(prefix)));
}

pub fn personal_config(model: &str) -> Value {
    json!({"model": model})
}

pub fn write_personal_config(home: &Path, model: &str) -> Result<PathBuf> {
    let relative = ".local/state/enginebox/agent-runs/glm-personal-provider.json";
    let body = serde_json::to_vec(&personal_config(model))?;
    ensure!(
        !body.windows(3).any(|w| w == b"key")
            && !serde_json::from_slice::<Value>(&body)?
                .as_object()
                .is_some_and(|m| m.keys().any(|k| k.contains("key") || k.contains("token"))),
        "GLM personal provider config must not contain secrets."
    );
    security::write_bytes(home, relative, &body, 4096)?;
    Ok(home.join(relative))
}

pub fn command(app: &Path, request: &crate::native::RunRequest, cwd: &Path) -> Result<Vec<String>> {
    let (bin, script, _) = app_layout(app);
    let mut args = vec![
        bin.to_string_lossy().into_owned(),
        script.to_string_lossy().into_owned(),
        format!("--prompt={}", request.prompt),
        "--output-format".into(),
        "stream-json".into(),
        "--cwd".into(),
        cwd.to_string_lossy().into_owned(),
        "--mode".into(),
        "yolo".into(),
    ];
    if let Some(id) = &request.resume_id {
        args.extend(["--resume".into(), id.clone()]);
    }
    Ok(args)
}

pub fn apply_run_env(
    env: &mut BTreeMap<String, String>,
    app: &Path,
    home: &Path,
    model: Option<&str>,
) -> Result<()> {
    strip_provider_env(env);
    let (_, _, builtin) = app_layout(app);
    env.insert("ELECTRON_RUN_AS_NODE".into(), "1".into());
    env.insert(
        BUILTIN_CONFIG_ENV.into(),
        builtin.to_string_lossy().into_owned(),
    );
    env.insert(
        DATA_DIR_ENV.into(),
        home.join(".zcode").to_string_lossy().into_owned(),
    );
    if let Some(model) = model {
        let path = write_personal_config(home, model)?;
        env.insert(
            PERSONAL_CONFIG_ENV.into(),
            path.to_string_lossy().into_owned(),
        );
    } else {
        env.remove(PERSONAL_CONFIG_ENV);
    }
    Ok(())
}

pub fn status(home: &Path, installed: bool) -> Value {
    let mut v = crate::auth::provider_status("glm", installed);
    if !installed {
        v["detail"] = json!(unavailable_message());
        return v;
    }
    match require_ready(home) {
        Ok(()) => {
            v["status"] = json!("connected");
            v["auth_method"] = json!("subscription");
        }
        Err(_) => {
            v["status"] = json!("disconnected");
            v["auth_method"] = json!("none");
            v["detail"] = json!(login_message());
        }
    }
    v
}

fn event_text(event: &Value) -> Option<&str> {
    event["delta"]["text"]
        .as_str()
        .or_else(|| event["text"].as_str())
        .or_else(|| event["part"]["text"].as_str())
}

/// Native assistant text from ZCode stream-json: `part.delta` chunks, else `result`.
pub fn assistant_text(output: &str) -> String {
    let mut chunks = Vec::new();
    let mut final_text = None;
    for event in output
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
    {
        match event["type"].as_str() {
            Some("part.delta") => {
                if let Some(text) = event_text(&event) {
                    chunks.push(text.to_owned());
                }
            }
            Some("result") => {
                if let Some(text) = event["result"].as_str() {
                    final_text = Some(text.to_owned());
                }
            }
            _ => {}
        }
    }
    if chunks.is_empty() {
        final_text.unwrap_or_default()
    } else {
        chunks.join("")
    }
}

pub fn session_id(output: &str) -> Option<String> {
    output
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find_map(|event| {
            let id = if event["type"] == "session.created" {
                event["session_id"]
                    .as_str()
                    .or_else(|| event["sessionId"].as_str())
            } else if event["type"] == "result" {
                event["session_id"].as_str()
            } else {
                None
            };
            id.filter(|s| enginebox_core::providers::glm_session_id(s))
                .map(str::to_owned)
        })
}

pub fn failed(output: &str) -> bool {
    output.lines().any(|line| {
        serde_json::from_str::<Value>(line)
            .ok()
            .is_some_and(|event| event["type"] == "turn.failed")
    })
}

/// Serializes tests that mutate `ENGINEBOX_ZCODE_APP`.
pub fn tests_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native;
    use serde_json::json;

    fn bundle(root: &Path) -> PathBuf {
        let app = root.join("ZCode.app");
        for relative in [
            "Contents/MacOS/ZCode",
            "Contents/Resources/glm/zcode.cjs",
            "Contents/Resources/config/provider/zcode-builtin.json",
        ] {
            let path = app.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"fixture").unwrap();
        }
        app
    }

    #[test]
    fn missing_bundle_is_unavailable() {
        let _guard = super::tests_env_lock();
        let previous = std::env::var_os(APP_ENV);
        unsafe {
            std::env::remove_var(APP_ENV);
        }
        let err = locate_app().unwrap_err().to_string();
        if previous.is_none() {
            assert!(err.contains("ZCode.app"));
        }
        if let Some(value) = previous {
            unsafe {
                std::env::set_var(APP_ENV, value);
            }
        }
    }

    #[test]
    fn identity_keys_are_names_only() {
        assert!(!coding_plan_ready(&json!({})));
        assert!(coding_plan_ready(
            &json!({"account-provider:account:zai-individual-coding-plan:identity":{"secret":"do-not-read"}})
        ));
        assert!(coding_plan_ready(
            &json!({"account-provider:account:bigmodel-individual-coding-plan:identity": true})
        ));
        assert!(!coding_plan_ready(
            &json!({"account-provider:account:zai-individual-coding-plan":{"identity":"nope"}})
        ));
    }

    #[test]
    fn child_env_strips_competing_provider_variables() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        crate::security::private_dir(home).unwrap();
        let app = bundle(home);
        let mut env = BTreeMap::from([
            ("ANTHROPIC_API_KEY".into(), "sk-ant".into()),
            ("ANTHROPIC_BASE_URL".into(), "https://example.test".into()),
            ("CLAUDE_CODE_OAUTH_TOKEN".into(), "oauth".into()),
            ("GLM_API_KEY".into(), "glm-secret".into()),
            ("KEEP".into(), "ok".into()),
        ]);
        apply_run_env(&mut env, &app, home, Some("glm-4.7")).unwrap();
        assert!(!env.contains_key("ANTHROPIC_API_KEY"));
        assert!(!env.contains_key("ANTHROPIC_BASE_URL"));
        assert!(!env.contains_key("CLAUDE_CODE_OAUTH_TOKEN"));
        assert!(!env.contains_key("GLM_API_KEY"));
        assert_eq!(env["KEEP"], "ok");
        assert_eq!(env["ELECTRON_RUN_AS_NODE"], "1");
        assert!(env[BUILTIN_CONFIG_ENV].ends_with("zcode-builtin.json"));
        let personal = PathBuf::from(&env[PERSONAL_CONFIG_ENV]);
        let body = std::fs::read_to_string(&personal).unwrap();
        assert_eq!(body, json!({"model":"glm-4.7"}).to_string());
        assert!(!body.to_ascii_lowercase().contains("secret"));
        assert!(!body.contains("key"));
    }

    #[test]
    fn launch_matches_zcode_stream_json_contract() {
        let dir = tempfile::tempdir().unwrap();
        let app = bundle(dir.path());
        let request = native::RunRequest::parse(json!({
            "provider":"glm",
            "prompt":"hello",
            "resume_id":"sess_abc123",
            "model":"glm-4.7"
        }))
        .unwrap();
        let cwd = dir.path();
        let args = command(&app, &request, cwd).unwrap();
        assert_eq!(args[0], app.join("Contents/MacOS/ZCode").to_string_lossy());
        assert_eq!(
            args[1],
            app.join("Contents/Resources/glm/zcode.cjs")
                .to_string_lossy()
        );
        assert_eq!(args[2], "--prompt=hello");
        assert!(
            args.windows(2)
                .any(|w| w == ["--output-format", "stream-json"])
        );
        assert!(
            args.windows(2)
                .any(|w| w == ["--cwd", cwd.to_str().unwrap()])
        );
        assert!(args.windows(2).any(|w| w == ["--mode", "yolo"]));
        assert!(args.windows(2).any(|w| w == ["--resume", "sess_abc123"]));
        assert!(!args.iter().any(|a| a == "--model" || a == "glm-4.7"));
    }

    #[test]
    fn events_map_to_text_and_session() {
        let output = concat!(
            r#"{"type":"session.created","session_id":"sess_abc"}"#,
            "\n",
            r#"{"type":"part.delta","delta":{"text":"Hel"}}"#,
            "\n",
            r#"{"type":"part.delta","text":"lo"}"#,
            "\n",
            r#"{"type":"turn.completed","usage":{"input_tokens":3,"output_tokens":2}}"#,
            "\n",
            r#"{"type":"result","result":"Hello","session_id":"sess_abc"}"#,
            "\n"
        );
        assert_eq!(assistant_text(output), "Hello");
        assert_eq!(session_id(output).as_deref(), Some("sess_abc"));
        assert!(!failed(output));
        assert!(failed(
            r#"{"type":"turn.failed","error":"provider"}
{"type":"result","result":""}"#
        ));
    }
}

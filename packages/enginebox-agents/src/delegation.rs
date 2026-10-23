use crate::{
    native,
    security::{self, token},
};
use anyhow::{Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
pub use enginebox_core::native_versions::{CODEX_VERSION, CURSOR_VERSION};
use serde_json::{Value, json};
use std::{fs, path::Path};
pub const TOKEN_FILE: &str = ".config/enginebox/controller-token.json";
pub fn native_file(provider: &str) -> &str {
    match provider {
        "codex" => ".codex/auth.json",
        "claude" => ".claude/.credentials.json",
        _ => ".config/cursor/auth.json",
    }
}
pub fn expiry(token: &Value) -> Option<f64> {
    let token = token.as_str()?;
    let part = token.split('.').nth(1)?;
    let v: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).ok()?).ok()?;
    v["exp"].as_f64().filter(|n| *n > 0. && *n < 253402300799.)
}
pub fn material(provider: &str, v: &Value) -> Result<Value> {
    crate::provider(provider)?;
    ensure!(
        v.is_object(),
        "No reusable native credentials are available."
    );
    Ok(match provider {
        "codex" => {
            let t = &v["tokens"];
            ensure!(
                t.is_object()
                    && ["chatgpt", "chatgptAuthTokens"]
                        .contains(&v["auth_mode"].as_str().unwrap_or("chatgpt"))
                    && v["OPENAI_API_KEY"].as_str().is_none_or(str::is_empty)
                    && token(&t["access_token"])
                    && token(&t["id_token"])
                    && t["account_id"].as_str().is_some_and(|s| !s.is_empty()
                        && s.len() <= 256
                        && !s.chars().any(char::is_control)),
                "Native ChatGPT connection required for delegation."
            );
            json!({"kind":"native","content":{"auth_mode":"chatgptAuthTokens","OPENAI_API_KEY":null,"tokens":{"id_token":t["id_token"],"access_token":t["access_token"],"account_id":t["account_id"],"refresh_token":""},"last_refresh":v["last_refresh"].as_str().map(str::to_owned).unwrap_or_else(crate::now)},"expires_at":expiry(&t["access_token"])})
        }
        "claude" => {
            let t = &v["claudeAiOauth"];
            ensure!(
                token(&t["accessToken"])
                    && t["expiresAt"]
                        .as_f64()
                        .is_some_and(|n| n > 0. && n < 253402300799000.),
                "Native Claude subscription connection with expiration required."
            );
            json!({"kind":"environment","content":{"CLAUDE_CODE_OAUTH_TOKEN":t["accessToken"]},"expires_at":t["expiresAt"].as_f64().unwrap()/1000.})
        }
        _ => {
            ensure!(
                token(&v["accessToken"]),
                "Native Cursor connection required."
            );
            json!({"kind":"native","content":{"accessToken":v["accessToken"],"refreshToken":v["accessToken"]},"expires_at":expiry(&v["accessToken"])})
        }
    })
}
pub fn attachment(provider: &str, material: &Value, retire_refresh: bool) -> Value {
    let mut v = material.clone();
    v["operation"] = json!("attach");
    v["provider"] = json!(provider);
    v["generation"] = json!(security::legacy_digest(material));
    v["expected_version"] = if material["kind"] == "native" {
        match provider {
            "codex" => json!(CODEX_VERSION),
            "cursor" => json!(CURSOR_VERSION),
            _ => Value::Null,
        }
    } else {
        Value::Null
    };
    v["retire_refresh"] = json!(retire_refresh);
    v
}
pub fn identity(paths: &[&Path], owner: u32) -> Result<String> {
    #[cfg(not(unix))]
    let _ = owner;
    let mut ids = Vec::new();
    let identity_pattern = regex::Regex::new(r"^[0-9a-f]{32}$")?;
    for path in paths {
        security::check_components(path)?;
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(path)?;
        let m = file.metadata()?;
        ensure!(m.is_file() && m.len() <= 128, "Invalid workspace identity.");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            ensure!(
                m.uid() == owner && m.mode() & 0o022 == 0,
                "Unsafe workspace identity."
            );
        }
        use std::io::Read;
        let mut raw = String::new();
        file.by_ref().take(129).read_to_string(&mut raw)?;
        ensure!(raw.len() <= 128, "Invalid workspace identity.");
        let v = raw.trim().to_lowercase();
        ensure!(
            identity_pattern.is_match(&v) && v != "0".repeat(32),
            "Invalid workspace identity."
        );
        ids.push(v);
    }
    ensure!(
        !ids.is_empty() && ids.iter().all(|x| x == &ids[0]),
        "Workspace identity mismatch."
    );
    Ok(ids[0].clone())
}
fn current_content(home: &Path, p: &str, kind: &str) -> Result<Value> {
    if p == "claude" {
        let v = security::read_json(home, ".claude/settings.json")?.unwrap_or(json!({}));
        return Ok(json!({"CLAUDE_CODE_OAUTH_TOKEN":v["env"]["CLAUDE_CODE_OAUTH_TOKEN"]}));
    }
    if p == "cursor" && kind == "environment" {
        return Ok(json!({"CURSOR_API_KEY":native::environment(home)?.get("CURSOR_API_KEY")}));
    }
    Ok(security::read_json(home, native_file(p))?.unwrap_or(json!({})))
}
pub fn transfer(home: &Path, request: &Value, instance_id: &str) -> Result<Value> {
    let p = request["provider"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing provider."))?;
    crate::provider(p)?;
    security::absolute_existing(home)?;
    if request["operation"] == "export" {
        return Ok(
            json!({"ok":true,"native":security::read_json(home,native_file(p))?.ok_or_else(||anyhow::anyhow!("No native credentials available."))?,"instance_id":instance_id}),
        );
    }
    ensure!(
        request["operation"] == "attach",
        "Unknown transfer operation."
    );
    let generation = request["generation"].as_str().unwrap_or("");
    ensure!(
        regex::Regex::new(r"^[a-f0-9]{64}$")?.is_match(generation),
        "Invalid generation."
    );
    if let Some(expected) = request["expected_instance_id"].as_str() {
        ensure!(
            expected.replace('-', "") == instance_id,
            "Workspace identity changed."
        );
    }
    let marker_path = format!(".config/enginebox/agents/{p}.json");
    let marker = security::read_json(home, &marker_path)?.unwrap_or(json!({}));
    let retire = request["retire_refresh"] == true;
    let existing = security::read_json(home, native_file(p))?.unwrap_or(json!({}));
    let settings = if p == "claude" {
        security::read_json(home, ".claude/settings.json")?.unwrap_or(json!({}))
    } else {
        json!({})
    };
    if !retire {
        if marker.as_object().is_none_or(|m| m.is_empty()) {
            let has = match p {
                "codex" => !existing["tokens"].is_null() || token(&existing["OPENAI_API_KEY"]),
                "claude" => !existing["claudeAiOauth"].is_null(),
                _ => token(&existing["accessToken"]) || token(&existing["refreshToken"]),
            };
            ensure!(
                !has,
                "Workspace has native credentials; adopt them explicitly."
            );
        }
        if p == "claude" {
            ensure!(
                existing["claudeAiOauth"].is_null()
                    && settings["env"]["ANTHROPIC_API_KEY"].is_null()
                    && settings["env"]["ANTHROPIC_AUTH_TOKEN"].is_null(),
                "Workspace provider override requires explicit adoption."
            );
            if marker.as_object().is_none_or(|m| m.is_empty()) {
                ensure!(
                    settings["env"]["CLAUDE_CODE_OAUTH_TOKEN"].is_null(),
                    "Workspace provider override requires explicit adoption."
                );
            }
        }
        if let Some(hash) = marker["content_hash"].as_str() {
            let current = current_content(home, p, marker["kind"].as_str().unwrap_or(""))?;
            ensure!(
                hash == security::legacy_digest(&current) || hash == security::digest(&current),
                "Workspace credentials changed; adopt explicitly."
            );
        }
    }
    if marker["generation"] == generation {
        return Ok(json!({"ok":true,"unchanged":true,"instance_id":instance_id}));
    }
    if let Some(expected) = request["expected_version"].as_str() {
        let env = native::environment(home)?;
        let exe = native::find_binary(native::binary(p), &env)
            .ok_or_else(|| anyhow::anyhow!("Native provider unavailable."))?;
        let actual = native::verify_client(&exe, p, &env, home)?;
        ensure!(
            actual.split_whitespace().any(|part| part == expected),
            "Native access format is not qualified for this client version."
        );
    }
    let content = &request["content"];
    ensure!(content.is_object(), "Invalid delegated content.");
    match p {
        "claude" => {
            ensure!(
                content.as_object().unwrap().len() == 1
                    && token(&content["CLAUDE_CODE_OAUTH_TOKEN"]),
                "Invalid delegated content."
            );
            let mut settings = settings;
            ensure!(
                settings["env"].is_null() || settings["env"].is_object(),
                "Invalid Claude settings."
            );
            if settings["env"].is_null() {
                settings["env"] = json!({});
            }
            settings["env"]["CLAUDE_CODE_OAUTH_TOKEN"] = content["CLAUDE_CODE_OAUTH_TOKEN"].clone();
            security::write_json(home, ".claude/settings.json", &settings)?;
            if retire {
                security::remove(home, native_file(p))?;
            }
        }
        "cursor" if request["kind"] == "environment" => {
            ensure!(
                content.as_object().unwrap().len() == 1 && token(&content["CURSOR_API_KEY"]),
                "Invalid delegated content."
            );
            let token = content["CURSOR_API_KEY"]
                .as_str()
                .unwrap()
                .replace('\'', "'\"'\"'");
            let relative = ".config/enginebox/agents/cursor.env";
            security::write_bytes(
                home,
                relative,
                format!("export CURSOR_API_KEY='{token}'\n").as_bytes(),
                65536,
            )?;
            let line = format!(
                "\n# Engine Box native Cursor credentials\n. '{}'\n",
                home.join(relative)
                    .to_string_lossy()
                    .replace('\'', "'\"'\"'")
            );
            for file in [".profile", ".bashrc"] {
                let mut prior = String::from_utf8(
                    security::read_bytes(home, file, 65536, false)?.unwrap_or_default(),
                )?;
                if !prior.contains(&line) {
                    prior.push_str(&line);
                    security::write_bytes(home, file, prior.as_bytes(), 65536)?;
                }
            }
        }
        "codex" => {
            ensure!(
                content["tokens"]["refresh_token"]
                    .as_str()
                    .is_none_or(str::is_empty),
                "Refresh credentials must remain on controller."
            );
            security::write_json(home, native_file(p), content)?;
        }
        "cursor" => {
            ensure!(
                token(&content["accessToken"]) && content["refreshToken"] == content["accessToken"],
                "Refresh credentials must remain on controller."
            );
            security::write_json(home, native_file(p), content)?;
        }
        _ => bail!("Unknown provider."),
    }
    security::write_json(
        home,
        &marker_path,
        &json!({"generation":generation,"expires_at":request["expires_at"],"refresh_credentials":false,"kind":request["kind"],"content_hash":security::legacy_digest(content)}),
    )?;
    Ok(json!({"ok":true,"instance_id":instance_id}))
}

//! Workspace profiles: validation, argv translation per provider, the
//! no-profile argv staying exactly as before, and bootstrap isolation.
use base64::Engine;
use enginebox_agents::{
    native,
    profile::{self, Envelope, Profile, ProviderProfile},
    security,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

const RUN: &str = "0123456789abcdef0123456789abcdef";
const SKILL: &[u8] = b"---\nname: review\ndescription: Review.\n---\nRead.\n";
const PLUGIN: &[u8] = b"{\"name\":\"lint\",\"version\":\"1.0.0\"}";
const HOOK: &[u8] = b"#!/bin/sh\nexit 0\n";

fn sha(b: &[u8]) -> String {
    profile::sha256_hex(b)
}
fn block(v: Value) -> anyhow::Result<ProviderProfile> {
    Ok(serde_json::from_value(v)?)
}
fn stored(sha: &str) -> bool {
    [SKILL, PLUGIN, HOOK]
        .iter()
        .any(|b| profile::sha256_hex(b) == sha)
}
fn check(provider: &str, v: Value) -> anyhow::Result<()> {
    block(v)?.validate(provider, &stored)
}
fn request(provider: &str) -> native::RunRequest {
    native::RunRequest::parse(json!({"provider":provider,"prompt":"hello"})).unwrap()
}
fn temp() -> (tempfile::TempDir, std::path::PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().canonicalize().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    (t, p)
}
fn files() -> BTreeMap<String, Vec<u8>> {
    [SKILL, PLUGIN, HOOK]
        .iter()
        .map(|b| (sha(b), b.to_vec()))
        .collect()
}
fn tree(root: &Path) -> BTreeSet<String> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeSet<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            // '/'-separated on every platform, so prefix checks hold on Windows too
            out.insert(
                path.strip_prefix(base)
                    .unwrap()
                    .display()
                    .to_string()
                    .replace('\\', "/"),
            );
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                walk(base, &path, out);
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(root, root, &mut out);
    out
}

#[test]
fn validation_refuses_bypass_flags_bad_hashes_paths_and_bounds() {
    let skill = json!([{"name":"review","files":[{"path":"SKILL.md","sha256":sha(SKILL)}]}]);
    for (provider, bad) in [
        ("claude", json!({"max_turns":0})),
        ("claude", json!({"max_turns":profile::MAX_TURNS + 1})),
        ("claude", json!({"permission_profile":"bypassPermissions"})),
        ("claude", json!({"permission_profile":"yolo"})),
        (
            "claude",
            json!({"allowed_tools":["--dangerously-skip-permissions"]}),
        ),
        ("claude", json!({"allowed_tools":["DangerouslySkip"]})),
        ("claude", json!({"allowed_tools":["BypassPermissions"]})),
        ("claude", json!({"allowed_tools":["Read","Read"]})),
        ("claude", json!({"allowed_tools":[]})),
        ("claude", json!({"allowed_tools":["Read Write"]})),
        ("claude", json!({"flags":["--yolo"]})),
        (
            "claude",
            json!({"extra_args":["--dangerously-skip-permissions"]}),
        ),
        (
            "claude",
            json!({"permission_profile":"read_only","allowed_tools":["Write"]}),
        ),
        (
            "claude",
            json!({"permission_profile":"read_only","allowed_tools":["Bash(git status)"]}),
        ),
        ("claude", json!({"setting_sources":["user","user"]})),
        ("claude", json!({"setting_sources":["policy"]})),
        ("claude", json!({"setting_sources":[]})),
        (
            "claude",
            json!({"skills":[{"name":"s","files":[{"path":"SKILL.md","sha256":"0".repeat(64)}]}]}),
        ),
        (
            "claude",
            json!({"skills":[{"name":"s","files":[{"path":"SKILL.md","sha256":sha(SKILL).to_uppercase()}]}]}),
        ),
        (
            "claude",
            json!({"skills":[{"name":"s","files":[{"path":"SKILL.md","sha256":"abc"}]}]}),
        ),
        (
            "claude",
            json!({"skills":[{"name":"s","files":[{"path":"README.md","sha256":sha(SKILL)}]}]}),
        ),
        (
            "claude",
            json!({"skills":[{"name":"s","files":[{"path":"../SKILL.md","sha256":sha(SKILL)}]}]}),
        ),
        (
            "claude",
            json!({"skills":[{"name":"s","files":[{"path":"/etc/SKILL.md","sha256":sha(SKILL)}]}]}),
        ),
        (
            "claude",
            json!({"skills":[{"name":"s","files":[{"path":"a/./SKILL.md","sha256":sha(SKILL)}]}]}),
        ),
        (
            "claude",
            json!({"skills":[{"name":"../s","files":[{"path":"SKILL.md","sha256":sha(SKILL)}]}]}),
        ),
        (
            "claude",
            json!({"skills":[{"name":"s","url":"https://example.com/s.tgz","files":[]}]}),
        ),
        (
            "claude",
            json!({"plugins":[{"name":"p","files":[{"path":"a","sha256":sha(PLUGIN)},{"path":"a/b","sha256":sha(HOOK)}]}]}),
        ),
        ("claude", json!({"plugins":[{"name":"p","files":[]}]})),
        (
            "claude",
            json!({"mcp_servers":[{"name":"x","url":"http://example.com/mcp"}]}),
        ),
        (
            "claude",
            json!({"mcp_servers":[{"name":"x","url":"https://u:p@example.com/mcp"}]}),
        ),
        (
            "claude",
            json!({"mcp_servers":[{"name":"x","command":"-rf"}]}),
        ),
        (
            "claude",
            json!({"mcp_servers":[{"name":"x","command":"node","url":"https://example.com"}]}),
        ),
        ("claude", json!({"mcp_servers":[{"name":"x"}]})),
        (
            "claude",
            json!({"mcp_servers":[{"name":"enginebox-browser","command":"node"}]}),
        ),
        (
            "claude",
            json!({"mcp_servers":[{"name":"x","url":"https://example.com","headers_from_env":{"Authorization":"CLAUDE_CODE_OAUTH_TOKEN"}}]}),
        ),
        (
            "claude",
            json!({"mcp_servers":[{"name":"x","command":"node","env_vars":["OPENAI_API_KEY"]}]}),
        ),
        ("claude", json!({"env":{"PATH":"/tmp"}})),
        (
            "claude",
            json!({"env":{"ANTHROPIC_BASE_URL":"https://proxy"}}),
        ),
        ("codex", json!({"max_turns":50})),
        ("codex", json!({"allowed_tools":["Read"]})),
        ("codex", json!({"setting_sources":["project"]})),
        (
            "codex",
            json!({"plugins":[{"name":"p","files":[{"path":"plugin.json","sha256":sha(PLUGIN)}]}]}),
        ),
        ("codex", json!({"env":{"CODEX_HOME":"/tmp/x"}})),
        ("cursor", json!({"skills":skill})),
        (
            "cursor",
            json!({"mcp_servers":[{"name":"x","command":"node"}]}),
        ),
        ("cursor", json!({"strict_mcp":true})),
        ("cursor", json!({"max_turns":5})),
        ("cursor", json!({"allowed_tools":["Read"]})),
    ] {
        assert!(
            check(provider, bad.clone()).is_err(),
            "{provider} {bad} accepted"
        );
    }
    let too_many: serde_json::Map<_, _> = (0..=native::MAX_ENV_VARS)
        .map(|i| (format!("VAR_{i}"), json!("v")))
        .collect();
    assert!(check("claude", json!({ "env": too_many })).is_err());
    for (provider, good) in [
        ("claude", json!({})),
        ("claude", json!({"max_turns":1})),
        (
            "claude",
            json!({"max_turns":profile::MAX_TURNS,"allowed_tools":["Read","Bash(npm test:*)","mcp__docs__*"],
            "setting_sources":["project","local"],"skills":skill,"strict_mcp":true,
            "mcp_servers":[{"name":"docs","url":"https://docs.example.com/mcp","headers_from_env":{"Authorization":"DOCS_TOKEN"}},
                           {"name":"local","command":"node","args":["server.js"],"env_vars":["LOCAL_KEY"]}],
            "plugins":[{"name":"lint","files":[{"path":".claude-plugin/plugin.json","sha256":sha(PLUGIN)},{"path":"hooks/run.sh","sha256":sha(HOOK),"executable":true}]}],
            "env":{"API_BASE":"https://api.example.com"}}),
        ),
        (
            "claude",
            json!({"permission_profile":"read_only","allowed_tools":["Read","Grep","mcp__docs__search"]}),
        ),
        (
            "claude",
            json!({"mcp_servers":[{"name":"dev","url":"http://127.0.0.1:8080/mcp"}]}),
        ),
        (
            "codex",
            json!({"permission_profile":"read_only","skills":skill,"mcp_servers":[{"name":"docs","url":"https://docs.example.com/mcp"}]}),
        ),
        (
            "cursor",
            json!({"permission_profile":"read_only","plugins":[{"name":"lint","files":[{"path":"plugin.json","sha256":sha(PLUGIN)}]}]}),
        ),
    ] {
        check(provider, good.clone()).unwrap_or_else(|e| panic!("{provider} {good}: {e}"));
    }
}

#[test]
fn profile_records_are_owner_managed_and_hide_secrets() {
    let parse = |v: Value| serde_json::from_value::<Profile>(v);
    let ok = parse(
        json!({"name":"verifier","template":"engine-godot","callback_secret":"s".repeat(40),
        "providers":{"claude":{"env":{"MODE":"value-that-is-secret"}}}}),
    )
    .unwrap();
    ok.validate(&stored).unwrap();
    assert_eq!(ok.owner, profile::OWNER);
    let public = ok.public().to_string();
    assert!(!public.contains(&"s".repeat(40)) && !public.contains("value-that-is-secret"));
    assert!(public.contains("\"env_keys\":[\"MODE\"]"));
    assert!(!format!("{ok:?}").contains("value-that-is-secret"));
    for bad in [
        json!({"name":"x","providers":{}}),
        json!({"name":"x","providers":{"gemini":{}}}),
        json!({"name":"x","owner":"tenant-b","providers":{"claude":{}}}),
        json!({"name":"x","template":"../t","providers":{"claude":{}}}),
        json!({"name":"x","callback_secret":"short","providers":{"claude":{}}}),
        json!({"name":"X","providers":{"claude":{}}}),
    ] {
        assert!(
            parse(bad.clone())
                .map_err(anyhow::Error::from)
                .and_then(|p| p.validate(&stored))
                .is_err(),
            "{bad} accepted"
        );
    }
}

#[test]
fn runs_without_a_profile_keep_todays_exact_argv() {
    let id = "adee2503-cabd-443e-9cef-0f0a5beb2f1a";
    let cases = [
        (
            json!({"provider":"claude","prompt":"hello"}),
            vec![
                "claude",
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--permission-mode",
                "acceptEdits",
                "--permission-prompts",
                "none",
                "--allowedTools",
                "Read,Write,Edit,Bash,mcp__enginebox-browser__*",
                "--max-turns",
                "20",
            ],
        ),
        (
            json!({"provider":"claude","prompt":"hello","resume_id":id,"model":"sonnet"}),
            vec![
                "claude",
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--permission-mode",
                "acceptEdits",
                "--permission-prompts",
                "none",
                "--allowedTools",
                "Read,Write,Edit,Bash,mcp__enginebox-browser__*",
                "--max-turns",
                "20",
                "--resume",
                id,
                "--model",
                "sonnet",
            ],
        ),
        (
            json!({"provider":"codex","prompt":"hello"}),
            vec![
                "codex",
                "exec",
                "--json",
                "--skip-git-repo-check",
                "-c",
                "approval_policy=\"never\"",
                "-c",
                "sandbox_mode=\"workspace-write\"",
                "-",
            ],
        ),
        (
            json!({"provider":"codex","prompt":"hello","resume_id":id,"model":"gpt-5"}),
            vec![
                "codex",
                "exec",
                "resume",
                id,
                "--json",
                "--skip-git-repo-check",
                "-c",
                "approval_policy=\"never\"",
                "-c",
                "sandbox_mode=\"workspace-write\"",
                "--model",
                "gpt-5",
                "-",
            ],
        ),
        (
            json!({"provider":"cursor","prompt":"hello"}),
            vec![
                "agent",
                "-p",
                "--output-format",
                "stream-json",
                "--trust",
                "--sandbox",
                "enabled",
                "--",
                "hello",
            ],
        ),
        (
            json!({"provider":"cursor","prompt":"hello","resume_id":id}),
            vec![
                "agent",
                "-p",
                "--output-format",
                "stream-json",
                "--trust",
                "--sandbox",
                "enabled",
                "--resume",
                id,
                "--",
                "hello",
            ],
        ),
    ];
    for (v, expected) in cases {
        let r = native::RunRequest::parse(v.clone()).unwrap();
        assert_eq!(
            native::native_command(&r, native::binary(&r.provider)),
            expected,
            "{v}"
        );
        // The guest payload for a run without a profile is unchanged too.
        let sent = serde_json::to_value(&r).unwrap();
        assert!(sent.get("profile").is_none() && sent.get("idempotency_key").is_none());
        assert!(sent.get("callback").is_none());
    }
}

fn materialized(root: &Path) -> profile::Materialized {
    profile::Materialized {
        root: root.into(),
        plugin_dirs: vec![root.join("plugins/lint"), root.join("skills-plugin")],
        mcp_config: Some(root.join("mcp.json")),
        codex_home: None,
    }
}
fn none(root: &Path) -> profile::Materialized {
    profile::Materialized {
        root: root.into(),
        plugin_dirs: vec![],
        mcp_config: None,
        codex_home: None,
    }
}

#[test]
fn a_profile_changes_argv_only_through_documented_options() {
    let root = Path::new("/home/agent/.local/state/enginebox/agent-runs/profiles/x");
    // Claude: turn limit, tools, settings, MCP, plugins; same permission prompts.
    let b = block(
        json!({"max_turns":300,"allowed_tools":["Read","Bash(npm test:*)","mcp__docs__*"],
        "setting_sources":["project","local"],"strict_mcp":true}),
    )
    .unwrap();
    let args = profile::command(&request("claude"), "claude", &b, &materialized(root)).unwrap();
    assert_eq!(
        args,
        [
            "claude",
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--permission-mode",
            "acceptEdits",
            "--permission-prompts",
            "none",
            "--allowedTools",
            "Read,Bash(npm test:*),mcp__docs__*",
            "--max-turns",
            "300",
            "--setting-sources",
            "project,local",
            "--mcp-config",
            &root.join("mcp.json").display().to_string(),
            "--strict-mcp-config",
            "--plugin-dir",
            &root.join("plugins/lint").display().to_string(),
            "--plugin-dir",
            &root.join("skills-plugin").display().to_string(),
        ]
    );
    // An empty Claude profile is today's argv.
    let empty = profile::command(
        &request("claude"),
        "claude",
        &ProviderProfile::default(),
        &none(root),
    )
    .unwrap();
    assert_eq!(empty, native::native_command(&request("claude"), "claude"));
    // Claude read_only: dontAsk with read tools only and writes denied.
    let ro = block(json!({"permission_profile":"read_only"})).unwrap();
    let args = profile::command(&request("claude"), "claude", &ro, &none(root)).unwrap();
    assert!(
        args.windows(2)
            .any(|w| w == ["--permission-mode", "dontAsk"])
    );
    assert!(
        args.windows(2)
            .any(|w| w == ["--allowedTools", "Read,Glob,Grep"])
    );
    assert!(args.windows(2).any(|w| w
        == [
            "--disallowedTools",
            "Write,Edit,MultiEdit,NotebookEdit,Bash"
        ]));
    // Codex: same argv shape; read_only switches the sandbox.
    let args = profile::command(
        &request("codex"),
        "codex",
        &ProviderProfile::default(),
        &none(root),
    )
    .unwrap();
    assert_eq!(args, native::native_command(&request("codex"), "codex"));
    let args = profile::command(&request("codex"), "codex", &ro, &none(root)).unwrap();
    assert!(args.contains(&"sandbox_mode=\"read-only\"".to_string()));
    assert!(!args.contains(&"sandbox_mode=\"workspace-write\"".to_string()));
    // Cursor: plugins and ask mode only.
    let cursor = block(json!({"permission_profile":"read_only"})).unwrap();
    let mut m = none(root);
    m.plugin_dirs = vec![root.join("plugins/lint")];
    let args = profile::command(&request("cursor"), "agent", &cursor, &m).unwrap();
    assert_eq!(
        args,
        [
            "agent",
            "-p",
            "--output-format",
            "stream-json",
            "--trust",
            "--sandbox",
            "enabled",
            "--mode",
            "ask",
            "--plugin-dir",
            &root.join("plugins/lint").display().to_string(),
            "--",
            "hello"
        ]
    );
}

#[test]
fn no_profile_ever_yields_a_bypass_flag() {
    let root = Path::new("/h/r");
    for provider in ["claude", "codex", "cursor"] {
        for permission in ["edit", "read_only"] {
            let b = block(json!({"permission_profile":permission})).unwrap();
            let args =
                profile::command(&request(provider), provider, &b, &materialized(root)).unwrap();
            assert!(
                !args.iter().any(|a| profile::forbidden_flag(a)),
                "{provider} {args:?}"
            );
            assert!(
                !args.iter().any(|a| a.contains("dangerously")
                    || a == "--yolo"
                    || a == "bypassPermissions")
            );
        }
    }
    let codex = profile::codex_config(&block(json!({"permission_profile":"edit"})).unwrap(), true);
    assert!(!codex.contains("danger-full-access") && !codex.contains("dangerously"));
}

#[test]
fn codex_config_carries_sandbox_mcp_and_disables_bundled_skills() {
    let b = block(json!({"permission_profile":"read_only","mcp_servers":[
        {"name":"docs","url":"https://docs.example.com/mcp","headers_from_env":{"Authorization":"DOCS_TOKEN"}},
        {"name":"local","command":"node","args":["s.js","--port","9\"0"],"env_vars":["LOCAL_KEY"]}]}))
    .unwrap();
    let text = profile::codex_config(&b, false);
    let config: toml::Value = toml::from_str(&text).unwrap();
    assert_eq!(config["approval_policy"].as_str(), Some("never"));
    assert_eq!(config["sandbox_mode"].as_str(), Some("read-only"));
    assert_eq!(
        config["skills"]["bundled"]["enabled"].as_bool(),
        Some(false)
    );
    for f in profile::CODEX_DISABLED_FEATURES {
        assert_eq!(config["features"][*f].as_bool(), Some(false), "{f}");
    }
    let servers = config["mcp_servers"].as_table().unwrap();
    assert_eq!(servers.len(), 2);
    assert_eq!(
        servers["docs"]["env_http_headers"]["Authorization"].as_str(),
        Some("DOCS_TOKEN")
    );
    assert_eq!(servers["local"]["args"][2].as_str(), Some("9\"0"));
    assert_eq!(servers["local"]["env_vars"][0].as_str(), Some("LOCAL_KEY"));
    assert!(!text.contains("enginebox-browser"));
    let with_browser: toml::Value = toml::from_str(&profile::codex_config(&b, true)).unwrap();
    assert!(
        with_browser["mcp_servers"]
            .get("enginebox-browser")
            .is_some()
    );
}

#[test]
fn codex_network_access_is_opt_in_and_edit_only() {
    let off: toml::Value = toml::from_str(&profile::codex_config(
        &block(json!({"permission_profile":"edit"})).unwrap(),
        false,
    ))
    .unwrap();
    assert!(off.get("sandbox_workspace_write").is_none());
    let b = block(json!({"permission_profile":"edit","network_access":true})).unwrap();
    let on: toml::Value = toml::from_str(&profile::codex_config(&b, false)).unwrap();
    assert_eq!(on["sandbox_mode"].as_str(), Some("workspace-write"));
    assert_eq!(
        on["sandbox_workspace_write"]["network_access"].as_bool(),
        Some(true)
    );
    assert!(
        check(
            "codex",
            json!({"permission_profile":"edit","network_access":true})
        )
        .is_ok()
    );
    assert!(
        check(
            "codex",
            json!({"permission_profile":"read_only","network_access":true})
        )
        .is_err()
    );
    assert!(check("claude", json!({"network_access":true})).is_err());
}

#[test]
fn bootstrap_writes_only_the_runs_private_directory_and_is_removed() {
    let (_t, home) = temp();
    security::write_bytes(
        &home,
        ".codex/auth.json",
        b"{\"tokens\":{\"access_token\":\"a\"}}",
        4096,
    )
    .unwrap();
    security::write_bytes(&home, ".claude/settings.json", b"{}", 4096).unwrap();
    std::fs::create_dir_all(home.join(".codex/sessions")).unwrap();
    security::write_bytes(&home, ".codex/config.toml",
        b"[mcp_servers.enginebox-browser]\ncommand='/usr/local/bin/enginebox-guest'\nargs=['browser','--client','codex']\n", 4096).unwrap();
    // Unrelated earlier state and another run's directory must stay untouched.
    let other = "fedcba9876543210fedcba9876543210";
    security::write_bytes(
        &home,
        &format!("{}/{other}/keep", profile::RUNS_DIR),
        b"x",
        16,
    )
    .unwrap();
    let before = tree(&home);
    let skill = json!([{"name":"review","files":[{"path":"SKILL.md","sha256":sha(SKILL)}]}]);
    let cases = [
        (
            "claude",
            json!({"skills":skill,"mcp_servers":[{"name":"docs","url":"https://docs.example.com/mcp","headers_from_env":{"Authorization":"DOCS_TOKEN"}}],
            "plugins":[{"name":"lint","files":[{"path":".claude-plugin/plugin.json","sha256":sha(PLUGIN)},{"path":"hooks/run.sh","sha256":sha(HOOK),"executable":true}]}]}),
        ),
        ("codex", json!({"skills":skill})),
        (
            "cursor",
            json!({"plugins":[{"name":"lint","files":[{"path":"plugin.json","sha256":sha(PLUGIN)}]}]}),
        ),
    ];
    let run_root = home.join(profile::RUNS_DIR).join(RUN);
    for (provider, v) in cases {
        let b = block(v).unwrap();
        b.validate(provider, &stored).unwrap();
        let m = profile::materialize(&home, RUN, provider, &b, &files()).unwrap();
        assert_eq!(m.root, run_root);
        let after = tree(&home);
        let added: Vec<_> = after.difference(&before).collect();
        let prefix = format!("{}/{RUN}", profile::RUNS_DIR);
        assert!(!added.is_empty());
        assert!(
            added
                .iter()
                .all(|p| p.starts_with(&prefix) || prefix.starts_with(p.as_str())),
            "{provider} wrote outside its run directory: {added:?}"
        );
        assert!(
            before.is_subset(&after),
            "{provider} removed existing state"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(std::fs::metadata(&run_root).unwrap().mode() & 0o777, 0o700);
        }
        match provider {
            "claude" => {
                assert_eq!(
                    std::fs::read(run_root.join("skills-plugin/skills/review/SKILL.md")).unwrap(),
                    SKILL
                );
                assert!(
                    run_root
                        .join("skills-plugin/.claude-plugin/plugin.json")
                        .is_file()
                );
                assert_eq!(
                    std::fs::read(run_root.join("plugins/lint/.claude-plugin/plugin.json"))
                        .unwrap(),
                    PLUGIN
                );
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    assert_eq!(
                        std::fs::metadata(run_root.join("plugins/lint/hooks/run.sh"))
                            .unwrap()
                            .mode()
                            & 0o777,
                        0o700
                    );
                    assert_eq!(
                        std::fs::metadata(run_root.join("mcp.json")).unwrap().mode() & 0o777,
                        0o600
                    );
                }
                let mcp: Value =
                    serde_json::from_slice(&std::fs::read(run_root.join("mcp.json")).unwrap())
                        .unwrap();
                assert_eq!(
                    mcp["mcpServers"]["docs"]["headers"]["Authorization"],
                    "${DOCS_TOKEN}"
                );
                assert_eq!(m.mcp_config, Some(run_root.join("mcp.json")));
                assert_eq!(
                    m.plugin_dirs,
                    [
                        run_root.join("plugins/lint"),
                        run_root.join("skills-plugin")
                    ]
                );
            }
            "codex" => {
                let codex = run_root.join("codex-home");
                assert_eq!(m.codex_home, Some(codex.clone()));
                assert_eq!(
                    std::fs::read(codex.join("skills/review/SKILL.md")).unwrap(),
                    SKILL
                );
                assert!(
                    std::fs::read_to_string(codex.join("config.toml"))
                        .unwrap()
                        .contains("enginebox-browser")
                );
                assert!(codex.join("auth.json").is_file());
                // the sessions link is Unix-only (profile.rs)
                #[cfg(unix)]
                assert!(
                    std::fs::symlink_metadata(codex.join("sessions"))
                        .unwrap()
                        .file_type()
                        .is_symlink()
                );
            }
            _ => assert_eq!(m.plugin_dirs, [run_root.join("plugins/lint")]),
        }
        profile::remove_run_dir(&home, RUN).unwrap();
        assert!(!run_root.exists());
        // Removal never follows the session link into the native home.
        assert!(home.join(".codex/sessions").is_dir());
        assert_eq!(tree(&home), before);
    }
    // A failed bootstrap leaves nothing behind.
    let missing = block(
        json!({"skills":[{"name":"review","files":[{"path":"SKILL.md","sha256":sha(SKILL)}]}]}),
    )
    .unwrap();
    assert!(profile::materialize(&home, RUN, "claude", &missing, &BTreeMap::new()).is_err());
    assert!(!run_root.exists());
    assert!(profile::materialize(&home, "../escape", "claude", &missing, &files()).is_err());
}

#[test]
fn guest_envelope_is_revalidated_against_digests() {
    let encode = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    let b = block(
        json!({"skills":[{"name":"review","files":[{"path":"SKILL.md","sha256":sha(SKILL)}]}]}),
    )
    .unwrap();
    let good = Envelope {
        request: request("claude"),
        profile: "verifier".into(),
        block: b.clone(),
        files: BTreeMap::from([(sha(SKILL), encode(SKILL))]),
    };
    assert_eq!(good.validate().unwrap()[&sha(SKILL)], SKILL);
    let mut tampered = good.clone();
    tampered.files.insert(sha(SKILL), encode(b"other"));
    assert!(tampered.validate().is_err());
    let mut extra = good.clone();
    extra.files.insert(sha(PLUGIN), encode(PLUGIN));
    assert!(extra.validate().is_err());
    let mut missing = good.clone();
    missing.files.clear();
    assert!(missing.validate().is_err());
    let mut wrong_provider = good.clone();
    wrong_provider.request = request("cursor");
    assert!(
        wrong_provider.validate().is_err(),
        "cursor cannot take skills"
    );
    // Merged profile and run env stay inside the run bounds; the run wins.
    let mut env_block = block(json!({"env":{"A":"profile","B":"profile"}})).unwrap();
    let mut r =
        native::RunRequest::parse(json!({"provider":"claude","prompt":"x","env":{"A":"run"}}))
            .unwrap();
    let merged = profile::merged_env(&env_block, &r).unwrap();
    assert_eq!(merged["A"], "run");
    assert_eq!(merged["B"], "profile");
    env_block.env = (0..native::MAX_ENV_VARS)
        .map(|i| (format!("P_{i}"), "v".into()))
        .collect();
    assert!(profile::merged_env(&env_block, &r).is_err());
    r.env.clear();
    assert!(profile::merged_env(&env_block, &r).is_ok());
    // The envelope is not a RunRequest: an old helper's `run` refuses it.
    let raw = serde_json::to_value(&good).unwrap();
    assert!(native::RunRequest::parse(raw).is_err());
}

#[cfg(unix)]
fn fake_client(home: &Path, name: &str, version: &str) {
    use std::os::unix::fs::PermissionsExt;
    security::safe_path(home, &format!(".local/bin/{name}"), true).unwrap();
    let exe = home.join(".local/bin").join(name);
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = --version ]; then echo '{version}'; exit 0; fi\ncat >/dev/null\n\
         {{ echo \"argv=$*\"; echo \"codex_home=${{CODEX_HOME:-unset}}\"; echo \"oauth=${{CLAUDE_CODE_OAUTH_TOKEN:-unset}}\"; \
         echo \"mode=$PROFILE_MODE\"; [ -n \"$CODEX_HOME\" ] && ls \"$CODEX_HOME/skills\" && cat \"$CODEX_HOME/config.toml\"; }} >>\"$HOME/.seen\"\n\
         echo \"{{\\\"value\\\":\\\"$PROFILE_MODE\\\"}}\"\n"
    );
    std::fs::write(&exe, script).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[cfg(unix)]
#[test]
fn a_profile_run_gets_exactly_its_profile_and_leaves_nothing_behind() {
    let (_t, root) = temp();
    let home = root.join("home");
    let workspace = root.join("workspace");
    security::private_dir(&home).unwrap();
    security::private_dir(&workspace).unwrap();
    fake_client(&home, "codex", "codex-cli 0.163.0");
    fake_client(&home, "claude", "2.1.310 (Claude Code)");
    security::write_bytes(
        &home,
        ".claude/settings.json",
        b"{\"env\":{\"CLAUDE_CODE_OAUTH_TOKEN\":\"delegated-access-token-1\"}}",
        4096,
    )
    .unwrap();
    let encode = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    let skill = json!([{"name":"review","files":[{"path":"SKILL.md","sha256":sha(SKILL)}]}]);
    let envelope = |provider: &str, b: Value| Envelope {
        request: native::RunRequest::parse(json!({"provider":provider,"prompt":"go"})).unwrap(),
        profile: "verifier".into(),
        block: block(b).unwrap(),
        files: BTreeMap::from([(sha(SKILL), encode(SKILL))]),
    };
    let secret = "profile-default-secret-9";
    let out = native::execute_profiled(
        &envelope(
            "codex",
            json!({"permission_profile":"read_only","skills":skill,"env":{"PROFILE_MODE":secret}}),
        ),
        &home,
        &workspace,
        RUN,
    )
    .unwrap();
    assert_eq!(out.exit_code, 0);
    assert!(!out.output.contains(secret), "{}", out.output);
    let seen = std::fs::read_to_string(home.join(".seen")).unwrap();
    let run_root = home.join(profile::RUNS_DIR).join(RUN);
    assert!(
        seen.contains(&format!(
            "codex_home={}",
            run_root.join("codex-home").display()
        )),
        "{seen}"
    );
    assert!(seen.contains("sandbox_mode=\"read-only\""), "{seen}");
    assert!(seen.contains("\nreview\n"), "{seen}");
    assert!(seen.contains(&format!("mode={secret}")));
    assert!(
        !run_root.exists(),
        "run directory must be removed after the run"
    );
    // A run without a profile sees none of it.
    std::fs::remove_file(home.join(".seen")).unwrap();
    native::execute(&request("codex"), &home, &workspace).unwrap();
    let seen = std::fs::read_to_string(home.join(".seen")).unwrap();
    assert!(
        seen.contains("codex_home=unset") && seen.contains("mode=\n"),
        "{seen}"
    );
    // Claude without user settings still receives its delegated access.
    std::fs::remove_file(home.join(".seen")).unwrap();
    let out = native::execute_profiled(
        &envelope(
            "claude",
            json!({"max_turns":250,"setting_sources":["project","local"],"skills":skill}),
        ),
        &home,
        &workspace,
        RUN,
    )
    .unwrap();
    assert_eq!(out.exit_code, 0);
    let seen = std::fs::read_to_string(home.join(".seen")).unwrap();
    assert!(seen.contains("oauth=delegated-access-token-1"), "{seen}");
    assert!(
        seen.contains("--max-turns 250") && seen.contains("--setting-sources project,local"),
        "{seen}"
    );
    assert!(
        seen.contains(&format!(
            "--plugin-dir {}",
            run_root.join("skills-plugin").display()
        )),
        "{seen}"
    );
    assert!(!run_root.exists());
    // Without a run ID the helper refuses a profile run.
    assert!(
        native::execute_profiled(&envelope("claude", json!({})), &home, &workspace, "bad").is_err()
    );
}

#[test]
fn a_bypass_permission_mode_is_refused_inside_any_argument() {
    for arg in [
        "bypassPermissions",
        "--permission-mode=bypassPermissions",
        "--permission-mode=BYPASSPERMISSIONS",
        "mode:bypasspermissions",
    ] {
        assert!(profile::forbidden_flag(arg), "{arg}");
    }
    for arg in [
        "--permission-mode",
        "acceptEdits",
        "dontAsk",
        "bypass",
        "permissions",
    ] {
        assert!(!profile::forbidden_flag(arg), "{arg}");
    }
}

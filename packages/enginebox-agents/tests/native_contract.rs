#[cfg(unix)]
use enginebox_agents::process;
use enginebox_agents::{auth, browser, delegation, native, security, usage};
use serde_json::json;
#[cfg(unix)]
use std::time::{Duration, Instant};
fn temp() -> (tempfile::TempDir, std::path::PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().canonicalize().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    security::private_dir(&p).unwrap();
    (t, p)
}
#[test]
fn rejects_unsafe_prompts_workdirs_and_resume() {
    for v in [
        json!({"provider":"codex","prompt":"x","workdir":"../other"}),
        json!({"provider":"claude","prompt":"x","resume_id":"latest"}),
        json!({"provider":"cursor","prompt":"x","timeout_seconds":0}),
        json!({"provider":"codex","prompt":"x","model":"--dangerously-bypass"}),
        json!({"provider":"claude","prompt":"x","extra":true}),
        json!({"provider":"codex","prompt":"\u{0000}"}),
    ] {
        assert!(native::RunRequest::parse(v).is_err())
    }
}
#[test]
fn native_cli_contract_preserves_exact_resume_and_sandbox() {
    let id = "adee2503-cabd-443e-9cef-0f0a5beb2f1a";
    for p in ["codex", "claude", "cursor"] {
        let r = native::RunRequest::parse(json!({"provider":p,"prompt":"hello","resume_id":id}))
            .unwrap();
        let args = native::native_command(&r, "client");
        assert!(args.contains(&id.into()));
        assert!(
            !args
                .iter()
                .any(|s| s.contains("dangerously") || s == "--yolo")
        );
        if p == "codex" {
            assert_eq!(args[1..4], ["exec", "resume", id]);
            assert_eq!(args.last().unwrap(), "-");
            assert!(!args.contains(&"hello".into()));
            // A leaf override lets native config preserve explicit per-tool
            // approvals, disabled_tools and unrelated MCP server policies.
            let overrides: Vec<_> = args
                .windows(2)
                .filter(|pair| pair[0] == "-c")
                .map(|pair| pair[1].as_str())
                .collect();
            assert_eq!(
                overrides,
                [
                    "approval_policy=\"never\"",
                    "sandbox_mode=\"workspace-write\"",
                    "mcp_servers.enginebox-browser.default_tools_approval_mode=\"approve\"",
                ]
            );
        }
        if p == "cursor" {
            assert!(args.windows(2).any(|x| x == ["--sandbox", "enabled"]));
            assert_eq!(args[args.len() - 2..], ["--", "hello"]);
        }
        if p == "claude" {
            // acceptEdits alone still denies Python/test commands in a headless
            // session. Preapprove coding tools without disabling native policy.
            assert!(args.windows(2).any(|x| x
                == [
                    "--allowedTools",
                    "Read,Write,Edit,Bash,mcp__enginebox-browser__*"
                ]));
            assert!(
                args.windows(2)
                    .any(|x| x == ["--permission-mode", "acceptEdits"])
            );
            assert!(
                args.windows(2)
                    .any(|x| x == ["--permission-prompts", "none"])
            );
            assert!(
                !args
                    .iter()
                    .any(|x| x == "--bare" || x == "bypassPermissions")
            );
        }
    }
}
#[test]
fn access_only_material_never_delegates_refresh() {
    let c=delegation::material("codex",&json!({"tokens":{"access_token":"access-token","id_token":"identity-token","account_id":"account","refresh_token":"REFRESH-SECRET"}})).unwrap();
    assert!(!c.to_string().contains("REFRESH-SECRET"));
    assert_eq!(c["content"]["auth_mode"], "chatgptAuthTokens");
    let c=delegation::material("claude",&json!({"claudeAiOauth":{"accessToken":"access-token","refreshToken":"REFRESH-SECRET","expiresAt":2000000000000_u64}})).unwrap();
    assert_eq!(c["kind"], "environment");
    assert!(!c.to_string().contains("REFRESH-SECRET"));
    let c = delegation::material(
        "cursor",
        &json!({"accessToken":"access-token","refreshToken":"REFRESH-SECRET"}),
    )
    .unwrap();
    assert_eq!(c["content"]["refreshToken"], "access-token");
}
#[test]
fn private_state_and_symlink_guard() {
    let (_t, root) = temp();
    security::write_json(&root, "nested/state.json", &json!({"a":1})).unwrap();
    assert_eq!(
        security::read_json(&root, "nested/state.json")
            .unwrap()
            .unwrap()["a"],
        1
    );
    security::write_json(&root, "nested/state.json", &json!({"a":2})).unwrap();
    assert_eq!(
        security::read_json(&root, "nested/state.json")
            .unwrap()
            .unwrap()["a"],
        2
    );
    security::remove(&root, "nested/state.json").unwrap();
    assert!(
        security::read_json(&root, "nested/state.json")
            .unwrap()
            .is_none()
    );
    security::write_json(&root, "nested/state.json", &json!({"a":3})).unwrap();
    assert!(security::write_json(&root, "../outside", &json!({})).is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, symlink};
        symlink(root.join("nested/state.json"), root.join("link")).unwrap();
        assert!(security::read_json(&root, "link").is_err());
        assert_eq!(
            std::fs::metadata(root.join("nested/state.json"))
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
    }
}
#[test]
fn workspace_private_lease_excludes_second_writer() {
    let (_t, root) = temp();
    let lease = security::Lease::acquire(&root, "state/run.lock").unwrap();
    #[cfg(unix)]
    assert_ne!(
        unsafe { libc::fcntl(lease.fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
    assert!(security::Lease::active(&root, "state/run.lock").unwrap());
    assert!(security::Lease::acquire(&root, "state/run.lock").is_err());
    drop(lease);
    // A concurrent fork may retain this CLOEXEC descriptor until its exec.
    // Closing the parent must preserve such child ownership, then release it.
    let _reacquired = acquire_after_exec_transitions(&root, "state/run.lock");
    assert!(security::Lease::active(&root, "state/run.lock").unwrap());
}
fn acquire_after_exec_transitions(root: &std::path::Path, relative: &str) -> security::Lease {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match security::Lease::acquire(root, relative) {
            Ok(lease) => return lease,
            Err(error)
                if error.to_string().contains("active operation")
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(error) => panic!("Lease did not release after the fork/exec transition: {error}"),
        }
    }
}

#[cfg(unix)]
#[test]
fn unselected_lease_survives_fork_only_until_exec() {
    use std::{
        io::{Read, Write},
        os::{
            fd::AsRawFd,
            unix::{net::UnixStream, process::CommandExt},
        },
    };
    let (_temporary, root) = temp();
    let lease = security::Lease::acquire(&root, "unselected.lock").unwrap();
    assert_ne!(
        unsafe { libc::fcntl(lease.fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
    let (mut ready, child_ready) = UnixStream::pair().unwrap();
    let (mut release, child_release) = UnixStream::pair().unwrap();
    ready
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let thread = std::thread::spawn(move || {
        let ready_fd = child_ready.as_raw_fd();
        let release_fd = child_release.as_raw_fd();
        let mut command = process::command(&["/bin/sleep".into(), "5".into()], None, None).unwrap();
        unsafe {
            command.pre_exec(move || {
                let mut byte = 1u8;
                if libc::write(ready_fd, (&byte as *const u8).cast(), 1) != 1
                    || libc::read(release_fd, (&mut byte as *mut u8).cast(), 1) != 1
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command.spawn().unwrap()
    });
    ready.read_exact(&mut [0]).unwrap();
    drop(lease);
    assert!(
        security::Lease::active(&root, "unselected.lock").unwrap(),
        "pre-exec child must still own the inherited descriptor"
    );
    release.write_all(&[1]).unwrap();
    let mut child = thread.join().unwrap();
    let _reacquired = acquire_after_exec_transitions(&root, "unselected.lock");
    let child_still_running = child.try_wait().unwrap().is_none();
    process::stop(&mut child);
    assert!(
        child_still_running,
        "lease must close at exec, before the child exits"
    );
}
#[cfg(windows)]
#[test]
fn windows_private_paths_accept_canonical_drives_and_reject_junctions() {
    let temporary = tempfile::tempdir().unwrap();
    let canonical = temporary.path().canonicalize().unwrap();
    security::check_components(temporary.path()).unwrap();
    security::private_dir(&canonical).unwrap();
    let target = temporary.path().join("target");
    security::private_dir(&target).unwrap();
    let link = temporary.path().join("junction");
    let result = std::process::Command::new("powershell.exe")
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command",
            "$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path $env:ENGINEBOX_TEST_LINK -Target $env:ENGINEBOX_TEST_TARGET | Out-Null"])
        .env("ENGINEBOX_TEST_LINK", &link)
        .env("ENGINEBOX_TEST_TARGET", &target)
        .output().unwrap();
    assert!(
        result.status.success(),
        "Junction fixture failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(security::check_components(&link).is_err());
    assert!(security::check_components(&canonical.join("junction")).is_err());
    assert!(security::private_dir(&link).is_err());
    assert!(
        security::write_json(
            &canonical,
            "junction/secret.json",
            &json!({"secret":"fixture"})
        )
        .is_err()
    );
    assert!(!target.join("secret.json").exists());
    assert!(security::check_components(std::path::Path::new("C:relative")).is_err());
}
#[test]
fn official_authorization_urls_only() {
    assert!(auth::authorization_url("codex", "https://auth.openai.com/codex/device").is_some());
    for url in [
        "https://auth.openai.com.evil.test/codex/device",
        "https://auth.openai.com/codex/device?token=secret",
        "https://user@auth.openai.com/codex/device",
        "https://auth.openai.com/codex/device#secret",
    ] {
        assert!(auth::authorization_url("codex", url).is_none());
    }
    assert!(auth::authorization_url("claude","https://claude.com/cai/oauth/authorize?code=true&redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback").is_some());
    assert!(
        auth::authorization_url(
            "claude",
            "https://claude.com/cai/oauth/authorize?code=SECRET"
        )
        .is_none()
    );
    assert!(
        auth::authorization_url(
            "cursor",
            "https://cursor.com/loginDeepControl?uuid=1&uuid=2"
        )
        .is_none()
    );
}
#[test]
fn login_parser_discards_clipboard_and_osc_link_targets() {
    let mut parser = auth::LoginOutput::new("codex");
    assert!(
        parser
            .feed(b"\x1b]52;clipboard;https://auth.openai.com/codex/device\n")
            .is_empty()
    );
    assert!(parser.feed(&vec![b'a'; 20000]).is_empty());
    assert!(parser.feed(b"\x07 hello\n").is_empty());
    let events = parser.feed(b"https://auth.openai.com/codex/device\nABCD-EFGH\n");
    assert_eq!(events.len(), 2);
    assert_eq!(events[1]["user_code"], "ABCD-EFGH");
    assert!(!parser.buffer.contains("clipboard"));
}
#[test]
fn provider_status_drops_native_identity_and_errors() {
    let v = auth::parse_status(
        "claude",
        0,
        r#"{"loggedIn":true,"authMethod":"claude.ai","email":"PRIVATE","token":"SECRET"}"#,
    );
    assert_eq!(v["status"], "connected");
    assert!(!v.to_string().contains("PRIVATE"));
    assert!(!v.to_string().contains("SECRET"));
}
#[test]
fn quota_reset_becomes_stale_never_full() {
    let mut v=usage::normalize_codex(&json!({"rateLimits":{"primary":{"usedPercent":25,"resetsAt":2000,"windowDurationMins":300},"credits":{"balance":"12.50","unlimited":false},"planType":"pro"}}),1000.).unwrap();
    usage::apply_freshness(&mut v, 1100, 1000);
    assert_eq!(v["allowance_windows"][0]["remaining_percentage"], 75.0);
    usage::apply_freshness(&mut v, 2001, 2000);
    assert!(v["allowance_windows"][0]["remaining_percentage"].is_null());
    assert_eq!(v["credits"][0]["unit"], "provider credits");
    assert!(v["billing_cycle"].is_null());
}
#[test]
fn quota_never_sums_repeated_buckets_or_accepts_bad_percent() {
    let v=usage::normalize_codex(&json!({"rateLimits":{"primary":{"usedPercent":99,"resetsAt":9999}},"rateLimitsByLimitId":{"codex":{"primary":{"usedPercent":12,"resetsAt":9999}},"bad":{"primary":{"usedPercent":101,"resetsAt":9999}}}}),1000.).unwrap();
    assert_eq!(v["allowance_windows"].as_array().unwrap().len(), 1);
    assert_eq!(v["allowance_windows"][0]["used_percentage"], 12.0);
}
#[test]
fn delegated_settings_preserve_memory_and_detect_manual_changes() {
    let (_t, root) = temp();
    security::write_json(
        &root,
        ".claude/settings.json",
        &json!({"permissions":{"allow":["Read"]},"env":{"EXISTING":"keep"}}),
    )
    .unwrap();
    security::write_bytes(&root, ".claude/CLAUDE.md", b"PRIVATE MEMORY", 65536).unwrap();
    let material = json!({"kind":"environment","content":{"CLAUDE_CODE_OAUTH_TOKEN":"access-token"},"expires_at":null});
    let request = delegation::attachment("claude", &material, false);
    let result = delegation::transfer(&root, &request, "12345678901234567890123456789012").unwrap();
    assert_eq!(result["ok"], true);
    let settings = security::read_json(&root, ".claude/settings.json")
        .unwrap()
        .unwrap();
    assert_eq!(settings["permissions"]["allow"][0], "Read");
    assert_eq!(settings["env"]["EXISTING"], "keep");
    assert_eq!(
        std::fs::read(root.join(".claude/CLAUDE.md")).unwrap(),
        b"PRIVATE MEMORY"
    );
    assert_eq!(
        delegation::transfer(&root, &request, "12345678901234567890123456789012").unwrap()["unchanged"],
        true
    );
    let mut changed = settings;
    changed["env"]["CLAUDE_CODE_OAUTH_TOKEN"] = json!("manual-token");
    security::write_json(&root, ".claude/settings.json", &changed).unwrap();
    assert!(delegation::transfer(&root, &request, "12345678901234567890123456789012").is_err());
}
#[test]
fn adoption_requires_matching_instance_and_explicit_ownership() {
    let (_t, root) = temp();
    security::write_json(
        &root,
        ".claude/.credentials.json",
        &json!({"claudeAiOauth":{"accessToken":"native-existing","refreshToken":"secret-refresh"}}),
    )
    .unwrap();
    let material = json!({"kind":"environment","content":{"CLAUDE_CODE_OAUTH_TOKEN":"access-token"},"expires_at":null});
    let mut req = delegation::attachment("claude", &material, false);
    assert!(delegation::transfer(&root, &req, "12345678901234567890123456789012").is_err());
    req["retire_refresh"] = json!(true);
    req["expected_instance_id"] = json!("DIFFERENT");
    assert!(delegation::transfer(&root, &req, "12345678901234567890123456789012").is_err());
    req["expected_instance_id"] = json!("12345678901234567890123456789012");
    delegation::transfer(&root, &req, "12345678901234567890123456789012").unwrap();
    assert!(!root.join(".claude/.credentials.json").exists());
}
#[test]
fn cursor_policy_is_narrow_to_helper_attachment() {
    assert!(
        browser::cursor_profile("/home/agent")
            .unwrap()
            .contains("/home/agent/.local/share/cursor-agent/versions/*/cursorsandbox")
    );
    for p in ["/home/*", "/", "/home/agent/../root", "/home/a[bc]"] {
        assert!(browser::cursor_profile(p).is_err());
    }
}
#[cfg(unix)]
#[test]
fn bounded_process_stops_output_flood_and_hung_group() {
    let start = Instant::now();
    let output = process::bounded(
        &[
            "/bin/sh".into(),
            "-c".into(),
            "while :; do printf 0123456789; done".into(),
        ],
        None,
        None,
        b"",
        Duration::from_secs(3),
        1024,
        None,
    )
    .unwrap();
    assert!(output.output_truncated);
    assert!(output.output.len() <= 1024);
    assert!(start.elapsed() < Duration::from_secs(4));
    let output = process::bounded(
        &["/bin/sh".into(), "-c".into(), "sleep 10 & wait".into()],
        None,
        None,
        b"",
        Duration::from_millis(100),
        1024,
        None,
    )
    .unwrap();
    assert!(output.timed_out);
}
#[cfg(unix)]
#[test]
fn fake_native_harness_exec_and_resume_keep_private_state() {
    use std::os::unix::fs::PermissionsExt;
    let (_t, root) = temp();
    let home = root.join("home");
    let workspace = root.join("workspace");
    security::private_dir(&home).unwrap();
    security::private_dir(&workspace).unwrap();
    security::safe_path(&home, ".local/bin/codex", true).unwrap();
    let exe = home.join(".local/bin/codex");
    std::fs::write(&exe,b"#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'codex-cli 0.163.0'; exit 0; fi\ncat >/dev/null\nprintf '{\"type\":\"thread.started\",\"thread_id\":\"adee2503-cabd-443e-9cef-0f0a5beb2f1a\"}\\n'\necho memory >>\"$HOME/.native-memory\"\n").unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
    for resume in [None, Some("adee2503-cabd-443e-9cef-0f0a5beb2f1a")] {
        let req = native::RunRequest::parse(
            json!({"provider":"codex","prompt":"hello","resume_id":resume}),
        )
        .unwrap();
        let out = native::execute(&req, &home, &workspace).unwrap();
        assert_eq!(out.exit_code, 0);
        assert_eq!(
            native::session_id(&out.output).as_deref(),
            Some("adee2503-cabd-443e-9cef-0f0a5beb2f1a")
        );
    }
    assert_eq!(
        std::fs::read_to_string(home.join(".native-memory")).unwrap(),
        "memory\nmemory\n"
    );
    assert!(!native::lease_active(&home, "codex").unwrap());
}
#[test]
fn unverified_expiry_used_only_for_metadata() {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let payload = URL_SAFE_NO_PAD.encode(br#"{"exp":1234567}"#);
    assert_eq!(
        delegation::expiry(&json!(format!("header.{payload}.signature"))),
        Some(1234567.)
    );
    assert!(delegation::expiry(&json!("invalid")).is_none());
}
#[test]
fn identity_mismatch_prevents_credential_delivery() {
    let (_t, root) = temp();
    let a = root.join("instance-id");
    let b = root.join("machine-id");
    std::fs::write(&a, "12345678901234567890123456789012").unwrap();
    std::fs::write(&b, "22345678901234567890123456789012").unwrap();
    #[cfg(unix)]
    let owner = unsafe { libc::geteuid() };
    #[cfg(not(unix))]
    let owner = 0;
    assert!(delegation::identity(&[&a, &b], owner).is_err());
}

#[test]
fn attachment_hashes_retain_v02_sorted_ascii_json_encoding() {
    use sha2::{Digest, Sha256};
    let value = json!({"z":"é😀","a":[true,null,"text"]});
    let original = r#"{"a": [true, null, "text"], "z": "\u00e9\ud83d\ude00"}"#;
    let expected = format!("{:x}", Sha256::digest(original.as_bytes()));
    assert_eq!(security::legacy_digest(&value), expected);
}

#[cfg(unix)]
#[test]
fn detached_descendant_retains_lease_without_blocking_supervisor_timeout() {
    let (_t, root) = temp();
    let lease = security::Lease::acquire(&root, "native.lock").unwrap();
    let mut environment = std::env::vars().collect::<std::collections::BTreeMap<_, _>>();
    environment.insert(
        "ENGINEBOX_FIXTURE_ROOT".into(),
        root.to_string_lossy().into_owned(),
    );
    environment.insert("ENGINEBOX_FIXTURE_ROLE".into(), "parent".into());
    let command = vec![
        std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        "--exact".into(),
        "__detached_native_fixture".into(),
        "--ignored".into(),
        "--nocapture".into(),
    ];
    let started = Instant::now();
    let output = process::bounded(
        &command,
        Some(&environment),
        None,
        b"",
        Duration::from_millis(700),
        4096,
        Some(&lease),
    )
    .unwrap();
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(output.timed_out);
    let pid = std::fs::read_to_string(root.join("child.pid"))
        .unwrap()
        .parse::<i32>()
        .unwrap();
    drop(lease);
    let retained = security::Lease::active(&root, "native.lock").unwrap();
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while security::Lease::active(&root, "native.lock").unwrap() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        retained,
        "closing the parent descriptor must not unlock the descendant's lease"
    );
    assert!(!security::Lease::active(&root, "native.lock").unwrap());
}

#[cfg(unix)]
#[test]
#[ignore = "subprocess fixture invoked only by detached-descendant regression"]
fn __detached_native_fixture() {
    let Ok(root) = std::env::var("ENGINEBOX_FIXTURE_ROOT") else {
        return;
    };
    if std::env::var("ENGINEBOX_FIXTURE_ROLE").unwrap_or_default() == "parent" {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "__detached_native_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("ENGINEBOX_FIXTURE_ROLE", "child")
            .spawn()
            .unwrap();
        let _ = child.wait();
    } else {
        assert!(unsafe { libc::setsid() } >= 0);
        std::fs::write(
            std::path::Path::new(&root).join("child.pid"),
            std::process::id().to_string(),
        )
        .unwrap();
        std::thread::sleep(Duration::from_secs(60));
    }
}

#[test]
fn handoff_usage_never_advertises_stale_credit_as_current_allowance() {
    let observation = usage::normalize_codex(
        &json!({"rateLimits":{"primary":{"usedPercent":30,"resetsAt":2000000000}}}),
        1900000000.,
    )
    .unwrap();
    let mut observation = json!({"usage":observation});
    observation["usage"]["allowance_windows"][0]["remaining_percentage"] = json!(70);
    let handoff = usage::handoff_snapshot(&observation);
    assert_eq!(handoff["usage"]["freshness"], "stale");
    assert!(handoff["usage"]["allowance_windows"][0]["remaining_percentage"].is_null());
    assert_eq!(
        observation["usage"]["allowance_windows"][0]["remaining_percentage"],
        70
    );
}

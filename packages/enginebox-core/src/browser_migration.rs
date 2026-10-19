//! Exact legacy browser entry migration. No inference/authentication or setup.
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};
const LIMIT: u64 = 1024 * 1024;
const BROWSER: &str = "enginebox-browser";
const NATIVE: &str = "/usr/local/bin/enginebox-guest";
const PROVIDERS: [&str; 3] = ["claude", "codex", "cursor"];

fn legacy(entry: &Value, provider: &str) -> bool {
    entry["command"] == "/usr/bin/python3"
        && entry["args"]
            == json!([
                "/opt/enginebox-runtime/browser_tools.py",
                "--client",
                provider
            ])
        && entry.get("type").is_none_or(|value| value == "stdio")
        && entry.get("url").is_none()
}
fn transform(provider: &str, bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    if provider == "codex" {
        let text = std::str::from_utf8(bytes)?;
        let original: toml::Value = toml::from_str(text)?;
        let value = serde_json::to_value(&original)?;
        if !legacy(&value["mcp_servers"][BROWSER], provider) {
            return Ok(None);
        }
        // Keep TOML comments, ordering, and all unrelated settings verbatim.
        let mut document: toml_edit::DocumentMut = text.parse()?;
        let entry = &mut document["mcp_servers"][BROWSER];
        let mut command = toml_edit::Value::from(NATIVE);
        *command.decor_mut() = entry["command"]
            .as_value()
            .context("Invalid command")?
            .decor()
            .clone();
        entry["command"] = toml_edit::Item::Value(command);
        let mut args =
            toml_edit::Value::Array(["browser", "--client", provider].into_iter().collect());
        *args.decor_mut() = entry["args"]
            .as_value()
            .context("Invalid args")?
            .decor()
            .clone();
        entry["args"] = toml_edit::Item::Value(args);
        let output = document.to_string();
        let parsed: toml::Value = toml::from_str(&output)?;
        let mut expected = value;
        expected["mcp_servers"][BROWSER]["command"] = json!(NATIVE);
        expected["mcp_servers"][BROWSER]["args"] = json!(["browser", "--client", provider]);
        ensure!(
            serde_json::to_value(parsed)? == expected,
            "Migration changed unrelated TOML settings"
        );
        Ok(Some(output.into_bytes()))
    } else {
        let mut value = crate::templates::decode_json(bytes)?;
        ensure!(value.is_object(), "MCP configuration must be an object");
        if !legacy(&value["mcpServers"][BROWSER], provider) {
            return Ok(None);
        }
        value["mcpServers"][BROWSER]["command"] = json!(NATIVE);
        value["mcpServers"][BROWSER]["args"] = json!(["browser", "--client", provider]);
        let mut output = serde_json::to_vec_pretty(&value)?;
        output.push(b'\n');
        Ok(Some(output))
    }
}

#[cfg(unix)]
fn secure_path(path: &Path, home: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    use std::path::Component;
    ensure!(
        path.is_absolute() && path.starts_with(home),
        "MCP path escapes guest home"
    );
    let mut current = PathBuf::new();
    for component in path.components() {
        ensure!(
            !matches!(component, Component::ParentDir),
            "Unsafe MCP path"
        );
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(meta) => {
                ensure!(
                    !meta.file_type().is_symlink(),
                    "MCP paths must not contain symlinks"
                );
                if current.starts_with(home) {
                    ensure!(
                        meta.uid() == crate::bootstrap::current_uid() && meta.mode() & 0o022 == 0,
                        "MCP paths must be guest-owned and not writable by others"
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
#[cfg(not(unix))]
fn secure_path(_path: &Path, _home: &Path) -> Result<()> {
    anyhow::bail!("Guest MCP migration requires Unix")
}

fn read(path: &Path, home: &Path) -> Result<Option<Vec<u8>>> {
    secure_path(path, home)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.len() <= LIMIT,
        "Invalid or oversized MCP configuration"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            meta.nlink() == 1
                && meta.uid() == crate::bootstrap::current_uid()
                && meta.mode() & 0o022 == 0,
            "MCP configuration ownership or links are unsafe"
        );
    }
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= LIMIT as usize,
        "MCP configuration exceeds bound"
    );
    Ok(Some(bytes))
}
fn directory(path: &Path, home: &Path) -> Result<()> {
    secure_path(path, home)?;
    let mut current = home.to_path_buf();
    for component in path.strip_prefix(home)?.components() {
        current.push(component);
        if !current.exists() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                fs::DirBuilder::new().mode(0o700).create(&current)?;
            }
            #[cfg(not(unix))]
            fs::create_dir(&current)?;
            #[cfg(unix)]
            File::open(current.parent().context("Directory parent missing")?)?.sync_all()?;
        }
        ensure!(current.is_dir(), "Migration state requires directories");
        secure_path(&current, home)?;
    }
    Ok(())
}
fn lease(path: &Path, home: &Path) -> Result<File> {
    directory(path.parent().context("Lease parent missing")?, home)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options.open(path)?;
    let meta = file.metadata()?;
    ensure!(meta.is_file(), "Migration lease is not a file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            meta.uid() == crate::bootstrap::current_uid()
                && meta.mode() & 0o077 == 0
                && meta.nlink() == 1,
            "Unsafe migration lease"
        );
    }
    file.try_lock_exclusive()
        .context("Native provider is active; retry migration after it exits")?;
    Ok(file)
}
struct Change {
    provider: &'static str,
    path: PathBuf,
    before: Vec<u8>,
    after: Vec<u8>,
}

pub(crate) fn migrate(home: &Path, providers: &[&str]) -> Result<Value> {
    ensure!(
        providers.iter().all(|p| PROVIDERS.contains(p)),
        "Invalid migration providers"
    );
    secure_path(home, home)?;
    let mut changes = Vec::new();
    for (provider, relative) in [
        ("claude", ".claude.json"),
        ("codex", ".codex/config.toml"),
        ("cursor", ".cursor/mcp.json"),
    ] {
        if !providers.contains(&provider) {
            continue;
        }
        let path = home.join(relative);
        if let Some(before) = read(&path, home)?
            && let Some(after) = transform(provider, &before)?
        {
            ensure!(
                after.len() <= LIMIT as usize,
                "Migrated configuration exceeds bound"
            );
            changes.push(Change {
                provider,
                path,
                before,
                after,
            });
        }
    }
    if changes.is_empty() {
        return Ok(json!({"changed":[],"scope":"exact legacy browser MCP entries"}));
    }
    let state = home.join(".local/state/enginebox");
    let mut leases = vec![lease(&state.join("browser-migration.lock"), home)?];
    // These are the same auth/run leases used by the Python and Rust harnesses.
    // Never wait while holding another lease: contention leaves configs intact.
    for provider in PROVIDERS {
        leases.push(lease(
            &state.join(format!("agent-runs/{provider}.lock")),
            home,
        )?);
        leases.push(lease(&state.join(format!("auth-{provider}.lock")), home)?);
    }
    for change in &changes {
        ensure!(
            read(&change.path, home)?.as_deref() == Some(&change.before),
            "MCP configuration changed during migration; retry after inspecting it"
        );
    }
    let id = uuid::Uuid::new_v4().simple().to_string();
    let backups = state.join("browser-migrations").join(&id);
    directory(&backups, home)?;
    for change in &changes {
        let path = backups.join(format!("{}.backup", change.provider));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let mut file = options.open(path)?;
        file.write_all(&change.before)?;
        file.sync_all()?;
    }
    #[cfg(unix)]
    File::open(&backups)?.sync_all()?;
    for change in &changes {
        ensure!(
            read(&change.path, home)?.as_deref() == Some(&change.before),
            "MCP configuration changed; originals retained in private migration backup"
        );
        let parent = change
            .path
            .parent()
            .context("Configuration parent missing")?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(&change.after)?;
        file.as_file().sync_all()?;
        file.persist(&change.path)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
    }
    drop(leases);
    Ok(
        json!({"changed":changes.iter().map(|c|c.provider).collect::<Vec<_>>(),"backup_id":id,"scope":"exact legacy browser MCP entries"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_json(provider: &str) -> Vec<u8> {
        serde_json::to_vec_pretty(&json!({
            "mcpServers": {
                "enginebox-browser": {
                    "command":"/usr/bin/python3",
                    "args":["/opt/enginebox-runtime/browser_tools.py","--client",provider],
                    "type":"stdio", "disabled":true, "env":{"CUSTOM":"kept"}
                },
                "custom":{"command":"my-server","args":["--safe"]}
            },
            "permissions":{"deny":["Bash(rm:*)"]},
            "privateMemory":{"notes":"keep this verbatim as data"},
            "oauthAccount":{"accountUuid":"fixture-only"}
        }))
        .unwrap()
    }
    const TOML: &str = r#"# Private settings, keep this comment
model = 'codex-test'
approval_policy = "never" # keep policy
[mcp_servers.enginebox-browser]
command = '/usr/bin/python3' # keep command note
args = ['/opt/enginebox-runtime/browser_tools.py', '--client', 'codex'] # keep args note
enabled = false
env = { CUSTOM = 'keep-me' }
[mcp_servers.custom]
command = 'my-server'
args = ['--safe']
[permissions]
deny = ['dangerous']
"#;

    #[test]
    fn exact_json_preserves_all_other_data_and_is_idempotent() {
        for provider in ["claude", "cursor"] {
            let original = legacy_json(provider);
            let output = transform(provider, &original).unwrap().unwrap();
            let mut expected: Value = serde_json::from_slice(&original).unwrap();
            expected["mcpServers"][BROWSER]["command"] = json!(NATIVE);
            expected["mcpServers"][BROWSER]["args"] = json!(["browser", "--client", provider]);
            assert_eq!(serde_json::from_slice::<Value>(&output).unwrap(), expected);
            assert!(transform(provider, &output).unwrap().is_none());
        }
    }

    #[test]
    fn toml_preserves_unrelated_bytes_comments_and_semantics() {
        let output = transform("codex", TOML.as_bytes()).unwrap().unwrap();
        let text = std::str::from_utf8(&output).unwrap();
        for retained in [
            "# Private settings, keep this comment",
            "model = 'codex-test'",
            "approval_policy = \"never\" # keep policy",
            "# keep command note",
            "# keep args note",
            "enabled = false",
            "env = { CUSTOM = 'keep-me' }",
            "[mcp_servers.custom]\ncommand = 'my-server'\nargs = ['--safe']",
            "[permissions]\ndeny = ['dangerous']",
        ] {
            assert!(text.contains(retained), "Lost {retained}");
        }
        assert!(transform("codex", &output).unwrap().is_none());
        // The native command can also be registered as an inline TOML table.
        let inline = "mcp_servers = { enginebox-browser = { command = '/usr/bin/python3', args = ['/opt/enginebox-runtime/browser_tools.py', '--client', 'codex'], enabled = false } }\n";
        assert!(transform("codex", inline.as_bytes()).unwrap().is_some());
    }

    #[test]
    fn custom_entries_are_never_overwritten() {
        for (field, replacement) in [
            ("command", json!("/custom/python3")),
            (
                "args",
                json!([
                    "/opt/enginebox-runtime/browser_tools.py",
                    "--client",
                    "claude",
                    "--extra"
                ]),
            ),
            (
                "args",
                json!([
                    "/opt/enginebox-runtime/browser_tools.py",
                    "--client",
                    "cursor"
                ]),
            ),
            ("url", json!("https://example.invalid/custom")),
            ("type", json!("http")),
        ] {
            let mut value: Value = serde_json::from_slice(&legacy_json("claude")).unwrap();
            value["mcpServers"][BROWSER][field] = replacement;
            assert!(
                transform("claude", &serde_json::to_vec(&value).unwrap())
                    .unwrap()
                    .is_none()
            );
        }
        assert!(
            transform("claude", b"{\"mcpServers\":{}}")
                .unwrap()
                .is_none()
        );
        assert!(transform("claude", b"{\"mcpServers\":{},\"mcpServers\":{}}").is_err());
    }

    #[cfg(unix)]
    fn fixture() -> (tempfile::TempDir, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(home.join(".codex")).unwrap();
        fs::create_dir(home.join(".cursor")).unwrap();
        fs::write(home.join(".claude.json"), legacy_json("claude")).unwrap();
        fs::write(home.join(".codex/config.toml"), TOML).unwrap();
        fs::write(home.join(".cursor/mcp.json"), legacy_json("cursor")).unwrap();
        (dir, home)
    }

    #[cfg(unix)]
    #[test]
    fn migration_is_atomic_private_backed_up_and_idempotent() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, home) = fixture();
        let result = migrate(&home, &PROVIDERS).unwrap();
        assert_eq!(result["changed"], json!(["claude", "codex", "cursor"]));
        let backup = home
            .join(".local/state/enginebox/browser-migrations")
            .join(result["backup_id"].as_str().unwrap());
        for (provider, relative, before) in [
            ("claude", ".claude.json", legacy_json("claude")),
            ("codex", ".codex/config.toml", TOML.as_bytes().to_vec()),
            ("cursor", ".cursor/mcp.json", legacy_json("cursor")),
        ] {
            let path = home.join(relative);
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let saved = backup.join(format!("{provider}.backup"));
            assert_eq!(fs::read(&saved).unwrap(), before);
            assert_eq!(
                fs::metadata(&saved).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let after = fs::read(&path).unwrap();
            assert!(transform(provider, &after).unwrap().is_none());
            assert_eq!(migrate(&home, &[provider]).unwrap()["changed"], json!([]));
            assert_eq!(fs::read(path).unwrap(), after);
        }
        assert_eq!(fs::read_dir(backup.parent().unwrap()).unwrap().count(), 1);
        assert!(
            crate::guest::clean_base_check(&home).is_err(),
            "Private backups must never enter reusable bases"
        );
    }

    #[cfg(unix)]
    #[test]
    fn active_run_or_auth_lease_blocks_every_config_write() {
        for lock in ["agent-runs/claude.lock", "auth-codex.lock"] {
            let (_dir, home) = fixture();
            let held = lease(&home.join(".local/state/enginebox").join(lock), &home).unwrap();
            let error = migrate(&home, &PROVIDERS).unwrap_err();
            assert!(error.to_string().contains("Native provider is active"));
            assert_eq!(
                fs::read(home.join(".claude.json")).unwrap(),
                legacy_json("claude")
            );
            assert_eq!(
                fs::read(home.join(".codex/config.toml")).unwrap(),
                TOML.as_bytes()
            );
            assert_eq!(
                fs::read(home.join(".cursor/mcp.json")).unwrap(),
                legacy_json("cursor")
            );
            assert!(
                !home
                    .join(".local/state/enginebox/browser-migrations")
                    .exists()
            );
            drop(held);
            assert!(migrate(&home, &PROVIDERS).is_ok());
        }
    }

    #[cfg(unix)]
    #[test]
    fn malformed_or_unsafe_config_refuses_before_any_write() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        for unsafe_case in [
            "malformed",
            "symlink",
            "hardlink",
            "writable",
            "directory-symlink",
        ] {
            let (_dir, home) = fixture();
            let path = home.join(".cursor/mcp.json");
            match unsafe_case {
                "malformed" => fs::write(&path, b"{").unwrap(),
                "symlink" => {
                    fs::remove_file(&path).unwrap();
                    symlink(home.join(".claude.json"), &path).unwrap();
                }
                "hardlink" => {
                    fs::remove_file(&path).unwrap();
                    fs::hard_link(home.join(".claude.json"), &path).unwrap();
                }
                "writable" => {
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap()
                }
                _ => {
                    fs::rename(home.join(".cursor"), home.join("other")).unwrap();
                    symlink(home.join("other"), home.join(".cursor")).unwrap();
                }
            }
            assert!(migrate(&home, &PROVIDERS).is_err(), "Allowed {unsafe_case}");
            assert_eq!(
                fs::read(home.join(".claude.json")).unwrap(),
                legacy_json("claude")
            );
            assert_eq!(
                fs::read(home.join(".codex/config.toml")).unwrap(),
                TOML.as_bytes()
            );
            assert!(
                !home
                    .join(".local/state/enginebox/browser-migrations")
                    .exists()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn no_legacy_entries_creates_no_state_and_accepts_empty_provider_selection() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        assert_eq!(migrate(&home, &PROVIDERS).unwrap()["changed"], json!([]));
        assert_eq!(migrate(&home, &[]).unwrap()["changed"], json!([]));
        assert_eq!(fs::read_dir(home).unwrap().count(), 0);
    }
}

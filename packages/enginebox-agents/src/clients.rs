//! Official native client installation into an owned controller HOME.
use crate::{native, process, security};
use anyhow::{Result, bail, ensure};
use enginebox_core::native_versions::{CLAUDE_VERSION, CODEX_VERSION, CURSOR_VERSION};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
const MARKER: &str = "Engine Box native controller clients v1\n";
fn recipe(provider: &str) -> (&'static str, &'static str, Vec<&'static str>, &'static str) {
    match provider {
        "codex" => (
            "https://chatgpt.com/codex/install.sh",
            "sh",
            vec!["--release", CODEX_VERSION],
            CODEX_VERSION,
        ),
        "claude" => (
            "https://claude.ai/install.sh",
            "bash",
            vec![CLAUDE_VERSION],
            CLAUDE_VERSION,
        ),
        _ => ("https://cursor.com/install", "bash", vec![], CURSOR_VERSION),
    }
}
pub fn plan(state_root: &Path, providers: &[String]) -> Result<Value> {
    ensure!(
        state_root.is_absolute(),
        "State directory must be absolute."
    );
    security::check_components(state_root)?;
    ensure!(
        !providers.is_empty()
            && providers.len() <= 3
            && providers
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                == providers.len(),
        "Choose distinct providers."
    );
    let mut actions = Vec::new();
    for p in providers {
        crate::provider(p)?;
        let (url, shell, args, version) = recipe(p);
        actions.push(json!({"provider":p,"native_binary":native::binary(p),"installer":url,"shell":shell,"arguments":args,"qualified_version":version,"location":state_root.join("agents/bin").join(native::binary(p)),"action":"install if absent; verify native identity and qualified version; preserve foreign files"}));
    }
    let supported = cfg!(unix);
    Ok(
        json!({"mode":"plan","supported":supported,"state_dir":state_root,"service_user_required":true,"actions":actions,"detail":if supported{"Installs original provider clients as the unprivileged controller user. No provider sign-in or inference occurs."}else{"Managed controller client installation is qualified on Linux/macOS. On Windows, use the official native provider installers and configure absolute client paths; Linux guests remain supported through a Linux fleet node."}}),
    )
}
fn owned_directory(path: &Path) -> Result<()> {
    security::private_dir(path)
}
pub fn apply(state_root: &Path, providers: &[String]) -> Result<Value> {
    let plan = plan(state_root, providers)?;
    ensure!(
        plan["supported"] == true,
        "Managed native client installation requires Linux or macOS; use official provider installation on Windows."
    );
    #[cfg(unix)]
    ensure!(
        unsafe { libc::geteuid() } != 0,
        "Run native client installation as the unprivileged controller service user."
    );
    let root = state_root.join("agents");
    let home = root.join("client-install");
    let bindir = root.join("bin");
    for path in [state_root, &root, &home, &bindir] {
        owned_directory(path)?;
    }
    let marker = security::read_bytes(&home, ".enginebox-agent-clients", 4096, true)?;
    if let Some(marker) = marker {
        ensure!(
            marker == MARKER.as_bytes(),
            "Native-client ownership marker is invalid."
        );
    } else {
        ensure!(
            fs::read_dir(&home)?.next().is_none(),
            "Refusing an unmarked nonempty native-client directory."
        );
        security::write_bytes(&home, ".enginebox-agent-clients", MARKER.as_bytes(), 4096)?;
    }
    let _lease = security::Lease::acquire(&root, "client-install.lock")?;
    let mut environment = BTreeMap::new();
    let mut paths = vec![home.join(".local/bin")];
    if let Some(paths_env) = std::env::var_os("PATH") {
        for path in std::env::split_paths(&paths_env) {
            if path.join("node").is_file() {
                paths.push(path);
                break;
            }
        }
    }
    paths.extend([
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ]);
    environment.insert("HOME".into(), home.to_string_lossy().into_owned());
    environment.insert("USER".into(), std::env::var("USER").unwrap_or_default());
    environment.insert(
        "LOGNAME".into(),
        std::env::var("LOGNAME").unwrap_or_default(),
    );
    environment.insert("SHELL".into(), "/bin/bash".into());
    environment.insert(
        "PATH".into(),
        std::env::join_paths(paths)?.to_string_lossy().into_owned(),
    );
    environment.insert("DISABLE_AUTOUPDATER".into(), "1".into());
    let mut versions = security::read_json(&root, "installed-versions.json")?.unwrap_or(json!({}));
    for p in providers {
        let binary = native::binary(p);
        let (url, shell, args, expected) = recipe(p);
        let executable = home.join(".local/bin").join(binary);
        let link = bindir.join(binary);
        if fs::symlink_metadata(&link).is_ok() {
            ensure!(
                link.symlink_metadata()?.file_type().is_symlink(),
                "Refusing to replace an existing native-client binary."
            );
            let target = fs::read_link(&link)?;
            let target = if target.is_absolute() {
                target
            } else {
                bindir.join(target)
            };
            ensure!(
                target == executable
                    || target
                        .canonicalize()
                        .ok()
                        .is_some_and(|p| Some(p) == executable.canonicalize().ok()),
                "Existing native-client link points outside its owned installation."
            );
        }
        if !executable.exists() {
            let temporary = tempfile::Builder::new()
                .prefix("install-")
                .tempdir_in(&home)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))?;
            }
            let script = temporary.path().join("installer");
            let command = [
                "curl",
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--proto",
                "=https",
                "--proto-redir",
                "=https",
                "--max-time",
                "60",
                "--max-filesize",
                "5242880",
                url,
                "--output",
            ]
            .map(str::to_owned)
            .into_iter()
            .chain([script.to_string_lossy().into_owned()])
            .collect::<Vec<_>>();
            process::checked(&command, Some(&environment), Some(&home), b"", 65, 65536)?;
            let bytes = fs::read(&script)?;
            ensure!(
                !bytes.is_empty() && bytes.len() <= 5242880,
                "Invalid official installer response."
            );
            let command = std::iter::once(shell.to_owned())
                .chain([script.to_string_lossy().into_owned()])
                .chain(args.into_iter().map(str::to_owned))
                .collect::<Vec<_>>();
            process::checked(&command, Some(&environment), Some(&home), b"", 300, 262144)?;
        }
        let version = native::verify_client(&executable.to_string_lossy(), p, &environment, &home)?;
        ensure!(
            version.split_whitespace().any(|part| part == expected),
            "Installed native client is not the qualified version; existing installation is preserved for explicit review."
        );
        if fs::symlink_metadata(&link).is_err() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(&executable, &link)?;
            #[cfg(not(unix))]
            bail!("Native client links require platform installation.");
        }
        versions[binary] = json!(version);
        security::write_json(&root, "installed-versions.json", &versions)?;
    }
    Ok(json!({"installed":versions,"location":bindir,"sign_in_performed":false}))
}
pub fn main(args: &[String]) -> Result<Value> {
    let mut root = None;
    let mut providers = Vec::new();
    let mut apply_changes = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--state-dir" => {
                i += 1;
                root =
                    Some(PathBuf::from(args.get(i).ok_or_else(|| {
                        anyhow::anyhow!("Missing state directory.")
                    })?));
            }
            "--provider" => {
                i += 1;
                providers.push(
                    args.get(i)
                        .ok_or_else(|| anyhow::anyhow!("Missing provider."))?
                        .clone(),
                );
            }
            "--apply" => apply_changes = true,
            "--plan" => apply_changes = false,
            _ => bail!("Unknown native-client installer option."),
        }
        i += 1;
    }
    let root = root
        .ok_or_else(|| anyhow::anyhow!("Provide --state-dir for controller persistent state."))?;
    let root = if root.is_absolute() {
        root
    } else {
        std::env::current_dir()?.join(root)
    };
    if providers.is_empty() {
        providers = crate::PROVIDERS.iter().map(|p| p.to_string()).collect();
    }
    if apply_changes {
        apply(&root, &providers)
    } else {
        plan(&root, &providers)
    }
}

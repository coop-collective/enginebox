use crate::{process, security};
pub fn screenshot() -> Result<Vec<u8>> {
    let directory = tempfile::tempdir()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
    }
    let path = directory.path().join("desktop.png");
    let display = std::env::var("ENGINEBOX_GUEST_DISPLAY")
        .unwrap_or_else(|_| std::env::var("DISPLAY").unwrap_or(":99".into()));
    let mut environment = std::env::vars().collect::<std::collections::BTreeMap<_, _>>();
    environment.insert("DISPLAY".into(), display);
    process::checked(
        &["scrot".into(), path.to_string_lossy().into_owned()],
        Some(&environment),
        None,
        b"",
        20,
        65536,
    )?;
    let metadata = std::fs::symlink_metadata(&path)?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() <= 20 * 1024 * 1024,
        "Screenshot exceeds bound."
    );
    let bytes = std::fs::read(path)?;
    ensure!(
        bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "Invalid PNG screenshot."
    );
    Ok(bytes)
}
use anyhow::{Result, ensure};
use std::{
    fs,
    path::{Path, PathBuf},
};
fn profile(s: &str) -> Result<()> {
    ensure!(
        regex::Regex::new(r"^[a-z0-9][a-z0-9_-]{0,63}$")?.is_match(s),
        "Browser profile requires 1–64 lowercase letters, digits, '-' or '_'."
    );
    Ok(())
}
pub fn settings(
    client: &str,
    profile_name: Option<&str>,
    headed: bool,
) -> Result<(Vec<String>, PathBuf)> {
    profile(client)?;
    let name = profile_name.unwrap_or(client);
    profile(name)?;
    let package = PathBuf::from(
        std::env::var("ENGINEBOX_BROWSER_PACKAGE")
            .unwrap_or("/opt/enginebox-browser/current".into()),
    );
    let root = PathBuf::from(std::env::var("ENGINEBOX_BROWSER_HOME").unwrap_or(format!(
        "{}/.local/share/enginebox/browser",
        std::env::var("HOME")?
    )));
    ensure!(
        package.is_absolute() && root.is_absolute(),
        "Browser paths must be absolute."
    );
    security::check_components(&root)?;
    let state = root.join(name);
    security::check_components(&state)?;
    let exe = fs::read_to_string(package.join("chromium-path.txt"))?
        .trim()
        .to_owned();
    ensure!(
        Path::new(&exe).is_absolute() && !exe.chars().any(char::is_control),
        "Invalid installed Chromium path."
    );
    let mut args = vec![
        "node".into(),
        package
            .join("node_modules/@playwright/mcp/cli.js")
            .to_string_lossy()
            .into_owned(),
        "--executable-path".into(),
        exe,
        "--user-data-dir".into(),
        state.join("profile").to_string_lossy().into_owned(),
        "--output-dir".into(),
        state.join("output").to_string_lossy().into_owned(),
        "--viewport-size".into(),
        "1440x900".into(),
        "--snapshot-mode".into(),
        "none".into(),
    ];
    if !headed {
        args.push("--headless".into());
    }
    args.push("--sandbox".into());
    Ok((args, state))
}
pub fn run(args: &[String]) -> Result<()> {
    let mut client = "manual";
    let mut name = None;
    let mut headed = false;
    let mut show = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--client" => {
                i += 1;
                client = args
                    .get(i)
                    .ok_or_else(|| anyhow::anyhow!("Missing client."))?
            }
            "--profile" => {
                i += 1;
                name = Some(
                    args.get(i)
                        .ok_or_else(|| anyhow::anyhow!("Missing profile."))?
                        .as_str(),
                )
            }
            "--headed" => headed = true,
            "--show-command" => show = true,
            _ => anyhow::bail!("Unknown browser option."),
        }
        i += 1;
    }
    let (command, state) = settings(client, name, headed)?;
    if show {
        println!("{}", serde_json::to_string(&command)?);
        return Ok(());
    }
    #[cfg(unix)]
    ensure!(
        unsafe { libc::geteuid() } != 0,
        "Run browser MCP as the non-root workspace user."
    );
    security::private_dir(&state)?;
    security::private_dir(&state.join("profile"))?;
    security::private_dir(&state.join("output"))?;
    let lease = security::Lease::acquire(&state, "session.lock")?;
    let mut cmd = std::process::Command::new(&command[0]);
    cmd.args(&command[1..]);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            let fd = lease.fd();
            let flags = libc::fcntl(fd, libc::F_GETFD);
            ensure!(
                flags >= 0 && libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) >= 0,
                "Unable to retain browser lease."
            );
        }
        Err(cmd.exec().into())
    }
    #[cfg(not(unix))]
    {
        let status = cmd.status()?;
        drop(lease);
        ensure!(status.success(), "Browser MCP failed.");
        Ok(())
    }
}
pub fn cursor_profile(home: &str) -> Result<String> {
    ensure!(
        regex::Regex::new(r"^/[A-Za-z0-9_./-]+$")?.is_match(home)
            && !home.split('/').any(|p| p == "..")
            && home != "/"
            && !home.ends_with('/'),
        "Cursor sandbox policy requires a safe absolute home."
    );
    Ok(format!(
        "# Engine Box: Cursor Agent CLI sandbox helper only.\nprofile enginebox_cursor_sandbox \"{home}/.local/share/cursor-agent/versions/*/cursorsandbox\" flags=(attach_disconnected) {{\n  file,\n  /** ix,\n  capability sys_admin,\n  capability net_admin,\n  capability chown,\n  capability setuid,\n  capability setgid,\n  capability setpcap,\n  userns,\n  network unix,\n  network inet stream,\n  network inet6 stream,\n  network inet dgram,\n  network inet6 dgram,\n  network netlink raw,\n  signal (receive) peer=unconfined,\n  signal (send, receive) peer=enginebox_cursor_sandbox,\n  mount,\n  remount,\n  umount,\n}}\n"
    ))
}
pub fn cursor_sandbox(args: &[String]) -> Result<serde_json::Value> {
    ensure!(
        cfg!(target_os = "linux"),
        "Cursor guest sandbox repair requires Linux."
    );
    let mut options = std::collections::BTreeMap::new();
    let mut apply = false;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--apply" {
            apply = true;
            i += 1;
            continue;
        }
        ensure!(
            ["--home", "--user", "--workspace"].contains(&args[i].as_str()),
            "Unknown cursor-sandbox option."
        );
        let value = args
            .get(i + 1)
            .ok_or_else(|| anyhow::anyhow!("Missing sandbox option."))?;
        options.insert(args[i].clone(), value.clone());
        i += 2;
    }
    let home = options
        .get("--home")
        .ok_or_else(|| anyhow::anyhow!("Missing home."))?;
    let user = options
        .get("--user")
        .ok_or_else(|| anyhow::anyhow!("Missing user."))?;
    let workspace = options
        .get("--workspace")
        .ok_or_else(|| anyhow::anyhow!("Missing workspace."))?;
    ensure!(
        regex::Regex::new(r"^[a-z_][a-z0-9_-]{0,31}$")?.is_match(user) && user != "root",
        "Provide non-root configured guest user."
    );
    let policy = cursor_profile(home)?;
    security::absolute_existing(Path::new(home))?;
    security::absolute_existing(Path::new(workspace))?;
    let passwd = process::checked(
        &["getent".into(), "passwd".into(), user.clone()],
        None,
        None,
        b"",
        10,
        4096,
    )?;
    let fields = passwd.trim().split(':').collect::<Vec<_>>();
    ensure!(
        fields.len() >= 7 && fields[2] != "0" && fields[5] == home,
        "Native home must belong to configured non-root guest user."
    );
    let mut command = vec![
        "runuser".into(),
        "-u".into(),
        user.clone(),
        "--".into(),
        "env".into(),
        "-i".into(),
        format!("HOME={home}"),
        format!("USER={user}"),
        format!("LOGNAME={user}"),
        format!("PATH={home}/.local/bin:/usr/local/bin:/usr/bin:/bin"),
        "DISABLE_AUTOUPDATER=1".into(),
        "/bin/bash".into(),
        "-c".into(),
        "cd \"$1\" && shift && exec \"$@\"".into(),
        "--".into(),
        workspace.clone(),
        format!("{home}/.local/bin/agent"),
    ];
    let mut version_command = command.clone();
    version_command.push("--version".into());
    let version = process::checked(&version_command, None, None, b"", 10, 1024)?
        .trim()
        .to_owned();
    ensure!(
        regex::Regex::new(r"^(Cursor Agent )?20[0-9]{2}\.[0-9]{2}\.[0-9]{2}-[a-zA-Z0-9]+$")?
            .is_match(&version),
        "Installed agent does not identify as Cursor Agent."
    );
    let active = Path::new("/sys/kernel/security/apparmor").is_dir();
    let mut result = serde_json::json!({"apparmor_active":active,"applied":apply,"preflight":"not_run","cursor_version":version});
    if active {
        let dir = Path::new("/etc/apparmor.d");
        security::check_components(dir)?;
        let path = dir.join("enginebox-cursor-sandbox");
        security::check_components(&path)?;
        for mode in ["disable", "force-complain"] {
            ensure!(
                fs::symlink_metadata(dir.join(mode).join("enginebox-cursor-sandbox")).is_err(),
                "Cursor profile has local mode override."
            );
        }
        let loaded = fs::read_to_string("/sys/kernel/security/apparmor/profiles")?;
        ensure!(
            !loaded
                .lines()
                .any(|l| l.starts_with("cursor_sandbox_agent_cli ")),
            "An operator Cursor AppArmor profile already exists."
        );
        ensure!(
            !loaded
                .lines()
                .any(|l| l.starts_with("enginebox_cursor_sandbox "))
                || path.exists(),
            "Engine Box profile loaded from another source."
        );
        let created = !path.exists();
        if !created {
            let m = fs::metadata(&path)?;
            ensure!(
                m.is_file() && m.len() <= 65536,
                "Unsafe existing Cursor profile."
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                ensure!(
                    m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o022 == 0,
                    "Existing profile owner or permissions unsafe."
                );
            }
            ensure!(
                fs::read_to_string(&path)?
                    .lines()
                    .any(|line| Some(line) == policy.lines().nth(1)),
                "Unexpected existing profile attachment."
            );
        } else if apply {
            use std::io::Write;
            let mut opts = fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o644).custom_flags(libc::O_NOFOLLOW);
            }
            let mut file = opts.open(&path)?;
            file.write_all(policy.as_bytes())?;
            file.sync_all()?;
        }
        result["profile"] = serde_json::json!(path);
        result["created"] = serde_json::json!(created);
        if apply {
            process::checked(
                &[
                    "apparmor_parser".into(),
                    "-r".into(),
                    path.to_string_lossy().into_owned(),
                ],
                None,
                None,
                b"",
                30,
                65536,
            )?;
            ensure!(
                fs::read_to_string("/sys/kernel/security/apparmor/profiles")?
                    .lines()
                    .any(|l| l == "enginebox_cursor_sandbox (enforce)"),
                "Cursor policy did not enter enforce mode."
            );
        }
    }
    if apply {
        command.extend(["sandbox".into(), "run".into(), "/bin/true".into()]);
        process::checked(&command, None, None, b"", 30, 65536)?;
        result["preflight"] = serde_json::json!("passed");
    }
    Ok(result)
}

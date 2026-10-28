use crate::{
    bootstrap, capacity,
    model::{GuestConfig, Host},
    native_versions::{CLAUDE_VERSION, CODEX_VERSION, verify_qualified_version},
    transport::{run_process, shell_quote},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{net::ToSocketAddrs, path::Path, time::Duration};
fn run(args: &[&str], timeout: u64) -> Result<Vec<u8>> {
    run_process(
        &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        None,
        Duration::from_secs(timeout),
    )
}
fn guest_run(
    g: &GuestConfig,
    args: &[&str],
    input: Option<&[u8]>,
    timeout: u64,
) -> Result<Vec<u8>> {
    let mut argv = vec![
        "runuser".into(),
        "-u".into(),
        g.user.clone(),
        "--".into(),
        "env".into(),
        "-i".into(),
        format!("HOME={}", g.home),
        format!("USER={}", g.user),
        format!("LOGNAME={}", g.user),
        format!("PATH={}/.local/bin:/usr/local/bin:/usr/bin:/bin", g.home),
        "SHELL=/bin/bash".into(),
        "DISABLE_AUTOUPDATER=1".into(),
        "DISPLAY=:99".into(),
        "/bin/bash".into(),
        "-c".into(),
        "cd \"$HOME\" && exec \"$@\"".into(),
        "--".into(),
    ];
    argv.extend(args.iter().map(|s| s.to_string()));
    run_process(&argv, input, Duration::from_secs(timeout))
}
fn write_root(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    if let Ok(m) = std::fs::symlink_metadata(path) {
        ensure!(
            m.is_file() && !m.file_type().is_symlink(),
            "Refusing non-regular provisioning file"
        );
    }
    let parent = path.parent().context("File has no parent")?;
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    use std::io::Write;
    file.write_all(data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    let _ = mode;
    file.as_file().sync_all()?;
    file.persist(path)?;
    // The rename itself must be durable before bootstrap can snapshot/reboot
    // the VM. fsync on the temporary inode alone does not persist its new name.
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}
fn ensure_root() -> Result<()> {
    ensure!(
        cfg!(target_os = "linux") && bootstrap::current_uid() == 0,
        "Guest provisioning requires Linux root"
    );
    Ok(())
}
pub fn assert_remote_idle(h: &Host, name: &str) -> Result<()> {
    let bx = h.engine_box();
    if bx.status(name)?["status"] != "Running" {
        return Ok(());
    }
    for provider in ["codex", "claude", "cursor"] {
        let bytes = bx.execute(
            name,
            &["enginebox-guest".into(), "lease".into(), provider.into()],
            Duration::from_secs(15),
        )?;
        let v: Value = serde_json::from_slice(&bytes)?;
        ensure!(
            v["active"] == false,
            "Wait for native agent session before changing workspace state"
        );
    }
    Ok(())
}
pub fn readiness(g: &GuestConfig) -> Result<Value> {
    g.validate()?;
    #[cfg(unix)]
    {
        ensure!(
            unsafe { libc::geteuid() } == g.uid && unsafe { libc::getegid() } == g.gid,
            "Guest identity mismatch"
        );
    }
    ensure!(
        std::fs::canonicalize(std::env::current_dir()?)? == std::fs::canonicalize(&g.workspace)?,
        "Guest workspace mismatch"
    );
    let passwd = String::from_utf8(run(&["getent", "passwd", &g.uid.to_string()], 5)?)?;
    let fields = passwd.trim().split(':').collect::<Vec<_>>();
    ensure!(
        fields.len() >= 7 && fields[0] == g.user && fields[5] == g.home,
        "Guest username/home mismatch"
    );
    run(
        &[
            "systemctl",
            "is-active",
            "--quiet",
            "enginebox-identity.service",
        ],
        5,
    )?;
    let id = std::fs::read_to_string("/var/lib/enginebox/instance-uuid")?;
    let machine = std::fs::read_to_string("/etc/machine-id")?;
    ensure!(
        !id.trim().is_empty() && id.trim() == machine.trim(),
        "Clone machine identity mismatch"
    );
    let pid1 = String::from_utf8(run(
        &[
            "busctl",
            "call",
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.DBus.Peer",
            "GetMachineId",
        ],
        5,
    )?)?;
    ensure!(
        pid1.trim() == format!("s \"{}\"", machine.trim()),
        "PID1 retains stale machine identity; rebuild template"
    );
    ensure!(
        Path::new("/etc/cloud/cloud-init.disabled").is_file(),
        "Completed cloud-init must be disabled"
    );
    let manifest: Value = serde_json::from_slice(&std::fs::read(
        "/opt/enginebox-runtime/installed-versions.json",
    )?)?;
    ensure!(
        manifest["user"] == g.user && manifest["clients"].is_object(),
        "Preparation manifest does not match configured guest"
    );
    let mut clients = Vec::new();
    for (command, details) in manifest["clients"].as_object().unwrap() {
        if !["codex", "claude", "agent", "node", "npm"].contains(&command.as_str()) {
            continue;
        }
        ensure!(
            details["returncode"] == 0
                && details["version"].as_str().is_some_and(|s| !s.is_empty()),
            "Prepared client unavailable"
        );
        run(
            &[
                "bash",
                "-lc",
                &format!("command -v {} >/dev/null", shell_quote(command)),
            ],
            5,
        )?;
        if ["codex", "claude", "agent"].contains(&command.as_str()) {
            clients.push(command.clone());
        }
    }
    let browser = !manifest["playwright_mcp"].is_null();
    if browser {
        let exe = std::fs::read_to_string("/opt/enginebox-browser/current/chromium-path.txt")?;
        let p = Path::new(exe.trim());
        ensure!(
            p.is_absolute() && p.is_file(),
            "Prepared browser executable unavailable"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            ensure!(
                p.metadata()?.permissions().mode() & 0o111 != 0,
                "Browser executable not executable"
            );
        }
    }
    let addresses: Value =
        serde_json::from_slice(&run(&["ip", "-j", "-4", "address", "show"], 5)?)?;
    let mut ips = Vec::new();
    for iface in addresses.as_array().context("Invalid network inventory")? {
        for address in iface["addr_info"].as_array().into_iter().flatten() {
            if address["scope"] == "global"
                && let Some(ip) = address["local"].as_str()
                && ip
                    .parse::<std::net::Ipv4Addr>()
                    .is_ok_and(|i| !i.is_loopback())
            {
                ips.push(ip.to_string());
            }
        }
    }
    let routes: Value =
        serde_json::from_slice(&run(&["ip", "-j", "-4", "route", "show", "default"], 5)?)?;
    ensure!(
        !ips.is_empty() && routes.as_array().is_some_and(|a| !a.is_empty()),
        "Guest IPv4/default route not ready"
    );
    ensure!(
        ("example.com", 443).to_socket_addrs()?.any(|a| a.is_ipv4()),
        "Guest DNS unavailable"
    );
    clients.sort();
    Ok(
        json!({"ready":true,"ipv4":ips,"native_clients":clients,"browser":browser,"dns":true,"identity":true}),
    )
}
fn identity() -> Result<String> {
    let id = std::fs::read_to_string("/sys/class/dmi/id/product_uuid")?
        .trim()
        .replace('-', "")
        .to_ascii_lowercase();
    ensure!(
        id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()) && id != "0".repeat(32),
        "No usable VM UUID; refusing shared identity"
    );
    Ok(id)
}
pub fn guest_identity() -> Result<Value> {
    ensure_root()?;
    ensure_prepared_guest()?;
    let id = identity()?;
    let marker = Path::new("/var/lib/enginebox/instance-uuid");
    if std::fs::read_to_string(marker).is_ok_and(|s| s.trim() == id) {
        return Ok(json!({"changed":false}));
    }
    write_root(
        Path::new("/etc/machine-id"),
        format!("{id}\n").as_bytes(),
        0o444,
    )?;
    let dbus = Path::new("/var/lib/dbus/machine-id");
    if dbus.exists() && !dbus.is_symlink() {
        write_root(dbus, format!("{id}\n").as_bytes(), 0o444)?;
    }
    let hostname = format!("ebox-{}", &id[..12]);
    write_root(
        Path::new("/etc/hostname"),
        format!("{hostname}\n").as_bytes(),
        0o644,
    )?;
    #[cfg(unix)]
    unsafe {
        ensure!(
            libc::sethostname(hostname.as_ptr().cast(), hostname.len() as _) == 0,
            "Cannot set clone hostname"
        );
    }
    let hosts = std::fs::read_to_string("/etc/hosts")?;
    let mut lines = hosts
        .lines()
        .filter(|l| l.split_whitespace().next() != Some("127.0.1.1"))
        .collect::<Vec<_>>()
        .join("\n");
    lines.push_str(&format!("\n127.0.1.1\t{hostname}\n"));
    write_root(Path::new("/etc/hosts"), lines.as_bytes(), 0o644)?;
    if Path::new("/usr/bin/ssh-keygen").is_file() {
        for item in std::fs::read_dir("/etc/ssh")? {
            let item = item?;
            let name = item.file_name().to_string_lossy().to_string();
            if name.starts_with("ssh_host_") && name.contains("_key") {
                ensure!(item.file_type()?.is_file(), "Unexpected SSH host key entry");
                std::fs::remove_file(item.path())?;
            }
        }
        run(&["ssh-keygen", "-A"], 30)?;
    }
    write_root(marker, format!("{id}\n").as_bytes(), 0o644)?;
    Ok(json!({"changed":true}))
}
pub fn reset_base_identity() -> Result<Value> {
    ensure_root()?;
    ensure_prepared_guest()?;
    write_root(Path::new("/etc/machine-id"), b"", 0o444)?;
    let dbus = Path::new("/var/lib/dbus/machine-id");
    if dbus.exists() && !dbus.is_symlink() {
        write_root(dbus, b"", 0o444)?;
    }
    Ok(json!({"reset":true}))
}
fn ensure_prepared_guest() -> Result<()> {
    ensure!(
        Path::new("/var/lib/enginebox/instance-uuid").is_file()
            && std::fs::read_to_string("/etc/systemd/system/enginebox-identity.service")
                .is_ok_and(|s| s.contains("/usr/local/bin/enginebox-host guest-identity")),
        "Identity operations require an Engine Box prepared guest; refusing host identity changes"
    );
    Ok(())
}
fn contains_secret(v: &Value) -> bool {
    match v {
        Value::Object(m) => m.iter().any(|(k, v)| {
            let k = k
                .to_lowercase()
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .collect::<String>();
            ([
                "accesstoken",
                "refreshtoken",
                "apikey",
                "token",
                "authorization",
                "oauthaccount",
                "clientsecret",
                "claudecodeoauthtoken",
                "anthropicapikey",
                "cursorapikey",
                "openaiapikey",
            ]
            .contains(&k.as_str())
                && !v.is_null()
                && v != false
                && v != "")
                || contains_secret(v)
        }),
        Value::Array(a) => a.iter().any(contains_secret),
        _ => false,
    }
}
pub fn clean_base_check(home: &Path) -> Result<Value> {
    ensure!(home.is_absolute(), "Absolute home required");
    let mut blocked = Vec::new();
    let backups = ".local/state/enginebox/browser-migrations";
    match std::fs::symlink_metadata(home.join(backups)) {
        Ok(_) => blocked.push(backups.to_owned()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    for relative in [
        ".codex/auth.json",
        ".claude/.credentials.json",
        ".cursor/auth.json",
        ".config/cursor/auth.json",
        ".git-credentials",
        ".netrc",
        ".aws/credentials",
    ] {
        if home.join(relative).exists() {
            blocked.push(relative.to_owned());
        }
    }
    let mut paths = vec![home.join(".claude.json")];
    for folder in [".codex", ".claude", ".cursor", ".config/cursor"] {
        if let Ok(entries) = std::fs::read_dir(home.join(folder)) {
            for p in entries.flatten().map(|e| e.path()) {
                if p.extension().is_some_and(|s| s == "json") {
                    paths.push(p);
                }
            }
        }
    }
    for p in paths {
        if let Ok(data) = std::fs::read(&p)
            && let Ok(v) = serde_json::from_slice::<Value>(&data)
            && contains_secret(&v)
        {
            blocked.push(p.strip_prefix(home)?.display().to_string());
        }
    }
    if let Ok(entries) = std::fs::read_dir(home.join(".ssh")) {
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.starts_with("id_") && !n.ends_with(".pub") && e.path().is_file() {
                blocked.push(format!(".ssh/{n}"));
            }
        }
    }
    for root in [home.join(".local/share/enginebox"), home.join(".enginebox")] {
        check_profiles(&root, home, &mut blocked, 0)?;
    }
    ensure!(
        blocked.is_empty(),
        "Refusing clean-base snapshot: credential/account/browser state found at {}",
        blocked.join(", ")
    );
    Ok(json!({"clean":true}))
}
fn check_profiles(p: &Path, home: &Path, blocked: &mut Vec<String>, depth: usize) -> Result<()> {
    if !p.exists() {
        return Ok(());
    }
    ensure!(depth < 32, "Excessively nested browser state");
    if p.is_symlink() {
        blocked.push(p.strip_prefix(home)?.display().to_string());
        return Ok(());
    }
    if p.is_dir() {
        let entries = std::fs::read_dir(p)?.collect::<std::io::Result<Vec<_>>>()?;
        if p.file_name().is_some_and(|n| n == "profile") && !entries.is_empty() {
            blocked.push(p.strip_prefix(home)?.display().to_string());
        }
        for entry in entries {
            if entry.path().is_dir() {
                check_profiles(&entry.path(), home, blocked, depth + 1)?;
            }
        }
    }
    Ok(())
}
fn apparmor_bwrap() -> Result<()> {
    if !Path::new("/sys/kernel/security/apparmor").exists() {
        return Ok(());
    }
    let path = Path::new("/etc/apparmor.d/bwrap-userns-restrict");
    for mode in ["disable", "force-complain"] {
        let p = Path::new("/etc/apparmor.d")
            .join(mode)
            .join("bwrap-userns-restrict");
        ensure!(
            std::fs::symlink_metadata(&p).is_err(),
            "bwrap profile has local mode override; reconcile explicitly"
        );
    }
    if let Ok(m) = std::fs::symlink_metadata(path) {
        ensure!(
            m.is_file() && !m.file_type().is_symlink(),
            "Refusing non-regular bwrap profile"
        );
    } else {
        write_root(
            path,
            &std::fs::read("/usr/share/apparmor/extra-profiles/bwrap-userns-restrict")?,
            0o644,
        )?;
    }
    run(&["apparmor_parser", "-r", path.to_str().unwrap()], 30)?;
    Ok(())
}
pub fn prepare_guest(c: &Value) -> Result<Value> {
    ensure_root()?;
    let c = bootstrap::normalize(c.clone())?;
    let g = bootstrap::guest(&c)?;
    let os = std::fs::read_to_string("/etc/os-release")?;
    ensure!(
        os.lines().any(|s| s == "ID=ubuntu")
            && os
                .lines()
                .any(|s| s == "VERSION_ID=\"24.04\"" || s == "VERSION_ID=24.04"),
        "Guest setup supports Ubuntu24.04 only"
    );
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        _ => bail!("Unsupported guest architecture"),
    };
    let apt = |packages: &[&str]| -> Result<()> {
        let mut args = vec![
            "env",
            "DEBIAN_FRONTEND=noninteractive",
            "NEEDRESTART_MODE=l",
            "apt-get",
            "install",
            "-y",
            "--no-install-recommends",
        ];
        args.extend_from_slice(packages);
        run(&args, 1800)?;
        Ok(())
    };
    run(&["apt-get", "update", "-qq"], 600)?;
    apt(&[
        "ca-certificates",
        "curl",
        "git",
        "ripgrep",
        "jq",
        "build-essential",
        "tmux",
        "xz-utils",
        "apparmor",
    ])?;
    let clients = c["agents"]["install"].as_array().unwrap();
    let enabled = |p: &str| clients.iter().any(|v| v == p);
    if enabled("codex") || enabled("cursor") {
        apt(&["bubblewrap", "apparmor-profiles", "apparmor-utils"])?;
        apparmor_bwrap()?;
    }
    if let Ok(passwd) = run(&["getent", "passwd", &g.user], 15) {
        let line = String::from_utf8(passwd)?;
        let fields = line.trim().split(':').collect::<Vec<_>>();
        ensure!(
            fields.len() >= 7
                && fields[2] == g.uid.to_string()
                && fields[3] == g.gid.to_string()
                && fields[5] == g.home,
            "Existing guest identity differs; refusing to change it"
        );
    } else {
        if run(&["getent", "group", &g.gid.to_string()], 15).is_err() {
            run(&["groupadd", "--gid", &g.gid.to_string(), &g.user], 15)?;
        }
        run(
            &[
                "useradd",
                "--create-home",
                "--uid",
                &g.uid.to_string(),
                "--gid",
                &g.gid.to_string(),
                "--home-dir",
                &g.home,
                "--shell",
                "/bin/bash",
                &g.user,
            ],
            30,
        )?;
    }
    for p in [
        &g.home,
        &g.workspace,
        &format!("{}/.local", g.home),
        &format!("{}/.local/bin", g.home),
        &format!("{}/.local/share", g.home),
        &format!("{}/.cache", g.home),
    ] {
        run(
            &[
                "install",
                "-d",
                "-o",
                &g.uid.to_string(),
                "-g",
                &g.gid.to_string(),
                "-m",
                if p == &g.home { "0700" } else { "0755" },
                p,
            ],
            15,
        )?;
    }
    let identity_unit = "[Unit]\nDescription=Engine Box clone identity\nDefaultDependencies=no\nAfter=local-fs.target\nBefore=network-pre.target systemd-networkd.service NetworkManager.service ssh.service\nWants=network-pre.target\n[Service]\nType=oneshot\nExecStart=/usr/local/bin/enginebox-host guest-identity\nRemainAfterExit=yes\n[Install]\nWantedBy=multi-user.target\n";
    write_root(
        Path::new("/etc/systemd/system/enginebox-identity.service"),
        identity_unit.as_bytes(),
        0o644,
    )?;
    write_root(Path::new("/etc/netplan/99-enginebox.yaml"),b"network:\n  version: 2\n  ethernets:\n    enginebox:\n      match:\n        name: en*\n      dhcp4: true\n      dhcp-identifier: mac\n",0o600)?;
    run(&["systemctl", "daemon-reload"], 30)?;
    run(&["systemctl", "enable", "enginebox-identity.service"], 30)?;
    if !Path::new("/var/lib/enginebox/instance-uuid").exists() {
        write_root(
            Path::new("/var/lib/enginebox/instance-uuid"),
            identity()?.as_bytes(),
            0o644,
        )?;
    }
    let temp = tempfile::tempdir()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o755))?;
    }
    if run(
        &[
            "node",
            "-e",
            "process.exit(Number(process.versions.node.split('.')[0]) >= 18 ? 0 : 1)",
        ],
        15,
    )
    .is_err()
    {
        let version = "22.23.3";
        let archive = format!("node-v{version}-linux-{arch}.tar.xz");
        let sums = temp.path().join("SHASUMS256.txt");
        let file = temp.path().join(&archive);
        run(
            &[
                "curl",
                "-fsSL",
                &format!("https://nodejs.org/dist/v{version}/SHASUMS256.txt"),
                "-o",
                sums.to_str().unwrap(),
            ],
            120,
        )?;
        run(
            &[
                "curl",
                "-fsSL",
                &format!("https://nodejs.org/dist/v{version}/{archive}"),
                "-o",
                file.to_str().unwrap(),
            ],
            300,
        )?;
        use sha2::{Digest, Sha256};
        let actual = format!("{:x}", Sha256::digest(std::fs::read(&file)?));
        let sums = std::fs::read_to_string(sums)?;
        ensure!(
            sums.lines().any(|l| {
                let p = l.split_whitespace().collect::<Vec<_>>();
                p.len() == 2 && p[0] == actual && p[1] == archive
            }),
            "Node archive checksum mismatch"
        );
        run(
            &[
                "tar",
                "-xJf",
                file.to_str().unwrap(),
                "-C",
                "/usr/local",
                "--strip-components=1",
            ],
            120,
        )?;
    }
    for (provider, command, url, interpreter, extras) in [
        (
            "codex",
            "codex",
            "https://chatgpt.com/codex/install.sh",
            "sh",
            vec!["--release", CODEX_VERSION],
        ),
        (
            "claude",
            "claude",
            "https://claude.ai/install.sh",
            "bash",
            vec![CLAUDE_VERSION],
        ),
        (
            "cursor",
            "agent",
            "https://cursor.com/install",
            "bash",
            vec![],
        ),
    ] {
        if !enabled(provider) {
            continue;
        }
        if let Ok(version) = guest_run(&g, &[command, "--version"], None, 30) {
            verify_qualified_version(provider, &String::from_utf8(version)?)?;
            continue;
        }
        let file = temp.path().join(format!("{command}-install"));
        run(&["curl", "-fsSL", url, "-o", file.to_str().unwrap()], 120)?;
        let mut args = vec![
            "env",
            "CODEX_NON_INTERACTIVE=1",
            interpreter,
            file.to_str().unwrap(),
        ];
        args.extend(extras);
        guest_run(&g, &args, None, 600)?;
        let version = guest_run(&g, &[command, "--version"], None, 30)?;
        verify_qualified_version(provider, &String::from_utf8(version)?)?;
    }
    if enabled("cursor") {
        run(
            &[
                "enginebox-guest",
                "cursor-sandbox",
                "--home",
                &g.home,
                "--user",
                &g.user,
                "--workspace",
                &g.workspace,
                "--apply",
            ],
            120,
        )?;
    }
    if enabled("codex") {
        guest_run(
            &g,
            &[
                "bash",
                "-c",
                "cd \"$1\" && shift && exec \"$@\"",
                "--",
                &g.workspace,
                "codex",
                "-c",
                "sandbox_mode=\"workspace-write\"",
                "sandbox",
                "--",
                "/bin/true",
            ],
            None,
            60,
        )?;
    }
    let profile = format!(
        "export PATH=\"$HOME/.local/bin:$PATH\"\nexport DISABLE_AUTOUPDATER=1\n{}",
        if c["modules"]["desktop"] == true {
            "export DISPLAY=:99\n"
        } else {
            ""
        }
    );
    write_root(
        Path::new("/etc/profile.d/enginebox-native.sh"),
        profile.as_bytes(),
        0o644,
    )?;
    if c["modules"]["browser"] == true {
        prepare_browser()?;
    }
    if c["modules"]["desktop"] == true {
        apt(&[
            "xvfb",
            "openbox",
            "xterm",
            "dbus-x11",
            "scrot",
            "xdotool",
            "x11vnc",
            "novnc",
            "websockify",
            "fonts-dejavu-core",
        ])?;
        prepare_desktop(&g)?;
    }
    guest_run(
        &g,
        &["enginebox-host", "setup-user-mcp"],
        Some(&serde_json::to_vec(&c)?),
        180,
    )?;
    let mut versions = serde_json::Map::new();
    for command in ["codex", "claude", "agent", "node", "npm"] {
        if let Ok(bytes) = guest_run(&g, &[command, "--version"], None, 30) {
            versions.insert(
                command.into(),
                json!({"version":String::from_utf8_lossy(&bytes).trim(),"returncode":0}),
            );
        }
    }
    let mut manifest = json!({"recorded_at":chrono::Utc::now(),"user":g.user,"architecture":std::env::consts::ARCH,"clients":versions});
    let browser =
        Path::new("/opt/enginebox-browser/current/node_modules/@playwright/mcp/package.json");
    if browser.exists() {
        let v: Value = serde_json::from_slice(&std::fs::read(browser)?)?;
        manifest["playwright_mcp"] = v["version"].clone();
    }
    write_root(
        Path::new("/opt/enginebox-runtime/installed-versions.json"),
        &serde_json::to_vec_pretty(&manifest)?,
        0o644,
    )?;
    if Path::new("/usr/bin/cloud-init").exists()
        && !Path::new("/etc/cloud/cloud-init.disabled").exists()
    {
        run(&["cloud-init", "status", "--wait"], 180)?;
        write_root(Path::new("/etc/cloud/cloud-init.disabled"), b"", 0o644)?;
    }
    Ok(json!({"prepared":true,"native_clients":clients}))
}
fn prepare_browser() -> Result<()> {
    let version = "0.0.83";
    let release = format!("/opt/enginebox-browser/releases/{version}");
    std::fs::create_dir_all(&release)?;
    std::fs::create_dir_all("/opt/enginebox-browser/chromium")?;
    let ready = Path::new(&release).join("ready");
    if !ready.exists() {
        run(
            &[
                "npm",
                "install",
                "--prefix",
                &release,
                "--save-exact",
                "--omit=dev",
                "--no-audit",
                "--no-fund",
                &format!("@playwright/mcp@{version}"),
            ],
            600,
        )?;
        run(
            &[
                "env",
                "PLAYWRIGHT_BROWSERS_PATH=/opt/enginebox-browser/chromium",
                &format!("{release}/node_modules/.bin/playwright"),
                "install",
                "--with-deps",
                "chromium",
            ],
            900,
        )?;
        let path = run(
            &[
                "env",
                "PLAYWRIGHT_BROWSERS_PATH=/opt/enginebox-browser/chromium",
                "node",
                "-e",
                "console.log(require(process.argv[1]).chromium.executablePath())",
                &format!("{release}/node_modules/playwright"),
            ],
            30,
        )?;
        let text = String::from_utf8(path)?;
        ensure!(
            Path::new(text.trim()).is_file(),
            "Chromium executable missing after installation"
        );
        write_root(
            &Path::new(&release).join("chromium-path.txt"),
            text.as_bytes(),
            0o644,
        )?;
        write_root(&ready, b"", 0o644)?;
    }
    let current = Path::new("/opt/enginebox-browser/current");
    if let Ok(m) = std::fs::symlink_metadata(current) {
        ensure!(
            m.file_type().is_symlink(),
            "Browser current path belongs to an operator"
        );
    }
    #[cfg(unix)]
    {
        let next =
            Path::new("/opt/enginebox-browser").join(format!(".current-{}", uuid::Uuid::new_v4()));
        std::os::unix::fs::symlink(&release, &next)?;
        std::fs::rename(next, current)?;
    }
    if Path::new("/sys/kernel/security/apparmor").exists() {
        let exe = std::fs::read_to_string(current.join("chromium-path.txt"))?;
        ensure!(
            !exe.trim().contains(['"', '\n', '\r', '\\']),
            "Invalid browser executable path"
        );
        let profile = format!(
            "abi <abi/4.0>,\ninclude <tunables/global>\nprofile enginebox-chromium \"{}\" flags=(unconfined) {{\n  userns,\n}}\n",
            exe.trim()
        );
        let path = Path::new("/etc/apparmor.d/enginebox-chromium");
        if path.exists() {
            ensure!(
                std::fs::read_to_string(path)? == profile,
                "Existing Chromium policy differs; reconcile explicitly"
            );
        } else {
            write_root(path, profile.as_bytes(), 0o644)?;
        }
        run(&["apparmor_parser", "-r", path.to_str().unwrap()], 30)?;
    }
    Ok(())
}
fn prepare_desktop(g: &GuestConfig) -> Result<()> {
    for (name, command, dependencies) in [
        (
            "x",
            "/usr/bin/Xvfb :99 -screen 0 1440x900x24 -nolisten tcp",
            "",
        ),
        (
            "desktop",
            "/usr/bin/dbus-run-session -- /usr/bin/openbox-session",
            "Requires=enginebox-x.service\nAfter=enginebox-x.service\n",
        ),
        (
            "vnc",
            "/usr/bin/x11vnc -display :99 -localhost -forever -shared -nopw -rfbport 5900",
            "Requires=enginebox-x.service\nAfter=enginebox-x.service\n",
        ),
        (
            "web",
            "/usr/bin/websockify --web=/usr/share/novnc 127.0.0.1:6080 localhost:5900",
            "Requires=enginebox-vnc.service\nAfter=enginebox-vnc.service\n",
        ),
    ] {
        let unit = format!(
            "[Unit]\nDescription=Engine Box {name}\n{dependencies}[Service]\nUser={}\nEnvironment=\"HOME={}\"\nEnvironment=DISPLAY=:99\nExecStart={command}\nRestart=on-failure\n[Install]\nWantedBy=multi-user.target\n",
            g.user,
            g.home.replace('%', "%%").replace('"', "\\\"")
        );
        write_root(
            &Path::new("/etc/systemd/system").join(format!("enginebox-{name}.service")),
            unit.as_bytes(),
            0o644,
        )?;
    }
    run(&["systemctl", "daemon-reload"], 30)?;
    run(
        &[
            "systemctl",
            "enable",
            "--now",
            "enginebox-x",
            "enginebox-desktop",
            "enginebox-vnc",
            "enginebox-web",
        ],
        60,
    )?;
    Ok(())
}
fn private_user_json(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    ensure!(
        !path.is_symlink(),
        "User configuration must not be a symlink"
    );
    let m = path.metadata()?;
    ensure!(m.len() <= 1024 * 1024, "User configuration exceeds bound");
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}
pub fn setup_user_mcp(c: &Value) -> Result<Value> {
    let c = bootstrap::normalize(c.clone())?;
    let g = bootstrap::guest(&c)?;
    ensure!(
        bootstrap::current_uid() == g.uid,
        "MCP registration must run as guest user"
    );
    let home = Path::new(&g.home);
    let migration = if c["modules"]["browser"] == true {
        let providers = c["agents"]["install"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap())
            .collect::<Vec<_>>();
        crate::browser_migration::migrate(home, &providers)?
    } else {
        json!({"changed":[]})
    };
    let codex_path = home.join(".codex/config.toml");
    let codex: toml::Value = if codex_path.exists() {
        ensure!(!codex_path.is_symlink(), "Codex config cannot be symlink");
        toml::from_str(&std::fs::read_to_string(codex_path)?)?
    } else {
        toml::Value::Table(Default::default())
    };
    let claude = private_user_json(&home.join(".claude.json"))?;
    let cursor_path = home.join(".cursor/mcp.json");
    let mut cursor = private_user_json(&cursor_path)?;
    let mut changed = false;
    for (name, enabled) in [
        (
            "enginebox-computer",
            c["modules"]["guest_computer_mcp"] == true,
        ),
        ("enginebox-browser", c["modules"]["browser"] == true),
    ] {
        if !enabled {
            continue;
        }
        for provider in c["agents"]["install"].as_array().unwrap() {
            let provider = provider.as_str().unwrap();
            let mut args = if name == "enginebox-computer" {
                vec!["/usr/local/bin/enginebox-mcp".into()]
            } else {
                vec![
                    "/usr/local/bin/enginebox-guest".into(),
                    "browser".into(),
                    "--client".into(),
                    provider.to_string(),
                ]
            };
            let env = if name == "enginebox-computer" {
                json!({"ENGINEBOX_GUEST":"1"})
            } else {
                json!({})
            };
            match provider {
                "codex" if codex.get("mcp_servers").and_then(|v| v.get(name)).is_none() => {
                    let mut cmd = vec!["codex".into(), "mcp".into(), "add".into(), name.into()];
                    if name == "enginebox-computer" {
                        cmd.extend(["--env".into(), "ENGINEBOX_GUEST=1".into()]);
                    }
                    cmd.push("--".into());
                    cmd.append(&mut args);
                    run_process(&cmd, None, Duration::from_secs(30))?;
                }
                "claude" if claude["mcpServers"].get(name).is_none() => {
                    let mut cmd = vec![
                        "claude".into(),
                        "mcp".into(),
                        "add".into(),
                        "--scope".into(),
                        "user".into(),
                        name.into(),
                    ];
                    if name == "enginebox-computer" {
                        cmd.extend(["--env".into(), "ENGINEBOX_GUEST=1".into()]);
                    }
                    cmd.push("--".into());
                    cmd.append(&mut args);
                    run_process(&cmd, None, Duration::from_secs(30))?;
                }
                "cursor" if cursor["mcpServers"].get(name).is_none() => {
                    ensure!(
                        cursor.is_object(),
                        "Cursor MCP configuration must be an object"
                    );
                    if cursor["mcpServers"].is_null() {
                        cursor["mcpServers"] = json!({});
                    }
                    ensure!(
                        cursor["mcpServers"].is_object(),
                        "Cursor MCP servers must be object"
                    );
                    cursor["mcpServers"][name] =
                        json!({"command":args[0],"args":args[1..],"env":env});
                    changed = true;
                }
                _ => {}
            }
        }
    }
    if changed {
        let parent = cursor_path.parent().unwrap();
        if parent.exists() {
            let metadata = std::fs::symlink_metadata(parent)?;
            ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "Cursor settings directory must be a real directory"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                ensure!(
                    metadata.uid() == bootstrap::current_uid() && metadata.mode() & 0o022 == 0,
                    "Cursor settings directory must be owned and not writable by others"
                );
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        crate::state::private_dir(parent)?;
        crate::state::write_json(&cursor_path, &cursor)?;
    }
    Ok(json!({"configured":true,"browser_migration":migration}))
}

/// Guest-user half of the explicit migration; no provisioning or native login.
pub fn migrate_user_browser_mcp(g: &GuestConfig) -> Result<Value> {
    g.validate()?;
    ensure!(
        bootstrap::current_uid() == g.uid,
        "Browser MCP migration must run as the configured guest user"
    );
    // This only edits this user's known config entries. The node half proves VM
    // custody; do not require or rewrite a legacy identity service here.
    crate::browser_migration::migrate(Path::new(&g.home), &["claude", "codex", "cursor"])
}

fn validate_browser_migration_target(vm: &Value, project: &str, name: &str) -> Result<()> {
    validate_helper_target(vm, project)?;
    let pool = capacity::pool_metadata(vm, &vm["config"]);
    ensure!(
        vm["config"]["user.enginebox.fleet"] == "true"
            && vm["config"]["user.enginebox.clean-base"] != "true"
            && !name.starts_with("ebox-template-")
            && !["ready", "preparing", "quarantined"]
                .contains(&pool["warm_pool_state"].as_str().unwrap_or("")),
        "Browser MCP migration requires an assigned private workspace"
    );
    Ok(())
}

/// Node administrator operation. The caller holds the shared host lifecycle
/// lease; private guest leases also protect each provider across controllers.
pub fn migrate_browser_mcp(project: &str, name: &str, g: &GuestConfig) -> Result<Value> {
    ensure_root()?;
    g.validate()?;
    crate::identifier(name, true)?;
    let bx = crate::transport::EngineBox::new("local", project, g.clone())?;
    let vm = bx.instance(name)?;
    validate_browser_migration_target(&vm, project, name)?;
    let host = Host {
        target: "local".into(),
        project: project.into(),
        guest: g.clone(),
        ..Default::default()
    };
    assert_remote_idle(&host, name)?;
    let result: Value = serde_json::from_slice(&bx.execute_with_input(
        name,
        &[
            "/usr/local/bin/enginebox-host".into(),
            "migrate-user-browser-mcp".into(),
        ],
        &serde_json::to_vec(g)?,
        Duration::from_secs(30),
    )?)?;
    Ok(json!({"name":name,"migration":result}))
}

/// Install only native Engine Box helper executables into an already owned VM.
/// The caller holds the host lifecycle lease. This never copies provider homes,
/// changes identity, installs native provider CLIs, or touches running sessions.
pub fn sync_guest_tools(project: &str, name: &str, binary_dir: Option<&Path>) -> Result<Value> {
    ensure_root()?;
    crate::identifier(name, true)?;
    let bx = crate::transport::EngineBox::new("local", project, GuestConfig::default())?;
    let vm = bx.instance(name)?;
    validate_helper_target(&vm, project)?;
    let exe = std::env::current_exe()?;
    let binary_dir = binary_dir.unwrap_or(exe.parent().context("Missing executable parent")?);
    let mut changed = Vec::new();
    for binary in ["enginebox-host", "enginebox-guest", "enginebox-mcp"] {
        let source = binary_dir.join(binary);
        bootstrap::validate_guest_binary(&source)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let m = source.metadata()?;
            ensure!(
                m.uid() == 0 && m.mode() & 0o022 == 0,
                "Release helper must be root-owned and not group/world writable"
            );
        }
    }
    for binary in ["enginebox-host", "enginebox-guest", "enginebox-mcp"] {
        let source = binary_dir.join(binary);
        let destination = format!("/usr/local/bin/{binary}");
        let bytes = std::fs::read(&source)?;
        use sha2::{Digest, Sha256};
        let checksum = format!("{:x}", Sha256::digest(&bytes));
        // The probe is intentionally read-only and fails closed for symlinks,
        // foreign file modes, and an executable that does not identify itself.
        let probe = format!(
            "if test -L {p}; then exit 41; elif test -e {p}; then test -f {p} && test \"$(stat -c %u {p})\" = 0 && test \"$(stat -c %a {p})\" = 755 || exit 42; timeout 15 {p} --version; sha256sum -- {p}; else printf 'missing\\n'; fi",
            p = shell_quote(&destination)
        );
        let current = String::from_utf8(bx.call(&["exec", name, "--", "sh", "-c", &probe])?)?;
        if current.trim() != "missing" {
            ensure!(
                current
                    .lines()
                    .next()
                    .is_some_and(|s| s.starts_with(&format!("{binary} "))),
                "Existing guest helper is unrecognized; inspect before replacement"
            );
            if current
                .lines()
                .any(|l| l.split_whitespace().next() == Some(&checksum))
            {
                continue;
            }
        }
        let temporary = format!("/usr/local/bin/.{binary}-{}", uuid::Uuid::new_v4().simple());
        bx.call(&[
            "file",
            "push",
            source.to_str().context("Invalid release binary path")?,
            &format!("{name}{temporary}"),
            "--mode=0755",
            "--uid=0",
            "--gid=0",
        ])?;
        let staged =
            String::from_utf8(bx.call(&["exec", name, "--", "sha256sum", "--", &temporary])?)?;
        ensure!(
            staged.split_whitespace().next() == Some(&checksum),
            "Guest helper checksum mismatch; staged file retained for inspection"
        );
        bx.call(&[
            "exec",
            name,
            "--",
            "mv",
            "-f",
            "--",
            &temporary,
            &destination,
        ])?;
        changed.push(binary);
    }
    Ok(
        json!({"name":name,"version":env!("CARGO_PKG_VERSION"),"changed":changed,"scope":"native helper executables only"}),
    )
}

pub fn ensure_remote_guest_tools(
    bx: &crate::transport::EngineBox,
    name: &str,
    host_lock_held: bool,
) -> Result<Value> {
    ensure_remote_guest_tools_from(bx, name, host_lock_held, None)
}

/// True when every native helper in the guest reports this release.
pub fn remote_guest_tools_current(bx: &crate::transport::EngineBox, name: &str) -> bool {
    ["enginebox-host", "enginebox-guest", "enginebox-mcp"]
        .iter()
        .all(|binary| {
            bx.execute(
                name,
                &[(*binary).into(), "--version".into()],
                Duration::from_secs(15),
            )
            .is_ok_and(|bytes| {
                String::from_utf8_lossy(&bytes).trim()
                    == format!("{binary} {}", env!("CARGO_PKG_VERSION"))
            })
        })
}

/// Bring the guest's native helpers to this release through the host
/// helper's `sync-guest-tools` (checksummed, atomic, root-owned sources only).
/// `binary_dir` names a release `bin` directory on the host to copy from
/// instead of the host helper's own; the host helper still refuses sources
/// that are not root-owned or are group/world writable. The result's
/// `current` says whether the guest reports this release afterwards: a host
/// helper from an older release copies its own, older helpers.
pub fn ensure_remote_guest_tools_from(
    bx: &crate::transport::EngineBox,
    name: &str,
    host_lock_held: bool,
    binary_dir: Option<&Path>,
) -> Result<Value> {
    if remote_guest_tools_current(bx, name) {
        return Ok(json!({"changed":[],"current":true}));
    }
    let h = Host {
        id: "helper-sync".into(),
        target: bx.host.clone(),
        project: bx.project.clone(),
        guest: bx.guest.clone(),
        ..Default::default()
    };
    let mut args = vec![
        "enginebox-host".into(),
        "sync-guest-tools".into(),
        "--project".into(),
        bx.project.clone(),
        "--name".into(),
        name.into(),
    ];
    if host_lock_held {
        args.push("--already-locked".into());
    }
    if let Some(dir) = binary_dir {
        args.push("--binary-dir".into());
        args.push(dir.to_str().context("Invalid release binary path")?.into());
    }
    let data = crate::transport::host_run(&h, &args, None, Duration::from_secs(180))?;
    let mut report: Value = serde_json::from_slice(&data)?;
    if report.is_object() {
        report["current"] = json!(remote_guest_tools_current(bx, name));
    }
    Ok(report)
}

pub fn validate_helper_target(vm: &Value, project: &str) -> Result<()> {
    let cfg = &vm["config"];
    ensure!(
        vm["type"] == "virtual-machine" && vm["status"] == "Running",
        "Native helper rollout requires a running VM"
    );
    let owned = cfg["user.enginebox.owner"] == project;
    let managed = cfg["user.enginebox.fleet"] == "true";
    let prepared_base = owned
        && cfg["user.enginebox.recipe"]
            .as_str()
            .is_some_and(|s| s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit()));
    ensure!(
        owned && (managed || prepared_base),
        "Native helper rollout requires Engine Box ownership and a fleet workspace or prepared recipe base"
    );
    let state = capacity::pool_metadata(vm, cfg);
    ensure!(
        state["warm_pool_state"] != "quarantined",
        "Quarantined reserves require explicit inspection before helper rollout"
    );
    Ok(())
}

#[cfg(test)]
mod browser_migration_guards {
    use super::*;

    #[test]
    fn explicit_migration_requires_running_owned_private_vm() {
        let base = json!({"type":"virtual-machine","status":"Running","config":{
            "user.enginebox.owner":"test-project", "user.enginebox.fleet":"true"
        }});
        assert!(validate_browser_migration_target(&base, "test-project", "private-01").is_ok());
        for (field, value) in [
            ("user.enginebox.owner", "foreign"),
            ("user.enginebox.fleet", "false"),
            ("user.enginebox.clean-base", "true"),
            ("user.enginebox.pool.state", "ready"),
            ("user.enginebox.pool.state", "preparing"),
            ("user.enginebox.pool.state", "quarantined"),
        ] {
            let mut vm = base.clone();
            vm["config"][field] = json!(value);
            assert!(
                validate_browser_migration_target(&vm, "test-project", "private-01").is_err(),
                "Allowed {field}={value}"
            );
        }
        assert!(
            validate_browser_migration_target(&base, "test-project", "ebox-template-abc").is_err()
        );
        for status in ["Frozen", "Stopped"] {
            let mut vm = base.clone();
            vm["status"] = json!(status);
            assert!(validate_browser_migration_target(&vm, "test-project", "private-01").is_err());
        }
    }

    #[test]
    fn clean_base_rejects_backups_even_when_original_configs_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let backups = dir.path().join(".local/state/enginebox/browser-migrations");
        std::fs::create_dir_all(&backups).unwrap();
        let error = clean_base_check(dir.path()).unwrap_err();
        assert!(error.to_string().contains("browser-migrations"));
    }

    #[test]
    fn clean_base_rejects_exact_delegated_environment_keys() {
        for name in [
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_API_KEY",
            "CURSOR_API_KEY",
            "OPENAI_API_KEY",
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join(".claude.json"),
                serde_json::to_vec(&json!({
                    "mcpServers":{"custom":{"env":{name:"fixture-secret"}}}
                }))
                .unwrap(),
            )
            .unwrap();
            assert!(clean_base_check(dir.path()).is_err(), "Missed {name}");
            assert!(!contains_secret(&json!({name:""})));
        }
        assert!(!contains_secret(
            &json!({"customTokenLabel":"not a known credential"})
        ));
    }
}

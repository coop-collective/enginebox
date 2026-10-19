use crate::release::{Installation, atomic_json};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Copy, Debug, clap::ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    Operator,
    Controller,
    IncusHost,
}
#[derive(Debug, Serialize)]
pub struct Dependency {
    pub name: String,
    pub present: bool,
    pub command: Vec<String>,
    pub explanation: String,
}
#[derive(Debug, Serialize)]
pub struct BootstrapPlan {
    pub operating_system: String,
    pub architecture: String,
    pub role: Role,
    pub dependencies: Vec<Dependency>,
    pub compute_backend: String,
    pub changes_disks: bool,
    pub changes_firewall: bool,
}
fn has(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}
pub fn plan(role: Role) -> Result<BootstrapPlan> {
    let os = std::env::consts::OS;
    if matches!(role, Role::IncusHost) {
        ensure!(
            os == "linux",
            "Incus compute hosts require Linux; macOS/Windows install operator/controller tools and may control a separately provisioned Linux host"
        );
    }
    let mut dependencies = vec![];
    if !matches!(role, Role::Operator) {
        let node_ok = Command::new("node")
            .arg("--version")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|v| semver::Version::parse(v.trim().trim_start_matches('v')).ok())
            .is_some_and(|v| v >= semver::Version::new(22, 12, 0));
        let (node_command, explanation) = match os {
            "linux" if has("apt-get") => (
                vec![
                    "sudo",
                    "apt-get",
                    "install",
                    "-y",
                    "nodejs",
                    "openssh-client",
                ],
                "Install distribution Node.js and OpenSSH; recheck Node >=22.12, never add an unreviewed package repository automatically.",
            ),
            "linux" if has("dnf") => (
                vec!["sudo", "dnf", "install", "-y", "nodejs", "openssh-clients"],
                "Install distribution Node.js and OpenSSH; Node must be >=22.12.",
            ),
            "macos" => (
                vec!["brew", "install", "node@24"],
                "Requires an existing Homebrew installation; use the LTS Node binary from brew's printed prefix if it is not on PATH.",
            ),
            "windows" => (
                vec![
                    "winget",
                    "install",
                    "--id",
                    "OpenJS.NodeJS.LTS",
                    "--exact",
                    "--accept-package-agreements",
                    "--accept-source-agreements",
                ],
                "Install official Node LTS through Windows Package Manager; reopen terminal after install if PATH changed.",
            ),
            _ => (
                vec![],
                "Install Node.js >=22.12 with your operating system package manager.",
            ),
        };
        dependencies.push(Dependency {
            name: "Node.js >=22.12 (dashboard)".into(),
            present: node_ok,
            command: node_command.into_iter().map(Into::into).collect(),
            explanation: explanation.into(),
        });
        dependencies.push(Dependency{name:"OpenSSH client".into(),present:Command::new("ssh").arg("-V").output().is_ok_and(|v|v.status.success()),command:vec![],explanation:"Used to reach Linux fleet hosts. Linux distro install above includes it; on Windows enable the optional OpenSSH Client feature if missing.".into()});
    }
    if matches!(role, Role::IncusHost) {
        let command = if has("apt-get") {
            vec![
                "sudo",
                "apt-get",
                "install",
                "-y",
                "incus",
                "qemu-system-x86",
                "qemu-utils",
            ]
        } else {
            vec![]
        };
        dependencies.push(Dependency{name:"Incus".into(),present:has("incus"),command:command.into_iter().map(Into::into).collect(),explanation:"Install packages only. Engine Box's host plan/apply separately provisions owned pools, profiles and networks after inspecting KVM, capacity and existing configuration; no incus admin init or disk repartition here.".into()});
        dependencies.push(Dependency{name:"KVM device".into(),present:Path::new("/dev/kvm").exists(),command:vec![],explanation:"Hardware virtualization must be enabled by the owner; nested virtualization may require host configuration and a reboot.".into()});
    }
    Ok(BootstrapPlan {
        operating_system: os.into(),
        architecture: std::env::consts::ARCH.into(),
        role,
        dependencies,
        compute_backend: if os == "linux" {
            "incus-linux-vm"
        } else {
            "remote-linux-incus-only"
        }
        .into(),
        changes_disks: false,
        changes_firewall: false,
    })
}
pub fn apply_dependencies(plan: &BootstrapPlan) -> Result<()> {
    for dependency in &plan.dependencies {
        if dependency.present {
            continue;
        }
        if let Some((program, args)) = dependency.command.split_first() {
            let result = Command::new(program)
                .args(args)
                .status()
                .with_context(|| format!("run dependency installer for {}", dependency.name))?;
            ensure!(
                result.success(),
                "dependency installer failed for {}",
                dependency.name
            );
        } else {
            bail!(
                "{} requires operator configuration: {}",
                dependency.name,
                dependency.explanation
            )
        }
    }
    let remaining = crate::bootstrap::plan(plan.role)?;
    ensure!(
        remaining.dependencies.iter().all(|d| d.present),
        "dependency installation completed but a required capability is still missing; inspect a fresh plan before starting services"
    );
    Ok(())
}
#[derive(Debug, Serialize)]
pub struct ServicePlan {
    pub service_manager: String,
    pub definition: PathBuf,
    pub content: String,
    pub activation_commands: Vec<Vec<String>>,
}
fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
fn safe_systemd(value: &Path) -> Result<String> {
    let s = value.to_str().context("non UTF-8 path")?;
    ensure!(
        !s.contains(['\n', '\r', '\0', '%']),
        "service paths cannot include controls or percent substitutions"
    );
    Ok(format!(
        "\"{}\"",
        s.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}
pub fn service_plan(install: &Installation, config: &Path) -> Result<ServicePlan> {
    let active = install
        .active()?
        .context("activate a staged release first")?;
    let binary = install
        .release_dir(&active.current)
        .join("bin")
        .join(if cfg!(windows) {
            "enginebox-supervisor.exe"
        } else {
            "enginebox-supervisor"
        });
    ensure!(binary.is_file(), "supervisor executable missing");
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .context("user home unavailable")?;
    let args = [
        binary.to_string_lossy().into_owned(),
        "--config".into(),
        config.to_string_lossy().into_owned(),
    ];
    let marker = "Managed by Engine Box versioned installer";
    match std::env::consts::OS {
        "linux" => {
            let definition = home.join(".config/systemd/user/enginebox-supervisor.service");
            let content = format!(
                "# {marker}\n[Unit]\nDescription=Engine Box stable supervisor\nAfter=network-online.target\n[Service]\nExecStart={} --config {}\nRestart=on-failure\nRestartSec=3\nUMask=0077\n# Local Incus nodes require the administrator-configured sudo helper.\n[Install]\nWantedBy=default.target\n",
                safe_systemd(&binary)?,
                safe_systemd(config)?
            );
            Ok(ServicePlan {
                service_manager: "systemd-user".into(),
                definition,
                content,
                activation_commands: vec![
                    vec!["systemctl", "--user", "daemon-reload"]
                        .into_iter()
                        .map(Into::into)
                        .collect(),
                    vec![
                        "systemctl",
                        "--user",
                        "enable",
                        "--now",
                        "enginebox-supervisor.service",
                    ]
                    .into_iter()
                    .map(Into::into)
                    .collect(),
                ],
            })
        }
        "macos" => {
            let definition = home.join("Library/LaunchAgents/org.enginebox.supervisor.plist");
            let arguments = args
                .iter()
                .map(|s| format!("<string>{}</string>", xml(s)))
                .collect::<String>();
            let content = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!-- {marker} -->\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>org.enginebox.supervisor</string><key>ProgramArguments</key><array>{arguments}</array><key>RunAtLoad</key><true/><key>KeepAlive</key><true/><key>Umask</key><integer>63</integer></dict></plist>\n"
            );
            let uid = Command::new("id").arg("-u").output()?;
            let domain = format!("gui/{}", String::from_utf8(uid.stdout)?.trim());
            Ok(ServicePlan {
                service_manager: "launchd-user".into(),
                definition: definition.clone(),
                content,
                activation_commands: vec![vec![
                    "launchctl".into(),
                    "bootstrap".into(),
                    domain,
                    definition.to_string_lossy().into_owned(),
                ]],
            })
        }
        "windows" => {
            ensure!(
                !args.iter().any(|s| s.contains('"')),
                "Windows service paths cannot contain quotes"
            );
            let identity = Command::new("whoami.exe")
                .args(["/user", "/fo", "csv", "/nh"])
                .output()?;
            ensure!(
                identity.status.success(),
                "cannot identify Windows service account"
            );
            let identity = String::from_utf8_lossy(&identity.stdout);
            let sid = identity
                .trim()
                .split(',')
                .next_back()
                .unwrap_or("")
                .trim_matches('"');
            ensure!(
                sid.starts_with("S-1-")
                    && sid
                        .chars()
                        .all(|c| c.is_ascii_digit() || c == 'S' || c == '-'),
                "invalid Windows service account SID"
            );
            let definition = install.root.join("etc/enginebox-supervisor-task.xml");
            let content = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n<!-- {marker} -->\n<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\"><Triggers><LogonTrigger><Enabled>true</Enabled></LogonTrigger></Triggers><Principals><Principal id=\"Author\"><UserId>{sid}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals><Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><ExecutionTimeLimit>PT0S</ExecutionTimeLimit><RestartOnFailure><Interval>PT1M</Interval><Count>3</Count></RestartOnFailure></Settings><Actions Context=\"Author\"><Exec><Command>{}</Command><Arguments>--config &quot;{}&quot;</Arguments></Exec></Actions></Task>\n",
                xml(&args[0]),
                xml(&args[2])
            );
            Ok(ServicePlan {
                service_manager: "windows-task-scheduler-user".into(),
                definition: definition.clone(),
                content,
                activation_commands: vec![
                    vec![
                        "schtasks".into(),
                        "/Create".into(),
                        "/TN".into(),
                        "EngineBoxSupervisor".into(),
                        "/XML".into(),
                        definition.to_string_lossy().into_owned(),
                    ],
                    vec![
                        "schtasks".into(),
                        "/Run".into(),
                        "/TN".into(),
                        "EngineBoxSupervisor".into(),
                    ],
                ],
            })
        }
        _ => bail!("unsupported service platform"),
    }
}
pub fn apply_service(plan: &ServicePlan) -> Result<()> {
    crate::release::reject_symlink_components(&plan.definition)?;
    ensure!(
        !plan.definition.exists(),
        "service definition already exists; refuse an implicit replacement or supervisor restart (service maintenance is separate from controller online updates)"
    );
    if plan.service_manager == "windows-task-scheduler-user" {
        let existing = Command::new("schtasks")
            .args(["/Query", "/TN", "EngineBoxSupervisor"])
            .output()?;
        ensure!(
            !existing.status.success(),
            "a scheduled task named EngineBoxSupervisor already exists; refuse replacing it"
        );
    }
    std::fs::create_dir_all(plan.definition.parent().context("missing service parent")?)?;
    if cfg!(windows) {
        let mut data = vec![0xff, 0xfe];
        for unit in plan.content.encode_utf16() {
            data.extend_from_slice(&unit.to_le_bytes());
        }
        std::fs::write(&plan.definition, data)?;
    } else {
        std::fs::write(&plan.definition, &plan.content)?;
    }
    for command in &plan.activation_commands {
        let result = Command::new(&command[0])
            .args(&command[1..])
            .stdin(std::process::Stdio::null())
            .status()?;
        ensure!(
            result.success(),
            "service activation command failed; definition retained for inspection"
        );
    }
    Ok(())
}
/// Initialize private supervisor settings. Secrets remain in a separate owner-provided file.
pub fn write_example_config(install: &Installation) -> Result<PathBuf> {
    let destination = install.root.join("etc/supervisor.example.json");
    if !destination.exists() {
        atomic_json(
            &destination,
            &serde_json::json!({"prefix":install.root,"fleet_config":install.root.join("etc/fleet.toml"),"token_file":install.root.join("etc/api-token"),"environment_file":install.root.join("etc/environment.json"),"listen":"127.0.0.1:8765","dashboard_listen":"127.0.0.1:3000","drain_timeout_seconds":300}),
        )?;
    }
    Ok(destination)
}

/// Publish only a root-owned Linux host helper into sudo's ordinary secure_path.
/// A user-writable installation must never become a privileged executable target.
pub fn expose_host(install: &Installation, apply: bool) -> Result<serde_json::Value> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
        let state = install
            .active()?
            .context("activate the node's pinned release first")?;
        let binary = install
            .release_dir(&state.current)
            .join("bin/enginebox-host");
        crate::release::reject_symlink_components(&binary)?;
        ensure!(binary.is_file(), "host helper executable missing");
        for path in binary.ancestors() {
            let meta = std::fs::metadata(path)?;
            ensure!(
                meta.uid() == 0 && meta.permissions().mode() & 0o022 == 0,
                "privileged host installation must be root-owned and not group/world writable: {}",
                path.display()
            );
        }
        let link = Path::new("/usr/local/bin/enginebox-host");
        crate::release::reject_symlink_components(link.parent().unwrap())?;
        let parent = std::fs::metadata(link.parent().unwrap())?;
        ensure!(
            parent.uid() == 0 && parent.permissions().mode() & 0o022 == 0,
            "/usr/local/bin must be root-owned and not writable by other users"
        );
        if std::fs::symlink_metadata(link).is_ok() {
            ensure!(
                link.is_symlink(),
                "refusing to replace an unrelated enginebox-host executable"
            );
            let previous = std::fs::read_link(link)?;
            ensure!(
                previous.starts_with(install.root.join("releases"))
                    && previous.file_name() == Some(std::ffi::OsStr::new("enginebox-host")),
                "refusing to replace a host link owned by another installation"
            );
        }
        let plan = serde_json::json!({"link":link,"target":binary,"version":state.current,"applied":apply});
        if apply {
            ensure!(
                unsafe { libc::geteuid() } == 0,
                "expose-host --apply requires root for this dedicated node installation"
            );
            let _lock = install.lock()?;
            let pending = link
                .parent()
                .unwrap()
                .join(format!(".enginebox-host-{}", uuid::Uuid::new_v4()));
            symlink(&binary, &pending)?;
            if let Err(error) = std::fs::rename(&pending, link) {
                let _ = std::fs::remove_file(pending);
                return Err(error.into());
            }
        }
        Ok(plan)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (install, apply);
        bail!("privileged Incus host helper installation is only supported on Linux")
    }
}

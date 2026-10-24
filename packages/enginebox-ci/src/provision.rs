//! Private native toolchain installation, sharing the worker's build/runner lock.
use crate::{
    Node, executor,
    model::Shell,
    templates::{Binding, Receipt, Template, digest},
};
use anyhow::{Context, Result, ensure};
use enginebox_core::{
    state,
    transport::{run_command_bounded, run_process},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Operation {
    InstallTemplate,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallRequest {
    pub operation: Operation,
    pub node: Node,
    pub manifest: Template,
    pub attempt: String,
    #[serde(default)]
    pub reinstall: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Outcome {
    pub ok: bool,
    pub phase: String,
    pub error: Option<String>,
    pub receipt: Option<Receipt>,
}
pub fn install_on(request: &InstallRequest) -> Result<Outcome> {
    request.node.validate()?;
    request.manifest.validate()?;
    if request.node.target == "local" {
        return install_at(request, &executor::root()?);
    }
    let argv = vec![
        "ssh".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "StrictHostKeyChecking=yes".into(),
        "-o".into(),
        "ConnectTimeout=10".into(),
        "--".into(),
        request.node.target.clone(),
        "ebox ci execute".into(),
    ];
    let bytes = run_process(
        &argv,
        Some(&serde_json::to_vec(request)?),
        Duration::from_secs(request.manifest.timeout_minutes * 60 + 30),
    )?;
    let outcome: Outcome = serde_json::from_slice(&bytes)
        .context("Upgrade the CI worker to support template installation")?;
    if outcome.ok {
        outcome
            .receipt
            .as_ref()
            .context("Missing installation receipt")?
            .validate(&request.node, &request.manifest)?;
    }
    Ok(outcome)
}
fn template_root(root: &Path, template: &Template) -> PathBuf {
    root.join("templates")
        .join(&template.id)
        .join(template.digest())
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn capacity(root: &Path, template: &Template) -> Result<()> {
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    let memory = system.available_memory();
    #[cfg(target_os = "linux")]
    let memory = system
        .cgroup_limits()
        .map_or(memory, |cgroup| memory.min(cgroup.free_memory));
    ensure!(
        std::thread::available_parallelism()?.get() >= template.minimum.cpu as usize
            && memory >= template.minimum.memory_mib * 1024 * 1024
            && fs2::available_space(root)? >= (template.minimum.disk_mib + 256) * 1024 * 1024,
        "insufficient-capacity"
    );
    Ok(())
}
fn checked_path(prefix: &Path, path: &str) -> Result<PathBuf> {
    let resolved = prefix.join(path).canonicalize()?;
    ensure!(
        resolved.starts_with(prefix.canonicalize()?),
        "Toolchain path escapes installation"
    );
    command_path(&resolved)
}
// Rust canonical Windows paths use a verbatim prefix. PowerShell's script
// authorization/discovery does not consistently accept that prefix in PATH.
// Convert only when the ordinary spelling resolves to the exact same entry.
fn command_path(path: &Path) -> Result<PathBuf> {
    let canonical = path.canonicalize()?;
    #[cfg(windows)]
    {
        let value = canonical
            .to_str()
            .context("Toolchain paths must be Unicode on Windows")?;
        let ordinary = if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
            PathBuf::from(format!(r"\\{unc}"))
        } else {
            PathBuf::from(value.strip_prefix(r"\\?\").unwrap_or(value))
        };
        ensure!(
            ordinary.canonicalize()? == canonical,
            "Toolchain path changes meaning without its verbatim prefix"
        );
        Ok(ordinary)
    }
    #[cfg(not(windows))]
    Ok(canonical)
}
/// Installed paths are private to one manifest digest and may not escape it.
pub fn environment(command: &mut Command, template: &Template, prefix: &Path) -> Result<()> {
    let mut paths = Vec::new();
    for path in &template.paths {
        let path = checked_path(prefix, path)?;
        ensure!(path.is_dir(), "Toolchain PATH entry is not a directory");
        paths.push(path);
    }
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    command
        .env("PATH", std::env::join_paths(paths)?)
        .env("ENGINEBOX_TOOLCHAIN", command_path(prefix)?);
    for (key, value) in &template.environment {
        command.env(key, checked_path(prefix, value)?);
    }
    Ok(())
}
fn script(
    template: &Template,
    prefix: &Path,
    run: &str,
    deadline: Instant,
    with_environment: bool,
) -> Result<Vec<u8>> {
    let file_name = match template.shell {
        Shell::Bash => ".enginebox-step.sh",
        Shell::Pwsh => ".enginebox-step.ps1",
    };
    let source = match template.shell {
        Shell::Bash => format!("set -euo pipefail\n{run}\n"),
        Shell::Pwsh => format!(
            "$ErrorActionPreference = 'Stop'\n$PSNativeCommandUseErrorActionPreference = $true\n{run}\nif ($LASTEXITCODE) {{ exit $LASTEXITCODE }}\n"
        ),
    };
    // The receipt becomes visible only after these fixed files finish executing.
    std::fs::write(prefix.join(file_name), source)?;
    let home = prefix.join(".home");
    state::private_dir(&home)?;
    let mut command = executor::command(
        if template.shell == Shell::Bash {
            "bash"
        } else {
            "pwsh"
        },
        prefix,
        &home,
    );
    if template.shell == Shell::Bash {
        command.args(["--noprofile", "--norc"]);
    } else {
        command.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-File"]);
    }
    // A relative script avoids PowerShell's verbatim Windows path authorization bug.
    command
        .arg(format!("./{file_name}"))
        .env("ENGINEBOX_TOOLCHAIN", command_path(prefix)?)
        .env("ENGINEBOX_ASSETS_DIR", command_path(prefix)?.join("assets"));
    if with_environment {
        environment(&mut command, template, prefix)?;
    }
    run_command_bounded(
        command,
        None,
        deadline
            .checked_duration_since(Instant::now())
            .context("Installation deadline exceeded")?,
        1024 * 1024,
        1024 * 1024,
    )
}
fn verify(
    template: &Template,
    prefix: &Path,
    deadline: Instant,
) -> Result<BTreeMap<String, String>> {
    let mut probes = BTreeMap::new();
    for probe in &template.verify {
        let output = script(template, prefix, &probe.run, deadline, true)?;
        ensure!(
            String::from_utf8_lossy(&output)
                .trim()
                .replace("\r\n", "\n")
                == probe.expected_stdout.trim().replace("\r\n", "\n"),
            "Capability probe returned an unexpected version"
        );
        probes.insert(
            probe.capability.clone(),
            format!("{:x}", Sha256::digest(&output)),
        );
    }
    Ok(probes)
}
fn download(template: &Template, prefix: &Path, deadline: Instant) -> Result<()> {
    let assets = prefix.join("assets");
    state::private_dir(&assets)?;
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 || attempt.url().scheme() != "https" {
                attempt.error("Unsafe or excessive asset redirect")
            } else {
                attempt.follow()
            }
        }))
        .connect_timeout(Duration::from_secs(15))
        .build()?;
    for asset in &template.assets {
        let response = client
            .get(&asset.url)
            .timeout(
                deadline
                    .checked_duration_since(Instant::now())
                    .context("Download deadline exceeded")?,
            )
            .send()?
            .error_for_status()?;
        ensure!(
            response
                .content_length()
                .is_none_or(|size| size <= asset.max_bytes),
            "Asset exceeds size bound"
        );
        let mut reader = response.take(asset.max_bytes + 1);
        let mut output = File::create(assets.join(&asset.name))?;
        let mut hash = Sha256::new();
        let mut size = 0;
        let mut buffer = [0; 65536];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            size += count as u64;
            ensure!(size <= asset.max_bytes, "Asset exceeds size bound");
            hash.update(&buffer[..count]);
            output.write_all(&buffer[..count])?;
        }
        output.sync_all()?;
        ensure!(
            format!("{:x}", hash.finalize()).eq_ignore_ascii_case(&asset.sha256),
            "Asset checksum mismatch"
        );
    }
    Ok(())
}
pub fn install_at(request: &InstallRequest, root: &Path) -> Result<Outcome> {
    request.node.require_local()?;
    request.manifest.validate()?;
    ensure!(
        request.manifest.compatible(&request.node)
            && request.attempt.len() == 64
            && request.attempt.bytes().all(|v| v.is_ascii_hexdigit()),
        "Incompatible template or invalid attempt"
    );
    state::private_dir(root)?;
    let _guard = state::lock(&root.join("worker.lock"), false)?;
    let template = &request.manifest;
    let directory = template_root(root, template);
    state::private_dir(&directory)?;
    let deadline = Instant::now() + Duration::from_secs(template.timeout_minutes * 60);
    let mut phase = "capacity";
    let mut log = directory.join(format!("{}.error.json", request.attempt));
    let result = (|| -> Result<Receipt> {
        capacity(root, template)?;
        let saved = directory.join("receipt.json");
        if !request.reinstall && saved.exists() {
            phase = "verification";
            let mut receipt: Receipt = serde_json::from_value(state::read_json(&saved)?)?;
            ensure!(
                receipt.manifest_digest == template.digest()
                    && receipt.installation.len() == 64
                    && receipt.installation.bytes().all(|v| v.is_ascii_hexdigit()),
                "Invalid saved installation"
            );
            let prefix = directory.join(&receipt.installation);
            ensure!(prefix.is_dir(), "Installed toolchain directory is missing");
            state::private_dir(&prefix)?;
            receipt.probes = verify(template, &prefix, deadline)?;
            receipt.node_binding = digest(&request.node);
            receipt.verified_at = now();
            receipt.validate(&request.node, template)?;
            state::write_json(&saved, &serde_json::to_value(&receipt)?)?;
            return Ok(receipt);
        }
        phase = "preparation";
        let prefix = directory.join(&request.attempt);
        // An interrupted script is never silently replayed with the same attempt.
        ensure!(
            !prefix.try_exists()?,
            "Installation attempt already exists; explicitly retry with a new attempt"
        );
        state::private_dir(&prefix)?;
        log = prefix.join("error.json");
        state::write_json(
            &prefix.join("attempt.json"),
            &serde_json::json!({"attempt":request.attempt,"manifest_digest":template.digest(),"node_binding":digest(&request.node)}),
        )?;
        phase = "download";
        download(template, &prefix, deadline)?;
        phase = "installation";
        script(template, &prefix, &template.install, deadline, false)?;
        phase = "verification";
        let probes = verify(template, &prefix, deadline)?;
        let receipt = Receipt {
            schema_version: 1,
            node_binding: digest(&request.node),
            template_id: template.id.clone(),
            revision: template.revision,
            manifest_digest: template.digest(),
            installation: request.attempt.clone(),
            verified_at: now(),
            probes,
        };
        phase = "receipt";
        state::write_json(&saved, &serde_json::to_value(&receipt)?)?;
        Ok(receipt)
    })();
    match result {
        Ok(receipt) => Ok(Outcome {
            ok: true,
            phase: "ready".into(),
            error: None,
            receipt: Some(receipt),
        }),
        Err(error) => {
            // Keep diagnostics on the private worker; publish authored phase codes only.
            let _ = state::write_json(
                &log,
                &serde_json::json!({"phase":phase,"error":format!("{error:#}").chars().take(16384).collect::<String>()}),
            );
            Ok(Outcome {
                ok: false,
                phase: phase.into(),
                error: Some(format!("{phase}-failed")),
                receipt: None,
            })
        }
    }
}
/// Called under worker.lock before each managed build. Recheck real tool versions.
pub fn ready(binding: &Binding, node: &Node, root: &Path, deadline: Instant) -> Result<PathBuf> {
    binding.manifest.validate()?;
    binding.receipt.validate(node, &binding.manifest)?;
    let directory = template_root(root, &binding.manifest);
    let receipt: Receipt =
        serde_json::from_value(state::read_json(&directory.join("receipt.json"))?)?;
    ensure!(
        receipt.installation == binding.receipt.installation
            && receipt.manifest_digest == binding.receipt.manifest_digest,
        "Toolchain installation was superseded"
    );
    let prefix = directory.join(&receipt.installation);
    ensure!(prefix.is_dir(), "Installed toolchain directory is missing");
    state::private_dir(&prefix)?;
    verify(&binding.manifest, &prefix, deadline)?;
    Ok(prefix)
}

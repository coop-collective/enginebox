#![cfg(unix)]
use std::{fs, os::unix::fs::PermissionsExt, process::Command};

// Exercise the downloaded/bootstrap script as a process. The fixture downloads
// are checksum-verified; only the owned installer binary/curl endpoint are fakes.
// A failed preflight must never reach package installation, staging or activation.
#[test]
fn shell_bootstrap_stops_before_mutation_when_required_network_fails() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path();
    let commands = root.join("commands");
    fs::create_dir(&commands).unwrap();
    let curl = commands.join("curl");
    fs::write(&curl, r#"#!/bin/sh
set -eu
destination=''
while [ "$#" -gt 0 ]; do
  case "$1" in --output) destination=$2; shift 2;; *) url=$1; shift;; esac
done
case "$url" in *.tar.gz) cp "$PROBE_FIXTURE_ARCHIVE" "$destination";; *.json) cp "$PROBE_FIXTURE_MANIFEST" "$destination";; *) exit 91;; esac
"#).unwrap();
    fs::set_permissions(&curl, fs::Permissions::from_mode(0o700)).unwrap();
    let binary = br#"#!/bin/sh
set -eu
shift 2
printf '%s\n' "$*" >> "$PROBE_FIXTURE_AUDIT"
case "$1" in network-check) exit 37;; plan) exit 0;; *) exit 92;; esac
"#;
    let archive = root.join("release.tar.gz");
    let output = flate2::write::GzEncoder::new(
        fs::File::create(&archive).unwrap(),
        flate2::Compression::default(),
    );
    let mut tar = tar::Builder::new(output);
    let mut header = tar::Header::new_gnu();
    header.set_size(binary.len() as u64);
    header.set_mode(0o700);
    header.set_cksum();
    tar.append_data(&mut header, "bin/enginebox-install", &binary[..])
        .unwrap();
    tar.into_inner().unwrap().finish().unwrap();
    let manifest = root.join("manifest.json");
    fs::write(&manifest, "{}").unwrap();
    let prefix = root.join("not-installed");
    let audit = root.join("audit");
    let path = format!("{}:{}", commands.display(), std::env::var("PATH").unwrap());
    let result = Command::new("/bin/sh")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/install.sh"))
        .args([
            "--version",
            "0.3.0",
            "--apply",
            "--install-dependencies",
            "--base-url",
            "https://fixture.invalid/releases",
            "--network-profile",
            "streaming",
            "--require-quic",
            "--with-tunnel",
        ])
        .arg("--prefix")
        .arg(&prefix)
        .arg("--archive-sha256")
        .arg(enginebox_install::release::sha256_file(&archive).unwrap())
        .arg("--manifest-sha256")
        .arg(enginebox_install::release::sha256_file(&manifest).unwrap())
        .env("PATH", path)
        .env("PROBE_FIXTURE_ARCHIVE", &archive)
        .env("PROBE_FIXTURE_MANIFEST", &manifest)
        .env("PROBE_FIXTURE_AUDIT", &audit)
        .output()
        .unwrap();
    assert_eq!(
        result.status.code(),
        Some(37),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!prefix.exists());
    assert_eq!(
        fs::read_to_string(audit).unwrap(),
        "plan --role controller\nnetwork-check --profile streaming --require-quic --with-tunnel\n"
    );
}

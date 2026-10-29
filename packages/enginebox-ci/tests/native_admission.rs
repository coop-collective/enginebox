#![cfg(target_os = "linux")]

use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
};

#[test]
fn capacity_shortage_never_allocates_or_delivers_jit_and_cannot_replay() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let bin = root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let facts = root.path().join("facts.json");
    fs::write(&facts, json!({"cpu_total":8,"memory_total_bytes":64_u64<<30,"memory_available_bytes":64_u64<<30,"disk_free_bytes":200_u64<<30,"pool_total_bytes":200_u64<<30,"pool_used_bytes":0,"instances":[{"project":"another-project","name":"ebox-busy","status":"Running","cpu":4,"memory_bytes":4_u64<<30,"disk_bytes":30_u64<<30,"state_disk_bytes":0,"pool":"other","managed":true}]}).to_string()).unwrap();
    let calls = root.path().join("calls.jsonl");
    let sudo = bin.join("sudo");
    fs::write(
        &sudo,
        format!(
            r#"#!/usr/bin/env python3
import json,sys
with open({calls}, 'a') as f: f.write(json.dumps(sys.argv[1:])+'\n')
assert sys.argv[1:3]==['-n','enginebox-host']
if sys.argv[3:] == ['lock']:
 print('locked',flush=True)
 sys.stdin.buffer.read()
elif sys.argv[3] == 'inventory':
 print(open({facts}).read())
else:
 raise AssertionError('No allocation or guest operation is allowed')
"#,
            calls = serde_json::to_string(&calls).unwrap(),
            facts = serde_json::to_string(&facts).unwrap()
        ),
    )
    .unwrap();
    fs::set_permissions(&sudo, fs::Permissions::from_mode(0o700)).unwrap();
    let fleet = root.path().join("fleet.toml");
    fs::write(
        &fleet,
        r#"schema_version = 1
[[hosts]]
id = "ci-local"
target = "local"
project = "enginebox-ci-test"
pool = "ci"
network = "cibr0"
reserve_cpu = 4
cpu_overcommit = 1.0
reserve_memory_gib = 8
reserve_disk_gib = 20
max_instances = 4
[[hosts.templates]]
id = "ci-linux"
source = "ebox-ci-base"
snapshot = "prepared"
minimum = { cpu = 4, memory_gib = 4, disk_gib = 30 }
recommended = { cpu = 4, memory_gib = 4, disk_gib = 30 }
"#,
    )
    .unwrap();
    let config = root.path().join("worker.json");
    enginebox_core::state::write_json(&config, &json!({"schema":1,"fleet":fleet,"state_root":state,"host_id":"ci-local","template":"ci-linux","resources":{"cpu":4,"memory_gib":4,"disk_gib":30},"runner_version":"2.338.2","layerfs":null,"projects":{}})).unwrap();
    let name = "ebox-ci-11111111111141118111111111111111";
    let request = json!({"schema":1,"name":name,"job_id":99,"repository":"fixture/repo","jitconfig":"never-deliver-this-token","timeout_seconds":60});
    let invoke = || {
        let path = std::env::join_paths(
            std::iter::once(bin.clone())
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_enginebox-ci-worker"))
            .arg("--config")
            .arg(&config)
            .env("PATH", path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(request.to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    let output = invoke();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "waiting_capacity");
    assert_eq!(result["allocation_started"], false);
    assert_eq!(result["cleaned"], true);
    assert!(result["allocation_id"].is_null());
    assert!(!state.join("allocations").exists());
    let before = fs::read_to_string(&calls).unwrap();
    assert_eq!(before.lines().count(), 2);
    assert!(!before.contains("never-deliver-this-token"));
    let receipt =
        fs::read_to_string(state.join("ci-receipts").join(format!("{name}.json"))).unwrap();
    assert!(!receipt.contains("never-deliver-this-token"));
    assert!(!invoke().status.success());
    assert_eq!(fs::read_to_string(calls).unwrap(), before);
}

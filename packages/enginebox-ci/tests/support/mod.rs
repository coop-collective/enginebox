use enginebox_ci::{
    Node,
    model::{Os, architecture},
    templates::Template,
};
use serde_json::json;

pub fn node() -> Node {
    serde_json::from_value(json!({"schema_version":1,"id":"fixture","target":"local","os":Os::current().unwrap(),"architecture":architecture().unwrap(),"image":"unprepared","capabilities":[]})).unwrap()
}
pub fn template() -> Template {
    let (shell, install, verify, capabilities, probes) = if cfg!(windows) {
        (
            "pwsh",
            "if ($env:ENGINEBOX_API_TOKEN -or $env:ENGINEBOX_CI_GITHUB_TOKEN) { throw 'credential leak' }\nNew-Item -ItemType Directory bin | Out-Null\nSet-Content -Path bin/ebox-fixture.ps1 -Value \"Write-Output 'fixture-1'\"",
            "ebox-fixture",
            json!(["fixture-tool", "pwsh"]),
            json!([{"capability":"pwsh","run":"if ($PSVersionTable.PSVersion -lt [version]'7.3') { throw 'PowerShell too old' }; Write-Output 'pwsh-ready'","expected_stdout":"pwsh-ready"}]),
        )
    } else {
        (
            "bash",
            "test -z \"${ENGINEBOX_API_TOKEN:-}\"\ntest -z \"${ENGINEBOX_CI_GITHUB_TOKEN:-}\"\nmkdir bin\nprintf '#!/bin/sh\\nprintf fixture-1\\n' > bin/ebox-fixture\nchmod 700 bin/ebox-fixture",
            "ebox-fixture",
            json!(["fixture-tool"]),
            json!([]),
        )
    };
    let mut probes = probes.as_array().unwrap().clone();
    probes.push(json!({"capability":"fixture-tool","run":verify,"expected_stdout":"fixture-1"}));
    serde_json::from_value(json!({"schema_version":1,"id":"fixture-toolchain","label":"Fixture toolchain","revision":1,"os":Os::current().unwrap(),"architecture":architecture().unwrap(),"image":"fixture-r1","capabilities":capabilities,"minimum":{"cpu":1,"memory_mib":1,"disk_mib":1},"shell":shell,"install":install,"verify":probes,"paths":["bin"],"timeout_minutes":1})).unwrap()
}

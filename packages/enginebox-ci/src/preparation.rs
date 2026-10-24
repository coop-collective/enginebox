//! Required Actions preparation. App credentials stay on the host; the guest only submits an identity.
use anyhow::{Context, Result, ensure};
use enginebox_core::{EngineBox, transport::shell_quote};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Admission {
    pub repository_id: u64,
    pub installation_id: u64,
    pub runner_id: u64,
    pub workflows: Vec<String>,
    pub verification_token: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub repository_id: u64,
    pub installation_id: u64,
    /// SHA-256 of the approved credential-free template/image recipe.
    pub image: String,
    pub pnpm: String,
    pub scripts: String,
}
pub fn validate(profile: &Profile, admission: &Admission) -> Result<()> {
    ensure!(
        profile.repository_id > 0
            && profile.repository_id == admission.repository_id
            && profile.installation_id > 0
            && profile.installation_id == admission.installation_id
            && admission.runner_id > 0
            && !admission.workflows.is_empty()
            && admission.workflows.len() <= 16
            && (1..=16384).contains(&admission.verification_token.len()),
        "Prepared runner admission differs from worker profile"
    );
    ensure!(
        profile.image.len() == 64
            && profile
                .image
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            && profile.pnpm.split('.').count() == 3
            && profile.pnpm.split('.').all(|v| v.parse::<u32>().is_ok())
            && ["run", "deny"].contains(&profile.scripts.as_str()),
        "Invalid preparation recipe"
    );
    Ok(())
}
fn root_args(name: &str, argv: Vec<String>) -> Vec<String> {
    let mut args = vec![
        "exec".into(),
        name.into(),
        "--user=0".into(),
        "--group=0".into(),
        "--".into(),
    ];
    args.extend(argv);
    args
}
pub fn bootstrap(
    bx: &EngineBox,
    name: &str,
    repository: &str,
    admission: &Admission,
    profile: &Profile,
    timeout_seconds: u64,
) -> Result<()> {
    validate(profile, admission)?;
    let policy = json!({"schema":1,"runner":name,"repository":repository,"repository_id":admission.repository_id,
        "root":"/project","protected_preparation":true,"workflows":admission.workflows,
        "recipe":{"schema":1,"recipe":"pnpm-installed-v1","namespace":format!("github-installation-{}",admission.installation_id),
        "repository":admission.repository_id.to_string(),"trust":"repository-read","image":profile.image,"pnpm":profile.pnpm,"scripts":profile.scripts}});
    let payload = json!({"policy":policy,"home":bx.guest.home,"uid":bx.guest.uid,"gid":bx.guest.gid,
        "hook":include_str!("../../enginebox-actions/src/hook.mjs"),"broker":include_str!("../../enginebox-actions/src/broker.py"),"seconds":timeout_seconds+180});
    // This runs only in the exact disposable VM after owned attachment, never on the physical host.
    let script = r#"import json,sys,os,pathlib,subprocess
p=json.load(sys.stdin)
assert p['uid'] > 0 and p['gid'] > 0, 'Prepared runner must be unprivileged'
import pwd,grp,shutil,stat
user=pwd.getpwuid(p['uid']).pw_name
assert shutil.which('setpriv'), 'Prepared image requires util-linux setpriv'
groups=set(os.getgrouplist(user,p['gid']))
for group in ['docker','incus','lxd','libvirt','disk']:
    try:
        assert grp.getgrnam(group).gr_gid not in groups, 'Prepared host-job runner has a privileged group'
    except KeyError:
        pass
for socket in ['/var/run/docker.sock','/var/run/incus/unix.socket','/var/snap/lxd/common/lxd/unix.socket']:
    if os.path.exists(socket):
        info=os.stat(socket)
        assert not (info.st_uid==p['uid'] or info.st_mode & stat.S_IWOTH or (info.st_gid in groups and info.st_mode & stat.S_IWGRP)), 'Prepared host-job runner can access a privileged service'
root=pathlib.Path('/project')
assert not root.is_symlink() and os.path.ismount(root), 'LayerFS mount required'
control=pathlib.Path('/run/enginebox-ci')
control.mkdir(mode=0o755)
requests=control/'requests'
requests.mkdir(mode=0o700)
os.chown(requests,p['uid'],p['gid'])
for name,content in [('policy.json',json.dumps(p['policy'])),('hook.mjs',p['hook']),('broker.py',p['broker']),('job-started.sh','#!/bin/bash\nset -eu\ntimeout --signal=TERM --kill-after=5s 130s node /run/enginebox-ci/hook.mjs\n')]:
    path=control/name
    path.write_text(content)
    path.chmod(0o555 if name.endswith('.sh') else 0o444)
for name in ['work','receipts']:
    path=root/name
    assert not path.exists() and not path.is_symlink(), 'Reusable generation contains prior job state'
    path.mkdir(mode=0o700)
    os.chown(path,p['uid'],p['gid'])
for name in ['caches','caches/packages','caches/metadata','caches/npm','prepared']:
    path=root/name
    assert not path.is_symlink(), 'Cache path must not be a symlink'
    path.mkdir(mode=0o700,exist_ok=True)
    os.chown(path,p['uid'],p['gid'])
private=control/'private'
private.mkdir(mode=0o700)
view=private/'source'
view.mkdir()
subprocess.run(['mount','--bind',str(root),str(view)],check=True)
subprocess.run(['mount','--make-private',str(view)],check=True)
subprocess.run(['mount','--bind',str(root/'prepared'),str(root/'prepared')],check=True)
subprocess.run(['mount','-o','remount,bind,ro,nosuid,nodev',str(root/'prepared')],check=True)
subprocess.Popen(['python3',str(control/'broker.py'),str(control),str(p['seconds'])],stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True)
runner=pathlib.Path(p['home'])/'ci-runner'
assert not runner.exists() and not runner.is_symlink()
for name in ['.runner','.credentials','.credentials_rsaparams']:
    assert not (pathlib.Path('/opt/actions-runner')/name).exists()
subprocess.run(['cp','-a','/opt/actions-runner',str(runner)],check=True)
work=runner/'_work'
assert not work.exists() and not work.is_symlink(), 'Runner base must not contain work data'
work.mkdir()
subprocess.run(['chown','-R',str(p['uid'])+':'+str(p['gid']),str(runner)],check=True)
# JIT work_folder is /project/work; the official runner creates its normal subdirectories there.
"#;
    let args = root_args(name, vec!["python3".into(), "-c".into(), script.into()]);
    bx.run_input(
        &args,
        &serde_json::to_vec(&payload)?,
        Duration::from_secs(60),
    )?;
    Ok(())
}
pub fn launch_setup() -> String {
    format!(
        "test -d ci-runner\nexport ACTIONS_RUNNER_HOOK_JOB_STARTED={}\n",
        shell_quote("/run/enginebox-ci/job-started.sh")
    )
}
pub fn check_identity(
    identity: &Value,
    admission: &Admission,
    name: &str,
    repository: &str,
) -> Result<()> {
    ensure!(
        identity["schema"] == 1
            && identity["runner"] == name
            && identity["repository"] == repository
            && identity["repository_id"] == admission.repository_id
            && identity["run_id"].as_u64().is_some_and(|v| v > 0)
            && identity["run_attempt"]
                .as_u64()
                .is_some_and(|v| v > 0 && v <= 999999999)
            && identity["workflow"]
                .as_str()
                .is_some_and(|w| admission.workflows.iter().any(|v| v == w)),
        "Guest identity does not match prepared admission"
    );
    Ok(())
}
pub fn assigned_job(
    jobs: &[Value],
    identity: &Value,
    admission: &Admission,
    name: &str,
) -> Result<Value> {
    let matching: Vec<_> = jobs
        .iter()
        .filter(|j| j["runner_id"] == admission.runner_id && j["status"] == "in_progress")
        .collect();
    ensure!(
        matching.len() == 1,
        "GitHub did not identify one assigned job"
    );
    let job = matching[0];
    ensure!(
        job["run_id"] == identity["run_id"]
            && job["run_attempt"] == identity["run_attempt"]
            && job["runner_name"] == name
            && job["id"].as_u64().is_some_and(|id| id > 0),
        "GitHub assignment differs from guest identity"
    );
    Ok(
        json!({"verified":true,"repository_id":admission.repository_id,"runner_id":admission.runner_id,"job_id":job["id"],"run_id":job["run_id"],"run_attempt":job["run_attempt"]}),
    )
}
pub fn verify(
    bx: &EngineBox,
    name: &str,
    repository: &str,
    admission: &Admission,
    stopped: &AtomicBool,
) -> Result<Value> {
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .build()?;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(120) && !stopped.load(Ordering::SeqCst) {
        let input = bx.execute(
            name,
            &[
                "head".into(),
                "-c".into(),
                "16385".into(),
                "/run/enginebox-ci/requests/identity.json".into(),
            ],
            Duration::from_secs(5),
        );
        if let Ok(input) = input {
            ensure!(input.len() <= 16384, "Oversized assignment request");
            let identity: Value = serde_json::from_slice(&input)?;
            check_identity(&identity, admission, name, repository)?;
            let mut jobs = vec![];
            for page in 1..=100 {
                let url = format!(
                    "https://api.github.com/repos/{repository}/actions/runs/{}/attempts/{}/jobs?per_page=100&page={page}",
                    identity["run_id"], identity["run_attempt"]
                );
                let response = client
                    .get(url)
                    .header("User-Agent", "enginebox-ci")
                    .header("X-GitHub-Api-Version", "2026-03-10")
                    .bearer_auth(&admission.verification_token)
                    .send()?;
                ensure!(
                    response.status().is_success(),
                    "GitHub assignment inspection failed"
                );
                use std::io::Read;
                let mut bytes = vec![];
                response.take(2 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
                ensure!(bytes.len() <= 2 * 1024 * 1024, "Oversized GitHub response");
                let value: Value = serde_json::from_slice(&bytes)?;
                let page_jobs = value["jobs"].as_array().context("Missing GitHub jobs")?;
                jobs.extend(page_jobs.iter().cloned());
                if page_jobs.len() < 100 {
                    break;
                }
                ensure!(
                    page < 100 && started.elapsed() < Duration::from_secs(110),
                    "Assignment inspection exceeded bounds"
                );
            }
            if let Ok(assignment) = assigned_job(&jobs, &identity, admission, name) {
                let script = "import sys,pathlib; p=pathlib.Path('/run/enginebox-ci/verified.tmp'); f=p.open('x'); f.write(sys.stdin.read()); f.close(); p.chmod(0o444); p.rename('/run/enginebox-ci/verified.json')";
                bx.run_input(
                    &root_args(name, vec!["python3".into(), "-c".into(), script.into()]),
                    &serde_json::to_vec(&assignment)?,
                    Duration::from_secs(10),
                )?;
                return Ok(assignment);
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    anyhow::bail!("Actual GitHub job was not verified before preparation deadline")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn assignment_is_bound_to_actual_runner_and_attempt_not_demand_job() {
        let admission = Admission {
            repository_id: 10,
            installation_id: 2,
            runner_id: 30,
            workflows: vec!["fixture/repo/.github/workflows/ci.yml@refs/heads/main".into()],
            verification_token: "fixture".into(),
        };
        let identity = json!({"schema":1,"repository_id":10,"repository":"fixture/repo","runner":"ebox-ci-fixture","run_id":55,"run_attempt":2,"workflow":admission.workflows[0]});
        check_identity(&identity, &admission, "ebox-ci-fixture", "fixture/repo").unwrap();
        let mut job = json!({"id":999,"runner_id":30,"runner_name":"ebox-ci-fixture","run_id":55,"run_attempt":2,"status":"in_progress"});
        assert_eq!(
            assigned_job(&[job.clone()], &identity, &admission, "ebox-ci-fixture").unwrap()["job_id"],
            999
        );
        job["run_attempt"] = json!(1);
        assert!(assigned_job(&[job], &identity, &admission, "ebox-ci-fixture").is_err());
        assert!(check_identity(&identity, &admission, "ebox-ci-fixture", "fixture/other").is_err());
    }
}

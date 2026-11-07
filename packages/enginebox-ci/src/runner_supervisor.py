"""One disposable listener: bounded idle wait, then a fresh per-job deadline.

Runs inside the private guest. No state produced here is reusable. The official
job-start hook starts the per-job deadline. Idle expiry only requests controller
retirement: a connected listener is NEVER signalled for idle expiry or drain,
because GitHub can assign before that hook. Capacity remains held until exit.
"""
import fcntl
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

idle_seconds, job_seconds = map(float, sys.argv[1:3])
assert idle_seconds > 0 and job_seconds > 0
jit = sys.stdin.readline().rstrip("\n")
assert jit
control = Path.cwd() / ".enginebox-lifetime"
control.mkdir(mode=0o700)
if len(sys.argv) > 3 and sys.argv[3] == "true":
    # Retirement was requested while this private VM was still provisioning.
    # Do not launch or deliver JIT to the listener at all.
    (control / "retired").write_text("administrator_retirement")
    (control / "result.json").write_text(json.dumps({"schema":1,"outcome":"idle_timeout","exit_code":0,
        "job_started_at_ms":None,"listener_exited_at_ms":int(time.time()*1000),"retired_reason":"administrator_retirement"}))
    (Path.cwd() / "_diag").mkdir(exist_ok=True)
    (Path.cwd() / "runner.log").touch()
    (Path.cwd() / "exit-code").write_text("0\n")
    sys.exit(0)
original_hook = os.environ.get("ACTIONS_RUNNER_HOOK_JOB_STARTED")
hook = control / "started.py"
hook.write_text('''import fcntl,json,os,pathlib,sys,time
root=pathlib.Path(__file__).parent
with (root/'lock').open('a') as lock:
    fcntl.flock(lock,fcntl.LOCK_EX)
    with (root/'started.json').open('x') as marker:
        json.dump({'monotonic':time.monotonic(),'started_at_ms':int(time.time()*1000)},marker)
        marker.flush()
        os.fsync(marker.fileno())
original=os.environ.get('ENGINEBOX_ORIGINAL_JOB_STARTED_HOOK')
if original:
    os.execv(original,[original])
''')
wrapper = control / "started.sh"
wrapper.write_text("#!/bin/sh\nexec python3 " + __import__("shlex").quote(str(hook)) + "\n")
wrapper.chmod(0o700)
env = dict(os.environ)
if original_hook:
    env["ENGINEBOX_ORIGINAL_JOB_STARTED_HOOK"] = original_hook
env["ACTIONS_RUNNER_HOOK_JOB_STARTED"] = str(wrapper)
started = time.monotonic()
assignment = None
outcome = "listener_exited"
with (Path.cwd() / "runner.log").open("wb") as log:
    process = subprocess.Popen(["./run.sh", "--jitconfig", jit], env=env,
                               stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    del jit
    while process.poll() is None:
        with (control / "lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            marker = control / "started.json"
            if assignment is None and marker.exists():
                assignment = json.loads(marker.read_text())
            now = time.monotonic()
            if assignment is not None:
                expired = now >= assignment["monotonic"] + job_seconds
                next_outcome = "job_timeout"
            else:
                if now >= started + idle_seconds and not (control / "idle-expired").exists():
                    (control / "idle-expired").write_text(str(int(time.time()*1000)))
                # Only the controller may retire the exact registration. Even
                # after that fence we wait for natural listener exit, preserving
                # assignments accepted by GitHub before the job-start hook.
                expired = False
                next_outcome = "idle_timeout"
            if expired:
                if not (control / "retired").exists():
                    (control / "retired").write_text(next_outcome)
                outcome = next_outcome
                # Only an assigned job's own timeout may signal its process
                # group; idle expiry and administrative retirement never do.
                try:
                    os.killpg(process.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                break
        time.sleep(0.05)
    try:
        code = process.wait(timeout=30)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        code = process.wait()
    # A very short job may finish before the watchdog's next observation.
    if assignment is None and (control / "started.json").exists():
        assignment = json.loads((control / "started.json").read_text())
    if outcome == "listener_exited" and assignment is not None:
        outcome = "job_finished"
    elif outcome == "listener_exited" and (control / "retired").exists():
        outcome = "idle_timeout"
result = {"schema": 1, "outcome": outcome, "exit_code": code,
          "job_started_at_ms": assignment["started_at_ms"] if assignment else None,
          "listener_exited_at_ms": int(time.time()*1000),
          "retired_reason": (control / "retired").read_text() if (control / "retired").exists() else None}
(control / "result.json").write_text(json.dumps(result))
(Path.cwd() / "exit-code").write_text(str(code) + "\n")

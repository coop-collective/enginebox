#!/usr/bin/env python3
"""Time native CLI cloud submission only, never infer VM readiness from acceptance."""
import argparse
import fcntl
import json
import os
import pathlib
import pty
import re
import select
import struct
import subprocess
import termios
import time

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--repo', required=True)
p.add_argument('--output', required=True)
p.add_argument('--trials', type=int, choices=range(1, 6), default=3)
p.add_argument('--run-live', required=True, action='store_true')
a = p.parse_args()
os.umask(0o077)
out = pathlib.Path(a.output)
out.mkdir(parents=True, exist_ok=False)
prompt = ('Startup benchmark only. Read package.json in the repository root and reply with its package '
          'name followed by ENGINEBOX_STARTUP_OK. Do not edit files, install dependencies, create commits, '
          'push branches, open pull requests, or call external tools. Stop after answering.')
rows = []
for trial in range(a.trials):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 160, 0, 0))
    start = time.monotonic()
    child = subprocess.Popen(['claude', '--cloud', prompt], cwd=a.repo, stdin=slave, stdout=slave,
                             stderr=slave, start_new_session=True,
                             env=os.environ | {'TERM': 'xterm-256color'})
    os.close(slave)
    chunks = []
    while child.poll() is None and time.monotonic() - start < 90:
        if select.select([master], [], [], .1)[0]:
            try:
                chunk = os.read(master, 65536)
            except OSError:
                break
            if not chunk:
                break
            chunks.append({'seconds': time.monotonic() - start, 'text': chunk.decode(errors='replace')})
    timed_out = child.poll() is None
    if timed_out:
        child.terminate()
    child.wait(timeout=10)
    while select.select([master], [], [], 0)[0]:
        try:
            chunk = os.read(master, 65536)
        except OSError:
            break
        if not chunk:
            break
        chunks.append({'seconds': time.monotonic() - start, 'text': chunk.decode(errors='replace')})
    os.close(master)
    (out / f'trial-{trial+1}.json').write_text(json.dumps(chunks, indent=2))
    raw = ''.join(c['text'] for c in chunks)
    accepted = next((c['seconds'] for c in chunks if 'Created cloud session:' in c['text']), None)
    session = re.search(r'https://claude.ai/code/(session_[A-Za-z0-9]+)', raw)
    row = {'trial': trial+1, 'exit_code': child.returncode, 'timed_out': timed_out,
           'submission_observed_seconds': accepted, 'cli_exit_seconds': time.monotonic()-start,
           'session_id': session.group(1) if session else None,
           'environment_ready_seconds': None, 'first_action_seconds': None, 'completion_seconds': None}
    rows.append(row)
    (out / 'receipt.json').write_text(json.dumps({'boundary': 'CLI launch to cloud session acknowledgement', 'trials': rows}, indent=2))
    print(json.dumps({k:v for k,v in row.items() if k != 'session_id'}), flush=True)
    if row['exit_code'] != 0 or accepted is None:
        break

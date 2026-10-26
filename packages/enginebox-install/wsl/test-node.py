#!/usr/bin/env python3
"""Disposable WSL-only proof of the real outbound worker and lease cleanup.

The loopback controller is a protocol fixture. Real controller authorization is
covered separately by contributor_http.rs; this test exercises Linux execution.
Never run on an enrolled distribution: revocation stops its private daemon.
"""
import hashlib
import http.server
import json
import pathlib
import subprocess
import sys
import tempfile
import threading
import time
import uuid

root = pathlib.Path('/var/lib/enginebox')
assert not (root / 'private/worker.json').exists(), 'Use an unenrolled disposable CI distribution'
settings = json.loads(pathlib.Path('/etc/enginebox/settings.json').read_text())
budget = {key: settings[key] for key in ('cpu', 'memory_gib', 'disk_gib')}
if len(sys.argv) == 3 and sys.argv[1] == '--stdin-code-sha256':
    registration_input = sys.stdin.buffer.read(512)
    code = registration_input.decode('utf-8').strip()
    assert len(code) == 97 and code[32] == '.'
    assert hashlib.sha256(code.encode()).hexdigest() == sys.argv[2], 'Windows-to-WSL registration input changed'
    node = code[:32]
else:
    assert len(sys.argv) == 1
    node = uuid.uuid4().hex
    code = node + '.' + uuid.uuid4().hex + uuid.uuid4().hex
    registration_input = code.encode()
workloads = {'ci': True, 'agents': True}
workspace = 'node-proof-' + uuid.uuid4().hex[:12]
operations = [
    {'operation': 'fork', 'project': 'welcome', 'workspace': workspace},
    {'operation': 'exec', 'workspace': workspace, 'argv': ['bash', '-c', 'test "$(id -u)" != 0 && printf node-proof > node-proof.txt && codex --version && claude --version']},
    {'operation': 'checkpoint', 'workspace': workspace},
    {'operation': 'remount', 'workspace': workspace},
    {'operation': 'exec', 'workspace': workspace, 'argv': ['bash', '-c', 'test "$(cat node-proof.txt)" = node-proof || exit 1; nohup sleep 600 >/dev/null 2>&1 & sleep .2; kill -0 $!']},
]
state = {'index': 0, 'assignment': None, 'token': None, 'failure': None, 'revoked': False}


class Controller(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def reply(self, value, status=200):
        payload = json.dumps(value).encode()
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_POST(self):
        try:
            body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            auth = self.headers.get('Authorization')
            if self.path == '/v1/node/enroll':
                assert auth == 'Bearer ' + code
                assert body['budget'] == budget and body['os'] == 'linux'
                state['token_hash'] = body['secret_hash']
                self.reply({'id': node, 'enrolled': True, 'workloads': workloads})
                return
            assert auth == 'Bearer ' + state['token']
            if state['revoked']:
                self.reply({'error': 'Fixture node revoked'}, 403)
            elif self.path == '/v1/node/hello':
                self.reply({'id': node, 'session': body['session'], 'epoch': 1, 'budget': budget, 'workloads': workloads, 'blocked': False})
            elif self.path == '/v1/node/heartbeat':
                assert body['epoch'] == 1 and body['tools']['workspace'] is True
                self.reply({'blocked': False})
            elif self.path == '/v1/node/poll':
                assert state['assignment'] is None
                assignment = uuid.uuid4().hex
                state['assignment'] = assignment
                self.reply({'job': {'id': assignment, 'node': node, 'deadline': int(time.time()) + 180,
                                   'run': {'request_id': assignment, 'kind': 'workspace', 'request': operations[state['index']]}}})
            elif self.path == '/v1/node/result':
                assert body['job'] == state['assignment']
                assert body['result']['ok'] is True, 'Native workspace job failed: ' + json.dumps(body['result'])
                state['index'] += 1
                state['assignment'] = None
                if state['index'] == len(operations):
                    state['revoked'] = True
                self.reply({'accepted': True})
            else:
                raise AssertionError('Unexpected node endpoint')
        except Exception as error:
            state['failure'] = str(error)
            self.reply({'error': 'Protocol fixture assertion failed'}, 400)


server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Controller)
threading.Thread(target=server.serve_forever, daemon=True).start()
unit = 'enginebox-proof-' + uuid.uuid4().hex[:12]
with tempfile.TemporaryDirectory(prefix='node-proof-', dir=root) as private:
    config = pathlib.Path(private) / 'worker.json'
    base = ['/usr/local/bin/enginebox-node']
    subprocess.run(base + ['register-worker', '--controller', f'http://127.0.0.1:{server.server_port}', '--config', str(config),
                          '--cpu', str(budget['cpu']), '--memory-gib', str(budget['memory_gib']), '--disk-gib', str(budget['disk_gib']),
                          '--test-loopback-http'], input=registration_input, check=True, capture_output=True)
    state['token'] = json.loads(config.read_text())['token']
    assert hashlib.sha256(json.dumps(state['token'], separators=(',', ':')).encode()).hexdigest() == state['token_hash']
    worker = subprocess.Popen(['systemd-run', '--quiet', '--wait', '--pipe', '--collect', '--slice=enginebox.slice', '--unit=' + unit]
                              + base + ['run-worker', '--config', str(config), '--test-loopback-http'], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        stdout, stderr = worker.communicate(timeout=300)
        assert not state['failure'], state['failure']
        assert state['index'] == len(operations), 'Worker exited before completing jobs: ' + stderr.decode()[-2000:]
        assert worker.returncode != 0, 'Revocation must end the worker lease'
        assert subprocess.run(['systemctl', 'is-active', '--quiet', 'enginebox-docker.service']).returncode != 0
        # Docker must terminate the sandbox and its detached child on revocation.
        membership = list(pathlib.Path('/sys/fs/cgroup/enginebox.slice').glob('docker-*.scope/cgroup.procs'))
        assert all(not path.read_text().strip() for path in membership), 'Detached workspace processes survived revocation'
        print('Real Linux node registration, outbound polling, LayerFS jobs, limits and revoked-lease cleanup passed.')
    finally:
        if worker.poll() is None:
            subprocess.run(['systemctl', 'stop', unit], check=False)
            worker.kill()
            worker.wait()
        server.shutdown()

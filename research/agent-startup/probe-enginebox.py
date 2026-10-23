#!/usr/bin/env python3
"""Manual, destructive-to-its-own-fixture startup experiment; not a product CLI."""
import argparse
import json
import os
import pathlib
import subprocess
import time
import urllib.request
import urllib.error
import uuid

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--api-url', default='http://127.0.0.1:8765')
parser.add_argument('--token-file', required=True)
parser.add_argument('--host', required=True)
parser.add_argument('--project', required=True)
parser.add_argument('--template', default='native-dev')
parser.add_argument('--output', required=True)
parser.add_argument('--trials', type=int, choices=range(1, 6), default=3)
parser.add_argument('--run-live', action='store_true', required=True)
parser.add_argument('--resume-allocated', action='store_true', help='Resume this receipt before any native runs')
parser.add_argument('--resume-lifecycle', action='store_true', help='Resume lifecycle checks after terminal native runs')
args = parser.parse_args()
os.umask(0o077)
output = pathlib.Path(args.output)
resuming = args.resume_allocated or args.resume_lifecycle
if output.exists() and not resuming:
    raise SystemExit('Refusing to overwrite an existing receipt')
output.parent.mkdir(parents=True, exist_ok=True)
token = pathlib.Path(args.token_file).read_text().strip()
label = 'ebox-timing-' + uuid.uuid4().hex[:12]
receipt = {'schema': 1, 'label': label, 'created_at': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
           'measurement': 'controller-local HTTP admission through terminal result; 100 ms polling',
           'runs': [], 'lifecycle': [], 'cleanup': False}
instance = None
identity = None
if resuming:
    receipt = json.loads(output.read_text())
    label = receipt['label']
    if (not label.startswith('ebox-timing-') or (receipt['runs'] and not args.resume_lifecycle) or receipt['cleanup']
            or receipt['allocation']['status'] != 'succeeded'
            or receipt['allocation']['result']['workspace_name'] != label):
        raise SystemExit('Only an allocated, unused fixture can be resumed')
    receipt['previous_error'] = receipt.pop('error', None)


def save():
    temp = output.with_suffix('.tmp')
    temp.write_text(json.dumps(receipt, indent=2) + '\n')
    temp.replace(output)


def api(path, data=None):
    req = urllib.request.Request(args.api_url + path,
        data=None if data is None else json.dumps(data).encode(),
        headers={'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json'})
    with urllib.request.urlopen(req, timeout=60) as response:
        return json.load(response)


def job(path, body):
    start = time.monotonic()
    result = api(path, body)['job']
    admitted = time.monotonic() - start
    while result['status'] in ('queued', 'running'):
        if time.monotonic() - start > 300:
            raise RuntimeError('Job outcome uncertain; preserve fixture for inspection')
        time.sleep(.1)
        result = api('/v1/jobs/' + result['id'])['job']
    result['observed_seconds'] = time.monotonic() - start
    result['admission_seconds'] = admitted
    if result['status'] != 'succeeded':
        receipt['failed_job'] = result
        save()
        raise RuntimeError('Fleet job failed; private receipt retained')
    return result


def incus(*argv, input=None):
    command = ['sudo', '-n', 'incus']
    if argv[0] != 'query':
        command += ['--project', args.project]
    r = subprocess.run(command + list(argv),
        input=input, text=True, capture_output=True, timeout=45)
    if r.returncode:
        raise RuntimeError('Incus operation failed: ' + r.stderr[:500])
    return r.stdout


def instance_record():
    return json.loads(incus('query', '/1.0/instances/' + instance + '?project=' + args.project))


def verify_identity():
    vm = instance_record()
    config = vm['config']
    if config.get('volatile.uuid') != identity or config.get('user.enginebox.owner') != args.project:
        raise RuntimeError('Fixture identity changed; refusing mutation')
    if config.get('user.enginebox.workspace', instance) != label:
        raise RuntimeError('Fixture ownership changed; refusing mutation')
    return vm


def action(name):
    verify_identity()
    result = job(f'/v1/hosts/{args.host}/instances/{instance}/actions', {'action': name})
    receipt['lifecycle'].append({'action': name, 'job': result})
    save()
    print(json.dumps({'event': name, 'seconds': result['duration_seconds']}), flush=True)
    return result


def run(provider, trial, resume=None):
    prompt = ('Startup benchmark only. Read package.json in the current directory and reply with its '
              'package name followed by ENGINEBOX_STARTUP_OK. Do not edit files, install dependencies, '
              'create commits, push branches, open pull requests, or call external tools. Stop after answering.')
    body = {'provider': provider, 'prompt': prompt, 'timeout_seconds': 120}
    if provider == 'claude':
        body['model'] = 'haiku'
    if resume:
        body['resume_id'] = resume
    path = f'/v1/hosts/{args.host}/instances/{instance}/agent-runs'
    start = time.monotonic()
    try:
        record = api(path, body)['run']
    except urllib.error.HTTPError as error:
        record = {'status': 'admission_failed', 'http_status': error.code}
        receipt['runs'].append({'provider': provider, 'trial': trial, 'resumed': bool(resume), 'record': record})
        save()
        print(json.dumps({'provider': provider, **record}), flush=True)
        return record
    admitted = time.monotonic() - start
    # 'running' means worker dispatch, not a first model token or first tool call.
    running = None
    while record['status'] in ('queued', 'running'):
        if record['status'] == 'running' and running is None:
            running = time.monotonic() - start
        if time.monotonic() - start > 180:
            raise RuntimeError('Native execution uncertain; retain fixture for inspection')
        time.sleep(.1)
        record = api(path + '/' + record['id'])['run']
    row = {'provider': provider, 'trial': trial, 'resumed': bool(resume),
           'admission_seconds': admitted, 'dispatch_observed_seconds': running,
           'completion_seconds': time.monotonic() - start, 'record': record}
    receipt['runs'].append(row)
    save()
    print(json.dumps({k: v for k, v in row.items() if k != 'record'} | {'status': record['status']}), flush=True)
    return record


try:
    save()
    allocation = receipt['allocation'] if resuming else job(
        f'/v1/hosts/{args.host}/allocations',
        {'name': label, 'template': args.template, 'size': 'recommended'})
    receipt['allocation'] = allocation
    instance = allocation['result']['name']
    receipt['instance'] = instance
    vm = instance_record()
    identity = receipt.get('instance_uuid', vm['config']['volatile.uuid'])
    receipt['instance_uuid'] = identity
    verify_identity()
    save()
    print(json.dumps({'event': 'allocated', 'startup': allocation['result'].get('startup'),
                      'ready_seconds': allocation['result'].get('ready_seconds'),
                      'job_seconds': allocation['duration_seconds']}), flush=True)
    # A tiny common file, prepared outside the measurement. No project checkout claim.
    incus('file', 'push', '-', instance + '/workspace/package.json', '--uid', '1001', '--gid', '1001',
          input='{"name":"enginebox"}\n')
    versions = incus('exec', instance, '--', 'su', '-', 'agent', '-c',
                     'codex --version; claude --version; agent --version')
    receipt['native_versions'] = versions.strip().splitlines()
    save()
    for provider in ([] if args.resume_lifecycle else ['codex', 'claude', 'cursor']):
        first_session = None
        for trial in range(1, args.trials + 1):
            result = run(provider, trial)
            if result['status'] != 'succeeded':
                break  # Do not repeatedly consume a failed/exhausted provider.
            first_session = first_session or result.get('native_session_id')
        if first_session:
            run(provider, 1, first_session)
    for trial in range(args.trials):
        before = incus('exec', instance, '--', 'cat', '/proc/sys/kernel/random/boot_id').strip()
        action('pause')
        result = action('resume')
        after = incus('exec', instance, '--', 'cat', '/proc/sys/kernel/random/boot_id').strip()
        receipt['lifecycle'][-1]['kind'] = 'memory-resume'
        receipt['lifecycle'][-1]['same_boot'] = before == after
        save()
    for trial in range(args.trials):
        before = incus('exec', instance, '--', 'cat', '/proc/sys/kernel/random/boot_id').strip()
        action('stop')
        result = action('resume')
        after = incus('exec', instance, '--', 'cat', '/proc/sys/kernel/random/boot_id').strip()
        receipt['lifecycle'][-1]['kind'] = 'cold-boot-existing-disk'
        receipt['lifecycle'][-1]['new_boot'] = before != after
        save()
    action('stop')
    # Only delete after every admitted operation is terminal and identity still matches.
    verify_identity()
    lock = subprocess.Popen(['sudo', '-n', 'enginebox-host', 'lock'], stdin=subprocess.PIPE,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        if lock.stdout.readline() != 'locked\n':
            raise RuntimeError('Cleanup lock unavailable; fixture retained')
        verify_identity()
        incus('delete', instance)
        receipt['cleanup'] = True
    finally:
        lock.stdin.close()
        lock.wait(timeout=10)
    save()
    print(json.dumps({'event': 'cleanup', 'deleted_owned_fixture': True}), flush=True)
except Exception as error:
    receipt['error'] = str(error)
    save()
    raise SystemExit(str(error))

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

test('Linux preparation fixture identifies an unavailable Docker executable', async t => {
  const path = await mkdtemp(join(tmpdir(), 'ebox-no-docker-'));
  t.after(() => rm(path, { recursive: true, force: true }));
  const result = spawnSync(process.execPath, [join(import.meta.dirname, 'linux.mjs')], {
    env: { ...process.env, PATH: path }, encoding: 'utf8', timeout: 10_000,
  });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /fixture failed \(docker build\): spawnSync docker ENOENT/);
});

test('Linux preparation fixture identifies the failed Docker operation and exit status', { skip: process.platform === 'win32' }, async t => {
  const path = await mkdtemp(join(tmpdir(), 'ebox-failed-docker-'));
  t.after(() => rm(path, { recursive: true, force: true }));
  await writeFile(join(path, 'docker'), '#!/bin/sh\necho "fixture Docker daemon unavailable" >&2\nexit 42\n', { mode: 0o700 });
  const result = spawnSync(process.execPath, [join(import.meta.dirname, 'linux.mjs')], {
    env: { ...process.env, PATH: path }, encoding: 'utf8', timeout: 10_000,
  });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /fixture Docker daemon unavailable/);
  assert.match(result.stderr, /fixture failed \(docker build\): exit 42/);
});

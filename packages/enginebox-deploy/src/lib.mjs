import { lstat, readFile } from 'node:fs/promises';
import { createReadStream } from 'node:fs';
import { createHash } from 'node:crypto';
import { resolve, dirname, join, isAbsolute } from 'node:path';
import { spawn } from 'node:child_process';

export function check(ok, code) { if (!ok) throw new Error(code); }
export function fields(value, allowed, required = allowed) {
  check(value && typeof value === 'object' && !Array.isArray(value), 'expected-object');
  check(Object.keys(value).every(k => allowed.includes(k)) && required.every(k => k in value), 'unexpected-or-missing-fields');
}
export function version(value) {
  check(typeof value === 'string' && /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*)?$/.test(value) && value.length < 100, 'exact-version-required');
  return value;
}
/**
 * The watch verdict from what was observed since the cutover: lost readiness for N consecutive checks, or start
 * failures (runs that failed before their agent started) both above a count and a share of new runs.
 */
export async function regular(file, maximum = 1024 * 1024) {
  check(isAbsolute(file), 'absolute-input-required');
  let cursor = file;
  while (true) {
    const meta = await lstat(cursor);
    check(!meta.isSymbolicLink(), 'symlink-input-refused');
    if (cursor === dirname(cursor)) break;
    cursor = dirname(cursor);
  }
  const meta = await lstat(file);
  check(meta.isFile() && meta.nlink === 1 && meta.size <= maximum, 'invalid-input-file');
  return meta;
}
export async function json(file, maximum) {
  await regular(file, maximum);
  try { return JSON.parse(await readFile(file, 'utf8')); } catch { throw new Error('invalid-json-input'); }
}
export async function sha256(file) {
  const hash = createHash('sha256');
  for await (const chunk of createReadStream(file)) hash.update(chunk);
  return hash.digest('hex');
}
export function manifest(value, pin, target) {
  fields(value, ['version', 'target', 'archive_sha256', 'state_schema', 'read_schema_min', 'read_schema_max', 'dashboard']);
  check(value.version === version(pin) && value.target === target && /^[a-f0-9]{64}$/.test(value.archive_sha256), 'artifact-identity');
  check(value.state_schema === 1 && value.read_schema_min === 1 && value.read_schema_max === 1 && value.dashboard === true, 'artifact-schema-or-dashboard');
  return value;
}
export async function selectArtifact(directory, pin, target) {
  version(pin);
  check(target === 'x86_64-unknown-linux-gnu', 'unsupported-artifact-target');
  const stem = `enginebox-${pin}-${target}`;
  const archive = join(resolve(directory), `${stem}.tar.gz`);
  const file = join(resolve(directory), `${stem}.json`);
  const value = manifest(await json(file, 65536), pin, target);
  await regular(archive, 4 * 1024 ** 3);
  check(await sha256(archive) === value.archive_sha256, 'artifact-checksum');
  return { archive, manifest: file, manifest_sha256: await sha256(file), value };
}
export function quote(value) { check(typeof value === 'string' && !value.includes('\0'), 'unsafe-command-value'); return `'${value.replaceAll("'", "'\"'\"'")}'`; }
export function command(argv) { return argv.map(quote).join(' '); }
export async function run(program, args, { cwd, input, timeout = 120000, inherit = false, env = process.env, safeJsonErrors = false } = {}) {
  return new Promise((resolveResult, reject) => {
    const child = spawn(program, args, { cwd, env, stdio: [input === undefined ? 'ignore' : 'pipe', inherit ? 'inherit' : 'pipe', inherit ? 'inherit' : 'pipe'] });
    let output = ''; let overflow = false;
    child.stdout?.on('data', data => { if (output.length + data.length > 4 * 1024 * 1024) { overflow = true; child.kill(); } else output += data; });
    child.stderr?.resume(); // Never relay native errors containing private configuration.
    const timer = setTimeout(() => child.kill('SIGKILL'), timeout);
    child.on('error', () => { clearTimeout(timer); reject(new Error('command-start-failed')); });
    child.on('close', code => {
      clearTimeout(timer);
      if (code === 0 && !overflow) return resolveResult(output);
      let reason = 'command-failed-inspect-private-server-logs';
      if (safeJsonErrors && !overflow) {
        try { const value = JSON.parse(output); if (value.ok === false && typeof value.error === 'string' && /^[a-z0-9-]+$/.test(value.error)) reason = value.error; } catch { /* No private child output. */ }
      }
      reject(new Error(reason));
    });
    child.stdin?.on('error', () => {});
    if (input !== undefined) child.stdin.end(input);
  });
}
export function cli(argv, allowed) {
  const values = {};
  for (let i = 0; i < argv.length; i += 2) {
    const key = argv[i]?.replace(/^--/, '');
    check(argv[i] === `--${key}` && allowed.includes(key) && !(key in values) && argv[i + 1] !== undefined, 'invalid-cli-arguments');
    values[key] = argv[i + 1];
  }
  return values;
}

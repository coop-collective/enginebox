import { lstat, readFile, realpath } from 'node:fs/promises';
import { createReadStream } from 'node:fs';
import { createHash } from 'node:crypto';
import { resolve, dirname, basename, join, isAbsolute } from 'node:path';
import { spawn } from 'node:child_process';

export function check(ok, code) { if (!ok) throw new Error(code); }
export function requireUnixClient() {
  // Windows chmod/getuid do not enforce this client's owner-only credential contract.
  check(process.platform !== 'win32' && typeof process.getuid === 'function', 'deployment-client-requires-unix-permissions');
}
export function fields(value, allowed, required = allowed) {
  check(value && typeof value === 'object' && !Array.isArray(value), 'expected-object');
  check(Object.keys(value).every(k => allowed.includes(k)) && required.every(k => k in value), 'unexpected-or-missing-fields');
}
export function version(value) {
  check(typeof value === 'string' && /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*)?$/.test(value) && value.length < 100, 'exact-version-required');
  return value;
}
export function path(value) {
  check(typeof value === 'string' && /^\/[A-Za-z0-9_./-]+$/.test(value) && value !== '/' && !value.split('/').some(p => p === '.' || p === '..'), 'unsafe-absolute-path');
  check(!value.includes('//') && !value.endsWith('/'), 'unsafe-absolute-path');
  return value;
}
export function loopback(value) {
  check(typeof value === 'string' && /^http:\/\/127\.0\.0\.1:[1-9][0-9]{0,4}$/.test(value), 'loopback-url-required');
  check(Number(new URL(value).port) <= 65535, 'invalid-port');
  return value;
}
function id(value) { check(typeof value === 'string' && /^[a-z0-9][a-z0-9-]{0,48}$/.test(value), 'invalid-id'); return value; }
export function config(value) {
  fields(value, ['schema_version', 'target', 'remote_node', 'controller', 'templates', 'node', 'smoke', 'watch'], ['schema_version', 'target', 'remote_node', 'controller', 'templates']);
  // Optional: the root-owned node helper installation workspaces refresh their guest helper from. Each controller
  // deploy stages and activates the same release there and republishes the host helper, so they never drift apart.
  // Only one environment per host may own it (production); a staging config omits it.
  if (value.node !== undefined) {
    fields(value.node, ['prefix', 'sudo']);
    path(value.node.prefix);
    path(value.node.sudo);
  }
  if (value.smoke !== undefined) smokeConfig(value.smoke);
  if (value.watch !== undefined) watchConfig(value.watch);
  check(value.schema_version === 1 && value.target === 'x86_64-unknown-linux-gnu', 'unsupported-deployment-platform');
  path(value.remote_node);
  fields(value.controller, ['prefix', 'installer', 'supervisor_config', 'state_dir', 'token_file', 'url']);
  for (const key of ['prefix', 'installer', 'supervisor_config', 'state_dir', 'token_file']) path(value.controller[key]);
  loopback(value.controller.url);
  check(value.controller.installer.startsWith(`${value.controller.prefix}/releases/`) && basename(value.controller.installer) === 'enginebox-install', 'installer-must-be-pinned-in-prefix');
  fields(value.templates, ['namespace', 'allow_ids', 'files', 'timeout_seconds']);
  id(value.templates.namespace);
  check(Array.isArray(value.templates.allow_ids) && value.templates.allow_ids.length <= 32, 'template-id-list');
  value.templates.allow_ids.forEach(id);
  check(new Set(value.templates.allow_ids).size === value.templates.allow_ids.length, 'duplicate-template-id');
  check(Array.isArray(value.templates.files) && value.templates.files.length <= 32 && new Set(value.templates.files).size === value.templates.files.length, 'template-file-list');
  for (const file of value.templates.files) check(typeof file === 'string' && /^templates\/[a-z0-9][a-z0-9-]*\.json$/.test(file), 'template-file-path');
  check(Number.isInteger(value.templates.timeout_seconds) && value.templates.timeout_seconds >= 15 && value.templates.timeout_seconds <= 7200, 'template-timeout');
  return value;
}
/** Post-deploy gate on one environment: reads always; a trivial pool run only when a pool and provider are named. */
export function smokeConfig(value) {
  fields(value, ['pool', 'provider', 'prompt', 'expect', 'timeout_seconds'], []);
  if (value.pool !== undefined) id(value.pool);
  if (value.provider !== undefined) check(['codex', 'claude', 'cursor'].includes(value.provider), 'smoke-provider');
  check((value.pool === undefined) === (value.provider === undefined), 'smoke-run-needs-pool-and-provider');
  if (value.prompt !== undefined) check(typeof value.prompt === 'string' && value.prompt.length >= 1 && value.prompt.length <= 500, 'smoke-prompt');
  if (value.expect !== undefined) check(typeof value.expect === 'string' && value.expect.length >= 1 && value.expect.length <= 100, 'smoke-expect');
  if (value.timeout_seconds !== undefined) check(Number.isInteger(value.timeout_seconds) && value.timeout_seconds >= 30 && value.timeout_seconds <= 1800, 'smoke-timeout');
  return value;
}
/** Post-promotion watch: how long, how often, and what counts as a regression worth an automatic rollback. */
export function watchConfig(value) {
  fields(value, ['seconds', 'interval_seconds', 'max_unready_checks', 'max_start_failures', 'max_start_failure_ratio'], []);
  const int = (v, lo, hi, code) => check(v === undefined || (Number.isInteger(v) && v >= lo && v <= hi), code);
  int(value.seconds, 0, 3600, 'watch-seconds');
  int(value.interval_seconds, 5, 300, 'watch-interval');
  int(value.max_unready_checks, 1, 60, 'watch-unready');
  int(value.max_start_failures, 1, 1000, 'watch-start-failures');
  check(value.max_start_failure_ratio === undefined || (typeof value.max_start_failure_ratio === 'number' && value.max_start_failure_ratio > 0 && value.max_start_failure_ratio <= 1), 'watch-ratio');
  return value;
}
export const WATCH_DEFAULTS = { seconds: 600, interval_seconds: 30, max_unready_checks: 3, max_start_failures: 3, max_start_failure_ratio: 0.5 };
/**
 * The watch verdict from what was observed since the cutover: lost readiness for N consecutive checks, or start
 * failures (runs that failed before their agent started) both above a count and a share of new runs.
 */
export function watchVerdict(observed, settings = {}) {
  const w = { ...WATCH_DEFAULTS, ...settings };
  if (observed.consecutive_unready >= w.max_unready_checks) return { regressed: true, reason: 'controller-lost-readiness' };
  if (observed.start_failures >= w.max_start_failures && observed.new_runs > 0 && observed.start_failures / observed.new_runs >= w.max_start_failure_ratio) return { regressed: true, reason: 'run-start-failures' };
  return { regressed: false, reason: null };
}
/** A run summary that failed before its agent did any work (admission/placement/launch), not an agent's own failure. */
export function startFailure(run) {
  if (!run || run.status !== 'failed') return false;
  const exit = run.result?.exit_code ?? run.exit_code;
  return exit === undefined || exit === null;
}
export function template(value, deployment) {
  fields(value, ['schema_version', 'id', 'revision', 'label', 'description', 'recipe', 'source_image', 'minimum', 'recommended', 'native_clients', 'config'], ['schema_version', 'id', 'revision', 'label', 'minimum', 'recommended']);
  id(value.id);
  check(value.id.startsWith(`${deployment.templates.namespace}-`) || deployment.templates.allow_ids.includes(value.id), 'template-outside-managed-namespace');
  check(value.schema_version === 1 && Number.isInteger(value.revision) && value.revision >= 1 && value.revision <= 2147483647, 'template-revision');
  check(typeof value.label === 'string' && value.label.trim() && [...value.label].length <= 120 && !/[\x00-\x1f\x7f]/.test(value.label), 'template-label');
  if ('description' in value) check(typeof value.description === 'string' && [...value.description].length <= 2000 && !/[\x00-\x09\x0b-\x1f\x7f]/.test(value.description), 'template-description');
  check(!value.recipe || value.recipe === 'native-clients-v1', 'template-recipe');
  check(!value.source_image || value.source_image === 'images:ubuntu/24.04/cloud', 'template-image');
  for (const kind of ['minimum', 'recommended']) {
    fields(value[kind], ['cpu', 'memory_gib', 'disk_gib']);
    for (const [key, min, max] of [['cpu', 1, 512], ['memory_gib', 1, 512], ['disk_gib', 12, 16384]]) {
      const n = value[kind][key];
      check(Number.isInteger(n) && n >= min && n <= max && (kind !== 'recommended' || n >= value.minimum[key]), 'template-resources');
    }
  }
  if ('native_clients' in value) check(Array.isArray(value.native_clients) && value.native_clients.every(v => ['claude', 'codex', 'cursor'].includes(v)) && new Set(value.native_clients).size === value.native_clients.length, 'template-clients');
  if ('config' in value) {
    fields(value.config, ['browser', 'desktop', 'guest_computer_mcp'], []);
    check(Object.values(value.config).every(v => typeof v === 'boolean') && (!value.config.guest_computer_mcp || value.config.desktop), 'template-options');
  }
  return value;
}
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
export async function loadConfig(file, packageRoot, includeTemplates = true) {
  // Canonicalize the repository's parent aliases (for example macOS /var),
  // then reject symlinks in administrator-selected descendants.
  const root = await realpath(packageRoot);
  const selected = resolve(file);
  const value = config(await json(selected));
  const templates = includeTemplates ? await Promise.all(value.templates.files.map(async name => template(await json(join(root, name)), value))) : [];
  check(new Set(templates.map(t => t.id)).size === templates.length, 'duplicate-template-id');
  return { config: value, templates };
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
export function installerArgs(deployment, operation, pin, artifact) {
  version(pin);
  const base = ['--prefix', deployment.controller.prefix];
  if (operation === 'stage') return [...base, 'stage', '--version', pin, '--manifest', artifact.manifest, '--manifest-sha256', artifact.manifest_sha256, '--archive', artifact.archive];
  check(operation === 'update' || operation === 'rollback', 'invalid-deployment-operation');
  // Rollback is an explicit retained version; never race the mutable previous pointer.
  return [...base, 'update', '--version', pin, '--url', deployment.controller.url, '--token-file', deployment.controller.token_file];
}
/** The node installation's installer argv (run through sudo with the new release's own installer). */
export function nodeInstallerArgs(deployment, operation, pin, artifact) {
  version(pin);
  const base = ['--prefix', deployment.node.prefix];
  if (operation === 'stage') return [...base, 'stage', '--version', pin, '--manifest', artifact.manifest, '--manifest-sha256', artifact.manifest_sha256, '--archive', artifact.archive];
  if (operation === 'activate') return [...base, 'activate', '--version', pin];
  check(operation === 'expose-host', 'invalid-node-operation');
  return [...base, 'expose-host', '--apply'];
}
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

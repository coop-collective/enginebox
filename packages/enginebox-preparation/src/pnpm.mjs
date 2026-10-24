// Generic preparation recipe. No GitHub credentials, runner protocol or cache publication authority.
import assert from 'node:assert/strict';
import { createHash, randomUUID } from 'node:crypto';
import { spawnSync } from 'node:child_process';
import { lstat, readdir, readFile, mkdir, writeFile, rename, realpath, readlink } from 'node:fs/promises';
import { join, resolve, relative, dirname, isAbsolute } from 'node:path';

export const RECIPE = 'pnpm-installed-v1';
export const sha = data => createHash('sha256').update(data).digest('hex');
export function canonical(value) {
  if (Array.isArray(value)) return `[${value.map(canonical).join(',')}]`;
  if (value && typeof value === 'object') return `{${Object.keys(value).sort().map(k => `${JSON.stringify(k)}:${canonical(value[k])}`).join(',')}}`;
  return JSON.stringify(value);
}
export async function json(path) {
  const s = await lstat(path);
  assert(s.isFile() && !s.isSymbolicLink() && s.size <= 256 * 1024, 'Invalid bounded JSON file');
  return JSON.parse(await readFile(path, 'utf8'));
}
export async function save(path, value) {
  await mkdir(dirname(path), { recursive: true, mode: 0o700 });
  await writeFile(path, JSON.stringify(value, null, 2) + '\n', { flag: 'wx', mode: 0o600 });
}
export function validatePolicy(p) {
  assert(p?.schema === 1 && p.recipe === RECIPE, 'Unsupported preparation recipe');
  for (const k of ['namespace', 'repository', 'trust']) assert(typeof p[k] === 'string' && /^[a-zA-Z0-9_.:/-]{1,160}$/.test(p[k]), `Invalid ${k}`);
  assert(/^[a-f0-9]{64}$/.test(p.image), 'Pin the credential-free base image digest');
  assert(p.pnpm === '12.15.0', 'The qualified recipe requires pnpm 12.15.0');
  assert(['run', 'deny'].includes(p.scripts), 'Explicit installation script policy required');
  return p;
}
export function command(argv, cwd, env = process.env, timeout = 600_000) {
  const r = spawnSync(argv[0], argv.slice(1), { cwd, env, encoding: 'utf8', timeout, maxBuffer: 16 * 1024 * 1024, windowsHide: true, shell: false });
  // Never include package logs or command environment in errors: they can contain registry credentials.
  assert(!r.error && r.status === 0, `Preparation command failed (${r.status ?? 'timeout'}): ${argv[0]}`);
  return r.stdout;
}
export async function inside(root, path) {
  const base = await realpath(root);
  const candidate = await realpath(path);
  const rel = relative(base, candidate);
  assert(rel !== '..' && !rel.startsWith(`..${process.platform === 'win32' ? '\\' : '/'}`) && !isAbsolute(rel), 'Path escapes preparation root');
  return candidate;
}
export async function exists(path) {
  try { await lstat(path); return true; } catch (e) { if (e.code === 'ENOENT') return false; throw e; }
}
// Hash content and modes, never mtimes. Limits also bound traversal of hostile checkouts.
export async function tree(root, { source = false } = {}) {
  const entries = [];
  let bytes = 0;
  async function visit(dir, prefix) {
    for (const name of (await readdir(dir)).sort()) {
      if (source && ['.git', 'node_modules'].includes(name)) continue;
      const path = join(dir, name), rel = prefix ? `${prefix}/${name}` : name;
      const s = await lstat(path);
      assert(entries.length < 100_000, 'Preparation tree is too large');
      if (s.isSymbolicLink()) {
        const target = await readlink(path);
        const resolved = resolve(dirname(path), target);
        assert(!isAbsolute(target) && !relative(root, resolved).startsWith('..'), 'External dependency/source symlink is unsupported');
        await inside(root, path);
        entries.push([rel, 'link', target]);
      } else if (s.isDirectory()) {
        entries.push([rel, 'dir', s.mode & 0o777]);
        await visit(path, rel);
      } else {
        assert(s.isFile() && s.size <= 256 * 1024 * 1024, 'Unsupported preparation file');
        bytes += s.size;
        assert(bytes <= 4 * 1024 * 1024 * 1024, 'Preparation tree byte limit exceeded');
        entries.push([rel, 'file', s.mode & 0o777, sha(await readFile(path))]);
      }
    }
  }
  await visit(root, '');
  return { digest: sha(canonical(entries)), entries, bytes };
}
// Version-qualified pnpm 12 configuration inputs. Reading bounded files avoids launching
// a second package-manager process on every hit, and includes settings omitted by config list.
async function configuration(source, env) {
  const settings = Object.fromEntries(Object.entries(env).filter(([key]) => /^(npm_config_|pnpm_)/i.test(key) || ['NODE_OPTIONS', 'NODE_ENV'].includes(key)));
  const value = key => Object.entries(env).find(([k]) => k.toLowerCase() === key)?.[1];
  const home = env.HOME || env.USERPROFILE;
  assert(home, 'A private recipe home is required');
  const configDir = value('pnpm_config_config_dir') || join(env.XDG_CONFIG_HOME || (process.platform === 'darwin' ? join(home, 'Library/Preferences') : process.platform === 'win32' ? join(env.LOCALAPPDATA || join(home, 'AppData/Local')) : join(home, '.config')), 'pnpm');
  const paths = [join(configDir, 'config.yaml'), join(configDir, 'rc'), join(configDir, 'auth.ini'), join(home, '.npmrc'), '/etc/npmrc', resolve(dirname(process.execPath), '../etc/npmrc')];
  if (env.PNPM_HOME) paths.push(join(env.PNPM_HOME, 'pnpm-workspace.yaml'));
  for (const key of ['npm_config_userconfig', 'npm_config_globalconfig', 'pnpm_config_userconfig', 'pnpm_config_globalconfig']) if (value(key)) paths.push(resolve(source, value(key)));
  const files = [];
  let reusable = !Object.keys(settings).some(key => /pnpmfile|config_dependencies/i.test(key));
  for (let index = 0; index < paths.length; index++) {
    const path = paths[index];
    if (!await exists(path)) continue;
    const info = await lstat(path);
    assert(info.isFile() && !info.isSymbolicLink() && info.size <= 256 * 1024, 'Unsupported global pnpm configuration');
    const bytes = await readFile(path);
    if (/pnpmfile|configDependencies/i.test(bytes.toString('utf8'))) reusable = false;
    files.push([index, sha(bytes)]);
  }
  for (let parent = dirname(source); parent !== dirname(parent); parent = dirname(parent)) {
    assert(!await exists(join(parent, 'pnpm-workspace.yaml')), 'Select the complete pnpm workspace, not a nested package');
  }
  return { digest: sha(canonical({ settings, files })), reusable };
}
async function selectedRuntime(source, env) {
  for (const entry of (env.PATH || '').split(process.platform === 'win32' ? ';' : ':')) {
    const candidate = resolve(source, entry, process.platform === 'win32' ? 'node.exe' : 'node');
    if (!await exists(candidate)) continue;
    if (await realpath(candidate) === await realpath(process.execPath) && env.NODE_OPTIONS === process.env.NODE_OPTIONS) {
      return { version: process.version, abi: process.versions.modules, platform: process.platform, arch: process.arch, libc: process.report.getReport().header.glibcVersionRuntime || 'other' };
    }
    break;
  }
  return JSON.parse(command(['node', '-p', 'JSON.stringify({version:process.version,abi:process.versions.modules,platform:process.platform,arch:process.arch,libc:process.report.getReport().header.glibcVersionRuntime||"other"})'], source, env));
}
export async function fingerprint(source, policy, env = process.env) {
  validatePolicy(policy);
  source = await realpath(source);
  const p = await json(join(source, 'package.json'));
  assert(await exists(join(source, 'pnpm-lock.yaml')), 'A committed pnpm lockfile is required');
  assert(command(['pnpm', '--version'], source, env).trim() === policy.pnpm, 'pnpm version differs from profile');
  const runtime = await selectedRuntime(source, env);
  const files = await tree(source, { source: true });
  // v1 is deliberately conservative for local dependencies, lifecycle hooks and monorepos.
  const config = files.entries.filter(e => /(^|\/)(package\.json|pnpm-lock\.yaml|pnpm-workspace\.yaml|\.npmrc|\.pnpmfile\.[cm]?js)$/.test(e[0]) || /(^|\/)patches\//.test(e[0]));
  const simple = !p.workspaces && !await exists(join(source, 'pnpm-workspace.yaml')) && !await exists(join(source, '.pnpmfile.cjs')) && !await exists(join(source, '.pnpmfile.mjs')) && !Object.values({ ...p.dependencies, ...p.devDependencies, ...p.optionalDependencies }).some(v => /^(file:|link:|workspace:|git|https?:)/.test(v)) && !['preinstall', 'install', 'postinstall', 'prepare'].some(k => p.scripts?.[k]);
  const effectiveConfig = await configuration(source, env);
  const reusable = effectiveConfig.reusable && !await exists(join(source, 'pnpm-workspace.yaml')) && !await exists(join(source, '.pnpmfile.cjs')) && !await exists(join(source, '.pnpmfile.mjs'));
  const inputs = { schema: 1, recipe: RECIPE, policy, runtime, effectiveConfig: effectiveConfig.digest, reusable, workspace: source, dependencies: sha(canonical(config)), source: simple && policy.scripts === 'deny' ? null : files.digest };
  return { key: sha(canonical(inputs)), inputs, sourceDigest: files.digest };
}
export function environment(root, env = process.env) {
  // Job-private writable caches. Immutable/frozen-store mode is not used with this fork.
  return { ...env, CI: 'true', PNPM_CONFIG_STORE_DIR: join(root, 'caches/packages'), PNPM_CONFIG_CACHE_DIR: join(root, 'caches/metadata'), PNPM_CONFIG_FROZEN_STORE: 'false', npm_config_cache: join(root, 'caches/npm') };
}
async function install(source, root, policy, env, offline) {
  await mkdir(join(root, 'caches/packages'), { recursive: true });
  await mkdir(join(root, 'caches/metadata'), { recursive: true });
  const args = ['pnpm', 'install', '--frozen-lockfile', '--package-import-method=copy', '--store-dir', join(root, 'caches/packages'), '--reporter', 'ndjson'];
  if (policy.scripts === 'deny') args.push('--ignore-scripts');
  if (offline) args.push('--offline');
  const output = command(args, source, environment(root, env));
  let downloaded = 0;
  for (const line of output.split('\n')) {
    try { const v = JSON.parse(line); if (v.name === 'pnpm:progress' && v.status === 'fetched') downloaded++; } catch { /* Package output is not a receipt. */ }
  }
  return downloaded;
}
// Administrator-only build command, intended for a fresh credential-free preparation environment.
// The job action never calls this function or publishes its mutable outputs.
export async function build({ source, root, policy, env = process.env, offline = false }) {
  assert(policy.scripts === 'deny', 'Installed-tree publication currently requires explicit deny script policy');
  source = await realpath(source); root = await realpath(root);
  env = environment(root, env);
  await inside(root, source);
  assert(!await exists(join(source, 'node_modules')), 'Build requires a fresh dependency tree');
  const before = await fingerprint(source, policy, env);
  assert(before.inputs.reusable, 'Installed-tree publication does not support workspace configuration or package hooks');
  const dest = join(root, 'prepared', before.key);
  assert(!await exists(dest), 'Preparation identity already exists');
  await install(source, root, policy, env, offline);
  const after = await fingerprint(source, policy, env);
  assert(before.key === after.key && before.sourceDigest === after.sourceDigest, 'Install changed source inputs');
  // Workspace-linked node_modules are outside v1; fail rather than capture unrelated source.
  const modules = await tree(join(source, 'node_modules'));
  await mkdir(dest, { recursive: true, mode: 0o700 });
  await rename(join(source, 'node_modules'), join(dest, 'node_modules'));
  const manifest = { ...before, modules: { digest: modules.digest, bytes: modules.bytes }, createdAt: new Date().toISOString() };
  await save(join(dest, 'manifest.json'), manifest);
  return manifest;
}
export async function prepare({ source, root, policy, env = process.env, offline = false, requireHit = false, activate }) {
  const started = performance.now();
  source = await realpath(source); root = await realpath(root);
  env = environment(root, env);
  await inside(root, source);
  if (await exists(join(source, 'node_modules'))) assert((await lstat(join(source, 'node_modules'))).isDirectory() && !(await lstat(join(source, 'node_modules'))).isSymbolicLink(), 'Existing node_modules must be a private directory');
  const before = await fingerprint(source, policy, env);
  const base = join(root, 'prepared', before.key);
  let reason = policy.scripts === 'deny' ? 'generation-missing' : 'lifecycle-scripts-require-install';
  if (!before.inputs.reusable) reason = 'workspace-or-package-hook-requires-install';
  if (await exists(join(source, 'node_modules'))) reason = 'dependency-directory-already-exists';
  let mode = 'install', downloadedPackages = null;
  if (before.inputs.reusable && policy.scripts === 'deny' && await exists(join(base, 'manifest.json')) && await exists(join(base, 'node_modules')) && !await exists(join(source, 'node_modules'))) {
    await inside(root, base);
    const manifest = await json(join(base, 'manifest.json'));
    assert(manifest.key === before.key && canonical(manifest.inputs) === canonical(before.inputs), 'Preparation manifest differs from actual inputs');
    if (activate) {
      // The root helper protects the approved tree read-only until its one-use handoff.
      await activate({ key: before.key, inputs: before.inputs, source });
    } else {
      const modules = await tree(join(base, 'node_modules'));
      assert(modules.digest === manifest.modules.digest, 'Prepared dependencies failed integrity verification');
      await rename(join(base, 'node_modules'), join(source, 'node_modules'));
    }
    mode = 'installed-tree'; reason = 'exact-compatible-generation'; downloadedPackages = 0;
  } else {
    assert(!requireHit, `Required prepared generation unavailable: ${reason}`);
    downloadedPackages = await install(source, root, policy, env, offline);
  }
  // A verified rename executes no dependency code and cannot change tools/configuration.
  // Avoid probing tools twice on a hit; still compare source content after the handoff.
  const after = mode === 'installed-tree' ? { key: before.key, sourceDigest: (await tree(source, { source: true })).digest } : await fingerprint(source, policy, env);
  assert(before.key === after.key && before.sourceDigest === after.sourceDigest, 'Preparation changed source inputs');
  const receipt = { schema: 1, recipe: RECIPE, key: before.key, mode, reason, validation: mode === 'installed-tree' ? (activate ? 'protected-generation' : 'content-sha256') : 'locked-install', downloadedPackages, elapsedMs: performance.now() - started, source: before.sourceDigest };
  await save(join(root, 'receipts', `${randomUUID()}.json`), receipt);
  return receipt;
}

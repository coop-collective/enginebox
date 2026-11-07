// Trusted, public-registry-only research builder. Run only in a disposable builder VM.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtemp, mkdir, readFile, writeFile, rm, realpath, readdir, lstat, readlink } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve, isAbsolute } from 'node:path';
import { createHash } from 'node:crypto';

const argv = process.argv.slice(2);
assert(argv.length === 14, 'Usage: seed.mjs --source PATH --output NEW_PATH --repository-id ID --trust TRUST --node VERSION --pnpm VERSION --base-image DIGEST');
const options = {};
for (let i = 0; i < argv.length; i += 2) {
  assert(['--source', '--output', '--repository-id', '--trust', '--node', '--pnpm', '--base-image'].includes(argv[i]) && argv[i + 1] && !options[argv[i]], 'Invalid or duplicate seed option');
  options[argv[i]] = argv[i + 1];
}
assert(process.env.ENGINEBOX_DISPOSABLE_PROOF === '1', 'Set ENGINEBOX_DISPOSABLE_PROOF=1 only in an owned clean builder');
assert.equal(process.platform, 'linux', 'Build in the target Linux image');
assert(/^[1-9]\d*$/.test(options['--repository-id']), 'Numeric repository scope required');
assert(/^[a-zA-Z0-9_.:/-]{1,160}$/.test(options['--trust']), 'Explicit trust scope required');
assert(/^[a-f0-9]{64}$/.test(options['--base-image']), 'Pin the clean base image digest');
assert.equal(process.version, options['--node'], 'Node version differs from seed contract');
assert.equal(options['--pnpm'], '12.15.0', 'The store layout is qualified for pnpm 12.15.0');
const source = await realpath(options['--source']), output = resolve(options['--output']);
assert(output !== source && !output.startsWith(`${source}/`), 'Keep the seed outside the source checkout');
const git = (...args) => execFileSync('git', ['-C', source, ...args], { encoding: 'utf8' }).trim();
assert.equal(git('status', '--porcelain', '--untracked-files=normal'), '', 'Seed source must be an exact clean commit');
const packageBytes = await readFile(join(source, 'package.json'));
const lockBytes = await readFile(join(source, 'pnpm-lock.yaml'));
let workspaceBytes;
try { workspaceBytes = await readFile(join(source, 'pnpm-workspace.yaml')); }
catch (error) { if (error.code !== 'ENOENT') throw error; }
if (workspaceBytes) {
  // This is the reviewed single-package Docker fixture's lifecycle policy,
  // not a general YAML/workspace reader. Unknown settings fail closed.
  const lines = workspaceBytes.toString('utf8').trimEnd().split('\n');
  assert.equal(lines.shift(), 'allowBuilds:', 'Only a standalone allowBuilds policy is supported');
  const seen = new Set();
  for (const line of lines) {
    const match = /^  (esbuild|workerd): (true|false)$/.exec(line);
    assert(match && !seen.has(match[1]), 'Unsupported or duplicate build allowlist entry');
    seen.add(match[1]);
  }
  assert(seen.size > 0, 'Empty build allowlist');
}
const packageJson = JSON.parse(packageBytes);
assert(!packageJson.workspaces, 'This standalone fixture seed does not support monorepos');
for (const value of Object.values({ ...packageJson.dependencies, ...packageJson.devDependencies, ...packageJson.optionalDependencies })) {
  assert(typeof value === 'string' && /^[~^]?\d[\d.a-zA-Z+<>=|~^ *-]*$/.test(value), 'Only public registry version dependencies are supported');
}
assert(!packageJson.pnpm?.patchedDependencies && !packageJson.pnpm?.configDependencies, 'Patch/config dependencies need a separately reviewed builder');
for (const bytes of [packageBytes, lockBytes]) {
  const text = bytes.toString('utf8');
  assert(!/(?:_authToken|_password|_auth|authorization)\s*["']?\s*[:=]/i.test(text), 'Credential-bearing package input refused');
  for (const match of text.matchAll(/https?:\/\/[^\s"'<>]+/g)) {
    const url = new URL(match[0]);
    assert(!url.username && !url.password, 'Credentials in an input URL refused');
  }
}
// No .npmrc, pnpm hooks or checkout credentials are copied. The only optional
// workspace input is the reviewed allowBuilds map above. Scripts cannot run.
assert(!/\b(?:tarball|repo|repository)\s*:\s*(?:git|ssh|https?:)/.test(lockBytes.toString('utf8')), 'Non-default lockfile package origins require a separate reviewed seed');
const scratch = await mkdtemp(join(tmpdir(), 'enginebox-clean-pnpm-seed-'));
await mkdir(output, { mode: 0o700 }); // Refuse replacing or extending any existing seed.
try {
  const work = join(scratch, 'source'), home = join(scratch, 'home');
  await mkdir(work, { mode: 0o700 }); await mkdir(home, { mode: 0o700 });
  await writeFile(join(work, 'package.json'), packageBytes);
  await writeFile(join(work, 'pnpm-lock.yaml'), lockBytes);
  if (workspaceBytes) await writeFile(join(work, 'pnpm-workspace.yaml'), workspaceBytes);
  const emptyConfig = join(scratch, 'empty.npmrc'); await writeFile(emptyConfig, '');
  const env = {
    PATH: process.env.PATH, HOME: home, CI: 'true', LANG: 'C.UTF-8',
    XDG_CONFIG_HOME: join(scratch, 'config'), XDG_CACHE_HOME: join(scratch, 'cache'),
    XDG_DATA_HOME: join(scratch, 'data'), XDG_STATE_HOME: join(scratch, 'state'),
    npm_config_userconfig: emptyConfig, npm_config_globalconfig: emptyConfig,
    PNPM_CONFIG_STORE_DIR: join(output, 'store'), PNPM_CONFIG_CACHE_DIR: join(output, 'metadata'),
    npm_config_registry: 'https://registry.npmjs.org/',
  };
  const run = (args, cwd = work, environment = env) => execFileSync('pnpm', args, { cwd, env: environment, encoding: 'utf8', timeout: 1200000, maxBuffer: 32 * 1024 * 1024 });
  assert.equal(run(['--version']).trim(), options['--pnpm'], 'pnpm version differs from seed contract');
  const install = ['install', '--frozen-lockfile', '--ignore-scripts', '--ignore-pnpmfile', '--store-dir', join(output, 'store'), '--reporter=silent'];
  const started = performance.now();
  run(install);
  assert.equal(Buffer.compare(await readFile(join(work, 'pnpm-lock.yaml')), lockBytes), 0, 'Seed install changed lockfile');
  await rm(join(work, 'node_modules'), { recursive: true });
  // pnpm 12.15's project backlinks describe the builder checkout, not reusable
  // packages. Strip this exact metadata namespace before and after verification.
  const projects = join(output, 'store/v11/projects');
  await rm(projects, { recursive: true, force: true });
  run([...install, '--offline']); // Same manifest/lock can install without downloads.
  await rm(projects, { recursive: true, force: true });
  // A second checkout and HOME must succeed from only the exported caches,
  // rather than accidentally using the builder's private XDG metadata.
  const portable = join(scratch, 'portable'); await mkdir(portable);
  for (const [name, bytes] of [['package.json', packageBytes], ['pnpm-lock.yaml', lockBytes], ['pnpm-workspace.yaml', workspaceBytes]]) {
    if (bytes) await writeFile(join(portable, name), bytes);
  }
  const portableEnv = { ...env, HOME: join(scratch, 'portable-home'), XDG_CONFIG_HOME: join(scratch, 'portable-config'), XDG_CACHE_HOME: join(scratch, 'portable-cache'), XDG_DATA_HOME: join(scratch, 'portable-data'), XDG_STATE_HOME: join(scratch, 'portable-state') };
  await mkdir(portableEnv.HOME);
  run([...install, '--offline'], portable, portableEnv);
  await rm(projects, { recursive: true, force: true });
  async function digestTree(root) {
    const hash = createHash('sha256'); let files = 0, bytes = 0;
    async function inspect(directory, prefix = '') {
    for (const name of (await readdir(directory)).sort()) {
      const path = join(directory, name), relative = `${prefix}${name}`, info = await lstat(path);
      if (info.isSymbolicLink()) {
        const target = await readlink(path);
        const destination = await realpath(path);
        const within = destination.startsWith(`${root}/`);
        assert(!isAbsolute(target) && within, `Seed cache symlink escapes output: ${relative}`);
        hash.update(JSON.stringify([relative, 'link', target])); files++;
      } else if (info.isDirectory()) await inspect(path, `${relative}/`);
      else {
        assert(info.isFile() && info.size <= 256 * 1024 * 1024, 'Unsupported seed file');
        files++; bytes += info.size;
        assert(files <= 250000 && bytes <= 8 * 1024 * 1024 * 1024, 'Seed exceeds research bounds');
        hash.update(JSON.stringify([relative, info.mode & 0o777, createHash('sha256').update(await readFile(path)).digest('hex')]));
      }
    }
    }
    await inspect(root);
    return { sha256: hash.digest('hex'), files, bytes };
  }
  const store = await digestTree(join(output, 'store'));
  const metadata = await digestTree(join(output, 'metadata'));
  const manifest = {
    schema: 1, kind: 'image-local-pnpm-store', repositoryId: options['--repository-id'], trust: options['--trust'],
    sourceCommit: git('rev-parse', 'HEAD'), sourceTree: git('rev-parse', 'HEAD^{tree}'),
    lockSha256: createHash('sha256').update(lockBytes).digest('hex'),
    packageSha256: createHash('sha256').update(packageBytes).digest('hex'),
    workspaceSha256: workspaceBytes ? createHash('sha256').update(workspaceBytes).digest('hex') : null,
    node: process.version, pnpm: options['--pnpm'], platform: process.platform, arch: process.arch,
    baseImage: options['--base-image'], builderScripts: 'deny', builderRegistry: 'https://registry.npmjs.org/',
    offlineInstallVerified: true, portableOfflineVerified: true, omittedBuilderMetadata: ['store/v11/projects'], store, metadata, elapsedMs: performance.now() - started,
    createdAt: new Date().toISOString(),
  };
  await writeFile(join(output, 'manifest.json'), JSON.stringify(manifest, null, 2) + '\n', { flag: 'wx', mode: 0o600 });
  console.log(JSON.stringify(manifest));
} finally {
  await rm(scratch, { recursive: true, force: true });
}

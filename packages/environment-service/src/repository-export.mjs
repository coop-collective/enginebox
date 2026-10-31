// Export one branch of a hosted or linked repository for a leased preview
// (docs/ENVIRONMENT-SERVICES.md, "Repository branch previews"). The operator
// wires it in as a bootstrap host's `repositoryCommand`; the environment
// service appends exactly `REPOSITORY BRANCH`:
//
//   node repository-export.mjs --controller http://127.0.0.1:8765 \
//     --token-file /srv/enginebox-environments/repository-read-token \
//     --cache /srv/enginebox-environments/repository-cache ideator-apps/site autopilot/feat_x-v2
//
// The token file holds a controller credential that may mint READ tokens for
// the repositories (a service token with a repositories read grant). Each
// export mints a short-lived read token, fetches the branch head with the
// token as a URL-scoped git header from the environment (never argv, never
// written to disk), extracts that commit's tree into an immutable export
// directory and prints `{revision, sourceRoot, manifest}` for the service to
// verify and copy. Nothing from the export is executed here.
import { spawn } from 'node:child_process';
import { createHash, randomUUID } from 'node:crypto';
import { createReadStream } from 'node:fs';
import { lstat, mkdir, readFile, readdir, readlink, realpath, rename, rm, utimes, writeFile } from 'node:fs/promises';
import { isAbsolute, join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { branchName, check, repositoryRef, sourcePath } from './config.mjs';

const TOKEN_TTL_SECONDS = 300;
const KEEP_EXPORTS = 3;
const MAX_ENTRIES = 100000;

const sha256 = value => createHash('sha256').update(value).digest('hex');
async function fileHash(file) { const h = createHash('sha256'); for await (const part of createReadStream(file)) h.update(part); return h.digest('hex'); }

export function parseArgs(argv) {
  const options = {}; const positional = [];
  for (let i = 0; i < argv.length; i++) {
    if (['--controller', '--token-file', '--cache'].includes(argv[i])) { options[argv[i].slice(2)] = argv[++i]; continue; }
    positional.push(argv[i]);
  }
  check(positional.length === 2 && repositoryRef(positional[0]) && branchName(positional[1]), 'invalid-repository-export-request');
  check(typeof options['token-file'] === 'string' && isAbsolute(options['token-file']) && typeof options.cache === 'string' && isAbsolute(options.cache), 'invalid-repository-export-config');
  const url = new URL(options.controller);
  check(['http:', 'https:'].includes(url.protocol) && !url.username && !url.password && !url.search && !url.hash && url.pathname === '/' && (url.protocol === 'https:' || ['127.0.0.1', '[::1]', 'localhost'].includes(url.hostname)), 'invalid-repository-export-controller');
  return { controller: url.origin, tokenFile: options['token-file'], cache: options.cache, repository: positional[0], branch: positional[1] };
}

/** The git header that presents a minted token to its remote. */
export function authHeader(token, auth) {
  return auth === 'github_token'
    ? `Authorization: Basic ${Buffer.from(`x-access-token:${token}`).toString('base64')}`
    : `Authorization: Bearer ${token}`;
}

function git(args, { env, cwd, stdout } = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn('git', args, { cwd, env, stdio: ['ignore', stdout ? 'pipe' : 'pipe', 'ignore'] });
    const chunks = []; let bytes = 0;
    child.stdout.on('data', d => { if (stdout) return stdout(d); bytes += d.length; if (bytes <= 16 * 1024 * 1024) chunks.push(d); });
    const timer = setTimeout(() => child.kill('SIGKILL'), 240000);
    child.once('error', () => { clearTimeout(timer); reject(new Error('repository-export-git-unavailable')); });
    child.once('close', code => { clearTimeout(timer); code === 0 ? resolve(Buffer.concat(chunks)) : reject(new Error(`repository-export-git-${args[0]}-failed`)); });
  });
}

async function mint(options) {
  const token = (await readFile(options.tokenFile, 'utf8')).trim();
  check(token.length >= 20, 'repository-export-token-required');
  const [project, name] = options.repository.split('/');
  const response = await fetch(`${options.controller}/v1/projects/${project}/repositories/${name}/tokens`, {
    method: 'POST', redirect: 'error', signal: AbortSignal.timeout(30000),
    headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
    body: JSON.stringify({ access: 'read', ttl_seconds: TOKEN_TTL_SECONDS }),
  });
  check(response.ok, 'repository-export-token-refused');
  const value = await response.json();
  check(typeof value.token === 'string' && value.token && typeof value.remote === 'string' && /^https:\/\//.test(value.remote), 'repository-export-token-invalid');
  return value;
}

// Every file and link of the commit's tree, in git's order, with the bytes'
// sha256 measured from the extracted copy. Submodules are left out.
async function inventory(env, repo, commit, root) {
  const listed = (await git(['-C', repo, 'ls-tree', '-r', '-z', '--full-tree', commit], { env })).toString('utf8').split('\0').filter(Boolean);
  check(listed.length <= MAX_ENTRIES, 'repository-export-entry-limit');
  const entries = [];
  for (const line of listed) {
    const tab = line.indexOf('\t'); const [mode, type] = line.slice(0, tab).split(' '); const path = line.slice(tab + 1);
    if (type !== 'blob' || !sourcePath(path)) continue;
    const file = join(root, path); const meta = await lstat(file);
    if (mode === '120000') { check(meta.isSymbolicLink(), 'repository-export-link-mismatch'); entries.push({ path, kind: 'symlink', target: await readlink(file) }); continue; }
    check(meta.isFile(), 'repository-export-file-mismatch');
    entries.push({ path, kind: 'file', size: meta.size, sha256: await fileHash(file), executable: mode === '100755' });
  }
  return entries;
}

async function prune(exportsDir, keep) {
  const names = [];
  for (const name of await readdir(exportsDir)) {
    if (!/^[a-f0-9]{40,64}$/.test(name)) continue;
    names.push({ name, at: (await lstat(join(exportsDir, name))).mtimeMs });
  }
  names.sort((a, b) => b.at - a.at);
  for (const { name } of names.slice(keep)) await rm(join(exportsDir, name), { recursive: true, force: true });
}

export async function exportBranch(options, { mintToken = mint } = {}) {
  const base = join(options.cache, sha256(options.repository).slice(0, 24));
  const repo = join(base, 'repository.git'); const exportsDir = join(base, 'exports');
  await mkdir(exportsDir, { recursive: true, mode: 0o700 });
  const minted = await mintToken(options);
  const env = { PATH: process.env.PATH ?? '/usr/bin:/bin', HOME: base, LANG: 'C.UTF-8', GIT_TERMINAL_PROMPT: '0', GIT_CONFIG_NOSYSTEM: '1', GIT_CONFIG_GLOBAL: '/dev/null',
    GIT_CONFIG_COUNT: '2', GIT_CONFIG_KEY_0: `http.${minted.remote}.extraHeader`, GIT_CONFIG_VALUE_0: authHeader(minted.token, minted.auth), GIT_CONFIG_KEY_1: 'core.hooksPath', GIT_CONFIG_VALUE_1: '/dev/null' };
  try { await lstat(repo); } catch { await git(['init', '-q', '--bare', repo], { env }); }
  await git(['-C', repo, 'fetch', '-q', '--depth=1', '--no-tags', '--force', '--', minted.remote, `+refs/heads/${options.branch}:refs/heads/tracked`], { env });
  const commit = (await git(['-C', repo, 'rev-parse', 'refs/heads/tracked^{commit}'], { env })).toString().trim();
  check(/^[a-f0-9]{40,64}$/.test(commit), 'repository-export-invalid-commit');
  const target = join(exportsDir, commit);
  let manifest;
  try { manifest = JSON.parse(await readFile(join(target, 'source.json'), 'utf8')); } catch { manifest = null; }
  if (!manifest) {
    const stage = join(exportsDir, `.stage-${randomUUID()}`); const root = join(stage, 'source');
    await mkdir(root, { recursive: true, mode: 0o700 });
    try {
      const tar = spawn('tar', ['-x', '-f', '-', '-C', root], { stdio: ['pipe', 'ignore', 'ignore'] });
      const extracted = new Promise((resolve, reject) => { tar.once('error', () => reject(new Error('repository-export-tar-unavailable'))); tar.once('close', code => code === 0 ? resolve() : reject(new Error('repository-export-tar-failed'))); });
      await git(['-C', repo, 'archive', '--format=tar', commit], { env, stdout: d => tar.stdin.write(d) });
      tar.stdin.end(); await extracted;
      const entries = await inventory(env, repo, commit, root);
      const treeHash = sha256(JSON.stringify(entries));
      manifest = { schema: 1, authority: 'repository-branch', repository: options.repository, branch: options.branch, commit, revision: treeHash, treeHash, entries };
      await writeFile(join(stage, 'source.json'), JSON.stringify(manifest), { mode: 0o600 });
      await rename(stage, target);
    } catch (error) { await rm(stage, { recursive: true, force: true }); throw error; }
  }
  // An export keyed by commit may be shared by branches; bind this request's branch.
  manifest = { ...manifest, branch: options.branch };
  const now = new Date(); await utimes(target, now, now); // most recently used: kept by prune
  await prune(exportsDir, KEEP_EXPORTS);
  return { revision: manifest.revision, sourceRoot: await realpath(join(target, 'source')), manifest };
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  try { process.stdout.write(JSON.stringify(await exportBranch(parseArgs(process.argv.slice(2))))); }
  catch (error) { process.stderr.write(/^[a-z0-9-]+$/.test(error.message) ? error.message : 'repository-export-failed'); process.exitCode = 1; }
}

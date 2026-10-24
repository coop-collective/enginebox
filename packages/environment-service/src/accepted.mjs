import { readFile, readdir, lstat, realpath, mkdir, copyFile, chmod, symlink, readlink } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { createReadStream } from 'node:fs';
import { join, dirname, resolve, sep } from 'node:path';
import { check } from './config.mjs';

const digest = value => typeof value === 'string' && /^[a-f0-9]{64}$/.test(value);
const hash = value => createHash('sha256').update(value).digest('hex');
async function boundedJson(file) { const meta = await lstat(file); check(meta.isFile() && meta.size <= 16 * 1024 * 1024, 'source-manifest-bound'); return JSON.parse(await readFile(file, 'utf8')); }
async function fileHash(file) { const hasher = createHash('sha256'); for await (const chunk of createReadStream(file)) hasher.update(chunk); return hasher.digest('hex'); }
async function api(source, path) {
  const token = (await readFile(source.tokenFile, 'utf8')).trim();
  check(token.length >= 32, 'accepted-source-token-required');
  const response = await fetch(new URL(path, source.controller), { redirect: 'error', headers: { authorization: `Bearer ${token}` }, signal: AbortSignal.timeout(10000) });
  check(response.ok, 'accepted-source-unavailable');
  const chunks = []; let bytes = 0;
  for await (const chunk of response.body) { bytes += chunk.length; check(bytes <= 4 * 1024 * 1024, 'accepted-response-bound'); chunks.push(chunk); }
  return JSON.parse(Buffer.concat(chunks).toString());
}
export async function acceptedSource(env, stateRoot) {
  const source = env.source;
  const project = await api(source, `/v1/projects/${source.project}`);
  check(project.project === source.project && digest(project.head), 'no-accepted-generation');
  const generation = await api(source, `/v1/projects/${source.project}/generations/${project.head}`);
  check(generation.id === project.head && generation.manifest?.project === source.project, 'accepted-generation-mismatch');
  const metadata = generation.manifest.metadata;
  check(digest(metadata.treeHash) && typeof metadata.integration === 'string' && /^[a-z0-9][a-z0-9-]{0,47}$/.test(metadata.integration), 'accepted-source-export-required');
  const directory = join(await realpath(source.exportsDir), metadata.integration);
  check(!(await lstat(directory)).isSymbolicLink(), 'symlink-export-refused');
  const manifest = await boundedJson(join(directory, 'source.json'));
  check(manifest.schema === 1 && manifest.project === source.project && manifest.generation === project.head && manifest.treeHash === metadata.treeHash && manifest.layerfs?.root === metadata.layerfsRoot && manifest.layerfs?.ref === metadata.layerfsRef, 'export-provenance-mismatch');
  check(Array.isArray(manifest.entries) && manifest.entries.length <= 100000 && hash(JSON.stringify(manifest.entries)) === manifest.treeHash, 'export-inventory-mismatch');
  const sourceRoot = join(directory, 'source');
  check((await realpath(sourceRoot)) === sourceRoot, 'symlink-source-root-refused');
  return { revision: project.head, root: join(stateRoot, env.id), sourceRoot, manifest };
}

// Copy only inventoried bytes and verify the copy. Never execute directly from
// an integration sandbox or trust mutable source paths as proof of acceptance.
export async function copyAccepted(source, checkout) {
  await mkdir(checkout, { mode: 0o700 });
  const seen = new Set(); let bytes = 0; const links = [];
  for (const entry of source.manifest.entries) {
    const path = entry.path;
    check(typeof path === 'string' && path.length <= 4096 && !path.includes('\\') && !path.includes('\0') && path.split('/').every(p => p && p !== '.' && p !== '..' && p !== '.git' && p !== '.enginebox-prepared') && !seen.has(path), 'unsafe-source-path');
    seen.add(path);
    check(['file', 'symlink'].includes(entry.kind), 'unsafe-source-kind');
    const input = join(source.sourceRoot, path); const output = join(checkout, path);
    // Refuse source parent symlinks, including a race before open/copy. Hashing
    // the private copy still binds every regular byte to the accepted manifest.
    check((await realpath(dirname(input))).startsWith(`${source.sourceRoot}${sep}`) || await realpath(dirname(input)) === source.sourceRoot, 'source-parent-escape');
    await mkdir(dirname(output), { recursive: true, mode: 0o700 });
    if (entry.kind === 'symlink') { links.push({ input, output, entry }); continue; }
    const meta = await lstat(input);
    check(meta.isFile() && Number.isSafeInteger(entry.size) && entry.size >= 0 && meta.size === entry.size && digest(entry.sha256), 'source-file-mismatch');
    bytes += entry.size; check(bytes <= 10 * 1024 ** 3, 'source-bytes-limit');
    await copyFile(input, output); check(await fileHash(output) === entry.sha256, 'source-digest-mismatch');
    await chmod(output, entry.executable ? 0o700 : 0o600);
  }
  for (const { input, output, entry } of links) {
    check(typeof entry.target === 'string' && !entry.target.includes('\0') && !entry.target.startsWith('/') && !entry.target.includes('\\') && resolve(dirname(output), entry.target).startsWith(`${checkout}${sep}`), 'source-link-escape');
    check((await lstat(input)).isSymbolicLink() && await readlink(input) === entry.target, 'source-link-mismatch');
    await symlink(entry.target, output);
  }
  for (const { output } of links) check((await realpath(output)).startsWith(`${checkout}${sep}`), 'source-link-chain-escape');
}

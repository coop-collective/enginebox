import { spawn } from 'node:child_process';
import { mkdir, lstat, readFile, writeFile, rm } from 'node:fs/promises';
import { join, isAbsolute } from 'node:path';
import { pathToFileURL } from 'node:url';
import { canonical, digest, identifier, requireThat, sha256, validateGeneration, validatePreparation, type Preparation } from '@enginebox/fleet-protocol';
import { Client } from './client.ts';
import { Cache } from './cache.ts';
import { Generations, type Published } from './generations.ts';

export async function resolvePreparation(client: Client, generations: Generations, project: string, key: string, expected: Pick<Preparation, 'namespace' | 'repository' | 'trust' | 'image' | 'pnpm'>, activate: (snapshot: string, manifest: Published, preparation: Preparation) => Promise<void>, ready?: (manifest: Published, preparation: Preparation) => Promise<boolean>) {
  identifier(project); digest(key);
  requireThat(expected && Object.keys(expected).sort().join(',') === 'image,namespace,pnpm,repository,trust', 'Exact preparation scope required', 403);
  digest(expected.image);
  requireThat(expected.trust === 'repository-read' && /^\d+\.\d+\.\d+$/.test(expected.pnpm) && [expected.namespace, expected.repository].every(v => typeof v === 'string' && /^[a-zA-Z0-9_.:/-]{1,160}$/.test(v)), 'Invalid preparation scope', 403);
  const route = `/v1/projects/${project}/preparations/${key}`;
  const preparation = validatePreparation(await client.json(route));
  requireThat(preparation.project === project && preparation.key === key && Object.entries(expected).every(([k, v]) => preparation[k as keyof Preparation] === v), 'Preparation scope or image differs', 403);
  const published = await client.json<Published>(`/v1/projects/${project}/generations/${preparation.generation}`);
  validateGeneration(published.manifest);
  requireThat(published.manifest.project === project && await sha256(canonical(published.manifest)) === published.id, 'Preparation manifest identity differs', 409);
  requireThat(published.id === preparation.generation && published.manifest.metadata.preparationKey === key && published.manifest.metadata.preparationRecipe === preparation.recipe, 'Preparation generation differs', 409);
  requireThat(await sha256(Buffer.from(preparation.projectRecord, 'base64')) === published.manifest.metadata.projectRecordHash, 'Preparation discovery record differs', 409);
  const started = performance.now();
  if (ready && await ready(published, preparation)) {
    requireThat(canonical(await client.json(route)) === canonical(preparation), 'Preparation approval changed during inspection', 409);
    return { preparation, hydrateAndActivateMs: performance.now() - started, stats: generations.stats, source: 'native-warm' };
  }
  const snapshot = await generations.hydrate(project, preparation.generation);
  const pin = `${project}/${preparation.generation}`;
  generations.snapshots.active.add(pin);
  try {
    // Transfer may take minutes. Check revocation again before activating private data.
    requireThat(canonical(await client.json(route)) === canonical(preparation), 'Preparation approval changed during transfer', 409);
    await activate(snapshot, published, preparation);
  } finally { generations.snapshots.active.delete(pin); }
  return { preparation, hydrateAndActivateMs: performance.now() - started, stats: generations.stats, source: 'hydrated' };
}
async function invoke(binary: string, argv: string[]) {
  return await new Promise<Record<string, unknown>>((resolve, reject) => {
    const child = spawn(binary, argv, { stdio: ['ignore', 'pipe', 'ignore'], signal: AbortSignal.timeout(900_000) });
    let output = ''; child.stdout.on('data', data => { output += data; if (output.length > 256 * 1024) child.kill(); });
    child.once('error', reject);
    child.once('exit', code => {
      try { const result = JSON.parse(output); requireThat(code === 0 && result.ok === true, 'Native preparation activation failed; inspect its private receipt', 409); resolve(result); }
      catch (e) { reject(e); }
    });
  });
}
// Trusted node-side resolver: never run this in a workflow or give its token to a job.
async function main(path: string) {
  const info = await lstat(path);
  requireThat(info.isFile() && !info.isSymbolicLink() && info.size < 64 * 1024 && (process.platform === 'win32' || (info.mode & 0o077) === 0), 'Private resolver configuration required');
  const config = JSON.parse(await readFile(path, 'utf8'));
  requireThat(config.schema === 1 && isAbsolute(config.state) && isAbsolute(config.ebox) && isAbsolute(config.layerfs), 'Invalid resolver paths');
  identifier(config.localProject);
  await mkdir(config.state, { recursive: true, mode: 0o700 });
  const state = await lstat(config.state);
  requireThat(state.isDirectory() && !state.isSymbolicLink() && (process.platform === 'win32' || (state.mode & 0o077) === 0), 'Private resolver state required');
  // An interrupted resolver retains this lock for explicit inspection, never implicit native replay.
  const lock = join(config.state, 'activation.lock'); await mkdir(lock, { mode: 0o700 });
  const cache = new Cache(config.state, config.cacheBytes ?? 1024 * 1024 * 1024);
  const client = new Client(config.controller, config.token);
  const generations = new Generations(client, cache, config.state);
  try {
    await generations.snapshots.recover();
    const result = await resolvePreparation(client, generations, config.project, config.key, config.expected, async (snapshot, published, preparation) => {
      const metadata = published.manifest.metadata;
      requireThat(metadata.projectName === config.localProject && typeof metadata.layerfsRevision === 'string', 'Native preparation project/revision required', 409);
      const record = join(lock, 'project.record');
      await writeFile(record, Buffer.from(preparation.projectRecord, 'base64'), { mode: 0o600, flag: 'wx' });
      await invoke(config.ebox, ['--host', 'local', 'storage', 'layerfs', '--config', config.layerfs, 'preparation-restore', config.localProject, '--snapshot', snapshot, '--snapshot-hash', published.manifest.snapshotHash, '--key', config.key, '--revision', metadata.layerfsRevision, '--record', record, '--record-hash', metadata.projectRecordHash!, '--credential-free']);
    }, async (published) => {
      const metadata = published.manifest.metadata;
      requireThat(metadata.projectName === config.localProject && typeof metadata.layerfsRevision === 'string', 'Native preparation project/revision required', 409);
      const value = await invoke(config.ebox, ['--host', 'local', 'storage', 'layerfs', '--config', config.layerfs, 'preparation-ready', config.localProject, '--snapshot-hash', published.manifest.snapshotHash, '--key', config.key, '--revision', metadata.layerfsRevision, '--record-hash', metadata.projectRecordHash!]);
      requireThat(typeof value.ready === 'boolean' && value.project === config.localProject && value.key === config.key, 'Invalid native preparation inspection', 409);
      return value.ready;
    });
    await writeFile(join(config.state, 'resolved.json'), canonical(result), { mode: 0o600 });
    await rm(lock, { recursive: true });
    const { projectRecord: _privateRecord, ...preparation } = result.preparation;
    console.log(JSON.stringify({ schema: 1, project: config.localProject, from: null, ...result, preparation }));
  } finally { cache.db.close(); }
}
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main(process.argv[2]).catch(() => { console.error('Preparation resolution failed; inspect private node receipts.'); process.exitCode = 1; });
}

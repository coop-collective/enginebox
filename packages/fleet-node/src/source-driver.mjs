// Operator-owned source lifecycle for an existing native VM, LayerFS connector,
// and protected engine build service. It never schedules or runs a model.
import { promises as fs } from 'node:fs';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { join, isAbsolute } from 'node:path';
import { fileURLToPath } from 'node:url';

export async function writeJson(path, value) {
  const file = await fs.open(path + '.tmp', 'w', 0o600);
  try { await file.writeFile(JSON.stringify(value, null, 2)); await file.sync(); } finally { await file.close(); }
  await fs.rename(path + '.tmp', path);
  const directory = await fs.open(join(path, '..'), 'r');
  try { await directory.sync(); } finally { await directory.close(); }
}
async function readJson(path) {
  try { return JSON.parse(await fs.readFile(path, 'utf8')); } catch (e) { if (e.code === 'ENOENT') return null; throw e; }
}
export class Refusal extends Error {}
export function validateConfig(c) {
  for (const name of ['project', 'sourceProject', 'host', 'workspace', 'incusProject', 'owner'])
    if (!/^[a-z0-9][a-z0-9-]{0,47}$/.test(c[name])) throw new Error('Invalid operator binding');
  for (const name of ['root', 'ebox', 'layerfs', 'nativeConfig', 'githubToken', 'buildConnection', 'incus'])
    if (typeof c[name] !== 'string' || !isAbsolute(c[name])) throw new Error('Absolute operator paths required');
  if (!/^[a-zA-Z0-9_.-]+\/[a-zA-Z0-9_.-]+$/.test(c.repository) || c.branch !== 'main') throw new Error('Invalid repository binding');
}
export async function lifecycle(config, request, fetcher = globalThis.fetch) {
  validateConfig(config);
  if (request.project !== config.project || !['prepare', 'complete'].includes(request.action) || typeof request.attempt !== 'string' || !/^[a-zA-Z0-9-]{1,128}$/.test(request.attempt)) throw new Error('Invalid source request');
  const id = createHash('sha256').update(request.attempt).digest('hex').slice(0, 32);
  await fs.mkdir(config.root, { recursive: true, mode: 0o700 });
  const lock = join(config.root, 'lock');
  await fs.mkdir(lock, { mode: 0o700 }); // Stale locks require operator inspection.
  const directory = join(config.root, id);
  await fs.mkdir(directory, { recursive: true, mode: 0o700 });
  const journalPath = join(directory, 'receipt.json'), currentPath = join(config.root, 'current.json');
  let journal = await readJson(journalPath), current = await readJson(currentPath);
  const log = await fs.open(join(directory, 'operations.log'), 'a', 0o600);
  async function exec(argv, timeout = 180_000) {
    await log.write(JSON.stringify({ time: new Date().toISOString(), command: argv[0], operation: argv.slice(1, 5) }) + '\n');
    return new Promise((resolve, reject) => {
      const child = spawn(argv[0], argv.slice(1), { stdio: ['ignore', 'pipe', 'pipe'], env: { PATH: '/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin', HOME: '/root' } });
      let bytes = Buffer.alloc(0), diagnostics = '', failed = false;
      const timer = setTimeout(() => { failed = true; child.kill('SIGKILL'); }, timeout);
      child.stdout.on('data', chunk => { if (bytes.length + chunk.length > 16 * 1024 * 1024) { failed = true; child.kill('SIGKILL'); } else bytes = Buffer.concat([bytes, chunk]); });
      child.stderr.on('data', chunk => { diagnostics = (diagnostics + chunk.toString()).slice(-16_384); void log.write(chunk); });
      child.on('error', reject);
      child.on('close', code => {
        clearTimeout(timer);
        if (code === 0 && !failed) resolve(bytes.toString());
        else { const error = new Error('Source command failed; inspect the private operation log'); error.hostLockBusy = !failed && diagnostics.startsWith('Error: Another Engine Box operation holds the host lock\n'); reject(error); }
      });
    });
  }
  const storage = async (...args) => {
    for (let n = 0; ; n++) {
      try { return JSON.parse(await exec([config.ebox, '--host', 'local', 'storage', 'layerfs', '--config', config.layerfs, ...args])); }
      catch (error) {
        // This exact refusal precedes all Incus/storage mutations. Other
        // failures, including uncertain SDK outcomes, must never be replayed.
        if (!error.hostLockBusy || n >= 120) throw error;
        await new Promise(r => setTimeout(r, 500));
      }
    }
  };
  const guest = (...args) => exec([config.incus, '--project', config.incusProject, 'exec', config.workspace, '--', ...args]);
  const nativeConfig = (await readJson(config.nativeConfig)).nativeAssistant;
  async function native(path, body) {
    const response = await fetcher(nativeConfig.endpoint + path, { method: body === undefined ? 'GET' : 'POST', headers: { authorization: 'Bearer ' + nativeConfig.token, 'content-type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body), redirect: 'error', signal: AbortSignal.timeout(20_000) });
    if (!response.ok) throw new Error('Native lifecycle refused the source operation');
    return response.json();
  }
  async function power(action) {
    const instances = JSON.parse(await exec([config.incus, '--project', config.incusProject, 'list', config.workspace, '--format=json']));
    const vm = instances.find(v => v.name === config.workspace);
    if (!vm || vm.config['user.enginebox.owner'] !== config.owner) throw new Error('Private VM ownership changed');
    if ((action === 'stop' && vm.status === 'Stopped') || (action === 'resume' && vm.status === 'Running')) return;
    await exec(['/usr/bin/flock', '-w', '60', '/run/lock/enginebox-host.lock', '/usr/bin/true']);
    const submitted = await native(`/v1/hosts/${config.host}/instances/${config.workspace}/actions`, { action });
    const jobId = submitted.job?.id ?? submitted.id;
    if (!/^[a-f0-9]{32}$/.test(jobId)) throw new Error('Native action acknowledgement missing');
    for (let n = 0; n < 120; n++) {
      const result = await native('/v1/jobs/' + jobId), job = result.job ?? result;
      if (job.status === 'succeeded') return;
      if (!['queued', 'running'].includes(job.status)) throw new Error('Native VM action failed');
      await new Promise(r => setTimeout(r, 1000));
    }
    throw new Error('Native VM lifecycle outcome uncertain');
  }
  const buildConnection = await readJson(config.buildConnection);
  async function build(path, body, headers = {}) {
    const response = await fetcher(buildConnection.url + path, { method: body === undefined ? 'GET' : 'POST', headers: { authorization: 'Bearer ' + buildConnection.token, ...headers }, body, redirect: 'error', signal: AbortSignal.timeout(840_000) });
    if (!response.ok) throw new Error('Engine build service refused the operation');
    return response.json();
  }
  async function remoteHead() {
    const credentials = await readJson(config.githubToken);
    const response = await fetcher(`https://api.github.com/repos/${config.repository}/git/ref/heads/${config.branch}`, { headers: { authorization: 'Bearer ' + credentials.token, accept: 'application/vnd.github+json', 'user-agent': 'enginebox-source-driver' }, redirect: 'error', signal: AbortSignal.timeout(15_000) });
    if (!response.ok) throw new Error('GitHub connection unavailable');
    return (await response.json()).object.sha;
  }
  async function seal() {
    if (!current || current.closed) return;
    await power('stop');
    await storage('incus-detach', current.workspace);
    await storage('checkpoint', current.workspace, '--message', 'Native assistant source checkpoint');
    await storage('close', current.workspace);
    current.closed = true;
    await writeJson(currentPath, current);
  }
  async function phase(value, fields = {}) {
    journal = { ...journal, ...fields, phase: value, updatedAt: new Date().toISOString() };
    await writeJson(journalPath, journal);
  }
  try {
    if (request.action === 'prepare') {
      if (journal) throw new Error('Existing source attempt requires reconciliation; it is never replayed');
      try { const health = await build('/health'); if (!health.ok || health.project !== config.sourceProject || health.busy) throw new Error(); }
      catch { throw new Refusal('The Unity build runner is offline or busy. Bring its registered machine online and retry. No model run started.'); }
      const settings = await storage('github-settings', config.sourceProject);
      if (!settings.sync.enabled || await remoteHead() !== settings.sync.last_synced_commit) throw new Refusal('GitHub has changes outside this workspace. Reconcile the repository before starting another edit. No model run started.');
      journal = { schema: 1, attempt: request.attempt, id, workspace: 'code-' + id.slice(0, 24), expectedMain: settings.main.revision, remoteCommit: settings.sync.last_synced_commit };
      await phase('preparing');
      await seal();
      await power('stop');
      const args = ['github-fork', config.sourceProject, journal.workspace];
      if (current && !current.published) args.push('--from', current.workspace);
      await storage(...args);
      await storage('incus-attach', journal.workspace, '--project', config.incusProject, '--instance', config.workspace, '--owner', config.owner, '--uid', '1001', '--gid', '1001');
      await power('resume');
      await storage('incus-verify', journal.workspace);
      await guest('mkdir', '-p', '/workspace/source');
      await guest('mount', '--bind', '/project', '/workspace/source');
      current = { workspace: journal.workspace, attempt: request.attempt, closed: false, published: false };
      await writeJson(currentPath, current);
      await phase('prepared');
      return { ok: true, summary: 'Private LayerFS source is ready for native Codex.' };
    }
    if (journal?.phase === 'published') return { ok: true, summary: journal.summary };
    if (journal?.phase !== 'prepared' || current?.attempt !== request.attempt) throw new Error('Source completion does not match its prepared attempt');
    await phase('sealing');
    await seal();
    const snapshot = await storage('dev-export', config.sourceProject, '--workspace', journal.workspace);
    const unchanged = await storage('dev-export', config.sourceProject);
    if (snapshot.manifest.treeHash === unchanged.manifest.treeHash) {
      current.published = true;
      await writeJson(currentPath, current);
      const summary = 'No source changes were needed. The existing GitHub revision and live game are unchanged.';
      await phase('published', { summary, unchanged: true });
      return { ok: true, summary };
    }
    const archive = join(directory, 'source.tar.gz');
    await exec(['/usr/bin/tar', '-czf', archive, '-C', snapshot.sourceRoot, '.']);
    const bytes = await fs.readFile(archive), digest = createHash('sha256').update(bytes).digest('hex');
    await phase('validating', { snapshotRevision: snapshot.revision, treeHash: snapshot.manifest.treeHash, sourceDigest: digest });
    const validated = await build('/builds/' + id, bytes, { 'content-type': 'application/gzip', 'x-source-sha256': digest });
    if (validated.status !== 'validated' || !validated.ok || validated.sourceDigest !== digest) {
      await phase('validation_failed', { build: validated });
      throw new Refusal('The Unity build or game-core tests failed. Your edits are retained for the next request; GitHub and the live game were not changed.');
    }
    const settings = await storage('github-settings', config.sourceProject);
    if (settings.main.revision !== journal.expectedMain || await remoteHead() !== journal.remoteCommit) {
      await phase('needs_reconciliation');
      throw new Refusal('The validated source is retained, but main or GitHub advanced during this edit. Reconcile before publishing.');
    }
    await phase('promoting', { build: validated });
    const promoted = await storage('github-promote', config.sourceProject, journal.workspace, '--expected-main', journal.expectedMain, '--message', 'Validated native assistant change', '--token-file', config.githubToken);
    await phase('promoted', { promotion: promoted });
    const attempt = promoted.sync?.attempt;
    if (attempt?.status !== 'delivered' || !/^[a-f0-9]{40}$/.test(attempt.delivery_commit)) throw new Error('Accepted source is waiting for GitHub reconciliation; publication was withheld');
    const accepted = await storage('dev-export', config.sourceProject);
    if (accepted.manifest.treeHash !== snapshot.manifest.treeHash) throw new Error('Reconciled source differs from the validated build; publication was withheld');
    await phase('publishing', { commit: attempt.delivery_commit });
    const publication = await build('/builds/' + id + '/publish', JSON.stringify({ commit: attempt.delivery_commit }), { 'content-type': 'application/json' });
    if (publication.status !== 'published' || publication.commit !== attempt.delivery_commit) throw new Error('Publication acknowledgement missing');
    current.published = true;
    await writeJson(currentPath, current);
    const summary = `Unity build and game-core tests passed. Synced GitHub commit ${attempt.delivery_commit.slice(0, 12)} and published ${publication.url}.`;
    await phase('published', { publication, summary });
    return { ok: true, summary };
  } catch (error) {
    await log.write(JSON.stringify({ error: error.message, phase: journal?.phase }) + '\n');
    return { ok: false, uncertain: !(error instanceof Refusal), summary: error instanceof Refusal ? error.message : 'The source operation needs owner reconciliation. Edits and private receipts are retained; the operation will not be replayed automatically.' };
  } finally { await log.close(); await fs.rmdir(lock); }
}
if (process.argv[1] === fileURLToPath(import.meta.url)) {
  try {
    let input = ''; for await (const chunk of process.stdin) { input += chunk; if (input.length > 4096) throw new Error('Request exceeds limit'); }
    const result = await lifecycle(JSON.parse(await fs.readFile(process.argv[2], 'utf8')), JSON.parse(input));
    process.stdout.write(JSON.stringify(result));
  } catch { process.stdout.write(JSON.stringify({ ok: false, uncertain: true, summary: 'The private source service requires owner inspection.' })); }
}

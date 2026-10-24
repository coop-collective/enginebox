// Operator-only publication. This produces an approval document; it never approves a job's output.
import { lstat, readFile, writeFile } from 'node:fs/promises';
import { isAbsolute } from 'node:path';
import { pathToFileURL } from 'node:url';
import { canonical, requireThat, sha256, validatePreparation, type Preparation } from '@enginebox/fleet-protocol';
import { Client } from './client.ts';
import { publishSnapshot } from './generations.ts';

type Scope = Pick<Preparation, 'namespace' | 'repository' | 'trust' | 'image' | 'pnpm'>;
type Snapshot = { ok: boolean; snapshot: string; projectRecord: string; projectRecordHash: string; layerfsRevision: string; projectName: string; preparationKey: string; preparationRecipe: string };
export async function publishPreparation(client: Client, project: string, state: string, snapshot: Snapshot, scope: Scope) {
  requireThat(snapshot.ok === true && snapshot.preparationRecipe === 'pnpm-installed-v1' && snapshot.layerfsRevision === '968a5d0be0ea3e9456b06f9148b65523138e9490', 'Pinned native preparation snapshot required');
  const info = await lstat(snapshot.projectRecord);
  requireThat(info.isFile() && !info.isSymbolicLink() && info.size > 0 && info.size <= 65535, 'Invalid discovery record');
  const record = await readFile(snapshot.projectRecord);
  requireThat(await sha256(record) === snapshot.projectRecordHash, 'Discovery record changed');
  const proposal = { schema: 1, project, key: snapshot.preparationKey, generation: '0'.repeat(64), recipe: 'pnpm-installed-v1', ...scope, scripts: 'deny', credentialFree: true, projectRecord: record.toString('base64') };
  validatePreparation(proposal);
  const published = await publishSnapshot(client, project, snapshot.snapshot, state, null, {
    preparationKey: snapshot.preparationKey, preparationRecipe: snapshot.preparationRecipe,
    projectRecordHash: snapshot.projectRecordHash, layerfsRevision: snapshot.layerfsRevision, projectName: snapshot.projectName,
  });
  return validatePreparation({ ...proposal, generation: published.id });
}
async function main(path: string) {
  const info = await lstat(path);
  requireThat(info.isFile() && !info.isSymbolicLink() && info.size < 64 * 1024 && (process.platform === 'win32' || (info.mode & 0o077) === 0), 'Private publication configuration required');
  const config = JSON.parse(await readFile(path, 'utf8'));
  requireThat(config.schema === 1 && config.credentialFree === true && isAbsolute(config.state) && isAbsolute(config.receipt) && isAbsolute(config.approval), 'Explicit credential-free publication required');
  const receiptInfo = await lstat(config.receipt);
  requireThat(receiptInfo.isFile() && !receiptInfo.isSymbolicLink() && receiptInfo.size < 64 * 1024, 'Invalid snapshot receipt');
  const receipt = JSON.parse(await readFile(config.receipt, 'utf8'));
  const approval = await publishPreparation(new Client(config.controller, config.token), config.project, config.state, receipt, config.scope);
  await writeFile(config.approval, canonical(approval), { mode: 0o600, flag: 'wx' });
  console.log(JSON.stringify({ published: true, approved: false, project: approval.project, key: approval.key, generation: approval.generation, approval: config.approval }));
}
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) main(process.argv[2]).catch(() => { console.error('Preparation publication failed; inspect private operator receipts.'); process.exitCode = 1; });

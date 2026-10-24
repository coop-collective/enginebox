import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { DatabaseSync } from 'node:sqlite';
import { canonical, sha256, type Preparation } from '@enginebox/fleet-protocol';
import { Client } from '../src/client.ts';
import { Cache } from '../src/cache.ts';
import { Generations, type Published } from '../src/generations.ts';
import { publishPreparation } from '../src/publish-preparation.ts';
import { resolvePreparation } from '../src/preparation.ts';

class Transport extends Client {
  objects = new Map<string, Uint8Array>();
  published!: Published;
  preparation!: Preparation;
  reads = 0;
  revokeAfter = Infinity;
  constructor() { super('http://fixture.invalid', 'test-only'); }
  override async request(path: string, init: RequestInit = {}) {
    if (init.method === 'PUT') {
      const bytes = new Uint8Array(init.body as Uint8Array);
      assert.equal(await sha256(bytes), path.split('/').at(-1));
      this.objects.set(path, bytes); return Response.json({ ok: true });
    }
    const bytes = this.objects.get(path);
    return new Response(init.method === 'HEAD' ? null : bytes as BodyInit, { status: bytes ? 200 : 404, headers: { 'content-length': String(bytes?.length ?? 0) } });
  }
  override async json<T>(path: string, body?: unknown): Promise<T> {
    if (body) { this.published = { id: await sha256(canonical(body)), manifest: body as Published['manifest'], sequence: 1 }; return this.published as T; }
    if (path.includes('/preparations/')) { if (++this.reads > this.revokeAfter) throw new Error('Revoked'); return this.preparation as T; }
    if (path.includes('/routes/')) return { generation: this.published.id, expires: Date.now() + 60_000, origins: [] } as T;
    return this.published as T;
  }
}
test('approved dependency generation hydrates on an empty node; revocation and wrong repository stop activation', async t => {
  const root = await mkdtemp(join(tmpdir(), 'ebox-prep-transfer-')); t.after(() => rm(root, { recursive: true, force: true }));
  const source = new DatabaseSync(join(root, 'source.sqlite'));
  source.exec("CREATE TABLE fixture(value TEXT); INSERT INTO fixture VALUES ('prepared-dependencies')"); source.close();
  const client = new Transport(), key = 'a'.repeat(64);
  const expected = { namespace: 'installation-2', repository: '101', trust: 'repository-read' as const, image: 'b'.repeat(64), pnpm: '12.15.0' };
  const record = join(root, 'project.record'); await writeFile(record, 'fixture-discovery', { mode: 0o600 });
  client.preparation = await publishPreparation(client, 'fixture', join(root, 'staging'), {
    ok: true, snapshot: join(root, 'source.sqlite'), projectRecord: record, projectRecordHash: await sha256('fixture-discovery'),
    layerfsRevision: '968a5d0be0ea3e9456b06f9148b65523138e9490', projectName: 'fixture', preparationKey: key, preparationRecipe: 'pnpm-installed-v1',
  }, expected);
  const state = join(root, 'empty-node'); await mkdir(state);
  const cache = new Cache(state, 16 * 1024 * 1024, 0); t.after(() => cache.db.close());
  const generations = new Generations(client, cache, state);
  let activated = 0;
  const activate = async (snapshot: string) => {
    const db = new DatabaseSync(snapshot, { readOnly: true });
    assert.equal(db.prepare('SELECT value FROM fixture').get()!.value, 'prepared-dependencies'); db.close(); activated++;
  };
  await resolvePreparation(client, generations, 'fixture', key, expected, activate);
  assert.equal(activated, 1); assert.ok(generations.stats.originFetches > 0);
  const fetched = generations.stats.originFetches;
  await resolvePreparation(client, generations, 'fixture', key, expected, activate);
  assert.equal(generations.stats.originFetches, fetched); assert.equal(activated, 2);
  const hydrate = generations.hydrate.bind(generations);
  generations.hydrate = async () => { throw new Error('Warm native base must not rescan/hydrate a snapshot'); };
  const warm = await resolvePreparation(client, generations, 'fixture', key, expected, activate, async () => true);
  assert.equal(warm.source, 'native-warm'); assert.equal(activated, 2);
  generations.hydrate = hydrate;
  await assert.rejects(resolvePreparation(client, generations, 'fixture', key, { ...expected, repository: '102' }, activate), /scope/);
  await assert.rejects(resolvePreparation(client, generations, 'fixture', key, {} as typeof expected, activate), /scope/);
  client.revokeAfter = client.reads + 1;
  await assert.rejects(resolvePreparation(client, generations, 'fixture', key, expected, activate), /Revoked/);
  assert.equal(activated, 2);
});

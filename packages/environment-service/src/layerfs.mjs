// The protected operator command reads the owning local LayerFS runtime (or,
// for a repository preview, exports one branch of a hosted or linked
// repository; src/repository-export.mjs). It is never supplied by a repository
// or a browser request. Distributed accepted generations use the separate
// `accepted` adapter.
import { createHash } from 'node:crypto';
import { realpath, lstat } from 'node:fs/promises';
import { isAbsolute, join } from 'node:path';
import { check } from './config.mjs';

export async function layerfsSource(env, stateRoot, run) {
  const [program, ...argv] = env.source.command;
  const value = JSON.parse(await run(program, argv, { timeout: 300000 }));
  const manifest = value.manifest;
  if (env.source.kind === 'repository') {
    check(/^[a-f0-9]{64}$/.test(value.revision) && manifest?.schema === 1 && manifest.authority === 'repository-branch'
      && manifest.revision === value.revision && manifest.repository === env.source.repository && manifest.branch === env.source.branch
      && /^[a-f0-9]{40,64}$/.test(manifest.commit ?? ''), 'repository-export-provenance-mismatch');
  } else check(/^[a-f0-9]{64}$/.test(value.revision) && manifest?.schema === 1 && manifest.authority === (env.source.kind === 'workspace' ? 'local-layerfs-workspace' : 'local-layerfs')
    && manifest.revision === value.revision && manifest.project === env.source.project
    && (manifest.workspace ?? null) === (env.source.workspace ?? null), 'layerfs-export-provenance-mismatch');
  if (env.source.kind === 'workspace') check(manifest.consistency === 'exclusive-command-boundary' && typeof manifest.sandbox === 'string' && typeof manifest.branch === 'string', 'workspace-consistency-proof-required');
  check(Array.isArray(manifest.entries) && manifest.entries.length <= 100000
    && createHash('sha256').update(JSON.stringify(manifest.entries)).digest('hex') === manifest.treeHash, 'layerfs-export-inventory-mismatch');
  check(typeof value.sourceRoot === 'string' && isAbsolute(value.sourceRoot)
    && await realpath(value.sourceRoot) === value.sourceRoot && (await lstat(value.sourceRoot)).isDirectory(), 'layerfs-export-root-invalid');
  return { revision: value.revision, sourceRoot: value.sourceRoot, root: join(stateRoot, env.id), manifest };
}

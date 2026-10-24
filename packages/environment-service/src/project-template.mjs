// Repo declarations contain container recipes, never host commands or mounts.
import { open, lstat } from 'node:fs/promises';
import { constants } from 'node:fs';
import { join, resolve } from 'node:path';
import { createHash } from 'node:crypto';
import { check, fields, id, sourcePath, validateConfig } from './config.mjs';

export const digest = value => createHash('sha256').update(typeof value === 'string' || Buffer.isBuffer(value) ? value : JSON.stringify(value)).digest('hex');
export const projectId = value => typeof value === 'string' && /^[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?$/.test(value);
export const configPath = '.enginebox/config.json';

export async function verifiedJson(source, path) {
  check(sourcePath(path), 'invalid-project-config-path');
  const entries = source.manifest.entries.filter(e => e.path === path);
  check(entries.length === 1 && entries[0].kind === 'file' && entries[0].size <= 65536, 'project-config-must-be-small-file');
  let file = source.sourceRoot;
  for (const part of path.split('/')) {
    file = join(file, part);
    const meta = await lstat(file);
    check(!meta.isSymbolicLink(), 'project-config-symlink-refused');
  }
  const handle = await open(file, constants.O_RDONLY | (constants.O_NOFOLLOW ?? 0) | (constants.O_NONBLOCK ?? 0));
  let bytes;
  try {
    const meta = await handle.stat(); check(meta.isFile() && meta.size === entries[0].size, 'project-config-file-mismatch');
    const buffer = Buffer.alloc(65537); let length = 0;
    while (length < buffer.length) {
      const read = await handle.read(buffer, length, buffer.length - length, length);
      if (!read.bytesRead) break; length += read.bytesRead;
    }
    bytes = buffer.subarray(0, length);
  } finally { await handle.close(); }
  check(bytes.length === entries[0].size && digest(bytes) === entries[0].sha256, 'project-config-integrity-mismatch');
  return JSON.parse(bytes.toString('utf8'));
}

export function validateTemplate(value) {
  fields(value, ['schema_version', 'kind', 'id', 'revision', 'image', 'prepare', 'command', 'cwd', 'ports', 'health', 'cpu', 'memoryMiB', 'timeoutSeconds', 'writableDirs', 'env', 'installInputs']);
  check(value.schema_version === 1 && value.kind === 'dev' && id(value.id) && Number.isSafeInteger(value.revision) && value.revision > 0, 'invalid-dev-template');
  check(Array.isArray(value.installInputs) && value.installInputs.length <= 64 && value.installInputs.every(sourcePath) && new Set(value.installInputs).size === value.installInputs.length, 'invalid-install-inputs');
  check(Array.isArray(value.ports), 'invalid-template-ports');
  for (const p of value.ports) {
    fields(p, ['name', 'protocol', 'port', 'expose']);
    check(p.expose === undefined || typeof p.expose === 'boolean', 'invalid-port-exposure');
    check(!p.expose || p.protocol === 'http', 'automatic-exposure-requires-http');
  }
  const env = templateEnvironment(value, 'validation', { kind: 'layerfs', project: 'validation', command: ['/export'] }, value.ports.map((_, i) => 20000 + i));
  env.source.command = [resolve('/export')];
  validateConfig({ schema: 1, stateDir: resolve('/state'), tokenFile: resolve('/token'), listenPort: 19999, baseDomains: ['example.com'], httpEntrypoint: 'web', httpTls: false, basicAuthUsers: [], transportEntrypoints: [], pollSeconds: 2, environments: [env] });
  return value;
}

export function templateEnvironment(t, name, source, ports) {
  return { id: name, source, image: t.image, updateMode: 'files', command: t.command, prepare: t.prepare ?? [], cwd: t.cwd ?? '.', env: t.env ?? {}, writableDirs: t.writableDirs ?? [], cpu: t.cpu ?? 1, memoryMiB: t.memoryMiB ?? 1024, timeoutSeconds: t.timeoutSeconds ?? 300,
    health: t.health, ports: t.ports.map((p, i) => ({ name: p.name, protocol: p.protocol, container: p.port, host: ports[i] })) };
}

export async function resolveProject(source, catalog = []) {
  if (!source.manifest.entries.some(e => e.path === configPath)) return null;
  const config = await verifiedJson(source, configPath);
  fields(config, ['schema_version', 'dev']);
  check(config.schema_version === 1, 'invalid-project-config-version');
  fields(config.dev, ['template']);
  const ref = config.dev.template;
  check(typeof ref === 'string', 'dev-template-required');
  let template; const paths = [configPath];
  if (ref.startsWith('./templates/')) {
    const path = `.enginebox/${ref.slice(2)}`;
    check(sourcePath(path) && path.endsWith('.json'), 'invalid-template-path');
    paths.push(path); template = await verifiedJson(source, path);
  } else {
    check(/^[a-z][a-z0-9-]{0,39}@[1-9][0-9]*$/.test(ref), 'invalid-template-reference');
    template = catalog.find(t => `${t.id}@${t.revision}` === ref);
    check(template, 'dev-template-not-registered');
  }
  validateTemplate(template);
  const inputs = Object.create(null);
  for (const path of [...paths, ...template.installInputs]) {
    const entries = source.manifest.entries.filter(e => e.path === path);
    check(entries.length === 1 && entries[0].kind === 'file' && /^[a-f0-9]{64}$/.test(entries[0].sha256), 'install-input-missing-or-invalid');
    inputs[path] = entries[0].sha256;
  }
  const templateDigest = digest(template);
  return { template, templateDigest, inputs, fingerprint: digest({ templateDigest, inputs }) };
}

import { createHash } from 'node:crypto';
import { copyFileSync, mkdirSync, openSync, closeSync, writeFileSync, writeSync } from 'node:fs';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
const root = process.argv[2];
mkdirSync(root, { recursive: false });
mkdirSync(join(root, 'src'));
mkdirSync(join(root, 'files'));
writeFileSync(join(root, 'README.md'), 'Engine Box disposable LayerFS fixture. No private source or credentials.\n');
writeFileSync(join(root, '.gitignore'), 'node_modules/\ndist/\n*.tsbuildinfo\n');
writeFileSync(join(root, 'package.json'), JSON.stringify({ name: 'enginebox-layerfs-fixture', private: true, version: '1.0.0', packageManager: 'pnpm@12.14.0', devDependencies: { typescript: '5.9.3' } }, null, 2));
writeFileSync(join(root, 'tsconfig.json'), JSON.stringify({ compilerOptions: { composite: true, removeComments: true, outDir: 'dist', target: 'ES2022', module: 'CommonJS' }, include: ['src/**/*.ts'] }, null, 2));
writeFileSync(join(root, 'src/index.ts'), 'export const answer: number = 42;\n');
writeFileSync(join(root, 'mtime-sentinel.txt'), 'unchanged content for explicit mtime-only commit probe\n');
copyFileSync(new URL('./probe.mjs', import.meta.url), join(root, 'validation-probe.mjs'));
copyFileSync(new URL('./manifest.mjs', import.meta.url), join(root, 'validation-manifest.mjs'));
const hash = createHash('sha256');
const file = openSync(join(root, 'payload.bin'), 'wx');
for (let i = 0; i < 4096; i++) {
  const block = Buffer.alloc(4096);
  for (let j = 0; j < 128; j++) createHash('sha256').update(`enginebox-layerfs-v1:${i}:${j}`).digest().copy(block, j * 32);
  writeSync(file, block); hash.update(block);
  if (i < 512) writeFileSync(join(root, 'files', `${String(i).padStart(4, '0')}.txt`), block.toString('hex'));
}
closeSync(file);
writeFileSync(join(root, 'payload.sha256'), `${hash.digest('hex')}  payload.bin\n`);
for (const args of [['init', '-q'], ['config', 'user.email', 'fixture@example.invalid'], ['config', 'user.name', 'Disposable fixture']]) {
  const r = spawnSync('git', args, { cwd: root, stdio: 'inherit' });
  if (r.status) process.exit(r.status);
}

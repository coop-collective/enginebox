import { mkdtemp, mkdir, readFile, writeFile, cp, rm, realpath } from 'node:fs/promises';
import { resolve, join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { check, cli, version, run, selectArtifact } from './lib.mjs';

export function pinnedSources(cargo, lock, pin) {
  version(pin);
  const section = /(^\[workspace\.package\][\s\S]*?^version = ")[^"\r\n]+(")(?=\r?$)/m;
  check(section.test(cargo), 'workspace-version-missing');
  let packages = 0;
  const nextLock = lock.replace(/(\[\[package\]\]\r?\nname = "enginebox-[^"]+"\r?\nversion = ")[^"\r\n]+("\r?\n)/g, (_, before, after) => { packages++; return `${before}${pin}${after}`; });
  check(packages >= 5, 'workspace-lock-records-missing');
  return { cargo: cargo.replace(section, (_, before, after) => `${before}${pin}${after}`), lock: nextLock };
}
export async function main(args = process.argv.slice(2)) {
  const options = cli(args, ['version', 'output']);
  const pin = version(options.version);
  check(options.output && process.platform === 'linux' && process.arch === 'x64', 'build-requires-linux-x86-64');
  const root = await realpath(resolve(dirname(fileURLToPath(import.meta.url)), '../../..'));
  const source = (await run('git', ['rev-parse', 'HEAD'], { cwd: root })).trim();
  check(/^[a-f0-9]{40}$/.test(source), 'source-commit');
  check((await run('git', ['status', '--porcelain', '--untracked-files=no'], { cwd: root })).trim() === '', 'build-requires-clean-tracked-checkout');
  await mkdir(join(root, '.state'), { recursive: true });
  const temporary = await mkdtemp(join(root, '.state/deploy-build-'));
  const checkout = join(temporary, 'source'); await mkdir(checkout);
  try {
    await run('git', ['archive', '--format=tar', '--output', join(temporary, 'source.tar'), source], { cwd: root });
    await run('tar', ['-xf', join(temporary, 'source.tar'), '-C', checkout]);
    const pinned = pinnedSources(await readFile(join(checkout, 'Cargo.toml'), 'utf8'), await readFile(join(checkout, 'Cargo.lock'), 'utf8'), pin);
    await writeFile(join(checkout, 'Cargo.toml'), pinned.cargo);
    await writeFile(join(checkout, 'Cargo.lock'), pinned.lock);
    const pkg = JSON.parse(await readFile(join(checkout, 'package.json'), 'utf8'));
    pkg.version = pin; await writeFile(join(checkout, 'package.json'), `${JSON.stringify(pkg, null, 2)}\n`);
    const target = join(root, 'target/deploy-linux');
    const env = { ...process.env, CARGO_TARGET_DIR: target };
    for (const args of [['install', '--frozen-lockfile'], ['check'], ['test'], ['lint:rust'], ['build']]) {
      await run('pnpm', args, { cwd: checkout, env, inherit: true, timeout: 3600000 });
    }
    const host = await run('pnpm', ['exec', 'rustc', '-vV'], { cwd: checkout, env });
    check(host.includes('host: x86_64-unknown-linux-gnu'), 'build-toolchain-target');
    const dashboard = join(temporary, 'dashboard');
    await cp(join(checkout, 'packages/dashboard/dist/standalone'), dashboard, { recursive: true, dereference: true });
    await run(join(target, 'release/enginebox-release'), ['--version', pin, '--target', 'x86_64-unknown-linux-gnu', '--bin-dir', join(target, 'release'), '--license', join(checkout, 'LICENSE'), '--notice', join(checkout, 'NOTICE'), '--dashboard', dashboard, '--dashboard-start', join(checkout, 'packages/dashboard/scripts/start.mjs'), '--output', resolve(options.output)], { timeout: 600000 });
    const selected = await selectArtifact(options.output, pin, 'x86_64-unknown-linux-gnu');
    await writeFile(join(resolve(options.output), 'provenance.json'), JSON.stringify({ source_commit: source, version: pin, target: selected.value.target, manifest_sha256: selected.manifest_sha256 }, null, 2));
    process.stdout.write(`${JSON.stringify({ built: pin, source_commit: source, manifest_sha256: selected.manifest_sha256 })}\n`);
  } finally {
    await rm(temporary, { recursive: true, force: true });
  }
}
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main().catch(error => {
  const code = /^[a-z0-9-]+$/.test(error.message) ? error.message : 'release-build-failed';
  process.stderr.write(`${code}\n`); process.exitCode = 1;
});

import { parseArgs } from 'node:util';
import { build, prepare, json } from './pnpm.mjs';
const { positionals, values } = parseArgs({ allowPositionals: true, options: { source: { type: 'string' }, root: { type: 'string' }, policy: { type: 'string' }, offline: { type: 'boolean', default: false }, 'require-hit': { type: 'boolean', default: false } } });
try {
  if (!['build', 'prepare'].includes(positionals[0]) || !values.source || !values.root || !values.policy) throw new Error('Use build|prepare --source PATH --root PATH --policy FILE [--offline] [--require-hit]');
  const operation = positionals[0] === 'build' ? build : prepare;
  console.log(JSON.stringify(await operation({ source: values.source, root: values.root, policy: await json(values.policy), offline: values.offline, requireHit: values['require-hit'] })));
} catch (error) { console.error(error.message); process.exitCode = 1; }

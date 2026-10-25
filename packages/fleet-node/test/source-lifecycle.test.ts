import { test } from 'node:test';
import assert from 'node:assert/strict';
import { sourceAction, SourceLifecycleError, validateLifecycle } from '../src/source-lifecycle.ts';

test('only an operator-configured executable receives the bounded source request', async () => {
  const command = [process.execPath, '-e', 'let s="";process.stdin.on("data",c=>s+=c);process.stdin.on("end",()=>{let r=JSON.parse(s);process.stdout.write(JSON.stringify({ok:true,summary:r.action+":"+r.project+":"+r.attempt}))})'];
  assert.equal(await sourceAction({ command }, 'prepare', 'demo', 'attempt', new AbortController().signal), 'prepare:demo:attempt');
  assert.throws(() => validateLifecycle({ command: ['relative-executable'] }));
});
test('a validation refusal is recoverable; a lost lifecycle receipt remains uncertain', async () => {
  const response = (value: unknown) => ({ command: [process.execPath, '-e', `process.stdout.write(${JSON.stringify(JSON.stringify(value))})`] });
  await assert.rejects(sourceAction(response({ ok: false, uncertain: false, summary: 'Unity compilation failed; source retained.' }), 'complete', 'demo', 'attempt', new AbortController().signal), e => e instanceof SourceLifecycleError && !e.uncertain);
  await assert.rejects(sourceAction({ command: [process.execPath, '-e', 'process.exit(1)'] }, 'complete', 'demo', 'attempt', new AbortController().signal), e => e instanceof SourceLifecycleError && e.uncertain);
});

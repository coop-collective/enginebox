// Manual Cursor Cloud API v1 experiment. Credentials stay in the environment.
import { mkdir, writeFile, access } from 'node:fs/promises';
import { dirname } from 'node:path';
import { parseArgs } from 'node:util';

const { values: options } = parseArgs({ options: {
  output: { type: 'string' }, repo: { type: 'string' }, ref: { type: 'string', default: 'main' },
  'run-live': { type: 'boolean', default: false },
} });
if (!options.output || !options.repo || !options['run-live']) {
  throw new Error('Require --run-live --repo <public-repository-url> --output <private-receipt.json>');
}
if (!process.env.CURSOR_API_KEY) throw new Error('Set CURSOR_API_KEY locally; do not paste it into a report');
await access(options.output).then(() => { throw new Error('Receipt already exists'); }, e => {
  if (e.code !== 'ENOENT') throw e;
});
await mkdir(dirname(options.output), { recursive: true, mode: 0o700 });
const authorization = `Basic ${Buffer.from(`${process.env.CURSOR_API_KEY}:`).toString('base64')}`;
const receipt = { schema: 1, recorded_at: new Date().toISOString(), provider: 'cursor-cloud', events: [] };
const save = () => writeFile(options.output, JSON.stringify(receipt, null, 2), { mode: 0o600 });
async function request(path, body, timeout = 30_000) {
  const response = await fetch(`https://api.cursor.com${path}`, {
    method: body ? 'POST' : 'GET', headers: { Authorization: authorization, 'Content-Type': 'application/json' },
    body: body ? JSON.stringify(body) : undefined, signal: AbortSignal.timeout(timeout), redirect: 'error',
  });
  if (!response.ok) {
    // Persist only status/code, never a response that could echo credentials.
    const error = new Error(`Cursor API ${response.status}`);
    error.status = response.status;
    throw error;
  }
  return response;
}
let ownedRun;
let terminal = false;
try {
  receipt.stage = 'authentication';
  await request('/v1/me');
  receipt.stage = 'launch';
  const start = performance.now();
  const body = {
    name: 'Engine Box read-only startup benchmark', autoCreatePR: false,
    repos: [{ url: options.repo, startingRef: options.ref }],
    prompt: { text: 'Startup benchmark only. Read package.json in the repository root and reply with its package name followed by ENGINEBOX_STARTUP_OK. Do not edit files, install dependencies, create commits, push branches, open pull requests, or call external tools. Stop after answering.' },
  };
  const created = await (await request('/v1/agents', body)).json();
  ownedRun = { agent: created.agent.id, run: created.run.id };
  receipt.agent_id = ownedRun.agent;
  receipt.run_id = ownedRun.run;
  receipt.admission_seconds = (performance.now() - start) / 1000;
  receipt.stage = 'stream';
  await save();
  const response = await request(`/v1/agents/${ownedRun.agent}/runs/${ownedRun.run}/stream`, undefined, 240_000);
  let buffer = '';
  const decoder = new TextDecoder();
  for await (const chunk of response.body) {
    buffer += decoder.decode(chunk, { stream: true }).replace(/\r\n/g, '\n');
    if (buffer.length > 2 * 1024 * 1024) throw new Error('SSE event exceeds limit');
    let boundary;
    while ((boundary = buffer.indexOf('\n\n')) >= 0) {
      const block = buffer.slice(0, boundary);
      buffer = buffer.slice(boundary + 2);
      const event = block.split('\n').find(l => l.startsWith('event:'))?.slice(6).trim();
      const data = block.split('\n').filter(l => l.startsWith('data:')).map(l => l.slice(5).trimStart()).join('\n');
      if (!event || !data) continue;
      const payload = JSON.parse(data);
      const seconds = (performance.now() - start) / 1000;
      if (['status', 'tool_call', 'result', 'error'].includes(event)) {
        receipt.events.push({ event, seconds, status: payload.status, name: payload.name, code: payload.code });
      }
      if (event === 'tool_call' && receipt.first_tool_observed_seconds === undefined) receipt.first_tool_observed_seconds = seconds;
      if (event === 'assistant' && receipt.first_text_observed_seconds === undefined) receipt.first_text_observed_seconds = seconds;
      if (event === 'result') {
        terminal = true;
        receipt.status = payload.status;
        receipt.completion_observed_seconds = seconds;
        receipt.provider_duration_ms = payload.durationMs;
        receipt.marker_present = payload.text?.includes('ENGINEBOX_STARTUP_OK') ?? false;
      }
    }
    if (terminal) break;
  }
  if (!terminal) throw new Error('Stream ended without terminal result');
  receipt.stage = 'complete';
} catch (error) {
  receipt.error = error.status ? `HTTP ${error.status}` : 'Probe interrupted; inspect the owned run before retrying';
  receipt.http_status = error.status;
  process.exitCode = 1;
} finally {
  if (ownedRun) {
    if (!terminal) {
      try {
        await request(`/v1/agents/${ownedRun.agent}/runs/${ownedRun.run}/cancel`, {});
        receipt.cancel_requested = true;
      } catch { receipt.cancel_requested = false; }
    }
    if (terminal) {
      try {
        await request(`/v1/agents/${ownedRun.agent}/archive`, {});
        receipt.archived = true;
      } catch { receipt.archived = false; }
    }
  }
  await save();
  console.log(JSON.stringify({ stage: receipt.stage, status: receipt.status, error: receipt.error,
    admission_seconds: receipt.admission_seconds, first_tool_seconds: receipt.first_tool_observed_seconds,
    completion_seconds: receipt.completion_observed_seconds }));
}

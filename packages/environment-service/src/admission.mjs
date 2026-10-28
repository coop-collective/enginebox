import { open } from 'node:fs/promises';
import { constants } from 'node:fs';
import { check, fields } from './config.mjs';

export const admissionBlocked = error => ['node-admission-denied', 'node-admission-unavailable'].includes(error?.message);

// This credential belongs only to the trusted operator process. Never pass it
// to source exporters, Docker arguments, application environments or receipts.
export async function assertAdmission(config) {
  if (!config.admission) return;
  const { controller, tokenFile, hostId } = config.admission;
  try {
    const file = await open(tokenFile, constants.O_RDONLY | constants.O_NOFOLLOW);
    let token;
    try {
      const metadata = await file.stat();
      check(metadata.isFile() && !(metadata.mode & 0o077) && metadata.size <= 4096, 'private-admission-token-required');
      token = (await file.readFile('utf8')).trim();
      check(token.length >= 32 && token.length <= 4096 && !/\s/.test(token), 'private-admission-token-required');
    } finally { await file.close(); }
    const response = await fetch(new URL(`/v1/hosts/${hostId}/admission`, controller), {
      headers: { authorization: `Bearer ${token}`, accept: 'application/json' },
      redirect: 'error', signal: AbortSignal.timeout(1500),
    });
    let body = '';
    try {
      check(response.status === 200 && /^application\/json(?:;|$)/i.test(response.headers.get('content-type') ?? ''), 'invalid-admission-response');
      for await (const chunk of response.body) {
        body += Buffer.from(chunk).toString('utf8');
        check(Buffer.byteLength(body) <= 4096, 'invalid-admission-response');
      }
    } finally { if (!response.bodyUsed) await response.body?.cancel(); }
    const value = JSON.parse(body);
    fields(value, ['host_id', 'allowed']);
    check(value.host_id === hostId && typeof value.allowed === 'boolean', 'invalid-admission-response');
    if (!value.allowed) throw new Error('node-admission-denied');
  } catch (error) {
    // Do not expose URLs, response bodies, token paths or upstream diagnostics.
    throw new Error(error.message === 'node-admission-denied' ? error.message : 'node-admission-unavailable');
  }
}

import { bodyJson, config, HttpError, json, requireOrigin, requireSession } from './server';

const nodeActions = new Set(['enroll', 'hello', 'heartbeat', 'poll', 'result']);
const quote = (value: string) => `'${value.replaceAll("'", "''")}'`;
export function installCommand(code: string, expires?: number) {
  const url = process.env.ENGINEBOX_NODE_INSTALLER_URL ?? '';
  const hash = process.env.ENGINEBOX_NODE_INSTALLER_SHA256 ?? '';
  const origin = process.env.ENGINEBOX_NODE_PUBLIC_ORIGIN ?? config().origin;
  if (!/^https:\/\/github\.com\/coop-collective\/enginebox\/releases\/download\/[a-zA-Z0-9_.-]+\/install\.ps1$/.test(url) || !/^[a-f0-9]{64}$/.test(hash))
    throw new HttpError(503, 'The Windows installer release is not configured on this controller.');
  const parsed = new URL(origin);
  if (parsed.protocol !== 'https:' || parsed.origin !== origin || parsed.username || parsed.password)
    throw new HttpError(503, 'Machine registration requires a public HTTPS controller origin.');
  if (!/^[a-f0-9]{32}\.[a-f0-9]{64}$/.test(code))
    throw new HttpError(502, 'Invalid registration response.');
  if (expires !== undefined && (!Number.isSafeInteger(expires) || expires < 1)) throw new HttpError(502, 'Invalid registration expiry.');
  return `& { $p=Join-Path $env:TEMP ('enginebox-'+[guid]::NewGuid()+'.ps1'); try { Invoke-WebRequest -UseBasicParsing ${quote(url)} -OutFile $p; if ((Get-FileHash -Algorithm SHA256 $p).Hash -ne ${quote(hash)}) { throw 'Installer checksum mismatch' }; & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $p -Controller ${quote(origin)} -EnrollmentCode ${quote(code)}${expires === undefined ? '' : ` -EnrollmentExpires ${expires}`} } finally { Remove-Item -LiteralPath $p -Force -ErrorAction SilentlyContinue } }`;
}
async function forward(path: string, method: string, token: string, body?: unknown) {
  const response = await fetch(`${config().apiUrl}${path}`, {
    method, headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body), redirect: 'error',
    signal: AbortSignal.timeout(15000), cache: 'no-store',
  });
  const text = await response.text();
  if (text.length > 350000) throw new HttpError(502, 'Controller response is too large.');
  let value: Record<string, unknown>;
  try { value = JSON.parse(text); } catch { throw new HttpError(502, 'Invalid controller response.'); }
  if (!response.ok) throw new HttpError(response.status, typeof value.error === 'string' ? value.error : 'Controller rejected this operation.');
  return value;
}
export async function contributors(request: Request, segments: string[]) {
  requireSession(request);
  if (request.method !== 'GET') requireOrigin(request);
  if (new URL(request.url).search) throw new HttpError(400, 'Query parameters are not supported.');
  const suffix = segments.join('/');
  const allowed = request.method === 'GET' ? suffix === '' : request.method === 'POST' &&
    (suffix === 'enrollments' || /^[a-f0-9]{32}\/(revoke|reconcile|runs)$/.test(suffix));
  if (!allowed) throw new HttpError(404, 'Unknown machine operation.');
  const body = request.method === 'POST' ? await bodyJson(request, 20000) : undefined;
  // Validate release configuration before consuming a new invitation slot.
  if (suffix === 'enrollments') {
    installCommand(`${'0'.repeat(32)}.${'0'.repeat(64)}`);
    const supervisor = await forward('/__enginebox/status', 'GET', config().apiToken);
    if (supervisor.node_protocol !== 1) throw new HttpError(503, 'The controller supervisor needs the Windows worker protocol update before registration.');
  }
  const result = await forward(`/v1/contributors${suffix ? `/${suffix}` : ''}`, request.method, config().apiToken, body);
  if (suffix === 'enrollments') {
    const command = installCommand(String(result.code), Number(result.expires));
    return json({ id: result.id, expires: result.expires, command });
  }
  return json(result);
}
export async function nodeProxy(request: Request, action: string) {
  if (request.method !== 'POST' || !nodeActions.has(action) || new URL(request.url).search)
    throw new HttpError(404, 'Unknown node operation.');
  if (request.headers.has('origin')) throw new HttpError(403, 'Node API is for installed workers.');
  const token = request.headers.get('authorization')?.match(/^Bearer ([a-f0-9]{32}\.[a-f0-9]{64})$/)?.[1];
  if (!token) throw new HttpError(401, 'Node credential required.');
  // The caller's scoped token is forwarded unchanged. Never substitute owner auth.
  return json(await forward(`/v1/node/${action}`, 'POST', token, await bodyJson(request, 350000)));
}

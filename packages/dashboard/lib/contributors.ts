import { bodyJson, config, HttpError, json, requireOrigin, requireRole, requireSession } from './server';
import { DEFAULT_ORG } from './identity/store';
import { forgetTenancy, hostOrg, tenancy } from './tenancy';

// engine-poll and engine-report are the engine session channel, parallel to the job slot (docs/ENGINE-SESSIONS.md).
const nodeActions = new Set(['enroll', 'hello', 'heartbeat', 'poll', 'result', 'engine-poll', 'engine-report']);
const quote = (value: string) => `'${value.replaceAll("'", "''")}'`;
const shellQuote = (value: string) => `'${value.replaceAll("'", "'\"'\"'")}'`;
type Platform = 'windows' | 'linux';
// Windows engine machines use the native node release's bootstrap; other Windows machines use the WSL installer.
export function installCommand(code: string, expires?: number, platform: Platform = 'windows', engine = false) {
  const linux = platform === 'linux';
  const url = (linux ? process.env.ENGINEBOX_LINUX_NODE_INSTALLER_URL : engine ? process.env.ENGINEBOX_ENGINE_NODE_INSTALLER_URL : process.env.ENGINEBOX_NODE_INSTALLER_URL) ?? '';
  const hash = (linux ? process.env.ENGINEBOX_LINUX_NODE_INSTALLER_SHA256 : engine ? process.env.ENGINEBOX_ENGINE_NODE_INSTALLER_SHA256 : process.env.ENGINEBOX_NODE_INSTALLER_SHA256) ?? '';
  const origin = process.env.ENGINEBOX_NODE_PUBLIC_ORIGIN ?? config().origin;
  const release = linux ? /^https:\/\/github\.com\/coop-collective\/enginebox\/releases\/download\/[a-zA-Z0-9_.-]+\/install-linux-node\.sh$/ : /^https:\/\/github\.com\/coop-collective\/enginebox\/releases\/download\/[a-zA-Z0-9_.-]+\/install\.ps1$/;
  if (!release.test(url) || !/^[a-f0-9]{64}$/.test(hash))
    throw new HttpError(503, `The ${linux ? 'Linux' : engine ? 'Windows engine machine' : 'Windows'} installer release is not configured on this controller.`);
  const parsed = new URL(origin);
  if (parsed.protocol !== 'https:' || parsed.origin !== origin || parsed.username || parsed.password)
    throw new HttpError(503, 'Machine registration requires a public HTTPS controller origin.');
  if (!/^[a-f0-9]{32}\.[a-f0-9]{64}$/.test(code))
    throw new HttpError(502, 'Invalid registration response.');
  if (expires !== undefined && (!Number.isSafeInteger(expires) || expires < 1)) throw new HttpError(502, 'Invalid registration expiry.');
  if (linux) {
    const args = `--controller ${shellQuote(origin)} --enrollment-code ${shellQuote(code)}`;
    return `( set -eu; umask 077; p=$(mktemp); trap 'rm -f -- "$p"' EXIT; curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL ${shellQuote(url)} -o "$p"; printf '%s  %s\\n' ${shellQuote(hash)} "$p" | sha256sum --check --status; if [ "$(id -u)" -eq 0 ]; then bash "$p" ${args}; else sudo bash "$p" ${args}; fi )`;
  }
  return `& { $p=Join-Path $env:TEMP ('enginebox-'+[guid]::NewGuid()+'.ps1'); try { Invoke-WebRequest -UseBasicParsing ${quote(url)} -OutFile $p; if ((Get-FileHash -Algorithm SHA256 $p).Hash -ne ${quote(hash)}) { throw 'Installer checksum mismatch' }; & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $p -Controller ${quote(origin)} -EnrollmentCode ${quote(code)}${expires === undefined ? '' : ` -EnrollmentExpires ${expires}`}${engine ? " -Workload 'engine'" : ''} } finally { Remove-Item -LiteralPath $p -Force -ErrorAction SilentlyContinue } }`;
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
  // Enrolling, renewing and revoking machines is fleet administration. Each
  // machine belongs to one org (docs/ORGS.md): an org sees and manages only its own.
  const who = request.method === 'GET' ? requireSession(request) : requireRole(request, 'admin');
  const view = await tenancy();
  const mine = (id: unknown) => typeof id === 'string' && hostOrg(view, id) === who.org_id;
  if (request.method !== 'GET') requireOrigin(request);
  if (new URL(request.url).search) throw new HttpError(400, 'Query parameters are not supported.');
  const suffix = segments.join('/');
  const allowed = request.method === 'GET' ? suffix === '' : request.method === 'POST' &&
    (suffix === 'enrollments' || /^[a-f0-9]{32}\/(renew-enrollment|revoke|reconcile|runs|engine-owner)$/.test(suffix));
  if (!allowed) throw new HttpError(404, 'Unknown machine operation.');
  const body = request.method === 'POST' ? await bodyJson(request, 20000) : undefined;
  const enrollment = suffix === 'enrollments' || suffix.endsWith('/renew-enrollment');
  let platform: Platform = 'windows';
  let path = `/v1/contributors${suffix ? `/${suffix}` : ''}`;
  let forwardedBody = body;
  let expectedHost: string | undefined;
  let engine = false;
  if (enrollment) {
    if (!body || typeof body !== 'object' || Array.isArray(body)) throw new HttpError(400, 'Send a registration object.');
    if (suffix === 'enrollments') {
      if (body.platform !== undefined && body.platform !== 'windows' && body.platform !== 'linux') throw new HttpError(400, 'Choose Windows or Linux.');
      platform = body.platform ?? 'windows';
      if (platform === 'linux') {
        // Configured Linux servers are part of the install's own configuration.
        if (who.org_id !== DEFAULT_ORG) throw new HttpError(403, 'Configured Linux servers belong to the platform organization. Enroll a Windows or WSL worker into this organization instead.');
        if (Object.keys(body).some(key => !['platform', 'host_id'].includes(key)) || typeof body.host_id !== 'string' || !/^[a-zA-Z0-9_-]{1,63}$/.test(body.host_id)) throw new HttpError(400, 'Select one configured Linux server.');
        expectedHost = body.host_id;
        path = `/v1/hosts/${expectedHost}/enrollment`;
        forwardedBody = {};
      } else {
        const {platform: _platform, ...invitation} = body;
        const workloads = (invitation as {workloads?: {ci?: unknown; agents?: unknown; engine?: unknown}}).workloads;
        engine = workloads?.engine === true;
        // An engine machine runs only its owner's engine sessions; it never takes CI or agent work.
        if (engine && (workloads?.ci === true || workloads?.agents === true)) throw new HttpError(400, 'An engine machine runs engine sessions only. Clear CI and agents.');
        forwardedBody = invitation;
      }
    } else {
      if (Object.keys(body).length) throw new HttpError(400, 'Renewal accepts an empty object.');
      // The durable record decides the installer. Never trust a caller-supplied
      // platform or rotate a code before validating that platform's release.
      const catalog = await forward('/v1/contributors', 'GET', config().apiToken);
      if (!Array.isArray(catalog.nodes)) throw new HttpError(502, 'Invalid registration inventory.');
      const node = catalog.nodes.find((node: Record<string, unknown>) => node.id === segments[0]);
      if (!node || !mine(node.id)) throw new HttpError(404, 'Registration unavailable.');
      if (node.enrolled || node.revoked) throw new HttpError(409, 'Only a pending registration can be renewed.');
      if (node.kind !== undefined && node.kind !== 'workspace' && node.kind !== 'incus') throw new HttpError(502, 'Unknown registration kind.');
      platform = node.kind === 'incus' ? 'linux' : 'windows';
      engine = platform === 'windows' && (node.workloads as {engine?: unknown} | undefined)?.engine === true;
      if (platform === 'linux') {
        if (typeof node.host_id !== 'string' || !/^[a-zA-Z0-9_-]{1,63}$/.test(node.host_id)) throw new HttpError(502, 'Invalid server registration.');
        expectedHost = node.host_id;
      }
    }
    installCommand(`${'0'.repeat(32)}.${'0'.repeat(64)}`, undefined, platform, engine);
    const supervisor = await forward('/__enginebox/status', 'GET', config().apiToken);
    if (supervisor.node_protocol !== 1) throw new HttpError(503, 'The controller supervisor needs the node protocol update before registration.');
  }
  if (request.method === 'POST' && !enrollment && !mine(segments[0])) throw new HttpError(404, 'Registration unavailable.');
  if (suffix.endsWith('/engine-owner')) {
    // A personal engine machine runs only its owner's sessions: the claiming admin
    // becomes the owner. The member comes from the session, never the request.
    if (!who.user_id) throw new HttpError(403, 'Sign in with your own account to claim an engine machine.');
    if (!body || typeof body !== 'object' || Array.isArray(body) || Object.keys(body).length) throw new HttpError(400, 'Claiming accepts an empty object.');
    forwardedBody = { member: who.user_id };
  }
  const result = await forward(path, request.method, config().apiToken, forwardedBody);
  if (suffix === 'enrollments' && who.org_id !== DEFAULT_ORG && typeof result.id === 'string') {
    // A machine enrolled from an org belongs to it.
    await forward(`/v1/tenancy/hosts/${result.id}/org`, 'POST', config().apiToken, { org_id: who.org_id });
    forgetTenancy();
  }
  if (enrollment) {
    if ((platform === 'linux' && (result.kind !== 'incus' || result.host_id !== expectedHost)) || (platform === 'windows' && result.kind !== undefined && result.kind !== 'workspace')) throw new HttpError(502, 'The controller returned a different server registration.');
    const command = installCommand(String(result.code), Number(result.expires), platform, engine);
    if (typeof result.id !== 'string' || !String(result.code).startsWith(`${result.id}.`)) throw new HttpError(502, 'Invalid registration identity.');
    return json({ id: result.id, expires: result.expires, code: result.code, command, platform, shell: platform === 'linux' ? 'Bash' : 'PowerShell', host_id: result.host_id ?? null });
  }
  if (request.method === 'GET' && Array.isArray(result.nodes)) return json({ ...result, nodes: result.nodes.filter((n: { id?: unknown }) => mine(n.id)) });
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

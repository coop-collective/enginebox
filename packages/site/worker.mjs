// Keep the public website and private controller on the same origin. The
// service binding selects the existing dashboard edge; clients cannot pick an
// upstream host, and the edge retains its authentication and VPC restrictions.

// Repository backend (docs/REPOSITORIES.md): the controller's signed requests
// to /_artifacts/<op> are forwarded over RPC to the Artifacts gateway Worker,
// which verifies them. This Worker holds no secret and changes nothing.
const ARTIFACTS_OPERATION = /^\/_artifacts\/(repos\.(?:create|get|list)|tokens\.(?:create|revoke))$/;
const ARTIFACTS_MAX_BODY = 16384;
const artifactsReply = (status, body) => new Response(JSON.stringify(body), {
  status,
  headers: { 'content-type': 'application/json', 'cache-control': 'no-store', 'x-content-type-options': 'nosniff' },
});
async function artifacts(request, url, env) {
  if (request.method !== 'POST') return artifactsReply(405, { error: 'method_not_allowed' });
  const match = ARTIFACTS_OPERATION.exec(url.pathname);
  if (!match || url.search) return artifactsReply(404, { error: 'not_found' });
  if (request.headers.has('origin')) return artifactsReply(403, { error: 'forbidden' });
  if (Number(request.headers.get('content-length') ?? 0) > ARTIFACTS_MAX_BODY) return artifactsReply(413, { error: 'too_large' });
  const bytes = await request.arrayBuffer();
  if (bytes.byteLength > ARTIFACTS_MAX_BODY) return artifactsReply(413, { error: 'too_large' });
  const header = (name) => request.headers.get(name) ?? '';
  let result;
  try {
    result = await env.ARTIFACTS.invoke({
      method: 'POST',
      path: `/${match[1]}`,
      timestamp: header('x-enginebox-timestamp'),
      nonce: header('x-enginebox-nonce'),
      signature: header('x-enginebox-signature'),
      body: new TextDecoder().decode(bytes),
    });
  } catch {
    console.error(JSON.stringify({ event: 'artifacts_gateway_unavailable' }));
    return artifactsReply(503, { error: 'unavailable' });
  }
  const status = Number.isInteger(result?.status) && result.status >= 200 && result.status <= 599 ? result.status : 502;
  return artifactsReply(status, result?.body ?? { error: 'upstream' });
}

// Email sign-in (docs/ORGS.md): the dashboard's signed send requests to
// /_email/send are forwarded over RPC to the email gateway Worker, which
// verifies them and sends with its Email Sending binding. This Worker holds no
// secret. Without the EMAIL_GATEWAY binding the route answers 503.
const EMAIL_MAX_BODY = 4096;
async function email(request, url, env) {
  if (request.method !== 'POST') return artifactsReply(405, { error: 'method_not_allowed' });
  if (url.pathname !== '/_email/send' || url.search) return artifactsReply(404, { error: 'not_found' });
  if (request.headers.has('origin')) return artifactsReply(403, { error: 'forbidden' });
  if (Number(request.headers.get('content-length') ?? 0) > EMAIL_MAX_BODY) return artifactsReply(413, { error: 'too_large' });
  const bytes = await request.arrayBuffer();
  if (bytes.byteLength > EMAIL_MAX_BODY) return artifactsReply(413, { error: 'too_large' });
  if (!env.EMAIL_GATEWAY) return artifactsReply(503, { error: 'unavailable' });
  const header = (name) => request.headers.get(name) ?? '';
  let result;
  try {
    result = await env.EMAIL_GATEWAY.send({
      timestamp: header('x-enginebox-timestamp'),
      nonce: header('x-enginebox-nonce'),
      signature: header('x-enginebox-signature'),
      body: new TextDecoder().decode(bytes),
    });
  } catch {
    console.error(JSON.stringify({ event: 'email_gateway_unavailable' }));
    return artifactsReply(503, { error: 'unavailable' });
  }
  const status = Number.isInteger(result?.status) && result.status >= 200 && result.status <= 599 ? result.status : 502;
  return artifactsReply(status, result?.body ?? { error: 'upstream' });
}

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (url.pathname === '/_artifacts' || url.pathname.startsWith('/_artifacts/')) return artifacts(request, url, env);
    if (url.pathname === '/_email' || url.pathname.startsWith('/_email/')) return email(request, url, env);
    if (url.pathname === '/dashboard') {
      return new Response(null, {
        status: 307,
        headers: { location: '/dashboard/' + url.search, 'cache-control': 'no-store' },
      });
    }
    const dashboard = url.pathname === '/dashboard/';
    // Connected apps (docs/OAUTH.md): OAuth discovery, the authorization server and the
    // token-scoped agent-run routes are the dashboard's too.
    const controller = dashboard || url.pathname.startsWith('/api/') ||
      url.pathname.startsWith('/_next/') || url.pathname === '/healthz' ||
      url.pathname.startsWith('/.well-known/oauth-') || url.pathname.startsWith('/oauth/') ||
      url.pathname.startsWith('/v1/');
    if (!controller) return env.ASSETS.fetch(request);

    // The native dashboard is a single page at /. Preserve the origin, query,
    // method, credentials and streaming body when mounting it at /dashboard/.
    if (dashboard) url.pathname = '/';
    try {
      return await env.DASHBOARD.fetch(new Request(url, request), { redirect: 'manual' });
    } catch {
      console.error(JSON.stringify({ event: 'dashboard_service_unavailable' }));
      return new Response('The dashboard is temporarily unavailable. Please try again.', {
        status: 503,
        headers: { 'cache-control': 'no-store', 'retry-after': '5' },
      });
    }
  },
};

import { bodyJson, HttpError, json, requireOrigin, requireRole, requireSession } from './server';

export async function environmentProxy(request: Request, path: string) {
  // Reading needs membership; publishing a route changes shared infrastructure (admin).
  if (request.method === 'GET') requireSession(request);
  else requireRole(request, 'admin');
  if (request.method !== 'GET') requireOrigin(request);
  if (!((request.method === 'GET' && path === 'v1/environments') || (request.method === 'POST' && path === 'v1/environments/routes')))
    throw new HttpError(404, 'Unknown environment operation.');
  const base = process.env.ENGINEBOX_ENVIRONMENTS_URL;
  const token = process.env.ENGINEBOX_ENVIRONMENTS_TOKEN;
  if (!base || !token || token.length < 32) throw new HttpError(503, 'Environment services are not configured on this dashboard.');
  const url = new URL(base);
  if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password || url.search || url.hash || url.pathname !== '/' || (url.protocol === 'http:' && !['127.0.0.1', '[::1]'].includes(url.hostname)))
    throw new HttpError(503, 'Invalid environment service configuration.');
  const body = request.method === 'POST' ? JSON.stringify(await bodyJson(request)) : undefined;
  let response: Response;
  try {
    response = await fetch(new URL(path, url), { method: request.method, headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' }, body, redirect: 'error', cache: 'no-store', signal: AbortSignal.timeout(10000) });
  } catch { throw new HttpError(502, 'Environment service is unavailable.'); }
  if ([401, 403].includes(response.status)) throw new HttpError(502, 'Environment service credentials were rejected.');
  return json(await response.json(), response.status);
}

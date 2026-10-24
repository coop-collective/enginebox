import { env } from 'cloudflare:workers';
import { version } from '../../release';
export async function GET() {
  let identity = await env.STATE.get('identity');
  if (!identity) { identity = crypto.randomUUID(); await env.STATE.put('identity', identity); }
  return Response.json({ version, identity, count: Number(await env.STATE.get('count') || 0) }, { headers: { 'cache-control': 'no-store' } });
}
export async function POST(request) {
  if (request.headers.has('origin') && new URL(request.headers.get('origin')).host !== new URL(request.url).host) return new Response('Forbidden', { status: 403 });
  const count = Number(await env.STATE.get('count') || 0) + 1;
  await env.STATE.put('count', String(count));
  return GET();
}

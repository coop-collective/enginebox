// Keep the public website and private controller on the same origin. The
// service binding selects the existing dashboard edge; clients cannot pick an
// upstream host, and the edge retains its authentication and VPC restrictions.
export default {
  async fetch(request, env) {
    const url = new URL(request.url);
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

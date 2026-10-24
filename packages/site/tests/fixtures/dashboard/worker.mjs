// Local-only stand-in for the native dashboard; never deployed. Browser tests
// exercise the production site's real service binding and static asset routing.
export default {
  fetch(request) {
    const url = new URL(request.url);
    if (url.pathname === '/') {
      return new Response('<!doctype html><h1>Dashboard fixture</h1><script src="/_next/static/fixture.js"></script>', {
        headers: { 'content-type': 'text/html' },
      });
    }
    if (url.pathname === '/_next/static/fixture.js') {
      return new Response('fetch("/api/session").then(r => r.json()).then(s => document.body.dataset.session = s.authenticated ? "yes" : "no")', {
        headers: { 'content-type': 'text/javascript' },
      });
    }
    if (url.pathname === '/api/session') return Response.json({ authenticated: false });
    if (url.pathname.startsWith('/api/node/')) return Response.json({ error: 'unauthorized' }, { status: 401 });
    return new Response('Not found', { status: 404 });
  },
};

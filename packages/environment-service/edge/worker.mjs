// The binding is a single provisioned VPC Service: the private HTTP gateway.
// Neither a URL parameter nor a request header can select another server port.
export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    const hosts = env.PUBLIC_HOSTS.split(',').map(host => host.trim());
    if (!hosts.includes(url.hostname)) return new Response('Unknown environment hostname', { status: 404 });
    if (['CONNECT', 'TRACE'].includes(request.method)) return new Response('Method not allowed', { status: 405 });
    if (url.protocol !== 'https:') { url.protocol = 'https:'; return Response.redirect(url.toString(), 307); }
    const headers = new Headers(request.headers);
    headers.delete('host');
    for (const name of [...headers.keys()]) if (name === 'forwarded' || name.startsWith('x-forwarded-')) headers.delete(name);
    headers.set('x-forwarded-proto', 'https');
    headers.set('x-forwarded-host', url.hostname);
    // VPC's fixed service selects the target; URL hostname becomes HTTP Host.
    url.protocol = 'http:';
    const upstream = new Request(url, request); upstream.headers.delete('host');
    for (const name of [...upstream.headers.keys()]) upstream.headers.delete(name);
    for (const [name, value] of headers) upstream.headers.set(name, value);
    try {
      const response = await env.GATEWAY.fetch(upstream, { redirect: 'manual' });
      if (response.status === 101) return response;
      const output = new Response(response.body, response);
      output.headers.set('cache-control', 'no-store');
      output.headers.set('x-content-type-options', 'nosniff');
      output.headers.set('x-robots-tag', 'noindex, nofollow');
      output.headers.set('strict-transport-security', 'max-age=86400');
      return output;
    } catch {
      console.error(JSON.stringify({ event: 'environment_gateway_unavailable', hostname: url.hostname }));
      return new Response('Environment gateway unavailable', { status: 503, headers: { 'retry-after': '5', 'cache-control': 'no-store' } });
    }
  },
};

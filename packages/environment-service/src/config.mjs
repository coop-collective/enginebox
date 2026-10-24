import { isAbsolute } from 'node:path';

export function check(value, code) { if (!value) throw new Error(code); }
export const id = value => typeof value === 'string' && /^[a-z][a-z0-9-]{0,39}$/.test(value);
export const port = value => Number.isInteger(value) && value >= 1024 && value <= 65535;
export const hostname = value => typeof value === 'string' && value.length <= 253 && value.split('.').length >= 2 && value.split('.').every(x => /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/.test(x));
export function fields(value, allowed) {
  check(value && typeof value === 'object' && !Array.isArray(value) && Object.keys(value).every(k => allowed.includes(k)), 'unknown-field');
}
export function validateConfig(config) {
  fields(config, ['schema', 'stateDir', 'listenPort', 'tokenFile', 'docker', 'baseDomains', 'httpEntrypoint', 'httpTls', 'basicAuthUsers', 'transportEntrypoints', 'environments', 'pollSeconds']);
  check(config.schema === 1 && isAbsolute(config.stateDir) && isAbsolute(config.tokenFile) && port(config.listenPort), 'invalid-config');
  check(Array.isArray(config.baseDomains) && config.baseDomains.length > 0 && config.baseDomains.every(hostname), 'base-domains-required');
  check(id(config.httpEntrypoint), 'http-entrypoint-required');
  check(typeof config.httpTls === 'boolean', 'explicit-http-tls-required');
  check(Array.isArray(config.basicAuthUsers) && config.basicAuthUsers.every(v => typeof v === 'string' && /^[^:\s]+:\$(2[aby]|apr1|5|6)\$/.test(v)), 'invalid-basic-auth-users');
  check(Array.isArray(config.transportEntrypoints) && config.transportEntrypoints.length <= 128, 'invalid-transport-entrypoints');
  const endpoints = new Set([config.httpEntrypoint]);
  const sockets = new Set();
  for (const ep of config.transportEntrypoints) {
    fields(ep, ['name', 'protocol', 'port', 'public']);
    check(id(ep.name) && ['tcp', 'udp'].includes(ep.protocol) && port(ep.port) && typeof ep.public === 'boolean', 'invalid-transport-entrypoint');
    check(!endpoints.has(ep.name) && !sockets.has(`${ep.protocol}:${ep.port}`), 'duplicate-entrypoint');
    endpoints.add(ep.name); sockets.add(`${ep.protocol}:${ep.port}`);
  }
  check(Number.isInteger(config.pollSeconds) && config.pollSeconds >= 2 && config.pollSeconds <= 300, 'invalid-poll-interval');
  check(Array.isArray(config.environments) && config.environments.length <= 64, 'invalid-environments');
  const names = new Set(); const bindings = new Set();
  for (const env of config.environments) {
    fields(env, ['id', 'source', 'image', 'command', 'prepare', 'ports', 'health', 'cpu', 'memoryMiB', 'timeoutSeconds']);
    check(id(env.id) && !names.has(env.id), 'duplicate-or-invalid-environment'); names.add(env.id);
    if (env.source.kind === 'git') {
      fields(env.source, ['kind', 'url', 'branch']);
      check(typeof env.source.url === 'string' && (isAbsolute(env.source.url) || /^https:\/\/[a-z0-9.-]+\/[A-Za-z0-9_./-]+$/.test(env.source.url)), 'invalid-source');
      check(typeof env.source.branch === 'string' && /^[A-Za-z0-9][A-Za-z0-9_./-]{0,127}$/.test(env.source.branch) && !env.source.branch.includes('..'), 'invalid-source-branch');
    } else if (env.source.kind === 'layerfs') {
      fields(env.source, ['kind', 'project', 'workspace', 'command']);
      check(id(env.source.project) && (env.source.workspace === undefined || id(env.source.workspace)), 'invalid-layerfs-project');
      check(Array.isArray(env.source.command) && env.source.command.length > 0 && env.source.command.length <= 64 && isAbsolute(env.source.command[0]) && env.source.command.every(v => typeof v === 'string' && v.length > 0 && v.length <= 4096 && !v.includes('\0')), 'invalid-layerfs-export-command');
    } else {
      fields(env.source, ['kind', 'controller', 'tokenFile', 'project', 'exportsDir']);
      check(env.source.kind === 'accepted' && id(env.source.project) && isAbsolute(env.source.exportsDir) && isAbsolute(env.source.tokenFile), 'invalid-accepted-source');
      const url = new URL(env.source.controller);
      check(['http:', 'https:'].includes(url.protocol) && !url.username && !url.password && !url.search && !url.hash && url.pathname === '/' && (url.protocol === 'https:' || ['127.0.0.1', '[::1]'].includes(url.hostname)), 'invalid-source-controller');
    }
    check(typeof env.image === 'string' && /^[A-Za-z0-9._:/-]+@sha256:[a-f0-9]{64}$/.test(env.image), 'pinned-image-required');
    for (const argv of [env.command, env.prepare ?? []]) check(Array.isArray(argv) && argv.length <= 64 && argv.every(v => typeof v === 'string' && v.length > 0 && v.length <= 8192 && !v.includes('\0')), 'invalid-command');
    check(env.command.length > 0 && Number.isFinite(env.cpu) && env.cpu >= 0.1 && env.cpu <= 8 && Number.isInteger(env.memoryMiB) && env.memoryMiB >= 64 && env.memoryMiB <= 16384, 'invalid-resource-limits');
    check(Number.isInteger(env.timeoutSeconds) && env.timeoutSeconds >= 5 && env.timeoutSeconds <= 1800, 'invalid-timeout');
    check(Array.isArray(env.ports) && env.ports.length > 0 && env.ports.length <= 16, 'invalid-ports');
    const portNames = new Set();
    for (const p of env.ports) {
      fields(p, ['name', 'protocol', 'container', 'host']);
      check(id(p.name) && !portNames.has(p.name) && ['http', 'tcp', 'udp', 'tls'].includes(p.protocol) && port(p.container) && port(p.host), 'invalid-service-port'); portNames.add(p.name);
      const binding = `${p.protocol === 'udp' ? 'udp' : 'tcp'}:${p.host}`;
      check(!bindings.has(binding) && !sockets.has(binding) && p.host !== config.listenPort, 'duplicate-port-binding'); bindings.add(binding);
    }
    fields(env.health, ['port', 'path']);
    check(portNames.has(env.health.port) && env.ports.find(p => p.name === env.health.port).protocol === 'http' && /^\/[A-Za-z0-9_/?=&.-]*$/.test(env.health.path), 'http-readiness-required');
  }
  return config;
}

export function validateRoutes(routes, config) {
  check(Array.isArray(routes) && routes.length <= 256, 'invalid-routes');
  const names = new Set(); const claims = new Set();
  for (const route of routes) {
    fields(route, ['id', 'environment', 'service', 'hostname', 'entrypoint', 'access']);
    check(id(route.id) && !names.has(route.id), 'duplicate-or-invalid-route'); names.add(route.id);
    const env = config.environments.find(v => v.id === route.environment);
    const service = env?.ports.find(v => v.name === route.service);
    check(service, 'unknown-service');
    check(['public', 'authenticated', 'private'].includes(route.access), 'explicit-access-required');
    if (['http', 'tls'].includes(service.protocol)) {
      check(hostname(route.hostname) && config.baseDomains.some(base => route.hostname.endsWith(`.${base}`)), 'hostname-outside-approved-domains');
    } else check(route.hostname === undefined, 'raw-transport-has-no-hostname');
    let claim;
    if (service.protocol === 'http') {
      check(route.entrypoint === undefined && ['public', 'authenticated'].includes(route.access), 'invalid-http-route');
      check(route.access !== 'authenticated' || config.basicAuthUsers.length > 0, 'gateway-auth-not-configured');
      claim = `http:${route.hostname}`;
    } else {
      const ep = config.transportEntrypoints.find(v => v.name === route.entrypoint);
      check(ep && ep.protocol === (service.protocol === 'udp' ? 'udp' : 'tcp'), 'unprovisioned-transport-entrypoint');
      check(route.access === (ep.public ? 'public' : 'private'), 'transport-exposure-mismatch');
      // One owner per transport listener avoids catch-all/SNI precedence mistakes.
      claim = `transport:${ep.name}`;
    }
    check(!claims.has(claim), 'route-conflict'); claims.add(claim);
  }
  return routes;
}

export function gatewayConfig(routes, config, healthy) {
  const result = { http: { routers: {}, services: {}, middlewares: {} }, tcp: { routers: {}, services: {} }, udp: { routers: {}, services: {} } };
  if (config.basicAuthUsers.length) result.http.middlewares.operator = { basicAuth: { users: config.basicAuthUsers, removeHeader: true } };
  for (const route of validateRoutes(routes, config)) {
    if (!healthy.has(route.environment)) continue;
    const service = config.environments.find(e => e.id === route.environment).ports.find(p => p.name === route.service);
    const protocol = service.protocol === 'tls' ? 'tcp' : service.protocol;
    const section = result[protocol];
    if (protocol === 'http') {
      section.routers[route.id] = { rule: `Host(\`${route.hostname}\`)`, entryPoints: [config.httpEntrypoint], service: route.id, ...(config.httpTls ? { tls: {} } : {}), ...(route.access === 'authenticated' ? { middlewares: ['operator'] } : {}) };
      section.services[route.id] = { loadBalancer: { servers: [{ url: `http://127.0.0.1:${service.host}` }], passHostHeader: true, healthCheck: { path: config.environments.find(e => e.id === route.environment).health.path, interval: '5s', timeout: '2s' } } };
      // Readiness can live on another HTTP port; do not probe the wrong service.
      if (service.name !== config.environments.find(e => e.id === route.environment).health.port) delete section.services[route.id].loadBalancer.healthCheck;
    } else {
      section.routers[route.id] = { entryPoints: [route.entrypoint], service: route.id, ...(protocol === 'tcp' ? { rule: service.protocol === 'tls' ? `HostSNI(\`${route.hostname}\`)` : 'HostSNI(`*`)' } : {}), ...(service.protocol === 'tls' ? { tls: { passthrough: true } } : {}) };
      section.services[route.id] = { loadBalancer: { servers: [{ address: `127.0.0.1:${service.host}` }] } };
    }
  }
  return result;
}

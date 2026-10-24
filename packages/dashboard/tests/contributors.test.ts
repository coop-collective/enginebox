import assert from 'node:assert/strict';
import { test, beforeEach } from 'node:test';
import { contributors, installCommand, nodeProxy } from '../lib/contributors';
import { sessionCookie, HttpError } from '../lib/server';
const origin='https://enginebox.example';
const owner='test-owner-credential-32-characters';
const code=`${'a'.repeat(32)}.${'b'.repeat(64)}`;
const budget={cpu:2,memory_gib:4,disk_gib:20};
beforeEach(()=>{
  process.env.ENGINEBOX_DASHBOARD_ORIGIN=origin;
  process.env.ENGINEBOX_DASHBOARD_PASSWORD='test-dashboard-password';
  process.env.ENGINEBOX_SESSION_SECRET='test-session-secret-32-characters';
  process.env.ENGINEBOX_API_TOKEN=owner;
  process.env.ENGINEBOX_API_URL='http://127.0.0.1:8765';
  process.env.ENGINEBOX_NODE_INSTALLER_URL='https://github.com/coop-collective/enginebox/releases/download/test-1/install.ps1';
  process.env.ENGINEBOX_NODE_INSTALLER_SHA256='f'.repeat(64);
  delete process.env.ENGINEBOX_NODE_PUBLIC_ORIGIN;
});
function ownerRequest(body:unknown, auth=true) {return new Request(`${origin}/api/contributors/enrollments`,{method:'POST',headers:{origin,'Content-Type':'application/json',...(auth?{cookie:sessionCookie().split(';')[0]}:{})},body:JSON.stringify(body)});}
test('one-copy installer pins release, checksum, host without imposing local resources without owner credentials',async t=>{
  t.mock.method(globalThis,'fetch',async (_url:string,options:RequestInit)=>{
    if (_url.endsWith('/__enginebox/status')) return Response.json({node_protocol:1});
    assert.equal((options.headers as Record<string,string>).Authorization,`Bearer ${owner}`);
    return Response.json({id:'a'.repeat(32),code,expires:Math.floor(Date.now()/1000)+600});
  });
  const response=await contributors(ownerRequest({budget}),['enrollments']);
  assert.equal(response.headers.get('cache-control'),'no-store');
  const value=await response.json();assert.doesNotMatch(value.command,/-Cpu|-MemoryGiB|-DiskGiB/);assert.ok(value.command.includes(code));assert.ok(!value.command.includes(owner));assert.ok(value.command.includes('Get-FileHash'));
  assert.equal(value.code,code);
  assert.match(value.command,/-EnrollmentExpires \d+/);
});
test('node proxy forwards only caller token and rejects browsers and administrative paths',async t=>{
  const fetch=t.mock.method(globalThis,'fetch',async (url:string,options:RequestInit)=>{
    assert.equal(url,'http://127.0.0.1:8765/v1/node/heartbeat');
    assert.equal((options.headers as Record<string,string>).Authorization,`Bearer ${code}`);assert.equal(options.redirect,'error');
    return Response.json({ok:true});
  });
  const req=()=>new Request(`${origin}/api/node/heartbeat`,{method:'POST',headers:{Authorization:`Bearer ${code}`,'Content-Type':'application/json'},body:'{}'});
  await nodeProxy(req(),'heartbeat');
  const browser=req();browser.headers.set('origin',origin);await assert.rejects(nodeProxy(browser,'heartbeat'),(e:unknown)=>e instanceof HttpError&&e.status===403);
  await assert.rejects(nodeProxy(req(),'../contributors'),(e:unknown)=>e instanceof HttpError&&e.status===404);
  const admin=req();admin.headers.set('Authorization',`Bearer ${owner}`);await assert.rejects(nodeProxy(admin,'heartbeat'));
  assert.equal(fetch.mock.callCount(),1);
});
test('invitation issuance requires owner session, same origin, and a configured immutable installer',async t=>{
  const fetch=t.mock.method(globalThis,'fetch',()=>{throw new Error('must not fetch');});
  await assert.rejects(contributors(ownerRequest({budget},false),['enrollments']));
  const cross=ownerRequest({budget});cross.headers.set('origin','https://attacker.example');await assert.rejects(contributors(cross,['enrollments']));
  delete process.env.ENGINEBOX_NODE_INSTALLER_SHA256;await assert.rejects(contributors(ownerRequest({budget}),['enrollments']));assert.equal(fetch.mock.callCount(),0);
});
test('command rejects shell injection and credential-bearing controller origins',()=>{
  assert.throws(()=>installCommand("';Invoke-Expression bad"));
  process.env.ENGINEBOX_NODE_PUBLIC_ORIGIN='https://user:password@enginebox.example';assert.throws(()=>installCommand(code));
});
test('renewal requires the owner and origin, keeps the identity, and returns a pinned recovery command',async t=>{
  const id='a'.repeat(32), path=[id,'renew-enrollment'];
  const calls:string[]=[];
  t.mock.method(globalThis,'fetch',async (url:string,options:RequestInit)=>{
    calls.push(url);
    assert.equal((options.headers as Record<string,string>).Authorization,`Bearer ${owner}`);
    if (url.endsWith('/__enginebox/status')) return Response.json({node_protocol:1});
    assert.equal(url,`http://127.0.0.1:8765/v1/contributors/${id}/renew-enrollment`);
    assert.equal(options.body,'{}');
    return Response.json({id,code,expires:Math.floor(Date.now()/1000)+3600});
  });
  await assert.rejects(contributors(ownerRequest({},false),path));
  const cross=ownerRequest({});cross.headers.set('origin','https://attacker.example');
  await assert.rejects(contributors(cross,path));
  const hash=process.env.ENGINEBOX_NODE_INSTALLER_SHA256;
  delete process.env.ENGINEBOX_NODE_INSTALLER_SHA256;
  await assert.rejects(contributors(ownerRequest({}),path));
  assert.equal(calls.length,0);
  process.env.ENGINEBOX_NODE_INSTALLER_SHA256=hash;
  const response=await contributors(ownerRequest({}),path), value=await response.json();
  assert.equal(response.headers.get('cache-control'),'no-store');
  assert.equal(value.id,id);assert.equal(value.code,code);
  assert.ok(value.command.includes(code));assert.match(value.command,/Get-FileHash/);
  assert.ok(!value.command.includes(owner));assert.equal(calls.length,2);
});
test('an older supervisor cannot issue an unusable worker invitation',async t=>{
  const fetch=t.mock.method(globalThis,'fetch',async (url:string)=>{assert.ok(url.endsWith('/__enginebox/status'));return Response.json({current:'old'});});
  await assert.rejects(contributors(ownerRequest({budget}),['enrollments']),(e:unknown)=>e instanceof HttpError&&e.status===503);
  assert.equal(fetch.mock.callCount(),1);
});

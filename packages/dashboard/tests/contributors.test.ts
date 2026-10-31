import assert from 'node:assert/strict';
import { test, beforeEach } from 'node:test';
import { createHash } from 'node:crypto';
import { mkdtemp, mkdir, writeFile, readFile, rm, access } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { spawnSync } from 'node:child_process';
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
  process.env.ENGINEBOX_LINUX_NODE_INSTALLER_URL='https://github.com/coop-collective/enginebox/releases/download/linux-node-preview-test/install-linux-node.sh';
  process.env.ENGINEBOX_LINUX_NODE_INSTALLER_SHA256='d'.repeat(64);
  process.env.ENGINEBOX_ENGINE_NODE_INSTALLER_URL='https://github.com/coop-collective/enginebox/releases/download/native-node-test/install.ps1';
  process.env.ENGINEBOX_ENGINE_NODE_INSTALLER_SHA256='e'.repeat(64);
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
    if (url.endsWith('/v1/contributors')) return Response.json({nodes:[{id,kind:'workspace',enrolled:false,revoked:false}]});
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
  assert.equal(calls.length,1); // Read the durable registration kind, but do not rotate it.
  process.env.ENGINEBOX_NODE_INSTALLER_SHA256=hash;
  const response=await contributors(ownerRequest({}),path), value=await response.json();
  assert.equal(response.headers.get('cache-control'),'no-store');
  assert.equal(value.id,id);assert.equal(value.code,code);
  assert.ok(value.command.includes(code));assert.match(value.command,/Get-FileHash/);
  assert.ok(!value.command.includes(owner));assert.equal(calls.length,4);
});
test('an older supervisor cannot issue an unusable worker invitation',async t=>{
  const fetch=t.mock.method(globalThis,'fetch',async (url:string)=>{assert.ok(url.endsWith('/__enginebox/status'));return Response.json({current:'old'});});
  await assert.rejects(contributors(ownerRequest({budget}),['enrollments']),(e:unknown)=>e instanceof HttpError&&e.status===503);
  assert.equal(fetch.mock.callCount(),1);
});

test('Linux issuance uses the host-bound route and a pinned Bash command without Windows settings',async t=>{
  delete process.env.ENGINEBOX_NODE_INSTALLER_SHA256;
  const calls:string[]=[];
  t.mock.method(globalThis,'fetch',async (url:string,options:RequestInit)=>{
    calls.push(url);
    assert.equal((options.headers as Record<string,string>).Authorization,`Bearer ${owner}`);
    if(url.endsWith('/__enginebox/status')) return Response.json({node_protocol:1});
    assert.equal(url,'http://127.0.0.1:8765/v1/hosts/metal/enrollment');
    assert.equal(options.body,'{}');
    return Response.json({id:'a'.repeat(32),code,expires:Math.floor(Date.now()/1000)+600,kind:'incus',host_id:'metal'});
  });
  const response=await contributors(ownerRequest({platform:'linux',host_id:'metal'}),['enrollments']);
  const value=await response.json();
  assert.equal(value.platform,'linux');assert.equal(value.shell,'Bash');assert.equal(value.host_id,'metal');
  assert.match(value.command,/install-linux-node\.sh/);assert.match(value.command,/sha256sum --check --status/);
  assert.match(value.command,/sudo bash/);assert.match(value.command,/--enrollment-code/);
  assert.doesNotMatch(value.command,/powershell|Cpu|MemoryGiB/);assert.ok(!value.command.includes(owner));
  assert.equal(calls.length,2);
});
test('Linux pin and host validation precede issuance; a wrong host cannot be smuggled into the route',async t=>{
  const fetch=t.mock.method(globalThis,'fetch',()=>{throw new Error('must not mutate');});
  for(const body of [{platform:'linux',host_id:'../metal'},{platform:'linux',host_id:'metal',budget},{platform:'macos'},null,[]]) {
    await assert.rejects(contributors(ownerRequest(body),['enrollments']));
  }
  delete process.env.ENGINEBOX_LINUX_NODE_INSTALLER_SHA256;
  await assert.rejects(contributors(ownerRequest({platform:'linux',host_id:'metal'}),['enrollments']));
  assert.equal(fetch.mock.callCount(),0);
});
test('renewal reads the stored Linux kind and refuses mutation until that installer pin is valid',async t=>{
  const id='a'.repeat(32),calls:{url:string;method:string}[]=[];
  t.mock.method(globalThis,'fetch',async(url:string,options:RequestInit)=>{
    calls.push({url,method:String(options.method)});
    if(url.endsWith('/v1/contributors')) return Response.json({nodes:[{id,kind:'incus',host_id:'metal',enrolled:false,revoked:false}]});
    if(url.endsWith('/__enginebox/status')) return Response.json({node_protocol:1});
    assert.equal(url,`http://127.0.0.1:8765/v1/contributors/${id}/renew-enrollment`);
    return Response.json({id,code,expires:Math.floor(Date.now()/1000)+600,kind:'incus',host_id:'metal'});
  });
  const hash=process.env.ENGINEBOX_LINUX_NODE_INSTALLER_SHA256;
  delete process.env.ENGINEBOX_LINUX_NODE_INSTALLER_SHA256;
  await assert.rejects(contributors(ownerRequest({}),[id,'renew-enrollment']));
  assert.deepEqual(calls.map(c=>c.method),['GET']);
  process.env.ENGINEBOX_LINUX_NODE_INSTALLER_SHA256=hash;
  delete process.env.ENGINEBOX_NODE_INSTALLER_SHA256;
  const value=await(await contributors(ownerRequest({}),[id,'renew-enrollment'])).json();
  assert.equal(value.shell,'Bash');assert.equal(value.id,id);assert.match(value.command,/install-linux-node\.sh/);
  assert.deepEqual(calls.map(c=>c.method),['GET','GET','GET','POST']);
  await assert.rejects(contributors(ownerRequest({platform:'windows'}),[id,'renew-enrollment']));
  assert.equal(calls.length,4);
});

test('generated Bash command verifies downloaded bytes before execution and always removes its temporary file',{skip:process.platform==='win32'},async()=>{
  const root=await mkdtemp(join(tmpdir(),'enginebox enrollment '));
  const bin=join(root,'bin'),scratch=join(root,'scratch'),marker=join(root,'executed'),download=join(root,'download-path');
  const payload='#!/bin/bash\nprintf passed > "$ENGINEBOX_FIXTURE_MARKER"\n';
  const executable=async(name:string,source:string)=>writeFile(join(bin,name),`#!${process.execPath}\n${source}`,{mode:0o700});
  try {
    await mkdir(bin);await mkdir(scratch);
    await executable('curl',`const fs=require('node:fs');const args=process.argv.slice(2),path=args[args.indexOf('-o')+1];fs.writeFileSync(path,${JSON.stringify(payload)});fs.writeFileSync(process.env.ENGINEBOX_FIXTURE_DOWNLOAD,path);`);
    await executable('id',`process.stdout.write('0\\n');`);
    await executable('sha256sum',`const fs=require('node:fs'),crypto=require('node:crypto');const line=fs.readFileSync(0,'utf8');const match=/^([a-f0-9]{64})  (.+)\\n$/.exec(line);if(!match)process.exit(2);const hash=crypto.createHash('sha256').update(fs.readFileSync(match[2])).digest('hex');process.exit(hash===match[1]?0:1);`);
    const env={...process.env,PATH:`${bin}:${process.env.PATH}`,TMPDIR:scratch,ENGINEBOX_FIXTURE_MARKER:marker,ENGINEBOX_FIXTURE_DOWNLOAD:download};
    process.env.ENGINEBOX_LINUX_NODE_INSTALLER_SHA256=createHash('sha256').update(payload).digest('hex');
    let result=spawnSync('bash',['-c',installCommand(code,undefined,'linux')],{env,encoding:'utf8',timeout:10000});
    assert.equal(result.status,0,result.stderr);
    assert.equal(await readFile(marker,'utf8'),'passed');
    await assert.rejects(access(await readFile(download,'utf8')));
    await rm(marker);
    process.env.ENGINEBOX_LINUX_NODE_INSTALLER_SHA256='0'.repeat(64);
    result=spawnSync('bash',['-c',installCommand(code,undefined,'linux')],{env,encoding:'utf8',timeout:10000});
    assert.notEqual(result.status,0);
    await assert.rejects(access(marker));
    await assert.rejects(access(await readFile(download,'utf8')));
  } finally {await rm(root,{recursive:true,force:true});}
});

test('engine machines pin the native node bootstrap and run its engine workload',async t=>{
  let forwarded:unknown;
  t.mock.method(globalThis,'fetch',async (url:string,options:RequestInit)=>{
    if (url.endsWith('/__enginebox/status')) return Response.json({node_protocol:1});
    forwarded=JSON.parse(String(options.body));
    return Response.json({id:'a'.repeat(32),code,expires:Math.floor(Date.now()/1000)+600});
  });
  const response=await contributors(ownerRequest({platform:'windows',label:'RTX',workloads:{ci:false,agents:false,engine:true}}),['enrollments']);
  const value=await response.json();
  assert.deepEqual(forwarded,{label:'RTX',workloads:{ci:false,agents:false,engine:true}});
  assert.ok(value.command.includes('native-node-test/install.ps1'));assert.ok(value.command.includes('e'.repeat(64)));
  assert.match(value.command,/-Workload 'engine'/);assert.ok(!value.command.includes('test-1/install.ps1'));
});
test('engine machines take no CI or agent work and need their own installer pin',async t=>{
  const fetch=t.mock.method(globalThis,'fetch',async ()=>Response.json({node_protocol:1}));
  await assert.rejects(contributors(ownerRequest({platform:'windows',label:'RTX',workloads:{ci:true,agents:false,engine:true}}),['enrollments']),(e:unknown)=>e instanceof HttpError&&e.status===400);
  assert.equal(fetch.mock.callCount(),0);
  delete process.env.ENGINEBOX_ENGINE_NODE_INSTALLER_SHA256;
  await assert.rejects(contributors(ownerRequest({platform:'windows',label:'RTX',workloads:{ci:false,agents:false,engine:true}}),['enrollments']),(e:unknown)=>e instanceof HttpError&&e.status===503);
  assert.equal(fetch.mock.callCount(),0);
  const response=await contributors(ownerRequest({platform:'windows',label:'WSL',workloads:{ci:true,agents:true}}),['enrollments']).catch(e=>e);
  assert.ok(!(response instanceof HttpError&&response.status===503&&/engine/.test(response.message)));
});
test('renewing an engine registration keeps the engine installer',async t=>{
  const id='c'.repeat(32);
  t.mock.method(globalThis,'fetch',async (url:string)=>{
    if (url.endsWith('/__enginebox/status')) return Response.json({node_protocol:1});
    if (url.endsWith('/v1/contributors')) return Response.json({nodes:[{id,kind:'workspace',enrolled:false,revoked:false,workloads:{ci:false,agents:false,engine:true}}]});
    return Response.json({id,code:`${id}.${'b'.repeat(64)}`,expires:Math.floor(Date.now()/1000)+600});
  });
  const request=new Request(`${origin}/api/contributors/${id}/renew-enrollment`,{method:'POST',headers:{origin,'Content-Type':'application/json',cookie:sessionCookie().split(';')[0]},body:'{}'});
  const value=await (await contributors(request,[id,'renew-enrollment'])).json();
  assert.ok(value.command.includes('native-node-test/install.ps1'));assert.match(value.command,/-Workload 'engine'/);
});

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp,writeFile,rm,symlink,readFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { publishFile,publishBuild } from '../src/artifacts.ts';
import { mediaCommand } from '../src/media.ts';
import { Client } from '../src/client.ts';
import { canonical,sha256 } from '@enginebox/fleet-protocol';

test('artifact size and symlinked manifest fail before any upload',async()=>{
 const root=await mkdtemp(join(tmpdir(),'ebox-publish-'));let calls=0;
 class Controller extends Client {override async request(){calls++;return Response.json({});}}
 const client=new Controller('http://localhost','private-fixture');
 try{
  const file=join(root,'file');await writeFile(file,'data');
  await assert.rejects(()=>publishFile(client,'demo',file,{sha256:'a'.repeat(64),bytes:999}),/size differs/);assert.equal(calls,0);
  if(process.platform!=='win32'){
   const manifest=join(root,'manifest');await symlink(file,manifest);
   await assert.rejects(()=>publishBuild(client,'candidate',manifest,root),/regular file/);assert.equal(calls,0);
  }
 }finally{await rm(root,{recursive:true,force:true});}
});
test('every artifact metadata publication honors abort, including final manifest',async()=>{
 const root=await mkdtemp(join(tmpdir(),'ebox-publish-cancel-'));
 try{
  await writeFile(join(root,'index.html'),'play');
  const manifest={schema:1,kind:'engine-build',project:'demo',generation:'a'.repeat(64),sourceTree:'b'.repeat(64),recipe:'c'.repeat(64),toolchain:{engine:'godot',version:'test',target:'web',architecture:'wasm32',fingerprint:'d'.repeat(64)},entrypoint:'index.html',files:[{path:'index.html',sha256:await sha256('play'),bytes:4,executable:false}],totalBytes:4};
  const path=join(root,'manifest.json');await writeFile(path,canonical(manifest));
  for(const selected of ['/artifact-files/','/artifact-file-pages/','/artifacts']){
   const cancel=new AbortController();let observed=false;
   class Controller extends Client {override async request(url:string,init:RequestInit={}){
    if(init.method==='HEAD')return new Response(null,{status:404});
    if(init.method==='POST'&&url.includes(selected)){
     observed=true;assert.equal(init.signal,cancel.signal);
     const pending=new Promise<Response>((_resolve,reject)=>init.signal!.addEventListener('abort',()=>reject(new Error('cancelled metadata request')),{once:true}));
     cancel.abort();return pending;
    }
    return Response.json({});
   }}
   await assert.rejects(()=>publishBuild(new Controller('http://localhost','private-fixture'),'candidate',path,root,cancel.signal),/cancelled metadata/);assert(observed);
  }
 }finally{await rm(root,{recursive:true,force:true});}
});
test('media cancellation kills descendants even when the parent closes its pipes early',{skip:process.platform==='win32'},async()=>{
 const root=await mkdtemp(join(tmpdir(),'ebox-media-descendant-'));let pid:number|undefined;
 try{
  const cancel=new AbortController();
  const pending=mediaCommand('/bin/sh',['-c',"(trap '' TERM; exec sleep 30) >/dev/null 2>&1 & echo $! > child.pid; sleep 30"],root,cancel.signal,10000);
  // Register the expected rejection immediately, before issuing cancellation.
  const rejected=assert.rejects(pending,/cancelled/);
  for(let i=0;i<100;i++){try{pid=Number(await readFile(join(root,'child.pid'),'utf8'));break;}catch{await new Promise(resolve=>setTimeout(resolve,10));}}
  assert(pid);cancel.abort();await rejected;await new Promise(resolve=>setTimeout(resolve,100));
  const state=spawnSync('ps',['-p',String(pid),'-o','stat='],{encoding:'utf8'}).stdout.trim();
  assert(!state||state.startsWith('Z'),`Owned descendant survived cancellation (${state})`);
 }finally{if(pid)try{process.kill(pid,'SIGKILL');}catch{}await rm(root,{recursive:true,force:true});}
});

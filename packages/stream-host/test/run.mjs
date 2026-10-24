import {spawn} from 'node:child_process';
import {mkdtemp,writeFile,readFile,mkdir,rm,chmod} from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import assert from 'node:assert/strict';
import {randomUUID} from 'node:crypto';
const directory=path.dirname(fileURLToPath(import.meta.url));
const prefix=process.env.STREAM_TEST_PREFIX||`enginebox-stream-proof-${process.pid}-${Date.now().toString(36)}`;
assert(/^enginebox-stream-proof-[a-z0-9-]{1,48}$/.test(prefix),'invalid owned test prefix');
let interrupted=false;const activeCommands=new Set();
const interrupt=()=>{interrupted=true;for(const child of activeCommands)child.kill('SIGTERM');};process.once('SIGINT',interrupt);process.once('SIGTERM',interrupt);
const checkInterrupted=()=>{if(interrupted)throw Error('Owned stream qualification interrupted');};
const temporary=await mkdtemp(path.join(os.tmpdir(),prefix));const evidence=process.env.STREAM_EVIDENCE_DIR||path.join(temporary,'evidence');await mkdir(evidence,{recursive:true});
const names={host:prefix+'-host',driver:prefix+'-driver',relay:prefix+'-relay',volume:prefix+'-state',network:prefix+'-net'};
const dockerInput=(input,...args)=>new Promise((resolve,reject)=>{const p=spawn('docker',args,{stdio:['pipe','pipe','pipe']});activeCommands.add(p);let out='',err='';p.stdout.on('data',v=>out+=v);p.stderr.on('data',v=>err+=v);p.on('close',code=>{activeCommands.delete(p);code===0?resolve((args[0]==='logs'?out+err:out).trim()):reject(Error(`docker ${args[0]} failed: ${err||out}`));});p.stdin.on('error',()=>{});p.stdin.end(input);});
const docker=(...args)=>dockerInput(undefined,...args);
const hostImage=process.env.STREAM_HOST_TEST_IMAGE||'enginebox-stream-host:qualification';
const browserImage=process.env.STREAM_BROWSER_TEST_IMAGE||'enginebox-stream-browser:qualification';
const relayImage=process.env.STREAM_RELAY_TEST_IMAGE||'enginebox-stream-relay:qualification';
const connection=process.env.STREAM_CONTROLLER_CONFIG?JSON.parse(await readFile(process.env.STREAM_CONTROLLER_CONFIG,'utf8')):null;
const engine=process.env.STREAM_ENGINE_CONFIG?JSON.parse(await readFile(process.env.STREAM_ENGINE_CONFIG,'utf8')):{};
const scene={width:engine.width||640,height:engine.height||360,clickX:300,clickY:200,pixelX:300,pixelY:200,...(process.env.STREAM_SCENE_KIND==='godot'?{clickX:100,clickY:170,pixelX:300,pixelY:220}:{})};
const resources=process.env.STREAM_SCENE_KIND==='godot'?{cpus:4,memory:'4g',pids:256}:{cpus:2,memory:'1g',pids:128};
try{
 await docker('network','create',names.network);await docker('volume','create',names.volume);
 const config={controller:'http://driver:8080',project:connection?.project||'fixture',allowLocalController:true,width:640,height:360,maxPeers:2,command:{executable:'/usr/local/bin/enginebox-test-window',directory:'/app'},...engine};
 let invitation=`native-fixture.enroll.${'a'.repeat(64)}`;
 if(connection){connection.nodeId='stream-'+randomUUID();const response=await fetch(connection.controller+`/v1/projects/${connection.project}/machine-invitations`,{method:'POST',headers:{authorization:`Bearer ${connection.ownerToken}`,'content-type':'application/json'},body:JSON.stringify({id:connection.nodeId,label:'Disposable native stream proof',ownerAccountId:'fixture-account',allowControl:true})});assert(response.ok,`nativeinvitationHTTP${response.status}`);invitation=(await response.json()).token;await writeFile(path.join(temporary,'controller.json'),JSON.stringify(connection),{mode:0o600});}
 await writeFile(path.join(temporary,'config.json'),JSON.stringify(config));await chmod(path.join(temporary,'config.json'),0o644);
 await docker('run','-d','--name',names.relay,'--network',names.network,'--network-alias','relay','--read-only','--tmpfs','/tmp:rw,nosuid,nodev,size=8m','--cap-drop=ALL','--security-opt=no-new-privileges',relayImage);
 await docker('run','-d','--name',names.driver,'--network',names.network,'--network-alias','driver','--add-host','host.docker.internal:host-gateway','--user',`${process.getuid?.()??1000}:${process.getgid?.()??1000}`,'-e','HOME=/tmp','--shm-size=256m','--memory=2g','--cpus=2','--cap-drop=ALL','--security-opt=no-new-privileges','-e',`STREAM_TEST_SCENE=${JSON.stringify(scene)}`,'-e',`STREAM_SCENE_KIND=${process.env.STREAM_SCENE_KIND||'native-x11'}`,...(connection?['--mount',`type=bind,source=${path.join(temporary,'controller.json')},target=/controller.json,readonly`,'-e','STREAM_CONTROLLER_CONFIG=/controller.json']:[]),...(process.env.STREAM_GAMEJAM_PROOF_ROOT?['--mount',`type=bind,source=${process.env.STREAM_GAMEJAM_PROOF_ROOT},target=/gamejam,readonly`,'-e','STREAM_GAMEJAM_PROOF=1']:[]),'--mount',`type=bind,source=${directory},target=/test,readonly`,'--mount',`type=bind,source=${evidence},target=/evidence`,'--entrypoint','node',browserImage,'/test/driver.mjs');
 for(let attempt=0;attempt<60;attempt++){checkInterrupted();if((await docker('logs',names.driver)).includes('STREAM_TEST_SERVER_READY'))break;if(attempt===59)throw Error('fixture server failed to start');await new Promise(r=>setTimeout(r,250));}
 const hostArgs=['--network',names.network,'--read-only','--tmpfs','/tmp:rw,nosuid,nodev,size=256m',`--memory=${resources.memory}`,`--cpus=${resources.cpus}`,`--pids-limit=${resources.pids}`,'--cap-drop=ALL','--security-opt=no-new-privileges','--mount',`type=volume,source=${names.volume},target=/var/lib/enginebox-stream`,'--mount',`type=bind,source=${path.join(temporary,'config.json')},target=/fixture/config.json,readonly`,'--entrypoint','/usr/bin/tini'];
 await dockerInput(invitation,'run','--rm','-i',...hostArgs,hostImage,'--','/usr/local/bin/enginebox-stream-host','--enroll-only','/fixture/config.json');
 const startHost=()=>docker('run','-d','--name',names.host,...hostArgs,hostImage,'--','/usr/local/bin/enginebox-stream-host','/fixture/config.json');
 await startHost();let restarted=false;const until=Date.now()+300000;
 while(Date.now()<until){checkInterrupted();const log=await docker('logs',names.driver);if(log.includes('STREAM_TEST_RESTART_READY')&&!restarted){const top=await docker('top',names.host,'-eo','pid,comm');assert(!/Xvfb|ffmpeg|enginebox-test-window|godot/.test(top),'owned capture children must stop before restart');await docker('restart',names.host);restarted=true;}const state=await docker('inspect','--format','{{.State.Running}}',names.driver);if(state==='false')break;await new Promise(r=>setTimeout(r,500));}
 const logs={host:await docker('logs',names.host),driver:await docker('logs',names.driver),relay:await docker('logs',names.relay)};
 for(const [name,log]of Object.entries(logs))await writeFile(path.join(evidence,name+'.log'),log);
 let receipt;try{receipt=JSON.parse(await readFile(path.join(evidence,'result.json'),'utf8'));}catch{throw Error(`Driver produced no receipt. Host: ${logs.host.slice(-3000)}\nDriver: ${logs.driver.slice(-3000)}`);}assert(receipt.passed,`${receipt.error}\nHost: ${logs.host.slice(-3000)}\nDriver: ${logs.driver.slice(-3000)}`);assert(restarted);
 receipt={...receipt,resources,images:{host:await docker('image','inspect',hostImage,'--format','{{.Id}}'),browser:await docker('image','inspect',browserImage,'--format','{{.Id}}'),relay:await docker('image','inspect',relayImage,'--format','{{.Id}}')},binding:{project:config.project,nodeId:connection?.nodeId||'native-fixture'}};
 await writeFile(path.join(evidence,'result.json'),JSON.stringify(receipt,null,2));console.log(JSON.stringify({...receipt,evidence},null,2));
}finally{for(const name of [names.host,names.driver,names.relay])await docker('rm','-f',name).catch(()=>{});await docker('volume','rm',names.volume).catch(()=>{});await docker('network','rm',names.network).catch(()=>{});if(process.env.STREAM_EVIDENCE_DIR)await rm(temporary,{recursive:true,force:true});}

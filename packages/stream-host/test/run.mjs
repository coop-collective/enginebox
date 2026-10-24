import {spawn} from 'node:child_process';
import {mkdtemp,writeFile,readFile,mkdir,rm,chmod} from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import assert from 'node:assert/strict';
const directory=path.dirname(fileURLToPath(import.meta.url));
const prefix=`enginebox-stream-proof-${process.pid}-${Date.now().toString(36)}`;
const temporary=await mkdtemp(path.join(os.tmpdir(),prefix));const evidence=process.env.STREAM_EVIDENCE_DIR||path.join(temporary,'evidence');await mkdir(evidence,{recursive:true});
await chmod(temporary,0o755); // Only disposable fixture configuration, readable by container UID1000.
const names={host:prefix+'-host',driver:prefix+'-driver',relay:prefix+'-relay',volume:prefix+'-state',network:prefix+'-net'};
const docker=(...args)=>new Promise((resolve,reject)=>{const p=spawn('docker',args,{stdio:['pipe','pipe','pipe']});let out='',err='';p.stdout.on('data',v=>out+=v);p.stderr.on('data',v=>err+=v);p.on('close',code=>code===0?resolve(out.trim()):reject(Error(`docker ${args[0]} failed: ${err||out}`)));p.stdin.end();});
const hostImage=process.env.STREAM_HOST_TEST_IMAGE||'enginebox-stream-host:qualification';
const browserImage=process.env.STREAM_BROWSER_TEST_IMAGE||'enginebox-stream-browser:qualification';
const relayImage=process.env.STREAM_RELAY_TEST_IMAGE||'enginebox-stream-relay:qualification';
try{
 await docker('network','create',names.network);await docker('volume','create',names.volume);
 const config={controller:'http://driver:8080',project:'fixture',allowLocalController:true,width:640,height:360,maxPeers:2,command:{executable:'/usr/local/bin/enginebox-test-window',directory:'/app'}};
 await writeFile(path.join(temporary,'config.json'),JSON.stringify(config));await writeFile(path.join(temporary,'invitation'),`native-fixture.enroll.${'a'.repeat(64)}`);
 await docker('run','-d','--name',names.relay,'--network',names.network,'--network-alias','relay','--read-only','--tmpfs','/tmp:rw,nosuid,nodev,size=8m','--cap-drop=ALL','--security-opt=no-new-privileges',relayImage);
 await docker('run','-d','--name',names.driver,'--network',names.network,'--network-alias','driver','--shm-size=256m','--memory=2g','--cpus=2','--cap-drop=ALL','--security-opt=no-new-privileges','--mount',`type=bind,source=${directory},target=/test,readonly`,'--mount',`type=bind,source=${evidence},target=/evidence`,'--entrypoint','node',browserImage,'/test/driver.mjs');
 const startHost=()=>docker('run','-d','--name',names.host,'--network',names.network,'--read-only','--tmpfs','/tmp:rw,nosuid,nodev,size=256m','--memory=1g','--cpus=2','--pids-limit=128','--cap-drop=ALL','--security-opt=no-new-privileges','--mount',`type=volume,source=${names.volume},target=/var/lib/enginebox-stream`,'--mount',`type=bind,source=${temporary},target=/fixture,readonly`,'--entrypoint','sh',hostImage,'-c','exec /usr/bin/tini -- /usr/local/bin/enginebox-stream-host /fixture/config.json < /fixture/invitation');
 await startHost();let restarted=false;const until=Date.now()+300000;
 while(Date.now()<until){const log=await docker('logs',names.driver);if(log.includes('STREAM_TEST_RESTART_READY')&&!restarted){const top=await docker('top',names.host,'-eo','comm');assert(!/Xvfb|ffmpeg|enginebox-test-window/.test(top),'owned capture children must stop before restart');await docker('restart',names.host);restarted=true;}const state=await docker('inspect','--format','{{.State.Running}}',names.driver);if(state==='false')break;await new Promise(r=>setTimeout(r,500));}
 const logs={host:await docker('logs',names.host),driver:await docker('logs',names.driver),relay:await docker('logs',names.relay)};
 for(const [name,log]of Object.entries(logs))await writeFile(path.join(evidence,name+'.log'),log);
 const receipt=JSON.parse(await readFile(path.join(evidence,'result.json'),'utf8'));assert(receipt.passed,receipt.error);assert(restarted);console.log(JSON.stringify({...receipt,evidence},null,2));
}finally{for(const name of [names.host,names.driver,names.relay])await docker('rm','-f',name).catch(()=>{});await docker('volume','rm',names.volume).catch(()=>{});await docker('network','rm',names.network).catch(()=>{});if(process.env.STREAM_EVIDENCE_DIR)await rm(temporary,{recursive:true,force:true});}

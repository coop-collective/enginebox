import assert from 'node:assert/strict';
import http from 'node:http';
import { createHash, randomUUID } from 'node:crypto';
import { readFile, writeFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
const require=createRequire(process.env.PLAYWRIGHT_PACKAGE||'/app/prototype/package.json');
const {chromium}=require('playwright');
const sessions=new Map();let secretHash=null,epoch=0,processSession=null,stats={},revoked=false;
const iceServers=[{urls:['turn:relay:3478?transport=udp'],username:'fixture',credential:'fixture-only'}];
const body=async req=>{let s='';for await(const c of req){s+=c;if(s.length>1024*1024)throw Error('body bound');}return s?JSON.parse(s):{};};
const server=http.createServer(async(req,res)=>{
  const send=(status,v)=>{res.writeHead(status,{'content-type':'application/json'});res.end(JSON.stringify(v));};
  try{
    const url=new URL(req.url,'http://driver');
    if(url.pathname==='/'){res.writeHead(200,{'content-type':'text/html'});return res.end(await readFile('/test/viewer.html'));}
    if(url.pathname==='/fixture/open'){
      const {mode}=await body(req);const id=randomUUID();const session={id,project:'fixture',nodeId:'native-fixture',nodeEpoch:epoch,actorId:'fixture-viewer',mode,state:'open',expires:Date.now()+60000,createdAt:Date.now(),iceServers,iceTransportPolicy:'relay',iceExpires:Date.now()+3600000};
      sessions.set(id,{session,signals:[],answer:null});return send(200,session);
    }
    if(url.pathname==='/fixture/status')return send(200,{epoch,stats,revoked});
    const fixture=url.pathname.match(/^\/fixture\/(offer|answer|end)\/([^/]+)$/);
    if(fixture){const s=sessions.get(fixture[2]);assert(s);if(fixture[1]==='offer'){const v=await body(req);s.signals.push({id:'browser-offer',sequence:1,signal:v});return send(200,{});}if(fixture[1]==='answer')return send(200,s.answer||{});s.session.state='closed';return send(200,{});}
    const authorization=req.headers.authorization||'';
    if(url.pathname==='/v1/node/enroll'){
      if(authorization!==`Bearer native-fixture.enroll.${'a'.repeat(64)}`)return send(401,{});const v=await body(req);if(secretHash&&secretHash!==v.nodeSecretHash)return send(409,{});secretHash=v.nodeSecretHash;return send(200,{id:'native-fixture'});
    }
    if(revoked||!secretHash||createHash('sha256').update(authorization.replace(/^Bearer /,'')).digest('hex')!==secretHash)return send(401,{});
    if(url.pathname==='/v1/node/hello'){const v=await body(req);processSession=v.session;epoch++;return send(200,{epoch,session:processSession,reservations:[]});}
    if(req.headers['x-enginebox-epoch']!==String(epoch)||req.headers['x-enginebox-session']!==processSession)return send(409,{});
    if(url.pathname==='/v1/node/heartbeat'){stats=(await body(req)).stats;return send(200,{ok:true});}
    if(url.pathname==='/v1/node/machine-sessions/poll')return send(200,{sessions:[...sessions.values()].filter(s=>s.session.state==='open'&&s.session.expires>Date.now()).map(s=>s.session),now:Date.now()});
    const signal=url.pathname.match(/^\/v1\/projects\/fixture\/machine-sessions\/([^/]+)\/(signals|end)$/);
    if(signal){const s=sessions.get(signal[1]);if(!s||s.session.nodeEpoch!==epoch)return send(403,{});if(signal[2]==='end'){s.session.state='closed';return send(200,{});}if(req.method==='GET'){const after=Number(url.searchParams.get('after'));return send(200,{session:s.session,signals:s.signals.filter(s=>s.sequence>after),cursor:s.signals.at(-1)?.sequence||0});}const v=await body(req);assert.equal(v.signal.type,'answer');if(s.answer)assert.deepEqual(s.answer,v.signal);s.answer=v.signal;return send(200,{sequence:2});}
    send(404,{});
  }catch(e){console.error('fixture route:',e.message);send(500,{error:e.message});}
});
await new Promise(r=>server.listen(8080,'0.0.0.0',r));
const wait=async(fn,label,timeout=45000)=>{const until=Date.now()+timeout;while(Date.now()<until){if(await fn())return;await new Promise(r=>setTimeout(r,200));}throw Error(`Timed out: ${label}`);};
let browser;
try{
  await wait(()=>epoch===1,'native host enroll/hello',180000);
  browser=await chromium.launch({headless:true,args:['--no-sandbox','--autoplay-policy=no-user-gesture-required']});
  const pages=await Promise.all([browser.newPage(),browser.newPage()]);
  const errors=[];for(const page of pages){page.on('pageerror',e=>errors.push(e.message));await page.goto('http://127.0.0.1:8080');}
  await Promise.all([pages[0].evaluate(()=>openSession('control')),pages[1].evaluate(()=>openSession('watch'))]);
  await wait(async()=>{for(const page of pages){if((await page.evaluate(()=>stats())).frames<10)return false;}return true;},'two real decoded RTP streams');
  const before=await Promise.all(pages.map(p=>p.evaluate(()=>pixel())));
  for(const page of pages){const s=await page.evaluate(()=>stats());assert.equal(s.localType,'relay');assert.equal(s.remoteType,'relay');assert.equal(s.width,640);assert.equal(s.height,360);}
  await wait(()=>pages[0].evaluate(()=>input.readyState==='open'),'control input open');
  await pages[0].evaluate(()=>clickNative());
  await wait(async()=>(await pages[0].evaluate(()=>pixel()))[0]>150&&(await pages[1].evaluate(()=>pixel()))[0]>150,'native click visible to both viewers');
  await wait(()=>stats.inputsForwarded===2,'two valid input messages');
  await pages[0].evaluate(()=>input.send(JSON.stringify({type:'pointerdown',x:-1,y:20})));
  await wait(()=>pages[1].evaluate(()=>input.readyState==='closed'),'watch input rejected');
  await new Promise(r=>setTimeout(r,1000));assert.equal(stats.inputsForwarded,2);
  await pages[0].evaluate(()=>closeSession());await wait(()=>stats.peers===1,'disconnect releases slot');
  await pages[0].evaluate(()=>openSession('control'));await wait(async()=>(await pages[0].evaluate(()=>stats())).frames>=10,'reconnect with a new session');
  const samples=await Promise.all(pages.map(p=>p.evaluate(()=>stats())));
  // Expire the control lease while a watcher keeps the engine alive: input stops locally.
  for(const s of sessions.values())if(s.session.state==='open'&&s.session.mode==='control')s.session.expires=Date.now()+500;
  await wait(()=>stats.peers===1,'lease expiry reclaims control');
  for(const s of sessions.values())s.session.state='closed';await wait(()=>stats.peers===0,'last peer ends capture');
  const frames=stats.framesEncoded;await new Promise(r=>setTimeout(r,6000));assert.equal(stats.framesEncoded,frames,'owned encoder stopped with last session');
  console.log('STREAM_TEST_RESTART_READY');
  await wait(()=>epoch===2,'persistent identity restart',90000);
  await pages[0].evaluate(()=>openSession('control'));await wait(async()=>(await pages[0].evaluate(()=>stats())).frames>=10,'capture after host restart');
  revoked=true;await wait(async()=>['disconnected','failed','closed'].includes((await pages[0].evaluate(()=>stats())).connection),'authority revocation closes peer');
  assert.deepEqual(errors,[]);
  const receipt={passed:true,fixture:'actual native X11 + FFmpeg VP8',controller:'protocol fixture; real fleet proof is separate',transport:'relay-only TURN; no host inbound ports',before,samples,identityRestartEpoch:epoch,inputMessages:2,watchInputRejected:true,invalidCoordinatesRejected:true,leaseReclaimed:true,revocationClosed:true};
  await writeFile('/evidence/result.json',JSON.stringify(receipt,null,2));console.log('STREAM_TEST_PASSED');
}catch(e){console.error(e.stack);await writeFile('/evidence/result.json',JSON.stringify({passed:false,error:e.message}));process.exitCode=1;}
finally{await browser?.close();server.close();}

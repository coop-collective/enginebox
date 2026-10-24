// Disposable real-controller adapter. Owner authority stays inside the test driver.
import {readFile} from 'node:fs/promises';
import {randomBytes,randomUUID,createHash} from 'node:crypto';
const operation=()=>randomUUID();
export async function fleetAdapter(file){
 const config=JSON.parse(await readFile(file,'utf8'));const sessions=new Map();
 const call=async(path,body,token=config.ownerToken,secret)=>{
  const response=await fetch(config.dockerController+path,{method:body===undefined?'GET':'POST',headers:{authorization:`Bearer ${token}`,'content-type':'application/json',...(secret?{'x-enginebox-stream-client':secret}:{})},...(body===undefined?{}:{body:JSON.stringify(body)})});
  if(!response.ok)throw Error(`real fleet ${path.split('/').at(-1)} HTTP${response.status}`);return response.json();
 };
 const base=`/v1/projects/${config.project}`;
 const app=(await call('/v1/nodes',{id:'app-'+randomUUID(),role:'agent',projects:[config.project],origin:null,slots:1,cacheBytes:16777216,taskOnly:true})).token;
 const grant=(id,mode)=>call(base+'/machine-session-grants',{id:operation(),sessionId:id,nodeId:config.nodeId,actorId:'fixture-account',mode});
 return {
  project:config.project,nodeId:config.nodeId,sessions,
  async open(mode){const id=operation(),secret=randomBytes(32).toString('hex');const g=await grant(id,mode);const value=await call(base+'/machine-sessions',{id,grant:g.grant,clientSecretHash:createHash('sha256').update(secret).digest('hex')},app);const session={...value.session,iceServers:value.iceServers,iceTransportPolicy:value.iceTransportPolicy,iceExpires:value.iceExpires};sessions.set(id,{session,secret,disableRenew:false});return session;},
  async offer(id,signal){const s=sessions.get(id);return call(base+`/machine-sessions/${id}/signals`,{id:operation(),signal},app,s.secret);},
  async answer(id){const s=sessions.get(id);const v=await call(base+`/machine-sessions/${id}/signals`,undefined,app,s.secret);return v.signals.find(v=>v.signal.type==='answer')?.signal||{};},
  async end(id){const s=sessions.get(id);await call(base+`/machine-sessions/${id}/end`,{id:operation()},app,s.secret);s.session.state='ended';},
  async renew(){for(const [id,s]of sessions){if(s.session.state!=='open'||s.disableRenew)continue;const g=await grant(id,s.session.mode);const v=await call(base+`/machine-sessions/${id}/renew`,{grant:g.grant},app,s.secret);s.session.expires=v.session.expires;}},
  async revoke(){return call(base+`/machines/${config.nodeId}/revoke`,{operationId:operation()});},
  async proxy(req,res,observed){const chunks=[];for await(const c of req)chunks.push(c);const b=Buffer.concat(chunks);const headers={};for(const name of ['authorization','content-type','x-enginebox-epoch','x-enginebox-session'])if(req.headers[name])headers[name]=req.headers[name];
   const response=await fetch(config.dockerController+req.url,{method:req.method,headers,...(b.length?{body:b}:{})});const bytes=Buffer.from(await response.arrayBuffer());
   if(response.ok){if(req.url==='/v1/node/hello'){const v=JSON.parse(bytes);observed.epoch(v.epoch);}if(req.url==='/v1/node/heartbeat')observed.stats(JSON.parse(b).stats);}
   if(!response.ok)console.error('fleet proxy status',response.status,req.url.split('?')[0]);res.writeHead(response.status,{'content-type':'application/json'});res.end(bytes);
  }
 };
}

import { createServer } from 'node:http';
import { mkdirSync, readFileSync, existsSync, writeFileSync, renameSync } from 'node:fs';
import { randomUUID } from 'node:crypto';
import { join } from 'node:path';
const dataDir=process.env.ENGINEBOX_DATA_DIR || '/tmp/enginebox-demo-data';mkdirSync(dataDir,{recursive:true});
const stateFile=join(dataDir,'state.json');
if(!existsSync(stateFile))writeFileSync(stateFile,JSON.stringify({identity:randomUUID(),count:0}));
const release=JSON.parse(readFileSync(new URL('./dist/release.json',import.meta.url)));
const page=readFileSync(new URL('./dist/index.html',import.meta.url));
createServer((req,res)=>{
 res.setHeader('cache-control','no-store');res.setHeader('x-content-type-options','nosniff');
 if(req.url==='/health'||req.url==='/api/state'){
  const state=JSON.parse(readFileSync(stateFile));
  if(req.method==='POST'&&req.url==='/api/state'){
   if(req.headers.origin && new URL(req.headers.origin).host!==req.headers.host){res.writeHead(403);return res.end();}
   state.count++;writeFileSync(`${stateFile}.tmp`,JSON.stringify(state));renameSync(`${stateFile}.tmp`,stateFile);
  }
  res.setHeader('content-type','application/json');return res.end(JSON.stringify({...state,version:release.version,revision:process.env.ENGINEBOX_REVISION||'local'}));
 }
 if(req.url!=='/'){res.writeHead(404);return res.end('Not found');}
 res.setHeader('content-type','text/html; charset=utf-8');res.end(page);
}).listen(Number(process.env.PORT||3000),'0.0.0.0');

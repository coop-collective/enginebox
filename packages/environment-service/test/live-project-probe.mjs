// Explicit live fixture probe. It writes only this disposable app's counter.
import assert from 'node:assert/strict';
import { readFile, writeFile } from 'node:fs/promises';
const [phase, main, preview, report] = process.argv.slice(2);
assert(process.env.ENGINEBOX_LIVE_PROJECT_PROBE === '1' && ['before','after'].includes(phase) && main && preview && report);
async function state(base, method='GET') {
 const url=new URL('/api/state',base);
 const response=await fetch(url,{method,redirect:'error',headers:{origin:url.origin},signal:AbortSignal.timeout(10000)});
 assert.equal(response.status,200);return response.json();
}
if(phase==='before'){
 const initial={main:await state(main),preview:await state(preview)};
 assert.equal(initial.main.version,'1');assert.equal(initial.preview.version,'2');assert.notEqual(initial.main.identity,initial.preview.identity);
 const saved={main:await state(main,'POST'),preview:await state(preview,'POST')};
 assert.equal(saved.main.count,initial.main.count+1);assert.equal(saved.preview.count,initial.preview.count+1);
 await writeFile(report,JSON.stringify({phase,initial,saved,checkedAt:new Date().toISOString()},null,2),{mode:0o600,flag:'wx'});
 console.log(JSON.stringify({mainVersion:'1',previewVersion:'2',isolatedPersistentData:true}));
}else{
 const before=JSON.parse(await readFile(report,'utf8'));let after;
 for(let n=0;n<30;n++){
  after={main:await state(main),preview:await state(preview)};
  if(after.main.version==='2')break;
  await new Promise(r=>setTimeout(r,2000));
 }
 assert.equal(after.main.version,'2');assert.equal(after.preview.version,'2');assert.notEqual(after.main.revision,before.saved.main.revision);
 for(const name of ['main','preview']){assert.equal(after[name].identity,before.saved[name].identity);assert.equal(after[name].count,before.saved[name].count);}
 await writeFile(report,JSON.stringify({...before,after,finishedAt:new Date().toISOString()},null,2));
 console.log(JSON.stringify({mainVersion:'2',previewVersion:'2',dataPreserved:true,mainRevision:after.main.revision}));
}

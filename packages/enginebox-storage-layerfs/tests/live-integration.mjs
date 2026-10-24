// Opt-in Linux qualification. It creates only a new provisioned storage root.
import assert from 'node:assert/strict';
import { readFileSync, writeFileSync, existsSync, mkdirSync, chmodSync, cpSync, createReadStream, readdirSync, statSync } from 'node:fs';
import { join, dirname, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import { createHash, randomFillSync } from 'node:crypto';
assert.equal(process.platform,'linux');
assert.equal(process.env.ENGINEBOX_LAYERFS_INTEGRATION,'1','Explicit disposable integration opt-in required');
const marker=process.env.ENGINEBOX_LAYERFS_MARKER;
assert(marker&&readFileSync(marker,'utf8').trim()==='integration-20261024','Owned fixture marker required');
const [ebox,configFile,reportFile]=process.argv.slice(2);
assert(ebox&&configFile&&reportFile,'Usage: live-integration.mjs ABSOLUTE_EBOX PRIVATE_CONFIG FRESH_REPORT');
const config=JSON.parse(readFileSync(configFile));
assert(!existsSync(config.root)&&!existsSync(reportFile),'Requires a fresh private root and report');
const directory=dirname(resolve(reportFile));mkdirSync(directory,{recursive:true,mode:0o700});
const seed=join(directory,'seed');assert(!existsSync(seed));mkdirSync(seed,{mode:0o700});
const canonical=value=>JSON.stringify(sort(value));
function sort(value){return Array.isArray(value)?value.map(sort):value&&typeof value==='object'?Object.fromEntries(Object.keys(value).sort().map(k=>[k,sort(value[k])])):value;}
const hash=value=>createHash('sha256').update(value).digest('hex');
async function fileHash(path){const h=createHash('sha256');for await(const block of createReadStream(path))h.update(block);return h.digest('hex');}
function run(argv,expect=0){const result=spawnSync(argv[0],argv.slice(1),{encoding:'utf8',timeout:900_000,maxBuffer:16*1024*1024});assert.equal(result.status,expect,`${argv.slice(0,5).join(' ')}: ${result.stderr}`);return result.stdout;}
const prefix=[ebox,'--host','local','storage','layerfs','--config',configFile];
const cli=(args,status=0)=>JSON.parse(run([...prefix,...args],status));
writeFileSync(join(seed,'scene.txt'),'first\nsecond\nthird\n');writeFileSync(join(seed,'readme.txt'),'base\n');
const binary=randomFillSync(Buffer.alloc(128*1024*1024));writeFileSync(join(seed,'environment.uasset'),binary);
const originalBinary=hash(binary);
if(process.env.ENGINEBOX_LAYERFS_EXTRA_SOURCE)cpSync(process.env.ENGINEBOX_LAYERFS_EXTRA_SOURCE,seed,{recursive:true});
run(['git','init','-q',seed]);run(['git','-C',seed,'add','.']);run(['git','-C',seed,'-c','user.name=Fixture','-c','user.email=fixture@example.invalid','commit','-qm','Reviewed fixture']);
const preview=cli(['preview','--repo',seed,'--revision','HEAD']);
const doctor=cli(['doctor']);
cli(['import','game','--repo',seed,'--revision',preview.commit,'--approval',preview.approval,'--credential-free','--source-only']);
const base=cli(['snapshot','game','base']);
const project=JSON.parse(readFileSync(join(config.root,'projects/game/project.json')));
const fixture=join(config.root,'changes.mjs');
const sdkScript=`
import {writeFileSync} from 'node:fs';
Object.assign(process.env,${JSON.stringify({LAYERFS_HOME:join(config.root,'projects/game/home'),LAYERFS_VOLUME:project.volume,LAYERFS_DAEMON:config.daemon,LAYERFS_NATIVE:join(config.sdk,'native/layerfs.node'),LAYERFS_CONTAINER_PREFIX:`ebox-lfs-${project.owner}-`,LAYERFS_STORE_PATH:'/layerfs-store/global/store.sqlite',LAYERFS_CONSTRUCTION_WORKERS:'1',UV_THREADPOOL_SIZE:'4'})});
const {LayerFS}=await import(${JSON.stringify(join(config.sdk,'src/index.js'))});
const {inspectFrozen}=await import(${JSON.stringify(join(config.root,'integration.mjs'))});
const client=new LayerFS('game',{storePath:process.env.LAYERFS_STORE_PATH});
try {
 const refs={};
 for(const [name,path,content] of [['left','/scene.txt','first edited\\nsecond\\nthird\\n'],['right','/readme.txt','new feature\\n'],['conflict','/scene.txt','incompatible first\\nsecond\\nthird\\n'],['binary-a','/environment.uasset',Buffer.from([0,1,2,3])],['binary-b','/environment.uasset',Buffer.from([0,4,5,6])],['lww-left','/scene.txt','LWW must not win\\n']]) {
  await client.branch.fork(name,${JSON.stringify(base.ref)});await client.fs.write(name,path,content,'Fixture change');
  if(name==='lww-left')await client.fs.write(name,'/.lww','scene.txt\\n','Forbidden conflict policy');
  refs[name]=await inspectFrozen(client,name,'read-'+name+'-');
 }
 writeFileSync(${JSON.stringify(join(config.root,'refs.json'))},JSON.stringify(refs));
} finally {await client.close();}
`;
writeFileSync(fixture,sdkScript,{mode:0o600});
run(['docker','--host',`unix://${config.docker_socket??'/var/run/docker.sock'}`,'run','--rm','--cpus=1','--memory=1g','--pids-limit=256','--mount',`type=bind,src=${config.root},dst=${config.root}`,'--mount',`type=bind,src=${config.sdk},dst=${config.sdk},readonly`,'--mount',`type=bind,src=${config.daemon},dst=${config.daemon},readonly`,'--mount',`type=volume,src=${project.volume},dst=/layerfs-store`,config.image,'node',fixture]);
const refs=JSON.parse(readFileSync(join(config.root,'refs.json')));
function request(id,names){return {id,project:'demo',generation:'0'.repeat(64),integration:{id,layerfsProject:'game',baseRef:base.ref,baseRoot:base.root,changes:names.map((name,i)=>({id:name,submission:name,generation:String(i+1).repeat(64),...refs[name]})),taskRequirements:{}}};}
function execute(spec,status=0){const path=join(directory,`${spec.id}.json`);writeFileSync(path,canonical(spec),{mode:0o600});return cli(['integrate','--request',path],status);}
const spec=request('merged',['left','right']);const merged=execute(spec);
assert.deepEqual(execute(spec),merged,'Identical integration must return the same receipt');
const source=JSON.parse(readFileSync(merged.sourceManifest));
assert.equal(source.treeHash,hash(canonical(source.entries)));
assert(!source.entries.some(e=>e.path==='.git'||e.path.startsWith('.git/')));
assert.equal(readFileSync(join(merged.sourceRoot,'scene.txt'),'utf8'),'first edited\nsecond\nthird\n');
assert.equal(readFileSync(join(merged.sourceRoot,'readme.txt'),'utf8'),'new feature\n');
assert.equal(statSync(join(merged.sourceRoot,'environment.uasset')).size,128*1024*1024);
assert.equal(await fileHash(join(merged.sourceRoot,'environment.uasset')),originalBinary);
for(const entry of source.entries)if(entry.kind==='file')assert.equal(await fileHash(join(merged.sourceRoot,entry.path)),entry.sha256);
const {DatabaseSync}=await import('node:sqlite');const db=new DatabaseSync(merged.snapshot,{readOnly:true});
assert.equal(Object.values(db.prepare('PRAGMA integrity_check').get())[0],'ok');assert.deepEqual(db.prepare('PRAGMA foreign_key_check').all(),[]);db.close();
const conflicts={};
for(const [id,names] of [['text-conflict',['left','conflict']],['binary-conflict',['binary-a','binary-b']],['lww-conflict',['lww-left','conflict']]]){
 const outcome=execute(request(id,names),2);assert.equal(outcome.ok,false);assert.equal(outcome.conflict.code,'Conflict');assert.deepEqual(execute(request(id,names),2),outcome);conflicts[id]=outcome.conflict;
}
const after=cli(['snapshot','game','after']);assert.equal(after.ref,base.ref);assert.equal(after.root,base.root);
const report={schema:1,scope:'same-Store pinned SDK; synthetic 128 MiB Unreal-style binary; no Unreal editor inference',doctor,base,refs,merged,sourceEntries:source.entries.length,sourceBytes:source.entries.reduce((n,e)=>n+(e.size??0),0),originalBinary,conflicts,checks:{sourceHashes:true,sqliteIntegrity:true,foreignKeys:true,retryIdentity:true,acceptedMainUnchanged:true},workspaceReceipts:readdirSync(join(config.root,'workspaces')).filter(p=>p.endsWith('.json')).length};
writeFileSync(reportFile,JSON.stringify(report,null,2),{mode:0o600});chmodSync(reportFile,0o600);console.log(JSON.stringify({report:reportFile,merged:merged.ref,treeHash:merged.treeHash,sourceBytes:report.sourceBytes,conflicts:Object.keys(conflicts)}));

import { spawn } from 'node:child_process';
import { createHash, randomUUID } from 'node:crypto';
import { lstat, mkdir, open, readFile, realpath, rename, writeFile } from 'node:fs/promises';
import { isAbsolute, join, parse, relative, resolve, sep } from 'node:path';
import { canonical, digest, exactKeys, identifier, record, requireThat, sha256, validateBuild, type Assignment, type EngineBuildJob } from '@enginebox/fleet-protocol';
import { Client } from './client.ts';
import { publishBuild } from './artifacts.ts';
import { UncertainNativeRun } from './native-assistant.ts';

export type EngineBuildConfig={
  localTrusted:true; binary:string; binarySha256:string;
  bindings:Record<string,{integrationRoot:string;state:string;recipes:Record<string,string>}>;
};
type BuildAssignment=Assignment & EngineBuildJob;
function active(assignment:BuildAssignment,signal:AbortSignal){requireThat(!signal.aborted && assignment.expires>Date.now(),'Build lease expired',409);}
async function safePath(path:string) {
  requireThat(isAbsolute(path) && path===resolve(path),'Build paths must be absolute and normalized');
  let part=parse(path).root;
  for(const component of path.slice(part.length).split(sep).filter(Boolean)){
    part=join(part,component);
    try {requireThat(!(await lstat(part)).isSymbolicLink(),'Private build paths cannot contain symbolic links');}
    catch(error){if((error as NodeJS.ErrnoException).code!=='ENOENT')throw error;}
  }
}
async function privateDirectory(path:string){
  await safePath(path);await mkdir(path,{recursive:true,mode:0o700});const info=await lstat(path);
  requireThat(info.isDirectory() && !info.isSymbolicLink(),'Private build directory required');
  if(process.platform!=='win32')requireThat((info.mode&0o077)===0 && info.uid===process.getuid?.(),'Build state must be owned and mode0700');
}
async function jsonFile(path:string,max:number):Promise<any>{
  await safePath(path);const info=await lstat(path);requireThat(info.isFile() && info.size<=max,'Invalid build JSON file');
  const data=await readFile(path);requireThat(data.length<=max,'Build JSON exceeds limit');return JSON.parse(data.toString('utf8'));
}
async function hashFile(path:string,check:()=>void){
  await safePath(path);const before=await lstat(path);requireThat(before.isFile(),'Build executable must be regular');
  const file=await open(path,'r');const hash=createHash('sha256');const buffer=Buffer.alloc(1024*1024);let bytes=0;
  try {const meta=await file.stat();requireThat(meta.ino===before.ino && meta.dev===before.dev,'Executable changed while opening',409);
    for(;;){check();const {bytesRead}=await file.read(buffer,0,buffer.length,null);if(!bytesRead)break;hash.update(buffer.subarray(0,bytesRead));bytes+=bytesRead;}
    requireThat(bytes===before.size,'Executable changed while hashing',409);return hash.digest('hex');
  }finally{await file.close();}
}
async function atomicJSON(path:string,value:unknown){const temp=`${path}.${randomUUID()}.tmp`;await writeFile(temp,canonical(value),{mode:0o600,flag:'wx'});await rename(temp,path);}
/** Matches Rust serde defaults; recipe bytes are never commands supplied by a job. */
export async function protectedRecipe(path:string){
  const input=await jsonFile(path,65536);record(input);
  exactKeys(input,['schema','engine','version','target','architecture','executable','executableSha256','inputs','entrypoint','timeoutSeconds','maxLogBytes','environment','unityMethod','unrealProject','cacheVersion']);
  const recipe:Record<string,unknown>={...input,inputs:input.inputs??[],maxLogBytes:input.maxLogBytes??8*1024*1024,environment:input.environment??{},unityMethod:input.unityMethod??null,unrealProject:input.unrealProject??null,cacheVersion:input.cacheVersion??'1'};
  return {value:recipe,hash:await sha256(canonical({adapter:1,recipe}))};
}
export class EngineBuilds {
  config:EngineBuildConfig;client:Client;
  constructor(config:EngineBuildConfig,client:Client){
    requireThat(config.localTrusted===true,'Local native builds require explicit private/trusted opt-in');
    requireThat(process.platform!=='win32','Use a qualified Linux build worker');
    requireThat(isAbsolute(config.binary),'Protected enginebox-build binary required');digest(config.binarySha256);
    for(const [project,binding] of Object.entries(config.bindings)){
      identifier(project);requireThat(isAbsolute(binding.integrationRoot)&&isAbsolute(binding.state),'Private build roots required');
      requireThat(Object.keys(binding.recipes).length>0 && Object.keys(binding.recipes).length<=32,'Configure 1–32 protected recipes');
      for(const [hash,path] of Object.entries(binding.recipes)){digest(hash);requireThat(isAbsolute(path),'Protected recipe path required');}
    }
    this.config=config;this.client=client;
  }
  async execute(assignment:Assignment,external:AbortSignal):Promise<Record<string,unknown>>{
    requireThat(assignment.kind==='engine-build','Engine build assignment required');
    const binding=this.config.bindings[assignment.project];requireThat(binding,'Build project is not configured',409);
    const recipePath=binding.recipes[assignment.build.recipe];requireThat(recipePath,'Protected build recipe is not installed',409);
    const deadline=Date.now()+assignment.timeoutSeconds*1000;
    const cancellation=new AbortController();
    const signal=AbortSignal.any([external,cancellation.signal,AbortSignal.timeout(assignment.timeoutSeconds*1000)]);
    const check=()=>active(assignment,signal);check();
    // Same-node custody is intentional until logical remote source import is qualified.
    const integration=await this.client.json<{status:string;assignment?:{node:string};spec:{kind:string};result?:string}>(`/v1/projects/${assignment.project}/jobs/${assignment.build.integration}`);
    requireThat(integration.status==='completed' && integration.spec.kind==='integration' && integration.assignment?.node===assignment.node && integration.result,'Build does not own the completed integration export',409);
    const report=JSON.parse(integration.result);
    requireThat(report.generation===assignment.generation && report.treeHash===assignment.build.sourceTree,'Integration export identity changed',409);
    const sourceDir=join(binding.integrationRoot,assignment.build.integration),sourceRoot=join(sourceDir,'source'),sourceManifest=join(sourceDir,'source.json');
    await safePath(sourceRoot);const source=await jsonFile(sourceManifest,32*1024*1024);
    requireThat(source.schema===1 && source.project===assignment.project && source.generation===assignment.generation && source.treeHash===assignment.build.sourceTree && source.layerfs?.ref===report.ref && source.layerfs?.root===report.root,'Frozen source provenance differs',409);
    const recipe=await protectedRecipe(recipePath);
    requireThat(recipe.hash===assignment.build.recipe && recipe.value.timeoutSeconds===assignment.timeoutSeconds,'Protected recipe or timeout changed',409);
    requireThat(await hashFile(this.config.binary,check)===this.config.binarySha256,'Build executor binary changed',409);
    await privateDirectory(binding.state);
    const attempts=join(binding.state,'attempts');await privateDirectory(attempts);
    requireThat(/^[a-zA-Z0-9-]{1,64}$/.test(assignment.attempt),'Invalid private attempt identity');
    const attemptDir=join(attempts,assignment.attempt);await privateDirectory(attemptDir);
    const lease=join(attemptDir,'lease.json');let leaseWrites=Promise.resolve();let leaseError:unknown;
    const update=()=>{leaseWrites=leaseWrites.then(()=>atomicJSON(lease,{attempt:assignment.attempt,expiresAtMs:Math.min(assignment.expires,deadline),cancelled:signal.aborted||!!leaseError})).catch(error=>{leaseError=error;});return leaseWrites;};
    await update();requireThat(!leaseError,'Cannot create private build lease');
    const timer=setInterval(()=>void update(),200);
    const abort=()=>void update();signal.addEventListener('abort',abort,{once:true});
    try {
      const output=await this.run(assignment,recipePath,sourceManifest,sourceRoot,binding.state,lease,attemptDir,signal,()=>leaseError,update,()=>cancellation.abort());
      check();requireThat(!leaseError,'Cannot renew private build lease');
      const receipt=JSON.parse(output);digest(receipt.artifactId);
      requireThat(typeof receipt.cacheHit==='boolean' && typeof receipt.manifestPath==='string' && typeof receipt.filesRoot==='string','Invalid build receipt');
      const artifacts=join(await realpath(binding.state),'artifacts');
      await safePath(receipt.manifestPath);await safePath(receipt.filesRoot);
      const manifestPath=await realpath(receipt.manifestPath),filesRoot=await realpath(receipt.filesRoot);
      const prefix=relative(artifacts,manifestPath).split(sep);
      requireThat(prefix.length===2 && /^[a-f0-9]{64}$/.test(prefix[0]) && prefix[1]==='manifest.json' && filesRoot===join(artifacts,prefix[0],'files'),'Build receipt escaped private artifact root',409);
      const manifest=validateBuild(await jsonFile(manifestPath,16*1024*1024));
      requireThat(manifest.project===assignment.project && manifest.generation===assignment.generation && manifest.sourceTree===assignment.build.sourceTree && manifest.recipe===assignment.build.recipe && await sha256(canonical(manifest))===receipt.artifactId,'Build output identity differs',409);
      check();const published=await publishBuild(this.client,assignment.build.candidate,manifestPath,filesRoot,signal);check();
      return {generation:assignment.generation,sourceTree:assignment.build.sourceTree,recipe:assignment.build.recipe,artifact:published.id,cacheHit:receipt.cacheHit};
    }finally{
      clearInterval(timer);signal.removeEventListener('abort',abort);await leaseWrites;
    }
  }
  private async run(assignment:BuildAssignment,recipe:string,source:string,root:string,state:string,lease:string,attemptDir:string,signal:AbortSignal,leaseError:()=>unknown,update:()=>Promise<void>,cancel:()=>void){
    const child=spawn(this.config.binary,['build','--recipe',recipe,'--source',source,'--root',root,'--state',state,'--lease',lease],{cwd:attemptDir,env:{PATH:'/usr/local/bin:/usr/bin:/bin',LANG:'C.UTF-8'},stdio:['ignore','pipe','pipe']});
    let closed=false;let exitCode:number|null=null;
    const completion = new Promise<string>((accept,reject)=>{
      let stdout='',stderr='',overflow=false,settled=false,cancelAt=0;
      const watchdog=setInterval(()=>{
        if(signal.aborted || assignment.expires<=Date.now() || leaseError() || overflow){
          void update();if(!cancelAt)cancelAt=Date.now();
          if(Date.now()-cancelAt>10_000){settled=true;clearInterval(watchdog);reject(new UncertainNativeRun(`Build supervisor did not confirm cancellation; inspect private attempt ${assignment.attempt} before releasing capacity`));}
        }
      },100);
      child.stdout.on('data',(value:Buffer)=>{if(stdout.length+value.length>65536){overflow=true;cancel();return;}stdout+=value.toString('utf8');});
      child.stderr.on('data',(value:Buffer)=>{if(stderr.length<65536)stderr+=value.toString('utf8').slice(0,65536-stderr.length);});
      child.once('error',error=>{clearInterval(watchdog);if(!settled){settled=true;reject(error);}});
      child.once('close',code=>{
        closed=true;exitCode=code;clearInterval(watchdog);if(settled)return;settled=true;
        if(code!==0 || overflow){reject(new Error(`Engine build failed (${code}): ${stderr.slice(-2048)}`));return;}
        accept(stdout);
      });
    });
    // Install exit/error observers before asynchronous journal IO. A failed
    // receipt write must still cancel and settle the already-started child.
    void completion.catch(()=>{});
    try {
      if(child.pid)await atomicJSON(join(attemptDir,'run.json'),{pid:child.pid,attempt:assignment.attempt,job:assignment.id,state:'running'});
      const output=await completion;
      await atomicJSON(join(attemptDir,'run.json'),{pid:child.pid,attempt:assignment.attempt,job:assignment.id,state:'exited',exitCode});
      return output;
    }catch(error){
      cancel();await update();
      try{await completion;}catch(settle){if(settle instanceof UncertainNativeRun)throw settle;}
      if(closed)await atomicJSON(join(attemptDir,'run.json'),{pid:child.pid??null,attempt:assignment.attempt,job:assignment.id,state:'exited',exitCode});
      throw error;
    }
  }
}

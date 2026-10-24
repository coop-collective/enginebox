import { spawn } from 'node:child_process';
import { mkdir, readFile, writeFile, rename } from 'node:fs/promises';
import { join, isAbsolute } from 'node:path';
import { canonical, identifier, requireThat, type Assignment } from '@enginebox/fleet-protocol';
import { UncertainNativeRun } from './native-assistant.ts';
import { Client } from './client.ts';
import { publishSnapshot } from './generations.ts';
export type IntegrationConfig = { binary:string; config:string; projects:Record<string,string> };
export async function executeIntegration(config:IntegrationConfig, assignment:Assignment, client:Client, state:string, signal:AbortSignal) {
  requireThat(assignment.kind==='integration','Integration assignment required');
  requireThat(isAbsolute(config.binary) && isAbsolute(config.config),'Provision absolute integration binary/config paths');
  requireThat(config.projects[assignment.project]===assignment.integration.layerfsProject,'No same-Store integration binding for this project');
  identifier(assignment.id);
  const dir=join(state,'integration',assignment.project,assignment.id);
  await mkdir(dir,{recursive:true,mode:0o700});
  const request=join(dir,'request.json');
  const payload=canonical({id:assignment.id,project:assignment.project,generation:assignment.generation,integration:assignment.integration});
  try { await writeFile(request,payload,{flag:'wx',mode:0o600}); }
  catch(error) { if((error as NodeJS.ErrnoException).code!=='EEXIST')throw error; requireThat(await readFile(request,'utf8')===payload,'Local integration request differs',409); }
  const stdout=await new Promise<string>((resolve,reject)=>{
    const child=spawn(config.binary,['--host','local','storage','layerfs','--config',config.config,'integrate','--request',request],{stdio:['ignore','pipe','pipe'],signal,env:{PATH:process.env.PATH,HOME:process.env.HOME}});
    let output='',stderr=''; let stopped=false;
    const timeout=setTimeout(()=>{stopped=true;child.kill('SIGTERM');},30*60_000);
    child.stdout.on('data',chunk=>{output+=chunk; if(output.length>1024*1024){stopped=true;child.kill('SIGTERM');}});
    child.stderr.on('data',chunk=>{stderr=(stderr+chunk).slice(-8192);});
    child.on('error',error=>reject(new UncertainNativeRun(`Integration launch interrupted: ${error.message}`))); child.on('close',code=>{clearTimeout(timeout); (code===0 || code===2)&&!stopped?resolve(output):reject(new UncertainNativeRun(`Integration executor failed or uncertain (${code}); inspect retained receipt: ${stderr}`));});
  });
  const raw=JSON.parse(stdout); const result=raw.result ?? raw;
  if(result.ok===false && result.conflict) throw new Error(`LayerFS merge conflict: ${JSON.stringify(result.conflict)}`);
  requireThat(result.ok!==false,"Integration executor refused output");
  for(const field of ['snapshot','sourceManifest','sourceRoot'])requireThat(typeof result[field]==='string'&&isAbsolute(result[field]),'Invalid integration export');
  const published=await publishSnapshot(client,assignment.project,result.snapshot,dir,assignment.generation,{layerfsRevision:result.layerfsRevision,projectName:assignment.integration.layerfsProject,layerfsRef:result.ref,layerfsRoot:result.root,treeHash:result.treeHash,integration:assignment.id});
  const source=JSON.parse(await readFile(result.sourceManifest,'utf8'));
  requireThat(source.project===assignment.project && source.treeHash===result.treeHash && source.layerfs.ref===result.ref && source.layerfs.root===result.root,'Export provenance differs');
  source.generation=published.id;
  await writeFile(result.sourceManifest+'.published',canonical(source),{mode:0o600});
  await rename(result.sourceManifest+'.published',result.sourceManifest);
  return {generation:published.id,ref:result.ref,root:result.root,treeHash:result.treeHash,sourceRoot:result.sourceRoot,sourceManifest:result.sourceManifest};
}

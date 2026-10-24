import assert from 'node:assert/strict';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { executeIntegration } from '../src/integration.ts';
import { UncertainNativeRun } from '../src/native-assistant.ts';
import { Client } from '../src/client.ts';
import type { Assignment } from '@enginebox/fleet-protocol';
const assignment:Assignment={id:'batch',project:'demo',generation:'0'.repeat(64),kind:'integration',integration:{id:'batch',layerfsProject:'game',baseRef:'32'+'1'.repeat(64),baseRoot:'2'.repeat(64),changes:[{id:'task',submission:'submission',generation:'3'.repeat(64),ref:'12'+'4'.repeat(64),root:'5'.repeat(64)}],taskRequirements:{}},node:'worker',epoch:1,session:'session',expires:Date.now()+30_000,attempt:'attempt'};
test('confirmed native conflicts fail normally; malformed or interrupted outcomes retain native capacity',{skip:process.platform==='win32'},async()=>{
 const directory=await mkdtemp(join(tmpdir(),'ebox-integration-'));
 try {
  for(const [name,script,uncertain] of [
   ['conflict',`process.stdout.write(JSON.stringify({ok:false,conflict:{code:'Conflict',message:'scene.gd changed on both sides'}})); process.exit(2);`,false],
   ['invalid',`process.stdout.write('not JSON');`,true],
   ['partial',`process.stdout.write(JSON.stringify({treeHash:'0'.repeat(64)}));`,true],
   ['interrupted',`process.exit(1);`,true],
  ] as const){
   const binary=join(directory,name);await writeFile(binary,`#!/usr/bin/env node\n${script}`,{mode:0o700});
   await assert.rejects(executeIntegration({binary,config:join(directory,'config'),projects:{demo:'game'}},assignment,new Client('http://127.0.0.1:1','unused'),join(directory,name+'-state'),new AbortController().signal),error=>{
    assert.equal(error instanceof UncertainNativeRun,uncertain);
    if(!uncertain)assert.match((error as Error).message,/scene.gd/);return true;
   });
  }
 } finally {await rm(directory,{recursive:true,force:true});}
});

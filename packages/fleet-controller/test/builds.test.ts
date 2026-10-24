import { expect,it } from 'vitest';
import { Builds } from '../src/builds.ts';
import { validateEngineBuild,validateChecks,validateNode } from '@enginebox/fleet-protocol';
function fixture(){
 const map=new Map<string,any>();const store={get:<T>(key:string)=>map.get(key) as T|undefined};
 const recipe='c'.repeat(64),generation='a'.repeat(64),tree='b'.repeat(64);
 const job=validateEngineBuild({id:'build-a',project:'demo',kind:'engine-build',generation,timeoutSeconds:30,build:{candidate:'integration-a',integration:'integration-a',sourceTree:tree,recipe},check:{candidate:'integration-a',name:'playable',recipe,scope:'full'}});
 map.set('job:integration-a',{spec:{id:'integration-a',kind:'integration',project:'demo'},status:'completed',assignment:{node:'worker'},result:JSON.stringify({generation,treeHash:tree})});
 map.set('candidate:integration-a',{spec:{id:'integration-a',generation}});
 map.set(`generation:${generation}`,{manifest:{project:'demo',metadata:{integration:'integration-a',treeHash:tree}}});
 const artifact='d'.repeat(64);const build={id:artifact,project:'demo',generation,sourceTree:tree,recipe,candidate:'integration-a',createdBy:'node:worker'};
 map.set(`artifact:${artifact}`,build);const report=JSON.stringify({generation,sourceTree:tree,recipe,artifact});
 return {map,job,build,report,checks:new Builds(store)};
}
it('build eligibility pins source custody and rejects missing or changed integrated provenance',()=>{
 const f=fixture();expect(f.checks.eligible(f.job,'worker')).toBe(true);expect(f.checks.eligible(f.job,'other')).toBe(false);expect(f.checks.eligible(f.job,undefined)).toBe(false);
 f.map.get(`generation:${f.job.generation}`).manifest.metadata.treeHash='f'.repeat(64);expect(()=>f.checks.source(f.job)).toThrow(/frozen integration/);
});
it('successful build evidence must be the exact published artifact and assigned executor',()=>{
 const f=fixture();expect(f.checks.complete(f.job,f.report,'worker')).toEqual(f.build);
 expect(()=>f.checks.complete(f.job,f.report,'other')).toThrow(/executor/);
 f.map.delete(`artifact:${f.build.id}`);expect(()=>f.checks.complete(f.job,f.report,'worker')).toThrow(/published artifact/);
 f.map.set(`artifact:${f.build.id}`,{...f.build,sourceTree:'f'.repeat(64)});expect(()=>f.checks.complete(f.job,f.report,'worker')).toThrow(/published artifact/);
 f.map.set(`artifact:${f.build.id}`,{...f.build,createdBy:'owner'});expect(()=>f.checks.complete(f.job,f.report,'worker')).toThrow(/published artifact/);
});
it('build capability and policy reject task-only credentials and arbitrary commands',()=>{
 const node={id:'worker',role:'agent',projects:['demo'],origin:null,slots:1,cacheBytes:16777216,buildProjects:['demo']};expect(validateNode(node,false).buildProjects).toEqual(['demo']);
 expect(()=>validateNode({...node,taskOnly:true},false)).toThrow(/Task-only/);
 expect(()=>validateNode({...node,buildProjects:['other']},false)).toThrow(/enrolled/);
 expect(validateChecks([{name:'playable',recipe:'a'.repeat(64),kind:'engine-build',timeoutSeconds:30}])[0].kind).toBe('engine-build');
 expect(()=>validateChecks([{name:'playable',recipe:'a'.repeat(64),kind:'engine-build',timeoutSeconds:30,argv:['arbitrary']}])).toThrow(/protected recipe/);
});

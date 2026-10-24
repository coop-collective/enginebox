import { env } from 'cloudflare:workers';
import { reset,runInDurableObject } from 'cloudflare:test';
import { afterEach,expect,it } from 'vitest';
import { canonical,sha256,validateTaskCreate } from '@enginebox/fleet-protocol';
afterEach(()=>reset());
it('replaying approval for an old candidate cannot ship a newer accepted task result',async()=>{
 const project=env.PROJECTS.getByName('approval-replay'),owner={id:'owner',owner:true};
 await project.create('approval-replay');
 const original=await project.createTask(validateTaskCreate({id:'task-one',title:'Tested feature',type:'idea'}),owner);
 const requirement=await sha256(canonical({title:original.title,description:original.description,type:original.type}));
 const oldGeneration='a'.repeat(64),newGeneration='b'.repeat(64);
 const oldCandidate={spec:{id:'candidate-old',project:'approval-replay',base:null,generation:oldGeneration,changes:[{id:'task-one',generation:oldGeneration}]},checks:[],fullJobs:{},taskRequirements:{'task-one':requirement}};
 const newCandidate={...oldCandidate,spec:{...oldCandidate.spec,id:'candidate-new',generation:newGeneration,changes:[{id:'task-one',generation:newGeneration}]}};
 // Seed frozen candidate/acceptance fixtures. This test isolates approval
 // idempotency; native builds and full validation have separate qualification.
 await runInDurableObject(project,instance=>{
  instance.put(`generation:${oldGeneration}`,{id:oldGeneration});
  instance.put(`generation:${newGeneration}`,{id:newGeneration});
  instance.put('candidate:candidate-old',oldCandidate);
  instance.put('candidate:candidate-new',newCandidate);
 });
 const testing=await project.moveTask('task-one',{operationId:'test-old',expectedRevision:original.revision,status:'testing',generation:oldGeneration,candidate:'candidate-old'},owner);
 const approval={operationId:'approve-old',expectedRevision:testing.revision};
 const reviewed=await project.approveTask('task-one',approval,owner);
 expect(reviewed.reviewReceipt?.candidate).toBe('candidate-old');
 const working=await project.moveTask('task-one',{operationId:'rework',expectedRevision:reviewed.revision,status:'in_progress'},owner);
 const latest=await project.moveTask('task-one',{operationId:'test-new',expectedRevision:working.revision,status:'testing',generation:newGeneration,candidate:'candidate-new'},owner);
 await runInDurableObject(project,instance=>instance.put(`acceptance:${newGeneration}`,{id:'new-decision',generation:newGeneration,validation:{candidate:newCandidate.spec,taskRequirements:newCandidate.taskRequirements}}));
 expect(latest.reviewReceipt).toBeUndefined();
 await project.approveTask('task-one',approval,owner);
 const after=await project.task('task-one');
 expect(after?.candidate).toBe('candidate-new');
 expect(after?.status).toBe('testing');
 expect(after?.reviewReceipt).toBeUndefined();
 expect(after?.revision).toBe(latest.revision);
});

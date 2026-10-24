import { canonical, digest, identifier, requireThat, validateIntegration, type ChangeSubmission, type IntegrationJob, type CheckRequirement, type Generation } from '@enginebox/fleet-protocol';
import { Tasks } from './tasks.ts';
type Store = { get<T>(key:string): T|undefined; put(key:string,value:unknown):void };
type Submission = { spec:ChangeSubmission; taskRequirements:Record<string,string> };
type Frozen = { job:IntegrationJob; checks:CheckRequirement[] };
export class Integrations {
  constructor(private store:Store, private tasks:Tasks) {}
  submit(spec:ChangeSubmission) {
    const previous=this.store.get<Submission>(`submission:${spec.id}`);
    if(previous) { requireThat(canonical(previous.spec)===canonical(spec),'Submission ID already has another specification',409); return previous; }
    requireThat(this.store.get('head')===spec.base,'Submission must name the accepted base',409);
    for(const [id,ref,root] of [[spec.base,spec.baseRef,spec.baseRoot],[spec.generation,spec.ref,spec.root]]) {
      const generation=this.store.get<{manifest:Generation}>(`generation:${id}`)?.manifest;
      requireThat(generation && generation.metadata.projectName===spec.layerfsProject && generation.metadata.layerfsRef===ref && generation.metadata.layerfsRoot===root,'Published generation does not bind this LayerFS source',409);
      if(id===spec.generation) requireThat(generation.parent===spec.base,'Change generation has another base',409);
    }
    const taskRequirements=this.tasks.snapshotRequirements({id:spec.id,project:spec.project,base:spec.base,generation:spec.generation,changes:[{id:spec.task??spec.id,generation:spec.generation}]});
    const result={spec,taskRequirements}; this.store.put(`submission:${spec.id}`,result); return result;
  }
  freeze(id:string, changeIds:string[]) {
    identifier(id); requireThat(changeIds.length>0 && changeIds.length<=128 && new Set(changeIds).size===changeIds.length,'Select 1–128 distinct submissions');
    for(const change of changeIds) identifier(change);
    const prior=this.store.get<Frozen>(`integration:${id}`);
    if(prior) { requireThat(canonical(prior.job.integration.changes.map(c=>c.submission))===canonical(changeIds),'Integration ID already frozen with different changes',409); return prior.job; }
    const submissions=changeIds.map(change=>{ const row=this.store.get<Submission>(`submission:${change}`); requireThat(row,'Unknown submission',404); return row; });
    const first=submissions[0].spec;
    requireThat(this.store.get('head')===first.base,'Accepted base changed; submit rebased results',409);
    const taskRequirements:Record<string,string>={};
    for(const row of submissions) {
      requireThat(row.spec.base===first.base && row.spec.layerfsProject===first.layerfsProject && row.spec.baseRef===first.baseRef && row.spec.baseRoot===first.baseRoot,'Submissions must share one exact base and LayerFS project',409);
      this.tasks.validateRequirements(row.taskRequirements); Object.assign(taskRequirements,row.taskRequirements);
    }
    const checks=this.store.get<CheckRequirement[]>('checks')??[];
    requireThat(checks.length>0,'Configure protected checks before integration',409);
    const job:IntegrationJob={id,project:first.project,generation:first.base,kind:'integration',integration:validateIntegration({id,layerfsProject:first.layerfsProject,baseRef:first.baseRef,baseRoot:first.baseRoot,changes:submissions.map(({spec})=>({id:spec.task??spec.id,submission:spec.id,generation:spec.generation,ref:spec.ref,root:spec.root})),taskRequirements})};
    requireThat(!this.store.get(`candidate:${id}`),'Candidate ID already exists',409);
    this.store.put(`integration:${id}`,{job,checks}); return job;
  }
  validate(job:IntegrationJob) { const frozen=this.store.get<Frozen>(`integration:${job.id}`); requireThat(frozen && canonical(frozen.job)===canonical(job),'Integration must use an owner-frozen submission batch',409); }
  complete(job:IntegrationJob, result:string) {
    this.validate(job); const frozen=this.store.get<Frozen>(`integration:${job.id}`)!;
    const report=JSON.parse(result); digest(report.generation); digest(report.treeHash);
    const manifest=this.store.get<{manifest:Generation}>(`generation:${report.generation}`)?.manifest;
    requireThat(manifest && manifest.parent===job.generation && manifest.metadata.integration===job.id && manifest.metadata.treeHash===report.treeHash && manifest.metadata.layerfsRef===report.ref && manifest.metadata.layerfsRoot===report.root && manifest.metadata.projectName===job.integration.layerfsProject,'Integration output generation is not published with exact provenance',409);
    requireThat(this.store.get('head')===job.generation,'Accepted base changed during integration',409);
    requireThat(canonical(this.store.get('checks')??[])===canonical(frozen.checks),'Protected checks changed during integration',409);
    this.tasks.validateRequirements(job.integration.taskRequirements);
    const candidate={spec:{id:job.id,project:job.project,base:job.generation,generation:report.generation,changes:job.integration.changes.map(({id,generation})=>({id,generation}))},checks:frozen.checks,fullJobs:{},taskRequirements:job.integration.taskRequirements};
    requireThat(!this.store.get(`candidate:${job.id}`),'Candidate ID already exists',409);
    this.store.put(`candidate:${job.id}`,candidate);
  }
}

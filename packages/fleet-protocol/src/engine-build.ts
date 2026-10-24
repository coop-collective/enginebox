import { digest, exactKeys, identifier, integer, record, requireThat } from './index.ts';
export type EngineBuildJob = {
  id:string; project:string; kind:'engine-build'; generation:string; timeoutSeconds:number;
  build:{candidate:string; integration:string; sourceTree:string; recipe:string};
  check:{candidate:string; name:string; recipe:string; scope:'full'};
  argv?:never;
};
export function validateEngineBuild(value:unknown):EngineBuildJob {
  record(value);exactKeys(value,['id','project','kind','generation','timeoutSeconds','build','check']);
  identifier(value.id);identifier(value.project);digest(value.generation);
  requireThat(value.kind==='engine-build','Engine build job required');integer(value.timeoutSeconds,1,7200);
  record(value.build);exactKeys(value.build,['candidate','integration','sourceTree','recipe']);
  identifier(value.build.candidate);identifier(value.build.integration);digest(value.build.sourceTree);digest(value.build.recipe);
  record(value.check);exactKeys(value.check,['candidate','name','recipe','scope']);
  identifier(value.check.name);
  requireThat(value.check.candidate===value.build.candidate && value.check.recipe===value.build.recipe && value.check.scope==='full','Build must be a full exact candidate check');
  return {id:value.id,project:value.project,kind:'engine-build',generation:value.generation,timeoutSeconds:value.timeoutSeconds,build:{candidate:value.build.candidate,integration:value.build.integration,sourceTree:value.build.sourceTree,recipe:value.build.recipe},check:{candidate:value.build.candidate,name:value.check.name,recipe:value.build.recipe,scope:'full'}};
}

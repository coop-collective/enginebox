import { digest, identifier, record, requireThat } from './index.ts';

/** Generic, owner-approved mapping; the GitHub adapter does not own this catalog. */
export type Preparation = {
  schema: 1;
  project: string;
  key: string;
  generation: string;
  recipe: 'pnpm-installed-v1';
  namespace: string;
  repository: string;
  trust: 'repository-read';
  image: string;
  pnpm: string;
  scripts: 'deny';
  credentialFree: true;
  projectRecord: string;
};
export function validatePreparation(value: unknown): Preparation {
  record(value);
  const fields = ['schema', 'project', 'key', 'generation', 'recipe', 'namespace', 'repository', 'trust', 'image', 'pnpm', 'scripts', 'credentialFree', 'projectRecord'];
  requireThat(Object.keys(value).length === fields.length && Object.keys(value).every(k => fields.includes(k)), 'Unexpected preparation fields');
  requireThat(value.schema === 1 && value.recipe === 'pnpm-installed-v1' && value.scripts === 'deny' && value.trust === 'repository-read' && value.credentialFree === true, 'Unsupported preparation contract');
  identifier(value.project); digest(value.key); digest(value.generation); digest(value.image);
  for (const key of ['namespace', 'repository']) requireThat(typeof value[key] === 'string' && /^[a-zA-Z0-9_.:/-]{1,160}$/.test(value[key]), 'Invalid preparation scope');
  requireThat(typeof value.pnpm === 'string' && /^\d+\.\d+\.\d+$/.test(value.pnpm), 'Pin pnpm version');
  requireThat(typeof value.projectRecord === 'string' && value.projectRecord.length > 0 && value.projectRecord.length <= 87380 && /^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(value.projectRecord), 'Invalid bounded project discovery record');
  return value as Preparation;
}

// Operator-only control: credentials stay in a private config, never argv.
import { readFile } from 'node:fs/promises';
import assert from 'node:assert/strict';
const [configFile, command, project, ...args]=process.argv.slice(2);
assert(configFile && project && ['submit','queue','status'].includes(command),'Usage: node integrate.mjs PRIVATE_CONFIG submit PROJECT SUBMISSION_JSON | queue PROJECT ID CHANGE... | status PROJECT ID');
assert(/^[a-z0-9][a-z0-9-]{0,47}$/.test(project),'Invalid project');
const config=JSON.parse(await readFile(configFile,'utf8'));
const endpoint=new URL(config.controller);
assert(['http:','https:'].includes(endpoint.protocol)&&!endpoint.username&&!endpoint.password&&!endpoint.search&&!endpoint.hash&&endpoint.pathname==='/','Invalid controller URL');
let body,path;
if(command==='submit') { assert(args.length===1); body=JSON.parse(await readFile(args[0],'utf8')); assert(body.project===project); path='submissions'; }
else if(command==='queue') { assert(args.length>1); body={id:args[0],changes:args.slice(1)}; path='integrations'; }
else { assert(args.length===1&&/^[a-z0-9][a-z0-9-]{0,47}$/.test(args[0])); path=`jobs/${args[0]}`; }
const response=await fetch(`${endpoint.origin}/v1/projects/${project}/${path}`,{method:body?'POST':'GET',headers:{authorization:`Bearer ${config.token}`,'content-type':'application/json'},...(body?{body:JSON.stringify(body)}:{})});
const result=await response.json();
assert(response.ok,JSON.stringify(result)); console.log(JSON.stringify(result,null,2));

import { env } from "cloudflare:workers";
import { reset, runInDurableObject, evictDurableObject } from "cloudflare:test";
import { afterEach, expect, it } from "vitest";
import worker from "../src/index.ts";
import { sha256 } from "@enginebox/fleet-protocol";

const admin="local-tests-only-0123456789abcdef0123456789";
const spec={id:"native-fixture",role:"native",projects:["demo"],origin:null,slots:1,cacheBytes:16777216};
async function call(path:string, body:unknown, token=admin, extra:Record<string,string>={}) {
  return worker.fetch(new Request(`https://fleet.test${path}`,{method:"POST",headers:{authorization:`Bearer ${token}`,...extra},body:JSON.stringify(body)}),env as Env);
}
async function invite() {
  expect((await call("/v1/projects",{id:"demo"})).status).toBe(201);
  const response=await call("/v1/node-enrollments",spec);
  expect(response.status).toBe(201);
  return await response.json() as {token:string;expires:number};
}
afterEach(reset);
it("consumes enrollment for one identity; retries survive eviction and remain scoped",async()=>{
  const invitation=await invite();
  const token=`native-fixture.${"a".repeat(64)}`;
  const body={nodeSecretHash:await sha256(token)};
  expect((await call("/v1/node/enroll",body,invitation.token)).status).toBe(200);
  await evictDurableObject(env.NODES.getByName(spec.id));
  expect((await call("/v1/node/enroll",body,invitation.token)).status).toBe(200);
  expect((await call("/v1/node/enroll",{nodeSecretHash:"b".repeat(64)},invitation.token)).status).toBe(409);
  expect((await call("/v1/projects",{id:"forbidden"},token)).status).toBe(403);
  expect((await call("/v1/node/claim",{},token)).status).toBe(403);
  const session=crypto.randomUUID();
  const hello=await (await call("/v1/node/hello",{session},token)).json() as {epoch:number};
  expect((await call("/v1/node/heartbeat",{stats:{hardwareEncodePassed:1}},token,{"x-enginebox-session":session,"x-enginebox-epoch":String(hello.epoch)})).status).toBe(200);
  expect((await call(`/v1/nodes/${spec.id}/revoke`,{})).status).toBe(200);
  expect((await call("/v1/node/hello",{session},token)).status).toBe(401);
  expect((await call("/v1/node/enroll",body,invitation.token)).status).toBe(409);
});
it("requires owner-issued unexpired credentials and prevents competing claims",async()=>{
  const invitation=await invite();
  expect((await call("/v1/node/enroll",{nodeSecretHash:"a".repeat(64)},`native-fixture.enroll.${"0".repeat(64)}`)).status).toBe(401);
  const responses=await Promise.all(["a","b"].map(c=>call("/v1/node/enroll",{nodeSecretHash:c.repeat(64)},invitation.token)));
  expect(responses.map(r=>r.status).sort()).toEqual([200,409]);
});
it("expires pending credentials and rejects executor privilege escalation",async()=>{
  const invitation=await invite();
  await runInDurableObject(env.NODES.getByName(spec.id),async(instance)=>{
    const pending=instance.get<{expires:number}>("enrollment")!;
    pending.expires=Date.now()-1; instance.put("enrollment",pending);
  });
  expect((await call("/v1/node/enroll",{nodeSecretHash:"a".repeat(64)},invitation.token)).status).toBe(401);
  expect((await call("/v1/node-enrollments",{...spec,id:"wrong",role:"agent"})).status).toBe(400);
  expect((await call("/v1/node-enrollments",{...spec,id:"wrong",assistantProjects:["demo"]})).status).toBe(400);
});

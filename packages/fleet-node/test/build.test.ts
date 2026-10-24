import { test } from "node:test";
import assert from "node:assert/strict";
import {
  mkdtemp,
  mkdir,
  writeFile,
  readFile,
  realpath,
  rm,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createHash } from "node:crypto";
import { createServer } from "node:http";
import {
  canonical,
  sha256,
  validateEngineBuild,
  type Assignment,
} from "@enginebox/fleet-protocol";
import { EngineBuilds, protectedRecipe } from "../src/build.ts";
import { Client } from "../src/client.ts";
const hash = (text: string) => createHash("sha256").update(text).digest("hex");
async function fixture(t: any, mode = "build") {
  const root = await realpath(
    await mkdtemp(join(tmpdir(), "ebox-build-node-")),
  );
  t.after(() => rm(root, { recursive: true, force: true }));
  const integrationRoot = join(root, "integrations"),
    source = join(integrationRoot, "integration-a");
  await mkdir(join(source, "source"), { recursive: true, mode: 0o700 });
  const generation = "a".repeat(64),
    tree = "b".repeat(64);
  await writeFile(
    join(source, "source.json"),
    canonical({
      schema: 1,
      project: "demo",
      generation,
      treeHash: tree,
      layerfs: { ref: "frozen-ref", root: "frozen-root" },
      entries: [],
    }),
  );
  const recipePath = join(root, "recipe.json");
  await writeFile(
    recipePath,
    canonical({
      schema: 1,
      engine: "godot",
      version: "test",
      target: "web",
      architecture: "wasm32",
      executable: "/tool/godot",
      executableSha256: "c".repeat(64),
      entrypoint: "index.html",
      timeoutSeconds: 30,
    }),
  );
  const recipe = (await protectedRecipe(recipePath)).hash;
  const binary = join(root, "enginebox-build");
  const script = `#!${process.execPath}
import fs from 'node:fs';import path from 'node:path';
const value=k=>process.argv[process.argv.indexOf(k)+1];
if(process.env.ENGINEBOX_BUILD_TEST_SECRET)process.exit(99);
const lease=value('--lease');
if(${JSON.stringify(mode)}==='cancel'){setInterval(()=>{if(JSON.parse(fs.readFileSync(lease)).cancelled)process.exit(7)},20)}
else{
const state=value('--state'),base=path.join(state,'artifacts','d'.repeat(64)),files=path.join(base,'files');fs.mkdirSync(files,{recursive:true});fs.writeFileSync(path.join(files,'index.html'),'play');
const manifest={schema:1,kind:'engine-build',project:'demo',generation:'${generation}',sourceTree:'${tree}',recipe:'${recipe}',toolchain:{engine:'godot',version:'test',target:'web',architecture:'wasm32',fingerprint:'e'.repeat(64)},entrypoint:'index.html',files:[{path:'index.html',sha256:'${hash("play")}',bytes:4,executable:false}],totalBytes:4};
const canonical=v=>Array.isArray(v)?'['+v.map(canonical).join(',')+']':v&&typeof v==='object'?'{'+Object.keys(v).sort().map(k=>JSON.stringify(k)+':'+canonical(v[k])).join(',')+'}':JSON.stringify(v);
const crypto=await import('node:crypto');const text=canonical(manifest);fs.writeFileSync(path.join(base,'manifest.json'),text);
console.log(JSON.stringify({artifactId:crypto.createHash('sha256').update(text).digest('hex'),manifestPath:path.join(base,'manifest.json'),filesRoot:${JSON.stringify(mode)}==='escape'?'/etc':files,cacheHit:false}));}
`;
  await writeFile(binary, script, { mode: 0o700 });
  const uploads: string[] = [];
  const server = createServer(async (req, res) => {
    let body = "";
    for await (const chunk of req) body += chunk;
    const url = req.url!;
    if (url.endsWith("/jobs/integration-a")) {
      res.setHeader("content-type", "application/json");
      res.end(
        JSON.stringify({
          status: "completed",
          spec: { kind: "integration" },
          assignment: { node: "worker" },
          result: canonical({
            generation,
            treeHash: tree,
            ref: "frozen-ref",
            root: "frozen-root",
            sourceRoot: "/etc/ignored",
          }),
        }),
      );
      return;
    }
    if (req.method === "HEAD") {
      res.writeHead(404).end();
      return;
    }
    uploads.push(url);
    if (url.endsWith("/artifacts")) {
      const { manifest, candidate } = JSON.parse(body);
      res.setHeader("content-type", "application/json");
      res.end(
        JSON.stringify({
          ...manifest,
          id: await sha256(canonical(manifest)),
          candidate,
          createdBy: "node:worker",
          createdAt: 1,
          fileCount: manifest.files.length,
        }),
      );
      return;
    }
    res.setHeader("content-type", "application/json");
    res.end("{}");
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  t.after(() => new Promise<void>((resolve) => server.close(() => resolve())));
  const address = server.address() as { port: number };
  const client = new Client(
    `http://127.0.0.1:${address.port}`,
    "fixture-private-token",
  );
  const config = {
    localTrusted: true as const,
    binary,
    binarySha256: hash(script),
    bindings: {
      demo: {
        integrationRoot,
        state: join(root, "state"),
        recipes: { [recipe]: recipePath },
      },
    },
  };
  const assignment = {
    ...validateEngineBuild({
      id: "build-a",
      project: "demo",
      kind: "engine-build",
      generation,
      timeoutSeconds: 30,
      build: {
        candidate: "integration-a",
        integration: "integration-a",
        sourceTree: tree,
        recipe,
      },
      check: {
        candidate: "integration-a",
        name: "playable",
        recipe,
        scope: "full",
      },
    }),
    node: "worker",
    attempt: "attempt-a",
    session: "session",
    epoch: 1,
    expires: Date.now() + 60_000,
  } as Assignment;
  return { config, client, assignment, root, uploads, source, recipePath };
}
test(
  "build adapter binds protected source and publishes actual CLI artifact with no inherited credentials",
  { skip: process.platform === "win32" },
  async (t) => {
    const f = await fixture(t);
    const previous = process.env.ENGINEBOX_BUILD_TEST_SECRET;
    process.env.ENGINEBOX_BUILD_TEST_SECRET = "must-not-enter-builder";
    t.after(() => {
      if (previous === undefined)
        delete process.env.ENGINEBOX_BUILD_TEST_SECRET;
      else process.env.ENGINEBOX_BUILD_TEST_SECRET = previous;
    });
    const outcome = await new EngineBuilds(f.config, f.client).execute(
      f.assignment,
      new AbortController().signal,
    );
    assert.match(String(outcome.artifact), /^[a-f0-9]{64}$/);
    assert.equal(outcome.generation, f.assignment.generation);
    assert(f.uploads.includes("/v1/projects/demo/artifacts"));
  },
);
test(
  "source mismatch and forged output paths publish nothing",
  { skip: process.platform === "win32" },
  async (t) => {
    const f = await fixture(t, "escape");
    await assert.rejects(
      () =>
        new EngineBuilds(f.config, f.client).execute(
          f.assignment,
          new AbortController().signal,
        ),
      /escaped|symbolic links/,
    );
    assert.equal(f.uploads.length, 0);
    const source = JSON.parse(
      await readFile(join(f.source, "source.json"), "utf8"),
    );
    source.generation = "f".repeat(64);
    await writeFile(join(f.source, "source.json"), JSON.stringify(source));
    await assert.rejects(
      () =>
        new EngineBuilds(f.config, f.client).execute(
          f.assignment,
          new AbortController().signal,
        ),
      /provenance/,
    );
    assert.equal(f.uploads.length, 0);
  },
);
test(
  "lease cancellation is confirmed by CLI exit before returning and cannot publish",
  { skip: process.platform === "win32" },
  async (t) => {
    const f = await fixture(t, "cancel"),
      cancel = new AbortController();
    const promise = new EngineBuilds(f.config, f.client).execute(
      f.assignment,
      cancel.signal,
    );
    setTimeout(() => cancel.abort(), 300);
    await assert.rejects(() => promise, /failed|expired/);
    assert.equal(f.uploads.length, 0);
    const lease = JSON.parse(
      await readFile(
        join(f.config.bindings.demo.state, "attempts/attempt-a/lease.json"),
        "utf8",
      ),
    );
    assert.equal(lease.cancelled, true);
  },
);
test(
  "untrusted local mode, recipe drift, and arbitrary job argv are rejected",
  { skip: process.platform === "win32" },
  async (t) => {
    const f = await fixture(t);
    assert.throws(
      () =>
        new EngineBuilds({ ...f.config, localTrusted: false as any }, f.client),
      /opt-in/,
    );
    const recipe = JSON.parse(await readFile(f.recipePath, "utf8"));
    recipe.timeoutSeconds = 10;
    await writeFile(f.recipePath, JSON.stringify(recipe));
    await assert.rejects(
      () =>
        new EngineBuilds(f.config, f.client).execute(
          f.assignment,
          new AbortController().signal,
        ),
      /recipe/,
    );
    assert.equal(f.uploads.length, 0);
    const { node, attempt, session, epoch, expires, ...spec } = f.assignment;
    assert.throws(
      () => validateEngineBuild({ ...spec, argv: ["curl", "private-url"] }),
      /Unknown field/,
    );
  },
);

test(
  "Windows native build execution requires the qualified Linux container",
  { skip: process.platform !== "win32" },
  () => {
    assert.throws(
      () =>
        new EngineBuilds(
          {
            localTrusted: true,
            binary: process.execPath,
            binarySha256: "0".repeat(64),
            bindings: {},
          },
          new Client("http://localhost", "test"),
        ),
      /qualified Linux build worker/,
    );
  },
);

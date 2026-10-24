import { constants } from "node:fs";
import { open, lstat, realpath } from "node:fs/promises";
import { createHash } from "node:crypto";
import { join, resolve, sep } from "node:path";
import {
  ARTIFACT_PAGE_FILES,
  canonical,
  MAX_ARTIFACT_JSON,
  MAX_JSON_BYTES,
  MAX_CHUNK_BYTES,
  MAX_PAGES,
  readBounded,
  CHUNK_BYTES,
  PAGE_CHUNKS,
  relativeFile,
  requireThat,
  sha256,
  validateBuild,
  type BuildRecord,
  type FileObject,
  type IndexPage,
  type Chunk,
} from "@enginebox/fleet-protocol";
import { Client } from "./client.ts";

async function postJSON<T>(client:Client,path:string,value:unknown,signal?:AbortSignal):Promise<T> {
  requireThat(!signal?.aborted,"Artifact publication cancelled",409);
  const response=await client.request(path,{method:"POST",headers:{"content-type":"application/json"},body:canonical(value),signal});
  const bytes=await readBounded(response.body,MAX_JSON_BYTES);
  requireThat(!signal?.aborted,"Artifact publication cancelled",409);
  return JSON.parse(new TextDecoder().decode(bytes)) as T;
}
async function readManifest(path:string) {
  const before=await lstat(path);
  requireThat(before.isFile() && !before.isSymbolicLink() && before.size<=MAX_ARTIFACT_JSON,"Build manifest must be a bounded regular file");
  const handle=await open(path,constants.O_RDONLY|(constants.O_NOFOLLOW??0));
  try {
    const info=await handle.stat();requireThat(info.isFile() && info.ino===before.ino && info.dev===before.dev && info.size<=MAX_ARTIFACT_JSON,"Build manifest changed while opening",409);
    const buffer=Buffer.alloc(MAX_ARTIFACT_JSON+1);let size=0;
    while(size<buffer.length){const part=await handle.read(buffer,size,buffer.length-size,null);if(!part.bytesRead)break;size+=part.bytesRead;}
    const after=await handle.stat();requireThat(size<=MAX_ARTIFACT_JSON && after.size===before.size && after.mtimeMs===before.mtimeMs,"Build manifest changed or exceeds metadata limit",409);
    return validateBuild(JSON.parse(buffer.subarray(0,size).toString('utf8')));
  }finally{await handle.close();}
}

export async function publishFile(
  client: Client,
  project: string,
  path: string,
  expected?: { sha256: string; bytes: number },
  signal?: AbortSignal,
): Promise<FileObject> {
  const before = await lstat(path);
  requireThat(
    before.isFile() && !before.isSymbolicLink(),
    "Artifact output must be a regular file",
  );
  requireThat(before.size<=MAX_CHUNK_BYTES*PAGE_CHUNKS*MAX_PAGES && (!expected || before.size===expected.bytes),"Artifact file size differs or exceeds limit",409);
  const file = await open(path, constants.O_RDONLY|(constants.O_NOFOLLOW??0)),
    hash = createHash("sha256");
  const pages: IndexPage[] = [];
  let chunks: Chunk[] = [],
    position = 0;
  function alive() {
    requireThat(!signal?.aborted, "Artifact publication cancelled", 409);
  }
  async function upload(kind: string, digest: string, bytes: Uint8Array) {
    alive();
    const url = `/v1/data/${project}/${kind}/${digest}`;
    const prior = await client.request(url, { method: "HEAD", signal });
    if (prior.status === 404)
      await client.request(url, {
        method: "PUT",
        body: bytes as BodyInit,
        signal,
      });
    else
      requireThat(
        Number(prior.headers.get("content-length")) === bytes.length,
        "Stored artifact object size differs",
        409,
      );
  }
  async function page() {
    if (!chunks.length) return;
    const bytes = new TextEncoder().encode(canonical(chunks)),
      digest = await sha256(bytes);
    await upload("pages", digest, bytes);
    pages.push({
      hash: digest,
      bytes: bytes.length,
      count: chunks.length,
      contentBytes: chunks.reduce((sum, chunk) => sum + chunk.bytes, 0),
    });
    chunks = [];
  }
  try {
    const actual = await file.stat();
    requireThat(
      actual.dev === before.dev && actual.ino === before.ino,
      "Artifact changed while opening",
      409,
    );
    while (position < before.size) {
      alive();
      const bytes = Buffer.alloc(Math.min(CHUNK_BYTES, before.size - position));
      let filled = 0;
      while (filled < bytes.length) {
        const read = await file.read(
          bytes,
          filled,
          bytes.length - filled,
          position + filled,
        );
        requireThat(read.bytesRead > 0, "Artifact was truncated", 409);
        filled += read.bytesRead;
      }
      hash.update(bytes);
      const digest = createHash("sha256").update(bytes).digest("hex");
      await upload("chunks", digest, bytes);
      chunks.push({ hash: digest, bytes: bytes.length });
      position += bytes.length;
      if (chunks.length === PAGE_CHUNKS) await page();
    }
    await page();
    alive();
    const after = await file.stat();
    requireThat(
      after.size === before.size && after.mtimeMs === before.mtimeMs,
      "Artifact changed during publication",
      409,
    );
  } finally {
    await file.close();
  }
  const descriptor = { sha256: hash.digest("hex"), bytes: position, pages };
  requireThat(
    !expected ||
      (expected.sha256 === descriptor.sha256 &&
        expected.bytes === descriptor.bytes),
    "Build output digest differs",
    409,
  );
  alive();
  await postJSON(client,`/v1/projects/${project}/artifact-files/${descriptor.sha256}`,descriptor,signal);
  return descriptor;
}
export async function publishBuild(
  client: Client,
  candidate: string,
  manifestPath: string,
  filesRoot: string,
  signal?: AbortSignal,
): Promise<BuildRecord> {
  const manifest=await readManifest(manifestPath);
  const root = await realpath(filesRoot);
  for (const file of manifest.files) {
    relativeFile(file.path);
    const path = resolve(root, ...file.path.split("/"));
    requireThat(path.startsWith(root + sep), "Artifact path escaped its root");
    let current = root;
    for (const component of file.path.split("/")) {
      current = join(current, component);
      requireThat(
        !(await lstat(current)).isSymbolicLink(),
        "Artifact path contains a symbolic link",
      );
    }
    await publishFile(client, manifest.project, path, file, signal);
  }
  for (let i = 0; i < manifest.files.length; i += ARTIFACT_PAGE_FILES) {
    requireThat(!signal?.aborted, "Build publication cancelled", 409);
    const page = manifest.files.slice(i, i + ARTIFACT_PAGE_FILES);
    await postJSON(client,`/v1/projects/${manifest.project}/artifact-file-pages/${await sha256(canonical(page))}`,page,signal);
  }
  requireThat(!signal?.aborted, "Build publication cancelled", 409);
  const result = await postJSON<BuildRecord>(client,`/v1/projects/${manifest.project}/artifacts`,{candidate,manifest},signal);
  requireThat(
    result.id === (await sha256(canonical(manifest))),
    "Published build identity differs",
    502,
  );
  return result;
}

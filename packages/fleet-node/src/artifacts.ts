import { open, lstat, realpath, readFile } from "node:fs/promises";
import { createHash } from "node:crypto";
import { join, resolve, sep } from "node:path";
import { ARTIFACT_PAGE_FILES, canonical, MAX_ARTIFACT_JSON, CHUNK_BYTES, PAGE_CHUNKS, relativeFile, requireThat, sha256, validateBuild, type BuildRecord, type FileObject, type IndexPage, type Chunk } from "@enginebox/fleet-protocol";
import { Client } from "./client.ts";

export async function publishFile(client: Client, project: string, path: string, expected?: {sha256: string; bytes: number}, signal?: AbortSignal): Promise<FileObject> {
  const before = await lstat(path);
  requireThat(before.isFile() && !before.isSymbolicLink(), "Artifact output must be a regular file");
  const file = await open(path, "r"), hash = createHash("sha256");
  const pages: IndexPage[] = []; let chunks: Chunk[] = [], position = 0;
  function alive() { requireThat(!signal?.aborted, "Artifact publication cancelled", 409); }
  async function upload(kind: string, digest: string, bytes: Uint8Array) {
    alive(); const url = `/v1/data/${project}/${kind}/${digest}`;
    const prior = await client.request(url,{method:"HEAD",signal});
    if (prior.status === 404) await client.request(url,{method:"PUT",body:bytes as BodyInit,signal});
    else requireThat(Number(prior.headers.get("content-length")) === bytes.length,"Stored artifact object size differs",409);
  }
  async function page() {
    if (!chunks.length) return;
    const bytes = new TextEncoder().encode(canonical(chunks)), digest = await sha256(bytes);
    await upload("pages",digest,bytes);
    pages.push({hash:digest,bytes:bytes.length,count:chunks.length,contentBytes:chunks.reduce((sum,chunk)=>sum+chunk.bytes,0)});
    chunks = [];
  }
  try {
    const actual = await file.stat(); requireThat(actual.dev === before.dev && actual.ino === before.ino,"Artifact changed while opening",409);
    while (position < before.size) {
      alive(); const bytes = Buffer.alloc(Math.min(CHUNK_BYTES,before.size-position)); let filled = 0;
      while (filled < bytes.length) {
        const read = await file.read(bytes,filled,bytes.length-filled,position+filled);
        requireThat(read.bytesRead > 0,"Artifact was truncated",409); filled += read.bytesRead;
      }
      hash.update(bytes); const digest = createHash("sha256").update(bytes).digest("hex");
      await upload("chunks",digest,bytes); chunks.push({hash:digest,bytes:bytes.length}); position += bytes.length;
      if (chunks.length === PAGE_CHUNKS) await page();
    }
    await page(); alive();
    const after = await file.stat(); requireThat(after.size === before.size && after.mtimeMs === before.mtimeMs,"Artifact changed during publication",409);
  } finally { await file.close(); }
  const descriptor = {sha256:hash.digest("hex"),bytes:position,pages};
  requireThat(!expected || expected.sha256 === descriptor.sha256 && expected.bytes === descriptor.bytes,"Build output digest differs",409);
  alive(); await client.json(`/v1/projects/${project}/artifact-files/${descriptor.sha256}`,descriptor);
  return descriptor;
}
export async function publishBuild(client: Client, candidate: string, manifestPath: string, filesRoot: string, signal?: AbortSignal): Promise<BuildRecord> {
  requireThat((await lstat(manifestPath)).size <= MAX_ARTIFACT_JSON, "Build manifest exceeds metadata limit");
  const manifest = validateBuild(JSON.parse(await readFile(manifestPath,"utf8")));
  const root = await realpath(filesRoot);
  for (const file of manifest.files) {
    relativeFile(file.path);
    const path = resolve(root,...file.path.split("/"));
    requireThat(path.startsWith(root + sep),"Artifact path escaped its root");
    let current = root;
    for (const component of file.path.split("/")) {
      current = join(current,component); requireThat(!(await lstat(current)).isSymbolicLink(),"Artifact path contains a symbolic link");
    }
    await publishFile(client,manifest.project,path,file,signal);
  }
  for (let i = 0; i < manifest.files.length; i += ARTIFACT_PAGE_FILES) {
    requireThat(!signal?.aborted,"Build publication cancelled",409);
    const page = manifest.files.slice(i,i+ARTIFACT_PAGE_FILES);
    await client.json(`/v1/projects/${manifest.project}/artifact-file-pages/${await sha256(canonical(page))}`,page);
  }
  requireThat(!signal?.aborted,"Build publication cancelled",409);
  const result = await client.json<BuildRecord>(`/v1/projects/${manifest.project}/artifacts`,{candidate,manifest});
  requireThat(result.id === await sha256(canonical(manifest)),"Published build identity differs",502);
  return result;
}

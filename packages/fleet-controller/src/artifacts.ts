import {
  ARTIFACT_PAGE_FILES,
  canonical,
  digest,
  exactKeys,
  MAX_ARTIFACT_JSON,
  MAX_CHUNK_BYTES,
  readBounded,
  record,
  relativeFile,
  requireThat,
  sha256,
  validateArtifactFiles,
  validateBuild,
  validateChunks,
  validateFileObject,
  type ArtifactFile,
  type BuildManifest,
  type BuildRecord,
  type CandidateSpec,
  type FileObject,
} from "@enginebox/fleet-protocol";

type Store = {
  get<T>(key: string): T | undefined;
  put(key: string, value: unknown): void;
  rows<T>(prefix: string): T[];
};
export class Artifacts {
  constructor(
    private store: Store,
    private sql: SqlStorage,
  ) {}
  register(
    id: string,
    manifest: BuildManifest,
    candidate: string,
    actor: string,
  ): BuildRecord {
    const staged = this.store.get<{ spec: CandidateSpec }>(
      `candidate:${candidate}`,
    );
    requireThat(
      staged?.spec.generation === manifest.generation,
      "Build does not match a frozen candidate",
      409,
    );
    const generation = this.store.get<{
      manifest: { metadata: { treeHash?: string } };
    }>(`generation:${manifest.generation}`);
    requireThat(
      !generation?.manifest.metadata.treeHash ||
        generation.manifest.metadata.treeHash === manifest.sourceTree,
      "Build source differs from the frozen generation",
      409,
    );
    const prior = this.store.get<BuildRecord>(`artifact:${id}`);
    if (prior) {
      requireThat(
        prior.candidate === candidate,
        "Artifact already belongs to another candidate",
        409,
      );
      return prior;
    }
    const key = `candidate-build:${candidate}:${manifest.recipe}`;
    const chosen = this.store.get<string>(key);
    requireThat(
      !chosen || chosen === id,
      "This candidate recipe already has an immutable build",
      409,
    );
    const { files, ...summary } = manifest;
    const result = {
      ...summary,
      id,
      candidate,
      fileCount: files.length,
      createdAt: Date.now(),
      createdBy: actor,
    };
    this.store.put(`artifact:${id}`, result);
    this.store.put(key, id);
    return result;
  }
  get(id: string) {
    return this.store.get<BuildRecord>(`artifact:${id}`) ?? null;
  }
  list(candidate?: string) {
    const query = candidate
      ? "SELECT value FROM records WHERE key LIKE 'artifact:%' AND json_extract(value,'$.candidate')=? ORDER BY rowid DESC LIMIT 100"
      : "SELECT value FROM records WHERE key LIKE 'artifact:%' ORDER BY rowid DESC LIMIT 100";
    return this.sql
      .exec<{ value: string }>(query, ...(candidate ? [candidate] : []))
      .toArray()
      .map((row) => JSON.parse(row.value) as BuildRecord);
  }
}

export async function immutableObject(
  bucket: R2Bucket,
  key: string,
  bytes: string | Uint8Array,
  metadata: Record<string, string>,
) {
  const written = await bucket.put(key, bytes, {
    onlyIf: { etagDoesNotMatch: "*" },
    customMetadata: metadata,
  });
  if (!written) {
    const old = await bucket.head(key);
    requireThat(
      old &&
        Object.entries(metadata).every(
          ([key, value]) => old.customMetadata?.[key] === value,
        ),
      "Immutable object differs",
      409,
    );
  }
}
async function jsonBody(request: Request) {
  return JSON.parse(
    new TextDecoder().decode(
      await readBounded(request.body, MAX_ARTIFACT_JSON),
    ),
  );
}
export async function artifactFile(
  bucket: R2Bucket,
  project: string,
  hash: string,
): Promise<FileObject> {
  digest(hash);
  const value = await bucket.get(`projects/${project}/artifact-files/${hash}`);
  requireThat(value, "Artifact file not found", 404);
  return validateFileObject(
    JSON.parse(
      new TextDecoder().decode(
        await readBounded(value.body, MAX_ARTIFACT_JSON),
      ),
    ),
  );
}
export async function artifactManifest(
  bucket: R2Bucket,
  project: string,
  id: string,
) {
  digest(id);
  const object = await bucket.get(
    `projects/${project}/artifacts/${id}/manifest`,
  );
  requireThat(object, "Artifact not found", 404);
  const bytes = await readBounded(object.body, MAX_ARTIFACT_JSON);
  requireThat(
    (await sha256(bytes)) === id,
    "Artifact manifest is corrupt",
    502,
  );
  return validateBuild(JSON.parse(new TextDecoder().decode(bytes)));
}
export function mediaType(path: string) {
  const ext = path.split(".").pop()?.toLowerCase();
  return (
    (
      {
        html: "text/html; charset=utf-8",
        js: "text/javascript; charset=utf-8",
        wasm: "application/wasm",
        json: "application/json",
        css: "text/css",
        png: "image/png",
        svg: "image/svg+xml",
        webm: "video/webm",
        mp4: "video/mp4",
        ogg: "audio/ogg",
        wav: "audio/wav",
      } as Record<string, string>
    )[ext ?? ""] ?? "application/octet-stream"
  );
}
export async function fileResponse(
  bucket: R2Bucket,
  project: string,
  file: Pick<ArtifactFile, "sha256" | "bytes" | "path">,
  request: Request,
) {
  const index = await artifactFile(bucket, project, file.sha256);
  requireThat(
    index.bytes === file.bytes && index.sha256 === file.sha256,
    "Artifact file metadata differs",
    502,
  );
  let start = 0,
    end = file.bytes - 1,
    partial = false;
  const range = request.headers.get("range");
  if (range) {
    const match = /^bytes=(\d*)-(\d*)$/.exec(range);
    if (!match || (!match[1] && !match[2]) || file.bytes === 0)
      return new Response(null, {
        status: 416,
        headers: { "content-range": `bytes */${file.bytes}` },
      });
    start = match[1]
      ? Number(match[1])
      : Math.max(0, file.bytes - Number(match[2]));
    end = match[1] && match[2] ? Number(match[2]) : end;
    if (
      !Number.isSafeInteger(start) ||
      !Number.isSafeInteger(end) ||
      start > end ||
      start >= file.bytes
    )
      return new Response(null, {
        status: 416,
        headers: { "content-range": `bytes */${file.bytes}` },
      });
    end = Math.min(end, file.bytes - 1);
    partial = true;
  }
  async function* bytes() {
    let offset = 0;
    for (const page of index.pages) {
      if (offset + page.contentBytes <= start) {
        offset += page.contentBytes;
        continue;
      }
      if (offset > end) break;
      const object = await bucket.get(`projects/${project}/pages/${page.hash}`);
      requireThat(object, "Artifact index is missing", 502);
      const raw = await readBounded(object.body, page.bytes);
      requireThat(
        (await sha256(raw)) === page.hash,
        "Artifact index is corrupt",
        502,
      );
      const chunks = validateChunks(JSON.parse(new TextDecoder().decode(raw)));
      for (const chunk of chunks) {
        const next = offset + chunk.bytes;
        if (next > start && offset <= end) {
          const content = await bucket.get(
            `projects/${project}/chunks/${chunk.hash}`,
          );
          requireThat(content, "Artifact chunk is missing", 502);
          const data = await readBounded(content.body, MAX_CHUNK_BYTES);
          requireThat(
            data.length === chunk.bytes && (await sha256(data)) === chunk.hash,
            "Artifact chunk is corrupt",
            502,
          );
          yield data.subarray(
            Math.max(0, start - offset),
            Math.min(data.length, end - offset + 1),
          );
        }
        offset = next;
      }
    }
  }
  const iterator = bytes();
  const stream = new ReadableStream<Uint8Array>({
    async pull(controller) {
      try {
        const next = await iterator.next();
        if (next.done) controller.close();
        else controller.enqueue(next.value);
      } catch (error) {
        controller.error(error);
      }
    },
    async cancel() {
      await iterator.return(undefined);
    },
  });
  const headers = new Headers({
    "content-type": mediaType(file.path),
    "content-length": String(Math.max(0, end - start + 1)),
    "accept-ranges": "bytes",
    etag: `"${file.sha256}"`,
    "cache-control": "private, no-store",
    "x-content-type-options": "nosniff",
  });
  if (partial)
    headers.set("content-range", `bytes ${start}-${end}/${file.bytes}`);
  return new Response(request.method === "HEAD" ? null : stream, {
    status: partial ? 206 : 200,
    headers,
  });
}
export async function artifactRoute(
  request: Request,
  env: Env,
  project: string,
  path: string[],
  writer: boolean,
  actor: string,
): Promise<Response> {
  const authority = env.PROJECTS.getByName(project),
    bucket = env.STORE;
  if (
    path[0] === "artifact-file-pages" &&
    path.length === 2 &&
    request.method === "POST"
  ) {
    requireThat(writer, "Artifact storage requires a trusted worker", 403);
    digest(path[1]);
    const files = validateArtifactFiles(await jsonBody(request));
    requireThat(
      files.length <= ARTIFACT_PAGE_FILES,
      "Artifact file page is too large",
    );
    const text = canonical(files);
    requireThat(
      (await sha256(text)) === path[1],
      "Artifact file page digest differs",
    );
    for (const file of files) {
      const object = await bucket.head(
        `projects/${project}/artifact-files/${file.sha256}`,
      );
      requireThat(
        object?.customMetadata?.bytes === String(file.bytes),
        "Artifact file has not been published",
        409,
      );
    }
    await immutableObject(
      bucket,
      `projects/${project}/artifact-file-pages/${path[1]}`,
      text,
      { sha256: path[1], count: String(files.length) },
    );
    return Response.json({ hash: path[1] });
  }
  if (path[0] === "artifact-files" && path.length === 2) {
    digest(path[1]);
    requireThat(writer, "Artifact storage requires a trusted worker", 403);
    if (request.method === "POST") {
      const value = validateFileObject(await jsonBody(request));
      requireThat(value.sha256 === path[1], "Artifact file identity differs");
      for (const page of value.pages) {
        const object = await bucket.head(
          `projects/${project}/pages/${page.hash}`,
        );
        requireThat(
          object?.size === page.bytes &&
            object.customMetadata?.count === String(page.count) &&
            object.customMetadata?.contentBytes === String(page.contentBytes),
          "Artifact index is not durably verified",
          409,
        );
      }
      const text = canonical(value);
      await immutableObject(
        bucket,
        `projects/${project}/artifact-files/${path[1]}`,
        text,
        {
          sha256: value.sha256,
          bytes: String(value.bytes),
          indexHash: await sha256(text),
        },
      );
      return Response.json(value);
    }
    if (request.method === "GET")
      return Response.json(await artifactFile(bucket, project, path[1]));
  }
  if (path[0] === "artifacts" && path.length === 1) {
    if (request.method === "GET")
      return Response.json({
        artifacts: await authority.listArtifacts(
          new URL(request.url).searchParams.get("candidate") ?? undefined,
        ),
      });
    if (request.method === "POST") {
      requireThat(writer, "Only trusted workers may publish builds", 403);
      const body = await jsonBody(request);
      record(body);
      exactKeys(body, ["candidate", "manifest"]);
      const manifest = validateBuild(body.manifest);
      requireThat(manifest.project === project, "Artifact project differs");
      requireThat(typeof body.candidate === "string", "Candidate required");
      for (let i = 0; i < manifest.files.length; i += ARTIFACT_PAGE_FILES) {
        const files = manifest.files.slice(i, i + ARTIFACT_PAGE_FILES),
          hash = await sha256(canonical(files));
        const object = await bucket.head(
          `projects/${project}/artifact-file-pages/${hash}`,
        );
        requireThat(
          object?.customMetadata?.sha256 === hash &&
            object.customMetadata?.count === String(files.length),
          "Artifact file page has not been verified",
          409,
        );
      }
      const text = canonical(manifest),
        id = await sha256(text);
      await immutableObject(
        bucket,
        `projects/${project}/artifacts/${id}/manifest`,
        text,
        { sha256: id },
      );
      return Response.json(
        await authority.registerArtifact(id, manifest, body.candidate, actor),
        { status: 201 },
      );
    }
  }
  if (path[0] === "artifacts" && path.length >= 2) {
    digest(path[1]);
    const build = await authority.artifact(path[1]);
    requireThat(build, "Unknown artifact", 404);
    if (path.length === 2 && request.method === "GET")
      return Response.json(build);
    if (
      path[2] === "files" &&
      path.length > 3 &&
      ["GET", "HEAD"].includes(request.method)
    ) {
      const name = path.slice(3).map(decodeURIComponent).join("/");
      relativeFile(name);
      const manifest = await artifactManifest(bucket, project, path[1]);
      const file = manifest.files.find((entry) => entry.path === name);
      requireThat(file, "Artifact file not found", 404);
      return fileResponse(bucket, project, file, request);
    }
  }
  return Response.json({ error: "Unknown artifact route" }, { status: 404 });
}

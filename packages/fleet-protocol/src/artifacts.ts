import {
  digest,
  exactKeys,
  identifier,
  integer,
  MAX_CHUNK_BYTES,
  MAX_JSON_BYTES,
  MAX_PAGES,
  PAGE_CHUNKS,
  record,
  requireThat,
  type IndexPage,
} from "./index.ts";

export const MAX_ARTIFACT_JSON = 8 * 1024 * 1024;
export const ARTIFACT_PAGE_FILES = 128;
export type ArtifactFile = {
  path: string;
  sha256: string;
  bytes: number;
  executable: boolean;
};
export type BuildManifest = {
  schema: 1;
  kind: "engine-build";
  project: string;
  generation: string;
  sourceTree: string;
  recipe: string;
  toolchain: {
    engine: "godot" | "unity" | "unreal";
    version: string;
    target: string;
    architecture: string;
    fingerprint: string;
  };
  entrypoint: string;
  files: ArtifactFile[];
  totalBytes: number;
};
export type FileObject = { sha256: string; bytes: number; pages: IndexPage[] };
export type BuildRecord = Omit<BuildManifest, "files"> & {
  id: string;
  candidate: string;
  fileCount: number;
  createdAt: number;
  createdBy: string;
};

export function relativeFile(value: unknown): asserts value is string {
  requireThat(
    typeof value === "string" &&
      value.length > 0 &&
      value.length <= 1024 &&
      !/[\\\x00-\x1f\x7f]/.test(value) &&
      !value.includes(":"),
    "Invalid artifact path",
  );
  requireThat(
    value
      .split("/")
      .every((part) => part !== "" && part !== "." && part !== ".."),
    "Artifact paths must stay inside their root",
  );
}
function label(value: unknown): asserts value is string {
  requireThat(
    typeof value === "string" &&
      value.length > 0 &&
      value.length <= 128 &&
      !/[\x00-\x1f\x7f]/.test(value),
    "Invalid toolchain label",
  );
}
export function validateBuild(value: unknown): BuildManifest {
  record(value);
  exactKeys(value, [
    "schema",
    "kind",
    "project",
    "generation",
    "sourceTree",
    "recipe",
    "toolchain",
    "entrypoint",
    "files",
    "totalBytes",
  ]);
  requireThat(
    value.schema === 1 && value.kind === "engine-build",
    "Unsupported build manifest",
  );
  identifier(value.project);
  digest(value.generation);
  digest(value.sourceTree);
  digest(value.recipe);
  record(value.toolchain);
  exactKeys(value.toolchain, [
    "engine",
    "version",
    "target",
    "architecture",
    "fingerprint",
  ]);
  requireThat(
    ["godot", "unity", "unreal"].includes(String(value.toolchain.engine)),
    "Unsupported engine",
  );
  label(value.toolchain.version);
  label(value.toolchain.target);
  label(value.toolchain.architecture);
  digest(value.toolchain.fingerprint);
  relativeFile(value.entrypoint);
  requireThat(
    Array.isArray(value.files) &&
      value.files.length > 0 &&
      value.files.length <= 20_000,
    "Invalid artifact file count",
  );
  const files = validateArtifactFiles(value.files);
  const total = files.reduce((size, file) => size + file.bytes, 0);
  integer(value.totalBytes, 0, Number.MAX_SAFE_INTEGER);
  requireThat(
    total === value.totalBytes &&
      files.some((file) => file.path === value.entrypoint),
    "Artifact size or entrypoint differs",
  );
  return { ...value, files } as BuildManifest;
}
export function validateArtifactFiles(value: unknown): ArtifactFile[] {
  requireThat(
    Array.isArray(value) && value.length > 0 && value.length <= 20_000,
    "Invalid artifact file count",
  );
  const paths = new Set<string>();
  return value.map((file) => {
    record(file);
    exactKeys(file, ["path", "sha256", "bytes", "executable"]);
    relativeFile(file.path);
    digest(file.sha256);
    integer(file.bytes, 0, MAX_CHUNK_BYTES * PAGE_CHUNKS * MAX_PAGES);
    requireThat(
      typeof file.executable === "boolean",
      "Invalid executable flag",
    );
    const folded = file.path.normalize("NFC").toLowerCase();
    requireThat(!paths.has(folded), "Colliding artifact paths");
    paths.add(folded);
    return {
      path: file.path,
      sha256: file.sha256,
      bytes: file.bytes,
      executable: file.executable,
    };
  });
}
export function validateFileObject(value: unknown): FileObject {
  record(value);
  exactKeys(value, ["sha256", "bytes", "pages"]);
  digest(value.sha256);
  integer(value.bytes, 0, MAX_CHUNK_BYTES * PAGE_CHUNKS * MAX_PAGES);
  requireThat(
    Array.isArray(value.pages) && value.pages.length <= MAX_PAGES,
    "Invalid artifact pages",
  );
  let total = 0;
  const pages = value.pages.map((page) => {
    record(page);
    exactKeys(page, ["hash", "bytes", "count", "contentBytes"]);
    digest(page.hash);
    integer(page.bytes, 1, MAX_JSON_BYTES);
    integer(page.count, 1, PAGE_CHUNKS);
    integer(page.contentBytes, 1, MAX_CHUNK_BYTES * PAGE_CHUNKS);
    total += page.contentBytes;
    return {
      hash: page.hash,
      bytes: page.bytes,
      count: page.count,
      contentBytes: page.contentBytes,
    };
  });
  requireThat(
    total === value.bytes && (value.bytes === 0 || pages.length > 0),
    "File/index size differs",
  );
  return { sha256: value.sha256, bytes: value.bytes, pages };
}

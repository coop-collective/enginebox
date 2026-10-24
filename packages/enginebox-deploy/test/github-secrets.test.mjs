import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, readFile, stat, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { materialize, secretFiles } from "../src/github-secrets.mjs";

test("GitHub credential staging is private, create-only, and returns metadata", async () => {
  const root = await mkdtemp(join(tmpdir(), "enginebox-secrets-test-"));
  try {
    const env = Object.fromEntries(Object.keys(secretFiles).map(name => [name, name.endsWith("PRIVATE_KEY") ? "-----BEGIN PRIVATE KEY-----\nfixture-format-only\n-----END PRIVATE KEY-----" : "fixture-private-value-at-least-32-characters"]));
    const directory = join(root, "staging");
    const result = await materialize(directory, env);
    assert.equal(result.files, 6);
    assert.ok(!JSON.stringify(result).includes("fixture-private"));
    assert.equal(await readFile(join(directory, "repos-client-secret"), "utf8"), env.ENGINEBOX_GITHUB_REPOS_CLIENT_SECRET);
    if (process.platform !== "win32") {
      assert.equal((await stat(directory)).mode & 0o777, 0o700);
      assert.equal((await stat(join(directory, "repos-private-key"))).mode & 0o777, 0o600);
    }
    await assert.rejects(materialize(directory, env), /EEXIST/);
    assert.equal(await readFile(join(directory, "repos-client-secret"), "utf8"), env.ENGINEBOX_GITHUB_REPOS_CLIENT_SECRET);
    await assert.rejects(materialize(join(root, "missing"), {}), /Missing or invalid ENGINEBOX_GITHUB_REPOS_CLIENT_SECRET/);
    await assert.rejects(stat(join(root, "missing")), /ENOENT/);
    assert.equal((await materialize(join(root, "repos"), env, true)).files, 3);
    await assert.rejects(materialize("relative", env), /absolute/);
  } finally { await rm(root, { recursive: true, force: true }); }
});

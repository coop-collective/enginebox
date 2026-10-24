import assert from "node:assert/strict";
import { afterEach, test } from "node:test";
import { api, errorMessage, failureMessage, RequestError } from "../lib/client";

const realFetch = globalThis.fetch;
afterEach(() => {
  globalThis.fetch = realFetch;
});

function respond(body: string, status: number) {
  globalThis.fetch = async () => new Response(body, { status });
}

test("errorMessage reads Error messages and falls back for unknown values", () => {
  assert.equal(errorMessage(new Error("boom")), "boom");
  assert.equal(errorMessage(new RequestError("denied", 403)), "denied");
  assert.equal(errorMessage("plain"), "plain");
  for (const value of [undefined, null, 42, {}, ""])
    assert.equal(errorMessage(value), "The request failed. Try again.");
});

test("failureMessage prefers error string, error.message, then detail", () => {
  assert.equal(failureMessage({ error: "a" }), "a");
  assert.equal(failureMessage({ error: { message: "b" } }), "b");
  assert.equal(failureMessage({ detail: "c" }), "c");
  assert.equal(
    failureMessage({ error: { code: 1 } }),
    "The request failed. Try again.",
  );
  assert.equal(failureMessage(undefined), "The request failed. Try again.");
});

test("api returns parsed JSON on success", async () => {
  respond('{"ok":true}', 200);
  assert.deepEqual(await api("/x"), { ok: true });
});

test("api turns a proxy HTML error page into a RequestError with its status", async () => {
  respond("<html><body>502 Bad Gateway</body></html>", 502);
  await assert.rejects(api("/x"), (err: unknown) => {
    assert.ok(err instanceof RequestError);
    assert.equal(err.status, 502);
    assert.equal(err.message, "The request failed. Try again.");
    return true;
  });
});

test("api reports the server error message from a JSON failure", async () => {
  respond('{"error":{"message":"no such node"}}', 404);
  await assert.rejects(api("/x"), { message: "no such node", status: 404 });
});

test("api rejects a successful but non-JSON or empty body", async () => {
  for (const body of ["<html>ok</html>", ""]) {
    respond(body, 200);
    await assert.rejects(api("/x"), (err: unknown) => {
      assert.ok(err instanceof RequestError);
      assert.equal(err.status, 200);
      return true;
    });
  }
});

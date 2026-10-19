import assert from "node:assert/strict";
import { test } from "node:test";
import { assignedWorkspace, officialAuthorizationUrl } from "../lib/accounts";
import type { Instance, ProviderId } from "../lib/types";

test("provider links accept only verified official HTTPS sign-in endpoints", () => {
  const expected: [ProviderId, string][] = [
    ["codex", "https://auth.openai.com/codex/device"],
    ["claude", "https://claude.com/cai/oauth/authorize?code=true"],
    ["cursor", "https://cursor.com/loginDeepControl?challenge=fixture"],
  ];
  for (const [provider, url] of expected)
    assert.equal(officialAuthorizationUrl(provider, url), url);
  for (const url of [
    "http://auth.openai.com/codex/device",
    "https://auth.openai.com.evil.example/codex/device",
    "https://owner@auth.openai.com/codex/device",
    "https://auth.openai.com:444/codex/device",
    "https://auth.openai.com/oauth/callback?code=secret",
    "javascript:alert(1)",
    "https://auth.openai.com/codex/device#access_token=secret",
    "https://auth.openai.com/codex/device?code=true",
    "https://auth.openai.com/codex/device?token=secret",
    " https://auth.openai.com/codex/device",
  ])
    assert.equal(officialAuthorizationUrl("codex", url), null);
  assert.equal(officialAuthorizationUrl("claude", expected[0][1]), null);
  assert.equal(
    officialAuthorizationUrl(
      "claude",
      "https://platform.claude.com/oauth/code/callback?code=secret",
    ),
    null,
  );
  for (const query of [
    "code=secret",
    "code=true&code=secret",
    "code=True",
    "access_token=secret",
    "redirect_uri=https://example.com/callback",
    "response_type=token",
    "code_challenge_method=plain",
    "unexpected=secret",
  ])
    assert.equal(
      officialAuthorizationUrl(
        "claude",
        `https://claude.com/cai/oauth/authorize?${query}`,
      ),
      null,
    );
  assert.equal(
    officialAuthorizationUrl(
      "cursor",
      "https://cursor.com/loginDeepControl?redirectTarget=https://example.com",
    ),
    null,
  );
});

test("only assigned managed workspaces may receive provider credentials", () => {
  const instance: Instance = {
    name: "ebox-test",
    status: "Running",
    stateful: false,
    ipv4: [],
    cpu: 4,
    memory_bytes: 4 * 1024 ** 3,
    managed: true,
  };
  assert.equal(assignedWorkspace(instance), true);
  assert.equal(
    assignedWorkspace({ ...instance, warm_pool_state: "claimed" }),
    true,
  );
  for (const warm_pool_state of ["ready", "preparing", "quarantined"] as const)
    assert.equal(assignedWorkspace({ ...instance, warm_pool_state }), false);
  assert.equal(assignedWorkspace({ ...instance, managed: false }), false);
});

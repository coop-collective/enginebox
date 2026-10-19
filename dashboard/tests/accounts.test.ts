import assert from "node:assert/strict";
import { test } from "node:test";
import { delegationLabel, officialAuthorizationUrl } from "../lib/accounts";
import type { ProviderAccount, ProviderId } from "../lib/types";

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

test("workspace availability requires both connected account and confirmed delegation", () => {
  const account: ProviderAccount = {
    id: "codex",
    label: "Codex",
    installed: true,
    status: "connected",
    auth_method: "account",
  };
  assert.equal(
    delegationLabel(account),
    "Workspace access has not been confirmed.",
  );
  assert.equal(
    delegationLabel({
      ...account,
      installed: false,
      delegation_status: "ready",
    }),
    "Workspace access has not been confirmed.",
  );
  assert.equal(
    delegationLabel({
      ...account,
      delegation_status: "ready",
      workspace_count: 2,
    }),
    "Available to 2 private workspaces.",
  );
  assert.equal(
    delegationLabel({
      ...account,
      delegation_status: "ready",
      status: "disconnected",
    }),
    "Workspace access has not been confirmed.",
  );
  assert.match(
    delegationLabel({ ...account, delegation_status: "unsupported" }),
    /not supported/,
  );
  assert.match(
    delegationLabel({ ...account, delegation_status: "reauth_required" }),
    /Reconnect/,
  );
});

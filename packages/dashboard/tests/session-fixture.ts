import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { openSession } from "../lib/identity/auth";
import { createUser, DEFAULT_ORG, makeInstanceOwner, membership, userByEmail, withIdentity } from "../lib/identity/store";

/** A fresh identity state directory, set as ENGINEBOX_IDENTITY_STATE_DIR. */
export function useIdentityDir(prefix = "enginebox-identity-") {
  const dir = mkdtempSync(join(tmpdir(), prefix));
  process.env.ENGINEBOX_IDENTITY_STATE_DIR = dir;
  return dir;
}

/**
 * A real member session cookie ("enginebox_session=..."), as email sign-in
 * would issue. Default: the instance owner (an owner of the platform org).
 */
export function memberCookie(email = "owner@fixture.example", now = Date.now()) {
  return memberSessionHeader(email, now).split(";")[0];
}
/** The full Set-Cookie header of a fresh member session. */
export function memberSessionHeader(email = "owner@fixture.example", now = Date.now()): string {
  return withIdentity((state, at) => {
    let user = userByEmail(state, email);
    if (!user) {
      user = createUser(state, email, "env", at);
      if (!state.users.some((u) => u.instance_owner)) makeInstanceOwner(state, user, "env", at);
    }
    const org = membership(state, DEFAULT_ORG, user.id) ? DEFAULT_ORG : null;
    return openSession(state, user, now, org);
  });
}

import { atLeast, principal, sessionCookie, verifyOperatorPassword } from "../server";
import { DEFAULT_ORG, identityEnabled } from "../identity/store";

/** The person who approves a client and owns the resulting grant. */
export type User = {
  /** Grant owner: the user ID (`usr_...`), or "owner" for the break-glass operator and pre-org grants. */
  id: string;
  label: string;
  /** The org the approver acts in; the grant belongs to it. */
  org_id: string;
  /** Approving an app hands out run and repository access: org admins and owners only. */
  may_approve: boolean;
};

/**
 * How the consent step learns who is approving. Grants are keyed by `User.id`
 * and belong to `User.org_id`; the consent step only asks for the current user
 * or a sign-in.
 */
export interface UserAuthenticator {
  /** The signed-in user for this request, or null. */
  current(request: Request): User | null;
  /** Fields the sign-in form collects (rendered by the consent page). */
  readonly signInFields: { name: string; label: string; type: "password" | "text" }[];
  /** Shown above the form: how members sign in when email sign-in is on. */
  readonly signInHint?: () => string | null;
  /** Verify a submitted sign-in form; returns the user and a Set-Cookie value, or throws. */
  signIn(form: URLSearchParams): { user: User; setCookie: string };
}

const OPERATOR: User = { id: "owner", label: "Engine Box owner", org_id: DEFAULT_ORG, may_approve: true };

/**
 * Members sign in with email on the dashboard (the session cookie is shared
 * with this page); the form here is only the host operator's break-glass password.
 */
export const ownerAuthenticator: UserAuthenticator = {
  current: (request) => {
    const who = principal(request);
    return who ? { id: who.grant_owner, label: who.label, org_id: who.org_id, may_approve: atLeast(who, "admin") } : null;
  },
  signInFields: [{ name: "password", label: "Operator password (break-glass, host operator only)", type: "password" }],
  signInHint: () =>
    identityEnabled()
      ? "Team members: sign in with your email on the Engine Box dashboard in this browser, then choose Continue below."
      : null,
  signIn(form) {
    verifyOperatorPassword(form.get("password"));
    return { user: OPERATOR, setCookie: sessionCookie() };
  },
};

let active: UserAuthenticator = ownerAuthenticator;
export function users(): UserAuthenticator {
  return active;
}
/** Replace the authenticator (tests). */
export function setUserAuthenticator(next: UserAuthenticator) {
  active = next;
}

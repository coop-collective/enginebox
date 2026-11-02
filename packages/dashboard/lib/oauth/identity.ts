import { atLeast, HttpError, principal } from "../server";
import { identityEnabled, membership, readIdentity } from "../identity/store";

/** A member's user ID, as the controller's engine sessions record their starter. */
export const MEMBER_ID = /^usr_[a-f0-9]{24}$/;

/**
 * Whether `userId` still belongs to `orgId`: engine-session grants act as that member, so a member who left
 * (or an Engine Box without accounts) ends them at the next refresh.
 */
export function stillMember(userId: string, orgId: string): boolean {
  if (!MEMBER_ID.test(userId) || !identityEnabled()) return false;
  try {
    return membership(readIdentity(), orgId, userId) !== null;
  } catch {
    return false;
  }
}

/** The person who approves a client and owns the resulting grant. */
export type User = {
  /** Grant owner: the user ID (`usr_...`), or "owner" on grants made before orgs. */
  id: string;
  label: string;
  /** The org the approver acts in; the grant belongs to it. */
  org_id: string;
  /**
   * Approving an app hands out run and repository access: org admins and owners only. Any member may approve
   * an engine-sessions-only grant, which acts only as themselves.
   */
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
  /** Fields the sign-in form collects (rendered by the consent page). Empty: no form. */
  readonly signInFields: { name: string; label: string; type: "password" | "text" }[];
  /** Shown above the form: how people sign in. */
  readonly signInHint?: () => string | null;
  /** Verify a submitted sign-in form; returns the user and a Set-Cookie value, or throws. */
  signIn(form: URLSearchParams): { user: User; setCookie: string };
}

const EMAIL_HINT = "Sign in with your email on the Engine Box dashboard in this browser, then choose Continue below.";

/**
 * Everyone signs in with an emailed code on the dashboard; the session cookie
 * is shared with this page. The consent page itself collects no credentials.
 */
export const ownerAuthenticator: UserAuthenticator = {
  current: (request) => {
    const who = principal(request);
    return who ? { id: who.grant_owner, label: who.label, org_id: who.org_id, may_approve: atLeast(who, "admin") } : null;
  },
  signInFields: [],
  signInHint: () => EMAIL_HINT,
  signIn() {
    throw new HttpError(400, EMAIL_HINT);
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

import { authenticated, sessionCookie, verifyOperatorPassword } from "../server";

/** The person who approves a client and owns the resulting grant. */
export type User = { id: string; label: string };

/**
 * How the consent step learns who is approving. Today there is one owner,
 * authenticated by the dashboard's operator password and session cookie. A
 * per-tenant account system replaces this implementation without changing the
 * OAuth code: grants are keyed by `User.id`, and the consent step only asks
 * for the current user or a sign-in.
 */
export interface UserAuthenticator {
  /** The signed-in user for this request, or null. */
  current(request: Request): User | null;
  /** Fields the sign-in form collects (rendered by the consent page). */
  readonly signInFields: { name: string; label: string; type: "password" | "text" }[];
  /** Verify a submitted sign-in form; returns the user and a Set-Cookie value, or throws. */
  signIn(form: URLSearchParams): { user: User; setCookie: string };
}

const OWNER: User = { id: "owner", label: "Engine Box owner" };

export const ownerAuthenticator: UserAuthenticator = {
  current: (request) => (authenticated(request) ? OWNER : null),
  signInFields: [{ name: "password", label: "Operator password", type: "password" }],
  signIn(form) {
    verifyOperatorPassword(form.get("password"));
    return { user: OWNER, setCookie: sessionCookie() };
  },
};

let active: UserAuthenticator = ownerAuthenticator;
export function users(): UserAuthenticator {
  return active;
}
/** Replace the authenticator (per-tenant accounts later; tests). */
export function setUserAuthenticator(next: UserAuthenticator) {
  active = next;
}

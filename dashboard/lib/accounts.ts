import type { AuthSession, Instance, ProviderId } from "./types";

export const providerLabels: Record<ProviderId, string> = {
  codex: "Codex (ChatGPT)",
  claude: "Claude",
  cursor: "Cursor",
};
export const providerIds = Object.keys(providerLabels) as ProviderId[];
export const activeAuth = (session?: AuthSession | null) =>
  session?.status === "starting" || session?.status === "waiting";
export const assignedWorkspace = (instance: Instance) =>
  !!instance.managed &&
  (!instance.warm_pool_state || instance.warm_pool_state === "claimed");

export function officialAuthorizationUrl(
  provider: ProviderId,
  value?: string | null,
) {
  if (!value || value.length > 8192 || /[\x00-\x20]/.test(value)) return null;
  const endpoints: Record<ProviderId, [string, string]> = {
    codex: ["auth.openai.com", "/codex/device"],
    claude: ["claude.com", "/cai/oauth/authorize"],
    cursor: ["cursor.com", "/loginDeepControl"],
  };
  try {
    const url = new URL(value);
    const [hostname, pathname] = endpoints[provider];
    const allowed: Record<ProviderId, string[]> = {
      codex: [],
      claude: [
        "client_id",
        "code",
        "code_challenge",
        "code_challenge_method",
        "redirect_uri",
        "response_type",
        "scope",
        "state",
      ],
      cursor: [
        "challenge",
        "mode",
        "redirectTarget",
        "supportsSelectedTeamLogin",
        "uuid",
      ],
    };
    const keys = [...url.searchParams.keys()];
    if (
      new Set(keys).size !== keys.length ||
      keys.some((key) => !allowed[provider].includes(key))
    )
      return null;
    const queryMatches = (expected: Record<string, string>) =>
      Object.entries(expected).every(
        ([key, expectedValue]) =>
          !url.searchParams.has(key) ||
          url.searchParams.get(key) === expectedValue,
      );
    // Claude's code=true requests a manual code flow; returned codes are never links.
    if (
      provider === "claude" &&
      !queryMatches({
        code: "true",
        response_type: "code",
        code_challenge_method: "S256",
        redirect_uri: "https://platform.claude.com/oauth/code/callback",
      })
    )
      return null;
    if (
      provider === "cursor" &&
      !queryMatches({
        redirectTarget: "cli",
        mode: "login",
        supportsSelectedTeamLogin: "true",
      })
    )
      return null;
    return url.protocol === "https:" &&
      !url.username &&
      !url.password &&
      !url.port &&
      !url.hash &&
      url.hostname === hostname &&
      url.pathname === pathname
      ? url.href
      : null;
  } catch {
    return null;
  }
}

# Agent usage and remaining allowance

Research checked 2026-10-19. The dashboard now includes account-level usage and billing information. Codex allowance collection is implemented; Claude/Cursor collection and subscription billing dates remain unavailable through this integration. A successful sign-in alone does not establish a paid entitlement.

The **Agents** cards show provider-reported allowance percentages, quota reset times and optional provider credit balances separately. They do not convert token counts into a guessed subscription balance or add the same account's allowance across workspaces. Billing renewal is explicitly **Not reported**; provider links lead to account usage/billing. The connected provider does not establish a specific subscription plan.

## Implemented behavior

Authenticated `GET /v1/agents/{provider}/usage` returns a sanitized controller-account observation. The Codex collector sends only native app-server initialization and `account/rateLimits/read`; it does not start a thread or model turn, read account identity, redeem resets, buy credits or alter billing. It uses the controller's private native account context, suppresses native stderr, limits output and runtime, and retains no raw response. API-key authentication is not treated as subscription allowance.

Reads are cached in memory for 60 seconds per connection fingerprint; reconnecting with different credentials invalidates the previous observation. Refresh failures preserve any earlier values only as stale. Both API and UI stop reporting a remaining percentage once the reported quota window expires, and the UI additionally expires missed observations after five minutes. The Agents view polls Codex once per minute while open and stops on navigation/logout. Connection status loads independently of usage.

Claude and Cursor currently return explicit unavailable states and provider links. Claude's documented status-line observation hook is not yet installed; ordinary native stream observations are not yet ingested. A billing cycle, subscription renewal date, personal Cursor balance, and Claude remaining allowance are **not** inferred or scraped. Provider credits preserve their reported unit and are never relabeled as dollars.

Offline validation covers malformed/missing fields, duplicate multi-bucket views, no-model native RPC traces, output/time bounds, account changes, concurrent cached reads, reset/failure staleness, authenticated read-only routes, and browser display. Live verification of the deployed collector is tracked separately in `VALIDATION.md`.

| Provider | Official machine-readable surface | Useful display | First-release recommendation |
| --- | --- | --- | --- |
| Codex / ChatGPT | Native app-server account methods | Remaining percentage per reported window, reset time, optional workspace credits; token activity separately | Implemented read-only quota adapter; unsupported native methods return unavailable |
| Claude Code | Native status-line JSON; interactive `/usage` | Five-hour/seven-day usage and resets when supplied; local session statistics separately | Provider usage link implemented; native observation ingestion remains planned |
| Cursor Agent | Enterprise Admin API; personal dashboard | Enterprise spending/activity where authorized; personal remaining allowance in provider dashboard | Provider usage links implemented; no personal balance API claimed |

## Codex

The official app-server exposes `account/rateLimits/read` and `account/rateLimits/updated`. Returned windows contain `usedPercent`, `windowDurationMins`, and `resetsAt`; prefer `rateLimitsByLimitId` when present. Optional `credits` represents workspace credit details; optional reset credits are a different resource. `account/usage/read` returns token-activity summaries and daily buckets, not remaining monetary credit. All optional fields can be absent. [Official app-server account methods](https://learn.chatgpt.com/docs/app-server#6-rate-limits-chatgpt).

Implemented collector: run the installed native app-server on private stdio in the controller's account context; initialize, read the documented quota method, then close. Only normalized usage fields are retained. Unsupported response shapes and methods produce an unavailable result. Token-activity methods are not called.

Live read-only check: Codex **0.163.0** successfully answered `account/rateLimits/read` through its native app-server, without a model turn or an account-identity request. This account reported one weekly window (`windowDurationMins: 10080`) with numeric `usedPercent` and `resetsAt`; its secondary window was null. The optional credit object contained `hasCredits`, `unlimited` and a non-null `balance`. Personal usage values were retained only in private evidence. This verifies the installed quota interface, not `account/usage/read`, every plan's schema, or a fixed pair of allowance windows.

The human fallback is `/status` in Codex or the [Codex usage dashboard](https://chatgpt.com/codex/settings/usage). OpenAI explicitly separates API token prices from subscription usage, so API list prices cannot determine included tasks remaining. [Pricing and usage guidance](https://learn.chatgpt.com/docs/pricing#where-can-i-see-my-current-usage-limits).

For a future managed product, the current app-server documentation distinguishes local/open-source applications from commercial hosted services and directs the latter to Sign in with ChatGPT. Evaluate that supported integration before reusing this owner-operated collector commercially. [App-server authentication scope](https://learn.chatgpt.com/docs/app-server#auth-endpoints).

## Claude Code

The supported status-line input includes `rate_limits.five_hour.used_percentage`, `rate_limits.seven_day.used_percentage`, and each window's `resets_at`. Documentation limits this subscription surface to Pro/Max, after the session's first API response; windows may independently disappear after reset. Gateway spend limits are a separate case. `context_window.remaining_percentage` measures conversation capacity, not subscription allowance. [Status-line schema and absence rules](https://code.claude.com/docs/en/statusline#available-data).

Proposed collector: a small observer in the documented status-line command writes only those usage fields and observation time to a private local file. Preserve any existing status-line command/output. Collect observations from normal user work; never generate a paid response merely to populate the widget. A missing or expired observation displays **Not reported**, not 100% remaining. Full subscription-login behavior and delegated model-only token behavior need separate verification.

During the separately authorized native acceptance run, Claude Code **2.1.302** also emitted `rate_limit_event` in `--output-format stream-json`. Its `rate_limit_info` contained `status`, `resetsAt`, `rateLimitType`, overage flags/reset fields and `unifiedWindows.five_hour` / `seven_day` with `utilization` and `resetsAt`. These are observed field names, not a verified universal schema or numerical balance. A future stream collector should validate units and absence behavior for the installed version before normalizing them; it can reuse responses from ordinary work.

The later delegated-access acceptance also emitted this event on both turns using `CLAUDE_CODE_OAUTH_TOKEN` from native settings. Optional fields differed: `utilization` and `surpassedThreshold` appeared, while `overageInUse` was absent. This confirms observations can arrive during delegated model use and reinforces the need to tolerate missing fields; it does not establish an on-demand account-balance endpoint.

Native `/usage` shows plan bars and local activity; `/cost` and `/stats` are aliases in current documentation. Its local attribution excludes other devices. Session dollar costs are estimates, not subscription charges. Credit spending/limits and prepaid balance are also distinct: the native credit row is not a general balance API. The provider's [Usage settings](https://claude.ai/settings/usage) show the available credit balance. A documented standalone personal balance JSON endpoint was not established by this research. [Usage, credit spending and billing distinctions](https://code.claude.com/docs/en/costs).

Optional later work: native `/usage` capture with a pinned terminal parser if an on-demand refresh is needed; prove it performs no model turn and handles last-known data explicitly. OpenTelemetry supplies local token/cost observations but cost values remain estimates. Enterprise Analytics and API Console reporting have different account scopes. [Native monitoring](https://code.claude.com/docs/en/monitoring-usage#cost-monitoring), [team analytics](https://code.claude.com/docs/en/analytics).

## Cursor

Current documentation distinguishes newer shared weekly subscription pools from legacy monthly Cursor pools. Reset timing and remaining allowance appear in the provider's usage UI; prepaid credit and additional spend caps are separate. Do not hardcode the older Pro/Pro+/Ultra dollar allowances or assume a billing cycle from a successful login. [Usage and limits](https://cursor.com/help/models-and-usage/usage-limits).

The current native CLI reference documents authentication status, not a personal remaining-balance command. `agent status --format json` was verified locally to return authentication fields; it does not establish subscription credit. The first version should offer **Open provider usage** and say **Balance unavailable through the supported integration**. Use the account's applicable dashboard: current plans route through the usage page described in the provider docs; legacy plans use Cursor's Spending tab. [CLI reference](https://cursor.com/docs/cli/reference/parameters).

An optional Enterprise integration can use the documented Admin API with a separately supplied admin credential. Spending data includes `spendCents`, `overallSpendCents`, per-user limits and `subscriptionCycleStart`; usage events provide activity detail. These are reporting/cap fields, not automatically prepaid balance. The current API overview lists Admin and Analytics access for Enterprise teams. Verify the account's access before enabling the connector; do not reuse a native browser session token against undocumented endpoints. [Admin API](https://cursor.com/docs/account/teams/admin-api), [API availability](https://cursor.com/docs/api).

## Controller-level accounts and delegated workspaces

Keep the canonical account observation in Agent Access and attach workspaces to that account record. Workers can contribute local activity, but shared provider windows must be deduplicated rather than summed. Preserve the provider's account/workspace scope without exposing account emails in routine telemetry.

Supported credential mechanisms differ:

- **Claude:** `claude setup-token` creates a one-year subscription OAuth token for `CLAUDE_CODE_OAUTH_TOKEN`. It supports model requests, but not Remote Control or claude.ai connectors. Do not assume it grants account-usage API access. [Long-lived tokens](https://code.claude.com/docs/en/authentication#generate-a-long-lived-token).
- **Cursor:** user API keys through `CURSOR_API_KEY` are the documented automation method. This is a Cursor credential; it must not be labeled an OpenAI/Anthropic API billing account. A documented browser-token delegation contract was not found. [CLI authentication](https://cursor.com/docs/cli/reference/authentication).
- **Codex:** `CODEX_ACCESS_TOKEN` and `codex login --with-access-token` are for workspace access tokens, currently Business/Enterprise. They are not browser OAuth-token import flags. The experimental `chatgptAuthTokens` route is separate. [Workspace access tokens](https://learn.chatgpt.com/docs/enterprise/access-tokens), [external-token app-server interface](https://learn.chatgpt.com/docs/app-server#3c-log-in-with-externally-managed-chatgpt-tokens-chatgptauthtokens).

Pinned Codex 0.163.0 source accepts `auth_mode: chatgptAuthTokens` from auth storage and skips native refresh for that mode, but its protocol source labels the interface internal/unstable. Direct file delegation therefore needs a version gate and regression coverage; it is not a stable documented CLI import contract. Never distribute refresh tokens merely to avoid this compatibility work. [Native loader and refresh implementation](https://github.com/openai/codex/blob/rust-v0.163.0/codex-rs/login/src/auth/manager.rs), [protocol caveat](https://github.com/openai/codex/blob/rust-v0.163.0/codex-rs/app-server-protocol/src/protocol/v2/account.rs).

## Dashboard contract and validation

Keep independent fields for `connection`, `plan` (nullable), `allowance_windows[]`, `credits` (nullable, provider unit preserved), `spend`, `activity`, `source`, `observed_at` and `freshness`. Derive a window's remaining percentage as `max(0, 100 - used_percentage)` only from a valid current observation. Preserve model/bucket labels. Never manufacture a fresh allowance when a countdown reaches zero; refresh or show stale data.

Delivery status:

1. **Implemented:** provider usage links and explicit unsupported/stale states for all three accounts.
2. **Implemented:** Codex native read-only usage collector, with bounded refresh frequency and unsupported-method handling.
3. **Planned:** Claude status-line observations, preserving existing configuration.
4. **Planned:** optional Cursor Enterprise reporting after an admin credential is configured.

Acceptance should compare the same account and observation time with native/provider UI, verify missing data and reset boundaries, and prove two workspaces using one account show one shared allowance. Test logged-out, unsupported-method, expired delegated credential, paused-worker and API-key modes. No background paid inference, automatic credit purchase, limit change, reset redemption or admin notification belongs in a usage refresh.

Observed native versions: Codex **0.163.0**, Claude Code **2.1.302**, Cursor Agent **2026.10.16-e3cb207**. Usage interfaces above were researched in official documentation; the Claude stream event is an additional live observation. A small Cursor inference attempt returned a provider team-usage-limit error and a reset date despite successful account authentication. No balance endpoint or billing setting was changed. See [native acceptance results](reports/VALIDATION.md#native-inference-and-conversation-resume) for the separate inference tests.

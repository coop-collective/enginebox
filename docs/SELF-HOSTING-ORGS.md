# Self-hosting orgs and email sign-in

This is the checklist for turning on [organizations and email sign-in](ORGS.md) on a self-hosted Engine Box behind Cloudflare. It assumes the dashboard already runs on your controller host behind a Cloudflare Tunnel and the site Worker (`packages/site`), as in [Dashboard](DASHBOARD.md).

Never paste secrets into the repository, an issue or a log. Generate them on the machine that stores them and pipe them straight into `wrangler secret put` and the private `environment.json`.

## Checklist

- [ ] **1. Domain on Cloudflare.** Register a domain with Cloudflare Registrar (Dashboard → Domain Registration → Register Domains), or add an existing domain as a zone (Dashboard → Add a domain) and switch its nameservers. Email Sending needs the zone on Cloudflare DNS.
- [ ] **2. Email Sending on the domain.** Onboard it and let Cloudflare add the SPF and DKIM records:
  ```sh
  npx wrangler login                      # once, in a browser
  npx wrangler email sending enable example.com
  npx wrangler email sending dns get example.com   # shows the records it needs; check they exist
  ```
  Or Dashboard → Compute & AI → Email Service → Email Sending → Onboard Domain → **Add records and onboard**. DNS usually propagates in 5–15 minutes. Add a DMARC record if the domain has none, for example `_dmarc TXT "v=DMARC1; p=quarantine; rua=mailto:dmarc@example.com"`.
- [ ] **3. Pick the sender**, an address on that domain such as `login@example.com`. It only sends; it needs no mailbox.
- [ ] **4. Deploy the email gateway Worker** (`packages/email-gateway`):
  ```sh
  cd packages/email-gateway
  ENGINEBOX_EMAIL_ACCOUNT_ID=<32-hex account id> \
  ENGINEBOX_EMAIL_FROM=login@example.com \
  ENGINEBOX_DASHBOARD_ORIGIN=https://enginebox.example.com \
  pnpm configure        # writes the private .state/email-gateway/wrangler.json
  pnpm deploy
  openssl rand -hex 32 | pnpm secret     # sets EMAIL_GATEWAY_SECRET without echoing it
  ```
  Keep the same value for the dashboard (step 6): generate it once into a private file (`umask 077; openssl rand -hex 32 > ~/.enginebox-email-secret`), pipe that file into `pnpm secret`, and copy it into `environment.json`.
- [ ] **5. Redeploy the site Worker** so it has the `EMAIL_GATEWAY` service binding (`packages/site/wrangler.jsonc`): `cd packages/site && pnpm deploy`. Deploy the gateway first; a missing service fails the site deploy.
- [ ] **6. Configure the dashboard.** Add to the supervisor's private `environment.json` (mode 0600):
  ```json
  {
    "ENGINEBOX_DASHBOARD_ORIGIN": "https://enginebox.example.com",
    "ENGINEBOX_IDENTITY_STATE_DIR": "/opt/enginebox-control/state/identity",
    "ENGINEBOX_OWNER_EMAIL": "you@example.com",
    "ENGINEBOX_EMAIL_ENDPOINT": "https://enginebox.example.com/_email/send",
    "ENGINEBOX_EMAIL_SECRET": "<the value from step 4>",
    "ENGINEBOX_CLIENT_IP_HEADER": "cf-connecting-ip"
  }
  ```
  The dashboard origin must be exactly the origin users open (links in emails must start with it, and the gateway refuses others). Use `cf-connecting-ip` only when the dashboard is reachable solely through the tunnel. A supervisor older than this release refuses unknown keys: either update the supervisor binary, or set these in the supervisor's service environment (systemd `Environment=`/`EnvironmentFile=` with mode 0600), which its children inherit.
- [ ] **7. Restart the supervisor** (or run the normal update). Sign in at the dashboard with your email. The **Organization** view should show *Default Org*, you as owner and instance owner, and no "Email sending is not configured" warning.
- [ ] **8. Invite or carry over your team.** **Organization → Invite someone**, or **Carry members over** with one email per line (for example your Ideator members, matched by email).

If you did not set `ENGINEBOX_OWNER_EMAIL`, sign in with the operator password (the labelled break-glass login), open **Organization → Name the instance owner** and verify the owner's email with the code it receives.

## Managed mode

A hosted deployment sets `ENGINEBOX_TENANCY_MODE=managed` on both the dashboard and the controller. Anyone can then sign up by email and gets a personal org; the platform org is the provider's, and its pools reach tenants only through capacity grants. Metering and billing plug in through the extension points in [ORGS.md](ORGS.md#extension-points-managed-offering); they are not part of this repository.

## Troubleshooting

| Symptom | Check |
| --- | --- |
| "Email sending is not configured" | `ENGINEBOX_EMAIL_ENDPOINT` and a ≥ 32-character `ENGINEBOX_EMAIL_SECRET` are set for the dashboard process. |
| Codes never arrive; dashboard logs `identity_email_rejected` with 401 | The dashboard and gateway secrets differ, or the host clock is more than 5 minutes off. |
| `identity_email_rejected` with 400 | `ENGINEBOX_DASHBOARD_ORIGIN` differs from the gateway's `DASHBOARD_ORIGIN`. |
| 503 from `/_email/send` | The site Worker has no `EMAIL_GATEWAY` binding, or the gateway is not deployed. |
| Gateway logs `email_gateway_failed` `E_SENDER_NOT_VERIFIED` / `E_SENDER_DOMAIN_NOT_AVAILABLE` | The domain is not onboarded to Email Sending or DNS has not propagated (step 2). |
| Everyone shares one rate-limit bucket | Set `ENGINEBOX_CLIENT_IP_HEADER`. |

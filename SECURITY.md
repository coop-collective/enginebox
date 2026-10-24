# Security policy

## Reporting a vulnerability

Please report suspected vulnerabilities privately through GitHub Security Advisories: open the repository's **Security** tab on [coop-collective/enginebox](https://github.com/coop-collective/enginebox) and choose **Report a vulnerability**. Do not file a public issue or pull request for an unfixed vulnerability.

Include the affected version or commit, the component (controller, node, dashboard, installer, storage), reproduction steps and the impact you observed. Never include real credentials, host inventories, provider tokens or customer data; use disposable fixtures.

We aim to acknowledge reports promptly and will coordinate disclosure with you through the advisory. Engine Box is an early development preview run by a small team, so response times are best effort.

## Scope and supported versions

Only the latest commit on `main` and the most recent tagged release receive fixes. Engine Box is currently a single trusted-owner tool; hostile multi-tenant isolation is not a supported claim (see the [design](docs/DESIGN.md) and [validation reports](docs/reports/VALIDATION.md) for the current qualification boundary). Reports about provider CLIs themselves belong with their vendors.

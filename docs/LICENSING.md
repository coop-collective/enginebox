# Engine Box licensing

New Engine Box versions use the **Sustainable Use License 1.0**, the source-available license originally published by n8n. The complete, unmodified terms are in [LICENSE](../LICENSE). This is not an OSI open-source license: it restricts use and commercial distribution. Engine Box can offer separate commercial agreements for uses outside those terms.

The intended product boundary is free personal and internal company self-hosting, with commercial hosting or redistribution requiring a separate agreement. Source availability does not grant ownership of the project or unrestricted resale rights.

| Use | Community license boundary |
|---|---|
| Run your agents on your own machines for personal work | Permitted personal use. |
| Run Engine Box internally for your company's development team | Permitted internal business use. |
| Modify the software for those uses | Permitted, subject to the license and notices. |
| Share copies free of charge for noncommercial purposes | Permitted, retaining the license and required notices. |
| Sell access to hosted Engine Box workspaces, its dashboard, API or MCP | Requires a separate commercial agreement. A different frontend does not change the underlying use. |
| Sell an Engine Box distribution or white-label hosted service | Requires a separate commercial agreement. |
| Charge for helping a customer install and maintain its own internal instance | Installation/support services are distinct from selling software access. The customer's use must remain within its own license. |
| Build independent software without using Engine Box code | Not controlled by the Engine Box license. |

These examples explain the intended boundary; the license text governs. This policy does not grant rights to provider subscriptions, third-party packages, Windows/macOS, Unreal/Unity, customer code or trademarks. Those retain their own terms.

## Historical Apache grant

Engine Box was already public under Apache-2.0 through commit `19f834a344d91bb1e6aa1cc46841ad42eea830ee`, including the then-public development branch. Those grants remain in effect. Merging that branch, tagging a release or changing LICENSE cannot revoke them. The [historical license](../LICENSE-APACHE-2.0) and [NOTICE](../NOTICE) preserve this distinction.

The new release identifies the licensing transition for subsequent work. It does not prevent someone from using the previously Apache-licensed version under its existing terms, and it cannot guarantee that no competitor can exist.

## Contributions and packaging

New contributions are accepted under the current repository license unless an explicit agreement says otherwise. Contributors must have authority to contribute their work. There is no contributor license agreement or blanket relicensing consent in place; do not assume that a future license change can relicense independent contributions without the necessary rights.

Runtime and dashboard packages carry LICENSE, NOTICE and the historical Apache license. Cargo and npm metadata point to LICENSE. The release builder includes LICENSE, NOTICE and the historical Apache license. Third-party licenses remain separate. The n8n-specific enterprise-file and unlicensed-branch exclusions are not part of Engine Box's policy.

## Why this license

PolyForm Perimeter also addresses competing products, but allows broader noncompeting uses. The Sustainable Use License expresses the chosen personal/internal-use and commercial-distribution boundary more directly. Elastic License 2.0 focuses on managed-service restrictions. Neither a no-competition condition nor an internal-use restriction qualifies as OSI open source.

Sources reviewed October 19, 2026: [n8n license text](https://github.com/n8n-io/n8n/blob/master/LICENSE.md), [n8n use-case guidance](https://support.n8n.io/article/can-i-use-your-license-for-my-use-case), [permission to reuse the license](https://blog.n8n.io/announcing-new-sustainable-use-license/), [PolyForm Perimeter](https://github.com/polyformproject/polyform-licenses/blob/1.0.0/PolyForm-Perimeter-1.0.0.md), [Apache-2.0 copyright grant](https://www.apache.org/licenses/LICENSE-2.0), and [Open Source Definition](https://opensource.org/osd).

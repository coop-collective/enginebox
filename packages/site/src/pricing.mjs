const esc = value => String(value).replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
const licenseDoc = 'https://github.com/coop-collective/enginebox/blob/main/docs/LICENSING.md';
const licenseText = 'https://github.com/coop-collective/enginebox/blob/main/LICENSE';

export const pricingTitle = 'Pricing';
export const pricingDescription = 'Engine Box is free for personal, non-commercial and non-production use. Self-hosting for business costs $10 per node per month.';

// checkoutUrl is the build-time CHECKOUT_URL. Without it the button is disabled: no checkout exists yet.
export const pricing = checkoutUrl => {
  const buy = checkoutUrl
    ? `<a class="button primary" href="${esc(checkoutUrl)}" data-checkout>Get a license <span aria-hidden="true">↗</span></a>`
    : '<button class="button primary" type="button" disabled data-checkout>Checkout opening soon</button>';
  return `
<main id="main">
<section class="pricing-hero wrap">
  <p class="eyebrow"><span class="tiny-square"></span> PRICING</p>
  <h1>Free to try.<br><span>$10 a node to run.</span></h1>
  <p class="hero-lede">Engine Box is source-available under the Business Source License 1.1. Personal and non-production use is free. Businesses pay per machine that runs workspaces. Nothing else is metered.</p>
</section>
<section class="wrap plans" aria-label="Plans">
  <article class="plan">
    <p class="plan-tag">PERSONAL</p>
    <h2>Free</h2>
    <p class="plan-lede">Personal and non-commercial use, plus evaluation and other non-production use.</p>
    <ul>
      <li>Run it on your own machines for personal or non-commercial work</li>
      <li>Test, evaluate and develop in any organization</li>
      <li>Modify the source under the same license</li>
      <li>Community support through GitHub</li>
    </ul>
    <a class="button secondary" href="/docs/quickstart/">Start from source <span aria-hidden="true">↗</span></a>
  </article>
  <article class="plan plan-featured">
    <p class="plan-tag">SELF-HOSTED</p>
    <h2>$10 <small>/ node / month</small></h2>
    <p class="plan-lede">Production use by or on behalf of a business, on hardware you control.</p>
    <ul>
      <li>Unlimited workspaces per node</li>
      <li>All features in this repository</li>
      <li>Your machines, your accounts, your provider subscriptions</li>
      <li>Community support through GitHub</li>
    </ul>
    ${buy}
    ${checkoutUrl ? '' : '<p class="plan-note">There is no checkout yet. The button will become active when licenses can be purchased.</p>'}
  </article>
  <article class="plan plan-planned">
    <p class="plan-tag">MANAGED HOSTING</p>
    <h2>Coming soon</h2>
    <p class="plan-lede">Engine Box run for you. This is planned and is not available today.</p>
    <ul>
      <li>Not offered yet; no pricing or dates</li>
      <li>Offering Engine Box to third parties as a hosted service requires a separate agreement</li>
    </ul>
    <p class="plan-note">Watch the <a href="https://github.com/coop-collective/enginebox">repository</a> for updates.</p>
  </article>
</section>
<section class="node-section wrap" id="nodes">
  <div class="section-heading"><p class="eyebrow">WHAT COUNTS AS A NODE</p><h2>One machine,<br>one node.</h2></div>
  <div class="node-copy">
    <p>A node is each machine registered with an Engine Box controller to run workspaces. A machine counts once, however many workspaces, templates or users it hosts.</p>
    <p>The controller itself does not count unless it also runs workspaces, that is, unless it is also registered as a node. A machine that only serves the dashboard, API or MCP is not a node.</p>
    <div class="node-example"><p class="diagram-label">WORKED EXAMPLE</p><div class="state-line"><span>Controller (no workspaces)</span><b>0 nodes</b></div><div class="state-line"><span>3 Linux hosts registered to run workspaces</span><b>3 nodes</b></div><div class="state-line node-total"><span>3 nodes × $10</span><b>$30 / month</b></div></div>
    <p class="small-note">The software does not currently enforce node counts. The license is the agreement. See <a href="/docs/licensing/">Licensing and pricing</a> for the full boundary.</p>
  </div>
</section>
<section class="faq-section wrap" id="faq"><div><p class="eyebrow">PRICING QUESTIONS</p><h2>The fine<br>print, plainly.</h2></div><div class="faq-list">
<details><summary>What counts as non-commercial?<span>＋</span></summary><p>Running Engine Box on your own machines for personal projects, study, hobby work or research that is not part of a commercial product is free. A company's development team, or a contractor running it for a client business, is a business use and needs a license. If your case is unclear, ask in a GitHub issue before relying on the free grant.</p></details>
<details><summary>Can I evaluate it for free?<span>＋</span></summary><p>Yes. Testing, evaluation and development are non-production use and are free, including inside a company. Running real work on it for a business is production use and needs a license for each node.</p></details>
<details><summary>Can I modify it?<span>＋</span></summary><p>Yes. You can copy, modify and redistribute the source. Modified copies and derivative works are covered by the same license, and you must keep the license on every copy.</p></details>
<details><summary>What happens at the Change Date?<span>＋</span></summary><p>Each version converts to the Apache License, Version 2.0 four years after it is first publicly released. From then on that version is free for any use. Later versions have their own dates.</p></details>
<details><summary>Does the price include Codex, Claude or Cursor?<span>＋</span></summary><p>No. Provider subscriptions and usage stay with the provider. Engine Box runs the official clients and does not resell inference.</p></details>
<details><summary>Is the node count enforced automatically?<span>＋</span></summary><p>No. The software does not currently enforce node counts. The license is the agreement, and you are responsible for licensing the nodes you run in production.</p></details>
<details><summary>Can I offer Engine Box as a hosted service?<span>＋</span></summary><p>Not under the free grant or the per-node license. Offering it to third parties as a hosted or managed service requires a separate agreement. Read the <a href="${licenseText}">license</a> and <a href="${licenseDoc}">licensing notes</a> for the exact terms.</p></details>
</div></section>
</main>`;
};

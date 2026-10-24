export function preparationWorkflow(input: { group: string; label: string; action: string; checkout: string }) {
  if (!/^[a-zA-Z0-9_. -]{1,100}$/.test(input.group)) throw new Error("Enter the GitHub runner group name.");
  if (!/^enginebox-[a-zA-Z0-9_-]+$/.test(input.label)) throw new Error("Select a prepared Enginebox pool.");
  if (!/^[a-zA-Z0-9_.-]+\/[a-zA-Z0-9_.-]+\/packages\/enginebox-actions@[a-f0-9]{40}$/.test(input.action)) throw new Error("Pin the Enginebox action to a full commit SHA.");
  if (!/^actions\/checkout@[a-f0-9]{40}$/.test(input.checkout)) throw new Error("Pin actions/checkout to a full commit SHA.");
  return `# Merge this job fragment into an approved workflow after reviewing its checkout and test requirements.
# Keep checkout options required by your repository. Review lifecycle scripts before choosing deny.
# Use the Node and pnpm versions pinned in the runner template.
jobs:
  prepared-test:
    runs-on:
      group: ${JSON.stringify(input.group)}
      labels: ${JSON.stringify(input.label)}
    timeout-minutes: 20
    permissions:
      contents: read
    steps:
      - uses: ${input.checkout}
        with:
          persist-credentials: false
      - uses: ${input.action}
        with:
          install-scripts: deny
          # Set true for a warm canary that must restore an exact generation.
          require-hit: 'false'
      - run: pnpm test
`;
}

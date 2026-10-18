# Working on Engine Box

- Keep provider harnesses native. Environment lifecycle tools are allowed; credential proxies, replacement agent loops and pooled subscription authentication are outside scope.
- Keep the core runtime independent of GitHub and of the optional CI module.
- Treat `docs/DESIGN.md` and `docs/IMPLEMENTATION-PLAN.md` as the current scope. Explicitly label proposed commands and unimplemented capabilities.
- Never put host inventories, account state, provider tokens, GitHub registration tokens, SSH keys, real customer projects or VM snapshots in Git.
- Bootstrap must inspect before changing a host. Never repartition disks, overwrite unrelated profiles or change global NFS/firewall services implicitly.
- Preserve the distinction between a credential-free reusable base and a private checkpoint that includes credentials and RAM.
- Run offline checks with `uv sync --project runtime --locked` and `runtime/.venv/bin/python -m unittest discover -s tests -v`.
- Live integration tests must create disposable environments. Do not pause, restore or stop a developer's workspace as a test fixture.
- Use exact provider documentation and record tested versions. Installation success is not proof of subscription eligibility or successful inference.
- Keep changes small and identify what was verified versus what remains planned.

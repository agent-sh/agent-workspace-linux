## Summary

What does this change and why?

## Checklist

- [ ] `cargo fmt --check` passes
- [ ] `cargo clippy --locked -- -D warnings` passes
- [ ] `cargo test --locked` passes
- [ ] `git diff --check` is clean (no trailing whitespace / conflict markers)
- [ ] `scripts/integration_smoke.sh` run for changes touching runtime behavior (MCP tools, permissions, workspace control, viewer, browser) — or N/A
- [ ] No secrets, credentials, personal data, or machine-specific absolute paths added
- [ ] Docs updated if behavior, flags, or the permission/trust model changed

## Notes for reviewers

Evidence the change works (test output, smoke excerpt, `doctor` output), and
anything that affects the action marks, the app allowlist or live control.

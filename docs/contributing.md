# Contributing

## Rules

- Keep one canonical runtime and public vocabulary.
- Add tests with behavior.
- Do not bypass the policy engine or event ledger.
- Never expose secrets in prompts, events, logs, or fixtures.
- Mark experimental behavior explicitly.
- Record copied/adapted upstream code in
  `licenses/UPSTREAM_ATTRIBUTION.md`.

## Checks

```powershell
cd 'F:\Project OpenSource\app'
cargo fmt --all -- --check
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p opensrc-cli -- validate-agents agents
& 'F:\Project OpenSource\scripts\smoke-runtime.ps1'
```

## Decision changes

Update `architecture/decision-log.md` with Problem, Options considered, Codex
behavior, Gemini behavior, Chosen approach, Reason, Tradeoffs, and Future
migration path.

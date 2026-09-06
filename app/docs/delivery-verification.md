# Desktop delivery verification

## Verified workflow

The live follow-up run `83f1cce1-ec16-4930-98d2-55d8999b711c` completed using
the configured OpenRouter `google/gemini-3.6-flash` provider. It planned a release
task against the existing Desktop calculator, inspected configuration, ran tests,
started its Node HTTP server, checked the response, stopped its tracked process,
and completed the task and root run. The latest path and command-identity fixes
were active. No source edits were needed during this verification run.

The calculator's five Node subtests passed. The HTTP check returned 200 and HTML.
Separate browser checks observed button input `7 * 8 = 56` and actual keyboard
input `9 / 3 = 3`. The unit suite's keyboard case simulates mapping logic; it is
not a replacement for browser interaction testing.

## Repeatable regression coverage

From `app`, with Rust, Node and npm installed:

```text
cargo test -p opensrc-runtime --test delivery_acceptance
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

The acceptance fixture uses a deterministic provider boundary and real files,
Node tests, and HTTP subprocesses. It verifies planning before folder creation,
a failing check, rejection of premature completion, repair and passing rerun,
independent release checks, persisted delivery scope, and zero approvals.
On Windows it exercises repeated path separators and `npm.cmd test` matching
the exact `npm test` contract. A timeout catches silent approval/process stalls.

## Limits

This is evidence for one local application workflow, not universal autonomous
delivery or production-readiness certification. Earlier live attempts failed
and were retained as failures; the successful run was a follow-up on existing
artifacts. Model quality, dependencies, test coverage and execution budgets
still matter. Destructive actions and publishing retain their approval gates.
The model's final prose is not authoritative evidence that there are no defects.

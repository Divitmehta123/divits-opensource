---
name: coding-delivery
description: Deliver source changes through inspection, implementation, verification, and a concrete handoff.
triggers: [implement, build, edit, fix, code]
---
Inspect the relevant implementation and tests before editing. Make the smallest coherent change
that completes the assigned contract, preserve unrelated work, and follow repository conventions.
Re-read every changed call site, format with the native toolchain, run focused validation, then run
the broader applicable suite. Do not finish with sample code or instructions when tools can produce
and verify the requested artifacts directly.

For a new application, work in the delivery folder selected by the plan, not the coding agent's
installation directory. Keep source, assets, tests, manifests, lockfiles and README together. Use
fs.write for new files (especially on Windows; Bash heredocs are not PowerShell). Install only the
dependencies the app needs. Run real automated tests and the production build where applicable.
Start the application, check its actual response/behavior, and stop temporary smoke-test processes.
Leave a reproducible startup command in README and report the absolute project path. Never count
Git metadata, file listing, a successful process spawn, or a written test file as a passing app test.

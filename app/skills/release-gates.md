---
name: release-gates
description: Derive and execute repository-native release checks without hiding skipped or failed gates.
triggers: [release, package, publish, smoke test]
---
Read project manifests and automation to derive the real release gates. Run formatting checks,
strict linting, unit and integration tests, optimized builds, binary smoke tests, and artifact
inspection as applicable. Confirm artifacts came from the tested source. Report warnings, skipped
checks, environment constraints, and upgrade risks explicitly; never publish without separate
authorization.

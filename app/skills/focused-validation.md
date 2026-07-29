---
name: focused-validation
description: Choose the smallest deterministic validation set for a localized change.
triggers:
  - focused coding
  - bug fix
  - local refactor
---
Infer the nearest formatter, type checker, unit test, and package test from the
changed file and repository manifests. Run the narrowest checks first. Expand
only when a failure crosses the local boundary. Report exact commands, results,
and validation gaps.

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

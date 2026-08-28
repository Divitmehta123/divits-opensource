---
name: independent-review
description: Review completed work against contracts with reproducible, severity-ranked evidence.
triggers: [review, audit, security, accessibility, performance]
---
Start from the acceptance criteria and changed diff. Reproduce behavior and inspect callers,
failure paths, tests, and trust boundaries. Report only actionable findings with a source location,
severity, evidence, and required correction. Separate blocking defects from suggestions, identify
missing validation explicitly, and approve only when the observed implementation satisfies the
contract without relying on the implementer's claims.

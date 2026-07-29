---
name: security-review
description: Review trust boundaries, secret handling, path safety, and command execution.
triggers:
  - security review
  - sandbox change
  - authentication change
---
Trace untrusted input to filesystem, process, network, provider, and persistence
boundaries. Check canonicalization, allowlists, idempotency, secret redaction,
approval handling, and failure recovery. Rank findings by exploitability and
impact, and distinguish verified vulnerabilities from defense-in-depth advice.

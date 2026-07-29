# Security

## Implemented foundations

- Explicit Agent tool allow/deny policy
- Read-only workspace write denial
- Provider keys kept out of canonical requests/events
- SQLite foreign keys and transactional domain/event writes
- Unique tool idempotency-key schema
- Explicit lifecycle validation
- Bounded Agent depth/count and wait timeout
- Structured delegated completion
- Canonical workspace path containment and parent-traversal rejection
- Hash-guarded full writes and unified patches
- Direct program/argument process launch with timeout
- Child-process environment cleared and rebuilt from a small safe allowlist
- Completed tool replay and in-flight tool-call refusal

## Open security work

- Platform-enforced filesystem/network/process sandboxes
- Platform-specific secret/keyring integration
- Additional symlink/junction and race-condition adversarial tests
- Approval UX and organization policy merging
- Plugin signature and permission verification
- MCP/browser isolation
- Worktree ownership/conflict enforcement
- Prompt-injection adversarial tests
- Crash recovery for in-flight tool calls

No experimental capability graduates until its enforcement and recovery tests
pass.

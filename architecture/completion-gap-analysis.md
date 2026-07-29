# Completion gap analysis

## Acceptance gate

`opensource` now launches one integrated, chat-first terminal client and local
server. The verified path covers provider setup, streamed multi-turn chat,
durable sessions, native tool loops, interactive approvals, reversible edits,
tests/process output, and executable child agents.

Database projections are not treated as proof of agent execution. Agent
behavior is covered by deterministic provider tests that execute parent and
child model loops and exchange structured completions.

## Current subsystem status

| Concern | Current implementation | Status |
|---|---|---|
| Conversation protocol | Typed content blocks preserve provider/canonical tool IDs, results, errors, approvals, summaries, attachments, usage, and continuations | Working |
| Persistence | User-level SQLite conversations, ordered messages, runs, selections, approvals, permissions, changes, checkpoints, tasks, agents, calls, timings, and events | Working |
| Providers | OpenAI-compatible and native Gemini request/stream/tool adapters, model listing, retries, fallback chains, connection tests | Working; other named families use the compatible adapter |
| TUI | Chat default, multiline editor, selection, clipboard, external editor, completion, streaming, pickers, approvals, diffs, sessions, agents/tasks/context/tools/MCP/metrics/logs/settings | Working |
| Core tools | File read/search/symbol/image/fetch/edit/move/delete/copy, patch, shell/test/process control, Git inspect and mutation | Working |
| Changes | Preimage checks, provenance, automatic/manual checkpoints, patch display, safe undo/redo/restore | Working for text patches |
| Agentic runtime | JSON planning, DAG validation, dependency scheduling, concurrent read-only/disjoint-owned agents, real model loops, spawn/message/wait/interrupt, synthesis | Working; write agents without disjoint ownership serialize |
| Skills | Embedded/project/user discovery, triggers, explicit activation, resources, CLI create/validate/enable/disable | Working |
| Custom commands | Project/user Markdown, YAML/TOML metadata, namespaces, completion, positional/named expansion, tool and mode/agent/model preferences | Working |
| MCP | Persistent stdio/HTTP configs, enable/disable/remove, initialization, discovery, approved invocation, debug projection | Working; OAuth is absent and SSE support is response parsing rather than a persistent transport |
| Server | Versioned routes, SSE, structured errors, attach, bearer auth off-loopback, configured CORS, graceful shutdown | Working; OpenAPI remains a compact route document rather than exhaustive generated schemas |
| Packaging | Embedded migrations/agents/skills, release build, Windows installer, PATH setup, five shell completion files | Working |
| Sandbox | Path, ownership, command, network, tool, approval and environment policy | Limited protection, not an OS sandbox |
| Plugins | No install/update/remove lifecycle | Incomplete |
| Browser automation | No isolated interactive browser backend | Incomplete |
| LSP | No language-server process management or semantic queries | Incomplete |
| Worktrees | Git inspection exists; isolated worktree provisioning/merge is not implemented | Incomplete |
| Credentials | Environment references and hidden setup input | OS keyring and multiple credentials are incomplete |

## Safety limitations

- `sandbox:limited` is intentionally visible in the TUI and doctor output.
- Binary file changes can be recorded by hash but are not reversible without a
  text patch.
- MCP commands inherit only an explicit environment mapping; nevertheless,
  external MCP servers are separate programs and should be treated as trusted.
- Approval scopes are application policy, not kernel enforcement.

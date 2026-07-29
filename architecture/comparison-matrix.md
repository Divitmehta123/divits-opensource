# Source-backed comparison matrix

Legend: `CODEX`, `GEMINI`, `MERGE`, `REIMPLEMENT`, `REMOVE`, and `EXPERIMENTAL`.

| Subsystem | Decision | Evidence and target interpretation |
|---|---|---|
| Application entrypoints | MERGE | Codex umbrella CLI plus Gemini's lazy UI loading; expose one binary with `serve`, `tui`, and headless commands. |
| CLI and TUI | CODEX | Ratatui client lifecycle from `codex-rs/tui`; reimplement, do not copy snapshots/widgets. |
| App-server architecture | CODEX | Separate processor/outbound loops in `app-server/src/lib.rs`; use a versioned local HTTP/SSE protocol. |
| Primary model loop | REIMPLEMENT | Both loops leak provider semantics; define provider-neutral cycles/actions/events. |
| Tool-call loop | MERGE | Codex router/orchestrator plus Gemini scheduler state machine and completion enforcement. |
| Streaming | MERGE | Codex durable event mapping with Gemini typed turn events; normalize into provider-neutral deltas. |
| Session lifecycle | CODEX | `ThreadManager`/rollout restoration is stronger; rename to Conversation/Run. |
| Conversation persistence | REIMPLEMENT | Replace Codex JSONL+mirror and Gemini JSONL with one SQLite event ledger. |
| Agent representation | GEMINI | Explicit definitions and output schemas; extend with durable identity and budgets. |
| Root-agent representation | CODEX | Root-scoped shared control plane; make root an ordinary agent with no parent. |
| Subagent creation | MERGE | Codex durable spawn/fork plus Gemini definition-driven construction. |
| Nested agents | CODEX | Persisted parent-child graph and depth accounting. |
| Agent messaging | CODEX | Separate inter-agent communication from executable follow-up. |
| Agent completion | GEMINI | Mandatory `complete_task` schema; persist completion object transactionally. |
| Agent interruption | CODEX | Event-driven interruption and descendant shutdown. |
| Agent restoration | CODEX | Reload unloaded V2 agents from durable history; add idempotent tool records. |
| Agent timeouts | GEMINI | Explicit time/turn budgets and final completion grace semantics. |
| Agent concurrency | MERGE | Codex spawn/execution guards plus provider/global rate limiters. |
| Tool registries | MERGE | Gemini per-agent registry over a Codex-style canonical handler registry. |
| Tool isolation | GEMINI | Allow/deny lists and agent-local exposure are mandatory. |
| Skills | GEMINI | Metadata-only initial exposure and explicit lazy activation. |
| Plugins | REIMPLEMENT | One signed/installable bundle model contributing skills, tools, MCP, and config. |
| MCP | MERGE | Keep protocol clients/managers; normalize MCP tools through the same policy boundary. |
| Browser automation | GEMINI | Optional specialist capability with dedicated process and tool policy. |
| Prompt assembly | REIMPLEMENT | Static/dynamic sections with measured token classes and provider cache hints. |
| Context selection | REIMPLEMENT | Repository maps, selected files, agent-specific history, and deterministic batching. |
| Context compaction | MERGE | Codex compaction budgets plus Gemini session summaries; one pipeline only. |
| Tool-output truncation | MERGE | Deterministic truncation, artifact references, and structured summaries. |
| Provider abstraction | REIMPLEMENT | Capability contract and adapter-native wire formats. |
| Model routing | GEMINI | Benchmark-led routing concepts; no role-to-model hard coding. |
| Sandboxing | CODEX | One policy decision path with platform enforcers and explicit escalation. |
| Approval policies | MERGE | Codex approval/sandbox orchestration plus Gemini scheduler confirmation state. |
| Command execution | CODEX | Process streaming, cancellation, hardening, and exec-server concepts. |
| Patch application | CODEX | Dedicated parser/application boundary with preimage validation. |
| Git integration | MERGE | Codex Git utilities plus Gemini worktree service; add ownership enforcement. |
| Workspace isolation | REIMPLEMENT | Explicit workspace modes, leases, ownership, conflicts, checkpoints, and merge. |
| Telemetry | MERGE | Normalize both into the required performance ledger. |
| Token accounting | REIMPLEMENT | Track instruction, user, context, schema, output, compaction, cache, and inheritance separately. |
| Latency accounting | REIMPLEMENT | Persist phase spans and aggregate per run/agent/provider/tool. |
| Error recovery | MERGE | Codex restart restoration plus Gemini scheduler/tool-call state recovery. |
| Retry behavior | REIMPLEMENT | Adapter-classified errors and persisted idempotency keys; no blind replay. |
| Testing infrastructure | MERGE | Rust unit/integration/snapshot tests plus scenario/eval/benchmark fixtures. |
| Legacy agent runtimes | REMOVE | No V1/V2 or local/experimental competing loops in the new public runtime. |
| Remote A2A | EXPERIMENTAL | Optional adapter after local lifecycle and security contracts are stable. |
| Model marketing defaults | REMOVE | Routing defaults require benchmark evidence. |

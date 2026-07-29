# Codex CLI architecture inventory

This inventory is based on source in `upstream/codex`, not only its README.
Paths below are relative to that directory.

## Entrypoints and clients

- `codex-rs/cli/src/main.rs` is the umbrella executable. It dispatches the
  interactive TUI, noninteractive execution, MCP server, app server, sandbox,
  and diagnostic commands.
- `codex-rs/tui/src/lib.rs` owns the Ratatui client lifecycle. `run_main` feeds
  into `run_ratatui_app`, keeping terminal setup and application behavior in a
  dedicated crate.
- `codex-rs/app-server/src/main.rs` is a standalone server entrypoint.
  `app-server/src/lib.rs` documents separate processor and outbound loops so slow
  client writes do not block JSON-RPC request processing.

## Conversation and model loop

- `codex-rs/core/src/thread_manager.rs` creates, resumes, tracks, and removes
  long-lived threads. `ThreadManagerState` is shared by roots and children.
- `codex-rs/core/src/codex_thread.rs` is the thread-facing API and owns session
  interaction.
- `codex-rs/core/src/client.rs` creates a turn-scoped `ModelClientSession`.
  It reuses a Responses WebSocket connection within a turn, carries sticky
  routing state, prewarms best-effort, retries, and falls back to HTTP Responses.
- The request shape is strongly coupled to OpenAI Responses API types. Provider
  metadata exists, but the main loop is not a wire-neutral provider contract.

## Durable session state

- `codex-rs/state/src/lib.rs` describes SQLite-backed rollout metadata. JSONL
  rollouts remain an important source, with metadata mirrored into SQLite.
- `codex-rs/state/src/migrations.rs`, `sqlite.rs`, and `runtime/recovery.rs`
  implement migrations, database access, and recovery.
- `codex-rs/agent-graph-store/src/store.rs` defines a storage-neutral boundary
  for directional parent-child spawn edges; `local.rs` persists them through the
  state database.
- `codex-rs/core/src/agent/control/spawn.rs` restores V2 agents from rollouts and
  supports fresh, full-history, and truncated-history forks.

## Multi-agent control plane

- `codex-rs/core/src/agent/control.rs` states that one `AgentControl` is shared
  by an entire root session tree. It provides spawning, input, inter-agent
  communication, status subscription, interruption, listing, shutdown, and
  restoration.
- `agent/registry.rs` reserves spawn slots, enforces limits, tracks canonical
  agent paths, roles, nicknames, and root registration.
- `agent/control/execution.rs` separately limits actively executing child turns.
- `agent/control/residency.rs` can unload idle resident V2 agents and reload them
  later from durable history.
- `agent/status.rs` derives agent status from emitted events.
- Model-visible tools are assembled in `core/src/tools/spec_plan.rs`, including
  spawn, send, follow-up, wait, interrupt, resume, close, and list operations.
- Completion is still inferred primarily from lifecycle/events and natural
  output; there is no universal mandatory structured completion object matching
  the target contract.

## Tools and scheduling

- `core/src/tools/registry.rs` stores handlers, exposure, and parallel-safety
  metadata.
- `core/src/tools/router.rs` resolves visible specifications and dispatches
  calls.
- `core/src/tools/parallel.rs` schedules calls concurrently when handlers opt in
  and preserves sequential behavior otherwise.
- `core/src/tools/spec_plan.rs` can register tools while hiding them from a
  model, which is useful for dynamic exposure.
- `core/src/tools/orchestrator.rs` combines approval, filesystem/network policy,
  sandbox selection, retry, and telemetry around executable tools.

## Context and skills

- `core/src/context/` models world state as sections with previous/current
  snapshots so only changed context needs to be rendered.
- `core/src/compact*.rs` implements local and remote compaction paths and token
  budget calculations.
- `core/src/thread_rollout_truncation.rs` implements bounded parent-history
  inheritance for forks.
- `core/src/skills.rs`, the `skills` crate, and app-server skill watching expose
  skill metadata and activation behavior.
- Tool-output truncation is implemented in dedicated utilities and propagated
  through tool runtime types.

## Safety and execution

- `core/src/tools/orchestrator.rs` selects sandbox/approval behavior for a tool
  attempt and handles controlled retry.
- `core/src/exec_policy.rs`, `execpolicy/`, `linux-sandbox/`,
  `windows-sandbox.rs`, `process-hardening/`, and `network-proxy/` split policy
  decisions from platform enforcement.
- `apply-patch/` is a dedicated patch parser/application crate.
- `exec/`, `shell-command/`, and `exec-server/` provide process execution and
  streaming.

## Observability and tests

- Model streaming, sandbox outcomes, tool dispatch, turn timing, token use, and
  retries have explicit telemetry paths.
- The repository has extensive unit, snapshot, integration, and protocol tests.
  The TUI alone contains many snapshots; this is useful evidence of maturity but
  also shows the cost of its current surface area.

## Assessment

Keep the durable topology, app-server boundary, event orientation, sandbox
orchestration, process/patch reliability, and Ratatui discipline. Reimplement
the provider boundary and public vocabulary. Avoid copying the legacy/V2
dualities and do not retain Responses-specific request objects in the canonical
domain.

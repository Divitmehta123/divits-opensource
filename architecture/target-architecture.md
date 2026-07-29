# Target architecture

## Canonical choice

Project OpenSource uses one Rust runtime and one SQLite database. The TUI and
headless CLI are clients of a versioned local app-server protocol. Provider
adapters own wire-specific requests and translate them into canonical model
events.

```text
Ratatui / headless CLI
          |
          v
Local protocol v1 (HTTP + SSE)
          |
          v
Canonical runtime
  | classifier: Direct / Focused / Agentic
  | conversation + run service
  | task DAG scheduler
  | persistent agent control plane
  | context pipeline
  | tool registry + policy engine
  | provider router + adapters
          |
          +--> platform executors / workspaces / browser / MCP
          |
          v
SQLite event ledger + materialized state
```

## Public domain

- **Conversation**: long-lived interaction bound to a project.
- **Run**: one user request and its execution.
- **Agent**: configured persistent worker.
- **Task**: schedulable unit in a dependency graph.
- **Cycle**: model response plus resulting actions.
- **Tool call**: local action requested by a model.
- **Workspace**: files available to an agent.
- **Sandbox policy**: enforceable execution restrictions.
- **Event**: immutable durable fact.

Internal transport attempts, leases, checkpoints, and provider deltas are
implementation types, not alternate public runtimes.

## Crate boundaries

- `opensrc-core`: canonical types, state transitions, capabilities, policies.
- `opensrc-store`: SQLite migrations, transactions, event ledger, projections.
- `opensrc-runtime`: mode classifier, agent/task control, tools, context,
  provider router, recovery.
- `opensrc-server`: versioned local protocol and event streaming.
- `opensrc-cli`: headless commands and Ratatui client.

Dependencies point inward. The store and server depend on core contracts; core
does not depend on UI, HTTP, SQLite, or a provider SDK.

## Execution modes

### Direct

One provider request with minimal instructions and no workspace/tool startup.
Only conversation, model-call, usage, and final events are persisted.

### Focused coding

The runtime exposes bounded file/search tools, hash-guarded full writes and
unified patches, direct argument-array processes, and bounded model/tool cycles.
Validation is an explicit tool call; deterministic validation planning remains
a staged refinement.

### Full agentic

The runtime validates a proposed task DAG, assigns specialist definitions,
leases workspaces, schedules ready tasks, enforces budgets, integrates change
sets, and verifies the result. Models propose work; the runtime owns state.

Escalation creates a durable event and preserves the same Run ID.

## Agent lifecycle

Agents have durable configuration plus volatile execution leases. Allowed
transitions are centrally validated:

```text
Created -> Queued -> Running -> Waiting -> Running
                  \-> Blocked
                  \-> Completed
                  \-> Failed
                  \-> Interrupted
Completed/Failed/Interrupted -> Unloaded -> Restoring -> Queued
```

`complete_task` is the only successful terminal contract for delegated tasks.
Natural-language output alone cannot set `Completed`.

## Task scheduler

Tasks form an acyclic graph. A task becomes ready only when all dependencies
completed successfully. Runtime checks cover:

- global, provider, root, and parent concurrency;
- agent depth and child limits;
- token, cost, time, turn, and retry budgets;
- workspace ownership/lease compatibility;
- required tool and sandbox policy;
- cancellation propagation.

## Provider contract

Each adapter declares capabilities for streaming, tools, parallel calls,
structured output, reasoning controls, prompt caching, continuation, context
reuse, native token counting, multimodal input, thought signatures, and batch
requests.

Canonical requests contain messages, context fragments, exposed tools,
structured-output schema, reasoning preference, sampling settings, cache hints,
and budget. Adapters own authentication, endpoints, wire format, stream parsing,
retry classification, and usage conversion.

## Persistence and recovery

SQLite is the system of record. Domain mutations append an event and update
materialized tables in one transaction. Tool execution uses an idempotency key
and states `planned -> approved -> running -> succeeded|failed|unknown`.
Recovery never replays `running` or `unknown` destructive calls automatically.

## Safety

The policy engine produces a decision from organization, conversation, agent,
workspace, and tool policy. Platform enforcers consume that decision. Prompt
text is never treated as enforcement.

Workspace modes are `shared-readonly`, `shared-write`, `owned-paths`,
`git-worktree`, `temporary-copy`, and `container-isolated`. Parallel writers
require disjoint ownership or isolated worktrees.

## TUI

The Ratatui client renders server projections only. Rendering remains
side-effect free and handles small terminals, loading, empty, error, long-text,
and reconnecting states. It shows run mode, task/agent graph, tools, changes,
tests, usage, cost, latency, approvals, failures, and waiting state.

## Current implementation status

Implemented now: canonical domain and SQLite ledger, Direct/Focused execution,
streaming OpenAI-compatible and Gemini adapters, dynamic policy-filtered tools,
idempotent tool records, lazy Skills, durable agent/task control, SSE protocol,
metrics, and the Ratatui dashboard.

Still staged: automatic Agentic scheduling, platform-grade sandboxes, approval
resumption, browser/A2A, worktree merging, provider pricing/rate limits,
credentialed contract tests, and benchmark-selected routing.

# Gemini CLI architecture inventory

This inventory is based on source in `upstream/gemini-cli`. Paths are relative
to that directory.

## Entrypoints and interface

- `packages/cli/src/gemini.tsx` is the main executable and lazily imports the
  heavy Ink UI.
- `packages/cli/src/ui/App.tsx` selects screen-reader or default layouts.
- The interactive interface and much of the runtime execute in one Node process;
  there is no Codex-style versioned local app-server boundary for the primary
  TUI.

## Primary model loop

- `packages/core/src/core/client.ts` owns chat initialization, history,
  bounded turns, model selection, recursive continuation, and streaming.
- `packages/core/src/core/turn.ts` calls `sendMessageStream` and translates
  provider chunks into typed stream events.
- `packages/core/src/scheduler/scheduler.ts` controls tool-call states,
  confirmations, policy decisions, queued requests, and concurrent execution.
- The primary request path is centered on Gemini content/function-call types,
  even though model routing and alternate generator layers exist.

## Sessions and persistence

- `packages/core/src/services/chatRecordingService.ts` appends durable JSONL
  conversation records, loads resumed sessions, migrates legacy JSON records,
  records subagent sessions, rewinds history, and deletes session artifacts.
- `sessionSummaryUtils.ts` generates and persists summary/scratchpad metadata.
- This is effective append-oriented history, but it is not a single relational
  ledger for conversations, runs, agents, tasks, approvals, tool execution, and
  idempotent recovery.

## Specialist agents

- `packages/core/src/agents/agentLoader.ts` parses Markdown front matter with
  schemas for local and remote agents, tool inclusion/exclusion, model config,
  timeouts, and other limits.
- `agents/types.ts` defines explicit local/remote agent definitions, output
  schemas, input schemas, tool configuration, and A2A-related types.
- `agents/registry.ts` loads built-ins and registers configured agents.
- Built-ins such as `codebase-investigator.ts`, `generalist-agent.ts`, and the
  browser agent create real behavioral differences.

## Local subagent execution

- `agents/local-executor.ts` creates an agent-local `ToolRegistry`, registers
  configured tools, adds a mandatory `CompleteTaskTool`, loops over turns,
  schedules tool calls, enforces time/turn limits, and gives a final grace turn
  when completion was omitted.
- Stopping tool use without `complete_task` is explicitly treated as an error.
- `agents/agent-scheduler.ts` delegates standard calls to the common scheduler.
- Local subagents are structured and well isolated, but their lifecycle is not
  a fully persistent hierarchical control plane equivalent to Codex V2.

## Remote agents and browser

- `remote-invocation.ts`, `remote-session-invocation.ts`,
  `remote-subagent-protocol.ts`, and `a2a-client-manager.ts` support A2A
  discovery, authentication, streaming, cancellation, and result reassembly.
- `agents/browser/` owns browser process/MCP management, input blocking,
  screenshot analysis, overlays, tool wrapping, and browser-specific prompts.
- These boundaries are valuable, but remote A2A and browser automation should
  remain optional capabilities rather than mandatory runtime dependencies.

## Tools, skills, plugins, and MCP

- `tools/tool-registry.ts` holds all tools, orders built-ins/discovered/MCP
  tools, and computes active tools after exclusions.
- `tools/activate-skill.ts` implements explicit lazy skill activation.
- `tools/read-many-files.ts` demonstrates deterministic batching.
- `tools/definitions/` supports model-family-dependent tool declarations and
  dynamic resolution.
- MCP clients/managers are distinct from normal tools.
- Extensions are broader bundles that can contribute configuration and tools,
  but the new system should make the plugin boundary explicit.

## Safety and workspaces

- `services/sandboxManager.ts` defines sandbox requests, permissions, execution
  policy, no-op/local implementations, and platform resolution.
- Docker/Podman sandbox flows exist at repository level.
- `services/worktreeService.ts` creates isolated Git worktrees for sessions.
- The scheduler checks policy and confirmation state, but policy, tool
  permissions, and sandbox enforcement are spread across several layers.

## Context, routing, and observability

- `context/`, compression services, session summaries, and tool-output masking
  manage context pressure.
- `routing/` and model configuration services provide model selection and
  fallback concepts.
- `telemetry/`, perf tests, evals, and recording generators collect useful
  measurements, though schemas remain influenced by Gemini provider types.

## Assessment

Keep the declarative agent schema, mandatory structured completion, isolated
registries, scheduler semantics, lazy activation, batching, browser boundary,
and optional A2A capability. Reimplement them in the canonical Rust runtime.
Do not keep Gemini-specific function-call/content objects or a second JSONL
session runtime.

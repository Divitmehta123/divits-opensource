# Architecture decision log

## ADR-001: One Rust runtime

**Problem:** Codex is primarily Rust and Gemini CLI TypeScript; combining both
would create two loops and packaging surfaces.

**Options considered:** Rust; TypeScript; Rust plus TypeScript sidecar.

**Codex behavior:** Rust runtime, server, process tools, sandboxing, and Ratatui.

**Gemini behavior:** TypeScript/Node runtime and Ink UI.

**Chosen approach:** Rust for all canonical runtime, server, CLI, and TUI code.

**Reason:** One lifecycle and deployment artifact, strong cancellation and
concurrency, direct Ratatui support, and better fit for local process isolation.

**Tradeoffs:** Gemini concepts must be reimplemented; contributors need Rust.

**Future migration path:** Provider or browser adapters may be separate
processes only behind the versioned adapter protocol, never a second runtime.

## ADR-002: SQLite event ledger

**Problem:** Codex combines rollout JSONL with a SQLite mirror; Gemini records
JSONL chats. Neither alone covers the required durable control plane.

**Options considered:** JSONL only; SQLite projections over JSONL; SQLite event
ledger; external database.

**Codex behavior:** JSONL rollout history plus SQLite metadata and agent edges.

**Gemini behavior:** append-only JSONL chat records and summaries.

**Chosen approach:** SQLite event ledger with transactional materialized tables.

**Reason:** Atomic state transitions, dependency queries, idempotency, recovery,
and one local dependency.

**Tradeoffs:** Migrations and event schema compatibility require discipline.

**Future migration path:** Export events to JSONL/Parquet; add a storage trait
only if self-hosted multi-node operation is proven necessary.

## ADR-003: Provider-native adapters

**Problem:** OpenAI Responses and Gemini function-calling semantics differ.

**Options considered:** force OpenAI compatibility; force Gemini types; generic
lowest-common-denominator; capability contract with native adapters.

**Codex behavior:** provider metadata around a Responses-centric request loop.

**Gemini behavior:** generator/routing abstractions around Gemini content types.

**Chosen approach:** canonical request/events plus adapter-declared capabilities.

**Reason:** Avoids provider lock-in without erasing useful native features.

**Tradeoffs:** Adapter test matrix is larger and capabilities may degrade.

**Future migration path:** Version the adapter contract and add provider
features through optional capability fields.

## ADR-004: Runtime-owned lifecycle, model-proposed work

**Problem:** Fully model-directed delegation is unsafe; rigid planning is weak
for unknown repositories.

**Options considered:** model-only; deterministic planner-only; hybrid.

**Codex behavior:** model-visible agent tools constrained by runtime guards.

**Gemini behavior:** explicit agent definitions and scheduler-enforced tools and
limits.

**Chosen approach:** models propose DAG changes; runtime validates and executes.

**Reason:** Flexible decomposition with deterministic permissions, dependencies,
budgets, ownership, cancellation, and transitions.

**Tradeoffs:** Proposal schemas and rejection feedback add complexity.

**Future migration path:** Add deterministic planners for common focused tasks
without changing task/agent contracts.

## ADR-005: Mandatory structured delegated completion

**Problem:** Natural-language child output cannot reliably signal success.

**Options considered:** infer from final text; event terminal state; explicit
completion tool.

**Codex behavior:** status is derived from thread events and output.

**Gemini behavior:** local agents must call `complete_task` with schema-checked
output and receive a grace turn if omitted.

**Chosen approach:** Persisted `complete_task` object required for task success.

**Reason:** Machine-checkable integration, audit, retries, and UI.

**Tradeoffs:** Providers without structured tools need adapter emulation and
strict validation.

**Future migration path:** Allow signed external-agent completion messages that
validate against the same schema.

## ADR-006: Dynamic tools and lazy skills

**Problem:** Sending every schema/instruction wastes tokens and expands attack
surface.

**Options considered:** static catalog; prompt-only filtering; runtime exposure.

**Codex behavior:** handlers may be registered but hidden by tool plans.

**Gemini behavior:** active-tool computation and explicit skill activation.

**Chosen approach:** metadata catalog first, explicit activation, policy-filtered
schemas per cycle.

**Reason:** Lower token cost and least privilege.

**Tradeoffs:** Models may need one discovery action before using a capability.

**Future migration path:** Cache exposure sets by agent role and task class.

## ADR-007: Workspace leases

**Problem:** Shared writable files let concurrent agents silently collide.

**Options considered:** prompts; global write lock; path leases; worktrees;
containers.

**Codex behavior:** shared thread tree with process sandbox concepts.

**Gemini behavior:** worktree service for isolated sessions.

**Chosen approach:** explicit workspace modes with ownership leases; worktrees
for parallel writers by default.

**Reason:** Enforceable conflict prevention with graded isolation cost.

**Tradeoffs:** Git integration and merge UX become runtime responsibilities.

**Future migration path:** Add container leases for untrusted plugins and remote
workers.

## ADR-008: Experimental capabilities stay off by default

**Problem:** Browser automation, A2A, and cross-platform strong sandboxes are
large security surfaces.

**Options considered:** ship enabled; omit; feature-gated experimental adapters.

**Codex behavior:** several mature platform components plus evolving features.

**Gemini behavior:** browser and remote A2A implementations exist.

**Chosen approach:** keep contracts and research, feature-gate implementations
until security/recovery suites pass.

**Reason:** Do not label incomplete isolation as production-ready.

**Tradeoffs:** Initial releases expose fewer integrations.

**Future migration path:** Graduate each capability using documented acceptance
criteria and benchmark/security evidence.

## ADR-009: Direct process tools and hash-guarded writes

**Problem:** A generic shell string and unguarded file overwrite make policy,
recovery, and concurrent-change detection unreliable.

**Options considered:** implicit shell strings; direct program/argument arrays;
full-file writes; unified patches; OS-specific worker only.

**Codex behavior:** structured process execution and a dedicated patch path.

**Gemini behavior:** declarative tools with scheduler and sandbox policy around
local execution.

**Chosen approach:** Direct program plus argument arrays, cleared child
environments rebuilt from a small safe list, canonical workspace containment,
optional preimage SHA-256, and parsed unified patches.

**Reason:** It creates enforceable command identity, avoids shell interpolation,
and detects stale model edits before changing a file.

**Tradeoffs:** Shell pipelines must be represented as explicit steps, and
platform-grade process/filesystem isolation still requires worker backends.

**Future migration path:** Move the same typed invocation contract behind
Windows, macOS, Linux, and container sandbox workers without changing Tools.

## ADR-010: Environment-referenced provider configuration

**Problem:** Multiple remote providers need distinct endpoints and credentials
without persisting secrets or forcing one wire protocol.

**Options considered:** CLI key flags; secrets embedded in JSON; environment
references; immediate keyring dependency.

**Codex behavior:** provider authentication is resolved outside model prompts.

**Gemini behavior:** provider configuration and authentication remain adapter
concerns.

**Chosen approach:** JSON defines adapter identity, native protocol, base URL,
capabilities, and an environment-variable name. The adapter receives the
resolved key privately. OpenAI-compatible and Gemini adapters own SSE parsing.

**Reason:** Configuration remains reproducible while keys stay out of canonical
requests, events, logs, and serialized config.

**Tradeoffs:** Environment variables are weaker than a keyring, and live
provider behavior still needs credentialed contract tests.

**Future migration path:** Add keyring/secret-provider references alongside
environment references while preserving the provider adapter contract.

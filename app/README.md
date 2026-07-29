# Divit's OpenSource Tool

Divit's OpenSource Tool is a local-first, provider-neutral terminal coding agent built in Rust
with Ratatui. It combines a chat-first interface with real filesystem/process/Git tools,
skills, MCP servers, persisted provider accounts, and a coordinated multi-agent runtime.

## Quick start

Install once from the project root:

```powershell
cargo install --path .\opensrc-cli --locked --force
```

Then open any terminal in the directory you want to work on and run:

```powershell
divit
```

That single command starts an isolated local runtime and opens the TUI. If the usual local
port is already occupied, OpenSource chooses another loopback port so it cannot accidentally
attach to an older build.

Run a non-interactive task with:

```powershell
divit run "Inspect this project, fix the failing tests, and verify the result"
```

## Providers and remembered models

Open `/settings` in the TUI to connect or switch providers. Credentials are stored in the
operating-system keyring when available; the provider file retains only a keyring reference.
Discovered models are cached in the provider configuration and refreshed from the live
provider catalog.

OpenRouter is the default first-run preset. AICredits, OpenAI-compatible, Gemini, DeepSeek,
Kimi, GLM, Qwen, and custom compatible endpoints can coexist. AICredits uses
`https://api.aicredits.in/v1` for requests and automatically discovers its catalog from its
provider-specific catalog endpoint. Use:

- `/models` to refresh and choose a provider/model.
- `/packs` to inspect available three-model packs.
- `/pack efficient-trio` or `/pack quality-trio` to select a pack.
- `/pack off` to return to a single model.
- `/reasoning` to choose a supported reasoning level.

Selections persist with the conversation. OpenSource retains the five newest conversations
for each provider. [`providers.example.json`](providers.example.json) contains non-secret,
environment-referenced examples for AICredits, OpenRouter, plus direct DeepSeek, Kimi, and Z.AI
connections. Provider model IDs remain editable; for example, the Kimi coding alias
can target either `kimi-k2.7-code` or `kimi-for-coding`.

## Role routing policy and optional packs

The built-in `opensource-multi-llm-v1` policy uses configurable model aliases rather than
binding every subsystem to an LLM:

- GLM handles fast routing, frontend work, accessibility, documentation, and bounded
  coordination.
- DeepSeek handles architecture, investigation, dependency/database reasoning, security, and
  independent review.
- Kimi handles sustained implementation, debugging, integration, refactoring, and visual/media
  work.
- Waiting, dependency readiness, repository indexing, and release gates use deterministic
  runtime services and consume no model tokens.

Each resolved role policy carries the actual provider/model, thinking and reasoning settings,
context policy, tool profile, workspace mode, budgets, output contract, and recorded fallback
chain. The policy is persisted as `routing-policy.json` in the application state directory and
can be changed without rewriting agent prompts. The selected route and every model transition
are emitted to the event trace.

A model pack contains exactly three distinct provider/model members. The router assigns
each task by stage and specialist role when the user explicitly selects a pack:

1. Planning and repository mapping.
2. Implementation and synthesis.
3. Review and validation.

Generated packs use the connected live catalog; custom packs are saved in
`%LOCALAPPDATA%\opensource\model-packs.json` on Windows. Packs are an optional user override;
the role policy remains the source of operational permissions and budgets.

## Real local work

Automatic mode classifies each prompt as direct, focused, or agentic. File-oriented requests
use the real local tool loop, including:

- Read, list, glob, stat, create directories, write, exact-edit, patch, copy, move, and delete.
- Safe directory removal with explicit approval for recursive deletion.
- Search, allowlisted processes, tests, background processes, and Git inspection/mutation.
- Images plus local image/audio/video/file attachments through paste or drag-and-drop.
- Skills discovered from project, user, and compatible agent skill directories, with
  prompt-driven Git/local installation and live same-session refresh.
- Configured MCP servers through the MCP registry, including prompt-driven stdio or remote
  HTTP connections and GitHub's official MCP endpoint.

Examples:

```text
Install the skill from https://github.com/owner/repository
Connect GitHub using the GITHUB_PAT environment variable
```

The current project is readable and writable by default. When a prompt mentions a path outside
the current workspace, OpenSource requests the required path scope once, persists the decision,
and resumes the same run. Workspace and filesystem roots cannot be recursively deleted.

## Coordinated agents

The automatic planner can build a dependency-aware DAG of up to eight bounded tasks. Built-in
roles include architecture, repository mapping, investigation, frontend/backend, integration,
database, refactoring, media, documentation, performance, dependency, security, accessibility,
testing/debugging, review, release, and monitoring specialists.

Each task receives an immutable contract containing:

- Objective, acceptance criteria, deliverables, and validation steps.
- Owned paths, allowed tools, budgets, and forbidden actions.
- Upstream structured completion objects and handoff notes.
- A mandatory evidence-backed completion schema.

Messages to a running agent enter its model context at the next turn boundary. Independent
read-only tasks run concurrently; overlapping writers receive dependency ordering. The compact
chat trace shows agent, skill, pack, tool, target, status, and elapsed time without exposing
private chain-of-thought or duplicating assistant output.

## Useful commands

```text
divit                         Open the local TUI
divit doctor                  Check installation and provider state
divit models                  List discovered models
divit providers list          List connected providers
divit agent list              List available agent definitions
divit skill list              List discovered skills
divit mcp list                List configured MCP servers
divit stats                   Show usage and runtime metrics
divit completions powershell  Generate shell completions
```

Inside the TUI, `/help` lists all commands and `/settings` opens configuration.

## State and security

User state lives under `%LOCALAPPDATA%\opensource` on Windows (or the platform-equivalent
state directory). Provider secrets are not written into conversation data. Tool calls are
policy-evaluated, idempotently recorded, and shown in the event trace. Destructive, external,
or recursively deleting operations require the relevant approval unless a matching persistent
rule already exists. Provider setup/debug values redact inline credentials, provider response
errors are scrubbed before persistence, and public API errors never echo upstream credential
text.

## Development and release gates

The supported Rust toolchain starts at Rust 1.88.

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
cargo build --workspace --release
cargo run -p divits-opensource -- doctor
```

The workspace is split into domain types (`opensrc-core`), SQLite persistence
(`opensrc-store`), provider adapters (`opensrc-providers`), tools/orchestration
(`opensrc-runtime`), HTTP/event APIs (`opensrc-server`), and the CLI/TUI (`opensrc-cli`).

## License

Apache-2.0. See [LICENSE](LICENSE).

# Divit's OpenSource Tool

Divit's OpenSource Tool is a provider-neutral terminal coding agent. Its default
experience is a streaming Ratatui chat interface backed by one local Rust
server and a durable SQLite conversation/event store.

## Install once, run with one command

### Windows: one download command (no Rust required)

After the repository is published, anyone can install the latest signed release with one
PowerShell command:

```powershell
irm https://raw.githubusercontent.com/Divitmehta123/divits-opensource/main/scripts/install-online.ps1 | iex
```

The bootstrap downloads the newest Windows release, verifies its SHA-256 checksum, installs it,
and adds the launcher to the user's PATH. Then, in any new terminal:

```text
divit
```

### Windows release archive

Download `divits-opensource-windows-x64.zip` from the GitHub Releases page, extract it,
and double-click `install.cmd`. Open a new Command Prompt in any folder and run:

```text
divit
```

The release workflow also publishes Linux x64 and macOS Intel/Apple Silicon archives.
Each Unix archive includes `install.sh`.

### From this checkout

```powershell
& 'F:\Project OpenSource\scripts\install.ps1'
```

Then open a new terminal in any project:

```powershell
divit
```

That is the normal launch flow. The command detects the current project,
starts or attaches to the local app server, restores recent project sessions,
and opens chat. It does not require a second terminal.
The installers also provide `divits-opensource` and the legacy `opensource`
aliases, while `divit` remains the short primary command.

On first launch, choose a provider in the setup dialog and enter an API key or
an environment-variable name. Keys entered directly are written to the current
user's process environment for the launched server and are never displayed
again; environment references are persisted instead of secret values.

## What works

- Streaming multi-turn chat with durable sessions, restore, rename, archive,
  fork, JSON import/export, Markdown export, and context compaction.
- Responsive chat workspace with an always-visible input cursor, live usage,
  automatic pipeline status, command/model/agent popups, and dedicated agent,
  task, skill, tool, process, change, context, metrics, and log views.
- OpenAI-compatible and Gemini-native typed tool-call protocols.
- Live model discovery across the built-in provider catalog, plus custom
  OpenAI-compatible and local endpoints.
- Direct, focused, automatic, and agentic modes.
- Persistent interactive approvals with run/project/global allow and deny
  rules, plus editable tool arguments.
- Project and explicitly granted local-directory reads, glob/search/symbol
  search, image inspection, URL fetch,
  exact edits, writes, patches, copy/move/delete, shell/test processes,
  long-running process input/poll/kill, and Git inspect/stage/unstage/restore/
  commit tools.
- Hash-guarded file history, automatic/manual checkpoints, diff display,
  undo, redo, and checkpoint restoration.
- Executable built-in/project agent roles, dependency-aware concurrent
  read-only or disjoint-owned subagents, model-invoked spawn/message/wait/
  interrupt, retries, fallback providers, and structured completions.
- Automatic role routing by default; manual agent selection is optional.
- Built-in, project, user, and explicit `$skill` workflows, including
  prompt-driven installation from local paths and Git repositories with live
  discovery in the current session.
- Namespaced project/user Markdown prompt commands with YAML or TOML front
  matter and positional/named arguments.
- Prompt-driven local stdio and remote HTTP MCP connection, persistence,
  discovery, and approved tool invocation. This includes GitHub through
  GitHub's official remote MCP server using a token environment reference.
- Scriptable auth/provider/model/session/agent/skill/MCP/stats/doctor and shell
  completion commands.

Use `divit --help` and `/help` inside the TUI for the connected command
surface.

Extensions are installed conversationally. Examples:

```text
Install the skill from https://github.com/owner/repository
Connect GitHub using the GITHUB_PAT environment variable
Connect the MCP server at https://example.com/mcp using SERVICE_TOKEN
```

The agent uses `skill.install` or `mcp.connect`, shows the approval before the
external download or connection, validates the result, and exposes it without
restarting the app. `/skills`, `/mcp`, and the Extensions tab show the live
state. Tokens are referenced by environment-variable name and are not copied
into MCP configuration.

Ask for work anywhere on the machine in natural language, for example
`Analyze F drive and list its folders`. The agent selects the filesystem tool
and the TUI asks for access to the outside path at that moment. `/add-dir`
remains an optional way to save a persistent directory grant; `/dirs` shows
saved roots and `/remove-dir` revokes one. Individual files need no directory
activation: drag them straight from Explorer into the prompt.
They appear as compact numbered badges such as `Image 1`, `Video 1`, and
`Video 2`, without exposing full directory paths in the composer.
Gemini-family models receive images, audio, and video as native inline media,
while OpenAI/Anthropic-family gateways receive the media types supported by
their wire protocol.

Slash commands and their values suggest automatically while typing. Up/Down
selects a result and Tab completes every built-in command, alias, discovered
custom command, or supported value. `/reasoning` values adapt to the selected
model.

Chat history scrolls with the mouse wheel, PageUp/PageDown, or Up/Down while
the prompt is empty. Ctrl+Home jumps to the oldest visible history and Ctrl+End
returns to the latest message; Ctrl+Up/Down keeps prompt-history navigation
available.

## Safety boundary

Filesystem paths, commands, network requests, MCP calls, and workspace
ownership are policy checked and approval gated. This is currently labeled
`sandbox:limited`: it is not an operating-system security boundary. Do not run
untrusted model output against sensitive projects or credentials.

Browser automation, LSP services, signed extension marketplaces, true
Git-worktree merge isolation, OS-native sandbox backends, OAuth MCP login, and
secure OS keyring storage remain incomplete and are not presented as working
commands.

## Verify the source

```powershell
cd 'F:\Project OpenSource\app'
& 'C:\Users\HP\.cargo\bin\cargo.exe' fmt --all -- --check
& 'C:\Users\HP\.cargo\bin\cargo.exe' clippy --workspace --all-targets -- -D warnings
& 'C:\Users\HP\.cargo\bin\cargo.exe' test --workspace --all-targets
& 'C:\Users\HP\.cargo\bin\cargo.exe' build --release -p divits-opensource
```

Architecture and exact subsystem status are documented under
[`architecture`](architecture/) and [`docs`](docs/).

## Source references

The original archives are unchanged. Extracted trees under `upstream/` are
read-only references. Attribution and hashes are recorded in
[`licenses/UPSTREAM_ATTRIBUTION.md`](licenses/UPSTREAM_ATTRIBUTION.md).

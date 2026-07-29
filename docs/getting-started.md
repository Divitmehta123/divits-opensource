# Getting started

## Requirements

- Windows, macOS, or Linux
- Rust 1.88 or newer when installing from source
- Git for Git-aware coding operations

## Install

```powershell
& 'F:\Project OpenSource\scripts\install.ps1'
```

The script builds and installs `opensource` into Cargo's user binary directory,
adds that directory to the user `PATH` when needed, and generates PowerShell
completion data.

For a portable flow, copy
`F:\Project OpenSource\app\target\release\opensource.exe` after a release build
to any directory on `PATH`. Built-in agents, skills, and database migrations
are embedded in the binary.

## Start from any project

```powershell
cd 'C:\path\to\your\project'
opensource
```

No separate server command is needed. Normal launch uses user-level durable
state under `%LOCALAPPDATA%\opensource`, project skills/commands under
`.opensource`, and port `4545` for its loopback app server. If a healthy server
is already running there, the TUI attaches to it.

## First provider

If no provider is configured, the setup dialog opens automatically:

1. Select OpenCode Zen, OpenCode Go, OpenAI, Gemini, DeepSeek, Kimi, GLM,
   Qwen, or a custom/local endpoint.
2. Toggle between direct API-key input and an environment-variable reference.
3. Confirm the base URL and model.
4. Submit to test the connection and select it.

OpenCode Zen uses `https://opencode.ai/zen/v1`, refreshes its current model
catalog from the gateway, and defaults to `gpt-5.6-sol`. OpenCode Go uses its
separate subscription endpoint at `https://opencode.ai/zen/go/v1`, refreshes
its current catalog, and defaults to `grok-4.5`. Get the corresponding key
from `https://opencode.ai/auth`. Environment mode defaults to
`OPENCODE_API_KEY` for Zen and `OPENCODE_GO_API_KEY` for Go.

Advanced users can set `OPENSOURCE_PROVIDER_CONFIG` to a provider JSON file.
The server token for non-loopback binding is `OPENSOURCE_SERVER_TOKEN`; it must
contain at least 24 characters.

## Everyday keys

- `Ctrl+Enter`: send
- `Enter`: newline
- `Ctrl+C`: cancel the running response; press again to quit
- `Ctrl+N`: new conversation
- `Ctrl+M`, `Ctrl+A`, `Ctrl+S`: model, agent, and session pickers
- `Ctrl+K`: type-to-filter command palette
- `Ctrl+D`, `Ctrl+T`, `Ctrl+L`: changes, terminal activity, and logs
- `Ctrl+E`: edit the prompt in `$VISUAL` or `$EDITOR`
- `Tab`: complete `/commands`, `@files`, or `@agent:roles`
- `F1` or `/help`: connected commands and keybindings
- `PageUp`, `PageDown`: inspect older or newer conversation output

Typing `/` opens command suggestions immediately. Continue typing to filter,
use Up/Down to move through the results, and press Tab to complete the selected
built-in, alias, or custom command.

Agent routing defaults to `auto`. Coding requests are classified and routed to
the appropriate built-in or project role without requiring `Ctrl+A`; broad
work uses the coordinating generalist pipeline. Skills are matched by trigger
and loaded automatically, while `/skill <name>` remains available for explicit
activation. The right-hand chat sidebar shows token usage, cost, live agents,
tasks, connected tools, skills, MCP servers, and changes.

Use the mouse wheel, PageUp/PageDown, or Up/Down on an empty prompt to scroll
the conversation. Ctrl+Home jumps to the oldest history and Ctrl+End returns
to the latest message. Ctrl+Up/Down navigates previous prompts.

## Local directories and media

The launch directory is available automatically. For another drive or
directory, just describe the work:

```text
Analyze F drive and tell me all folder names.
```

The agent calls the appropriate filesystem tool and the TUI asks for access to
that path when it is needed. Approving resumes the same run automatically.
`/add-dir`, `/dirs`, and `/remove-dir` remain available when you want to save,
inspect, or revoke a persistent project grant. Persistent grants are stored in
`.opensource\workspace-roots.json`.
No grant is needed for an individual attachment: drag files directly from
Explorer into the prompt. The composer shows numbered `Image`, `Video`,
`Audio`, or `File` badges instead of directory paths. Backspace on an empty
prompt removes the last attachment. Images are sent natively to supported
OpenAI, Anthropic, Gemini, and OpenCode model protocols. Audio and video are
sent natively through Gemini-family protocols; other model families retain a
file reference when their protocol does not accept that media type.

## Scriptable examples

```powershell
opensource run "Explain the current project architecture"
opensource providers list
opensource models openai
opensource session list
opensource agent create migration-reviewer --tools fs.read,search.*,git.diff
opensource skill create release-check --triggers release,ship
opensource mcp add filesystem --command npx --args -y,@modelcontextprotocol/server-filesystem,C:\work
opensource mcp debug filesystem
opensource doctor
opensource completions powershell
```

`opensource run` starts a temporary local server automatically when required.

## Project extensions

- Agents: `.opensource\agents\<name>.md`
- Skills: `.opensource\skills\<name>\SKILL.md`
- Commands: `.opensource\commands\<namespace>\<name>.md`

Custom command templates support YAML (`---`) or TOML (`+++`) front matter,
`$1` positional arguments, `$ARGUMENTS`, and `{{named}}` placeholders supplied
as `--named value`.

## Development verification

```powershell
cd 'F:\Project OpenSource\app'
& 'C:\Users\HP\.cargo\bin\cargo.exe' fmt --all -- --check
& 'C:\Users\HP\.cargo\bin\cargo.exe' clippy --workspace --all-targets -- -D warnings
& 'C:\Users\HP\.cargo\bin\cargo.exe' test --workspace --all-targets
& 'C:\Users\HP\.cargo\bin\cargo.exe' build --release -p opensrc-cli
```

The product currently reports `sandbox:limited`. Policy and approvals reduce
risk but do not replace an OS sandbox.

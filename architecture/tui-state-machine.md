# TUI state machine

## Top-level states

```text
Boot
  -> FirstRunSetup (no configured provider)
  -> LoadingSession
  -> ChatIdle

ChatIdle
  -> EditingPrompt
  -> OverlayOpen
  -> Submitting
  -> Quitting

Submitting
  -> Streaming
  -> Error
  -> ChatIdle

Streaming
  -> AwaitingApproval
  -> Cancelling
  -> Error
  -> ChatIdle
```

`Chat` is the default view. Agents, Tasks, Changes, Terminal, Sessions,
Context, Skills, Tools, MCP, Plugins, Metrics, Logs, and Settings are secondary
views. A view may render an honest unavailable state while its backing service
is unfinished; it must not simulate data.

## State ownership

- `App`: selected view, overlay, conversation transcript, editor, active run,
  event cursor, connection/provider/model/mode state and errors.
- `PromptEditor`: text buffer, cursor, history and local undo/redo.
- `ClientEvent`: background HTTP/SSE results delivered over a channel.
- render functions: deterministic and side-effect free.
- server API: all conversation/run/provider business logic and persistence.

Network requests and SSE consumption run in Tokio tasks. The render/input loop
does not block on model calls. `TerminalSession` restores raw mode, alternate
screen and cursor state on every exit path.

## Key transitions

| Input | State | Result |
|---|---|---|
| printable/paste | EditingPrompt | insert at cursor |
| Enter | EditingPrompt | newline |
| Ctrl+Enter | EditingPrompt | submit non-empty buffer |
| Ctrl+C | Streaming | cancel active run |
| Ctrl+C twice | idle/cancelling | quit |
| Esc | overlay | close overlay |
| Ctrl+N | idle | create conversation |
| Ctrl+P | idle/editing | command palette |
| Ctrl+M | idle/editing | model picker |
| Tab/Shift+Tab | idle | cycle views |


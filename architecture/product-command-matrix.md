# Product command matrix

The central Rust command registry is the source for slash completion, help, and
dispatch. Only connected actions are registered.

| Product action | TUI/key | Slash | CLI/API |
|---|---|---|---|
| Launch current project | default | n/a | `opensource` |
| New/clear | `Ctrl+N` | `/new`, `/clear` | conversation API |
| List/resume sessions | `Ctrl+S` | `/sessions`, `/resume` | `session list` |
| Rename/archive/fork | session actions | `/rename`, `/delete`, `/fork` | session/API actions |
| Export/import/compact | session actions | `/export`, `/import`, `/compact` | `session export/import/compact` |
| Provider setup | setup overlay | `/connect`, `/disconnect` | `auth login/list/logout` |
| Provider/model selection | `Ctrl+M` | `/providers`, `/models`, `/model` | `providers list`, `models` |
| Reasoning/mode | header/prompt | `/reasoning`, `/variant`, `/mode` | `run --mode` |
| Agent selection/status | `Ctrl+A`, Agents | `/agent`, `/agents` | `agent create/list`, agent API |
| Task status | Tasks | `/tasks` | task API |
| Changes | `Ctrl+D` | `/diff`, `/changes` | changes API |
| Undo/redo/checkpoint | Changes | `/undo`, `/redo`, `/checkpoint` | changes/checkpoints API |
| Process/test output | `Ctrl+T` | `/terminal`, `/test` | tool activity API |
| Skills | Skills | `/skills`, `/skill` | `skill create/list/validate/enable/disable` |
| Tools | Tools | `/tools` | tools API |
| MCP | MCP | `/mcp` | `mcp add/list/remove/enable/disable/debug` |
| Permissions | Settings | `/permissions` | permissions API |
| Metrics | Metrics | `/stats`, `/cost`, `/tokens` | `stats` |
| Logs | `Ctrl+L` | `/logs` | event API |
| Help/quit | `F1`, `Ctrl+C` | `/help`, `/quit` | `--help` |
| Custom prompts | Tab completion | project/user namespaced command | commands API |

Not registered because their services remain incomplete: `/plugins`,
`/browser`, LSP commands, update, and plugin lifecycle actions.

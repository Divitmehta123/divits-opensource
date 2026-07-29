# Local protocol v1

The local HTTP API is bound to loopback by default.

| Method | Path | Purpose |
|---|---|---|
| GET | `/v1/health` | Protocol/runtime health |
| POST | `/v1/conversations` | Create a Conversation |
| POST | `/v1/runs` | Classify/create a Run |
| GET | `/v1/runs/{id}` | Get a Run projection |
| POST | `/v1/runs/{id}/execute` | Execute a Direct or Focused Run |
| GET | `/v1/providers` | List configured provider adapters |
| GET | `/v1/tools` | List dynamically registered Tool metadata |
| GET | `/v1/skills` | List lazy Skill metadata |
| POST | `/v1/skills/{name}/activate` | Load one Skill's instructions |
| GET | `/v1/metrics` | Aggregate or per-Run performance projection |
| GET | `/v1/agents` | List Agents, optionally by Run |
| GET | `/v1/agents/{id}` | Get Agent status/config projection |
| POST | `/v1/agents/root` | Create root Agent |
| POST | `/v1/agents/spawn` | Spawn configured child Agent |
| POST | `/v1/agents/wait` | Wait for terminal statuses with timeout |
| POST | `/v1/agents/{id}/messages` | Send non-executable message |
| POST | `/v1/agents/{id}/followups` | Assign executable follow-up Task |
| POST | `/v1/agents/{id}/start` | Queue-to-running transition |
| POST | `/v1/agents/{id}/wait` | Running-to-waiting transition |
| POST | `/v1/agents/{id}/interrupt` | Interrupt Agent and descendants |
| POST | `/v1/agents/{id}/resume` | Restore terminal/unloaded Agent |
| POST | `/v1/agents/{id}/complete` | Submit structured completion |
| GET | `/v1/tasks` | List Task projections |
| GET | `/v1/tasks/ready` | List dependency-ready Task IDs for a Run |
| GET | `/v1/tasks/{id}` | Get a Task projection |
| POST | `/v1/tasks/{id}/start` | Claim a dependency-ready Task |
| POST | `/v1/tasks/{id}/reassign` | Reassign a nonterminal Task |
| GET | `/v1/events` | Read durable events after a cursor |
| GET | `/v1/events/stream` | Stream durable events over SSE |

Event polling and SSE streaming are implemented. The server must remain
loopback-only until authentication exists. Automatic Agentic planning,
approvals, workspace merges, settings, Plugins, MCP, and configured pricing are
reserved for later compatible additions.

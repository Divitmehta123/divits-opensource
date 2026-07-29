# Agents

Agents are persistent configured workers, not model labels. Their durable
record includes identity/path, parent, role/task, status, provider/model,
reasoning, context policy, tool policy, workspace/sandbox policy, budgets,
retry/fallback, completion schema, and timestamps.

## Definitions

Markdown definitions live in `app/agents`. YAML front matter configures
machine-enforced properties; the body is the role instruction. Validate all
definitions with:

```powershell
cargo run -p opensrc-cli -- validate-agents agents
```

## Control operations

Protocol v1 exposes:

- `spawn_agent`
- `send_message`
- `assign_followup`
- `wait_for_agents`
- `list_agents`
- `get_agent_status`
- `interrupt_agent`
- `resume_agent`
- `complete_task`

Messages do not start executable work. Follow-ups create Tasks. Successful
delegated completion requires the structured `TaskCompletion` object. Agent
and Task status transitions are centrally validated in `opensrc-core`.
Dependency-ready Tasks must enter `running` before completion; completion
updates the Task and Agent projections from the same structured contract.

## Limits

The initial defaults are depth 4, four children per parent, and 16 Agents per
Run. These are runtime guards, not prompt instructions.

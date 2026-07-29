# Storage schema

SQLite migration v1 creates:

- `conversations`
- `runs`
- `agents`
- `tasks`
- `task_dependencies`
- `events`
- `tool_calls`
- `model_calls`
- `token_usage`
- `timings`
- `file_changes`
- `approvals`
- `errors`
- `checkpoints`
- `completion_objects`
- `providers`
- `workspace_leases`

Domain creation and status changes update projections and append Events within
one transaction. Foreign keys are enabled. WAL and normal synchronous mode are
the local defaults.

The database is authoritative; JSON fields preserve complete versioned domain
objects while indexed columns support control-plane queries. Future migrations
must be additive or include an explicit backfill/recovery plan.

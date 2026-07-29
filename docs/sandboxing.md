# Sandboxing

The canonical policy model covers filesystem reads/writes, network, processes,
protected environment variables, command allow/deny rules, and tool/agent
policy. Workspace modes are:

- `shared-readonly`
- `shared-write`
- `owned-paths`
- `git-worktree`
- `temporary-copy`
- `container-isolated`

The current code enforces tool allow/deny rules, read/write workspace
containment, owned-path checks, command allow/deny rules, network/process
approval decisions, direct argument-array process launch, and execution
timeouts. It never accepts an implicit shell command string.

Platform enforcers for Windows, macOS, Linux, and containers are not implemented
yet. Approval persistence/resumption is also incomplete. Until those are
implemented, the application must not be described as safely sandboxing
untrusted commands.

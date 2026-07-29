# Tools

A Tool is an executable action. The runtime registry stores name, description,
input schema, parallel-safety, destructive status, and required capability.

Each Agent receives only policy-visible tools. Allow/deny patterns support exact
names, `*`, and namespaces such as `fs.*`. Deny wins. The policy engine also
checks workspace mode and whether network/process activity needs approval.

Implemented built-ins include:

- `fs.read`, `fs.read_many`, `fs.list`, `fs.glob`, `fs.view_image`,
  `fs.write`, `fs.edit_exact`, `fs.copy`, `fs.move`, `fs.delete`
- `search.text`, `search.symbol`, `search.fetch`
- `patch.apply`
- `shell.run`, `shell.test`, `process.start`, `process.input`,
  `process.poll`, `process.kill`
- `git.diff`, `git.status`, `git.log`, `git.show`, `git.branch`,
  `git.worktree`, `git.stage`, `git.unstage`, `git.restore`, `git.commit`
- `docs.write`

Relative paths resolve beneath the canonical project root. Absolute paths are
available through an on-demand approval: the agent calls the tool from the
user's natural-language request, the TUI displays the exact path, and an
approval resumes that call with a run-scoped grant. `/add-dir` is an optional
persistent grant; `/dirs` and `/remove-dir` inspect or revoke saved roots.
Canonical containment still rejects parent traversal and symlink escapes.
Writes can require a preimage SHA-256.
`patch.apply` parses and applies a unified diff in memory before writing.
Processes are launched directly from a program plus argument array, never
through an implicit shell, and require command policy approval/allowlisting.

SQLite tool records use a unique idempotency key. Completed calls replay the
stored result; an in-flight call is not silently executed again. Browser,
deployment, platform sandbox, and approval-resumption executors remain staged.

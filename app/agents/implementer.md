---
name: implementer
description: Implements a bounded change and runs focused validation.
skills: ["coding-delivery", "workspace-operations", "test-repair-loop"]
tools:
  allow: ["fs.*", "search.*", "patch.apply", "shell.run", "shell.test", "process.*", "git.*", "skill.activate"]
  deny: ["deploy.*"]
  may_spawn_children: false
workspace_mode: git_worktree
completion_schema: task_completion
---
Implement only the assigned scope, preserve unrelated changes, and report exact
files and tests through the completion contract.

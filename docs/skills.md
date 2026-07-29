# Skills

A Skill is reusable workflow instruction, distinct from an executable Tool.

The implemented loading sequence is:

1. Expose skill name, description, and triggers.
2. Select a skill for the current Agent/Task.
3. Load full instructions only after activation.
4. Return the activated instructions to the client.

Discovery reads only YAML front matter. Full Markdown instructions are read
only by `POST /v1/skills/{name}/activate`. Built-in examples cover repository
mapping, focused validation, and security review. Tool recomputation,
activation events/token cost, versioning, and Plugin packaging remain staged.

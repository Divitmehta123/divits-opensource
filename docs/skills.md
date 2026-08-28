# Skills

A Skill is reusable workflow instruction, distinct from an executable Tool.

The implemented loading sequence is:

1. Expose skill name, description, and triggers.
2. Assign dedicated skills in Agent front matter or select one explicitly.
3. Automatically load each assigned skill into that Agent's system prompt.
4. Return the activated instructions to the client.

Discovery reads only YAML front matter. Full Markdown instructions are loaded
when an agent role starts or through `POST /v1/skills/{name}/activate`.
All built-in roles declare at least one resolvable dedicated skill. The built-in
catalog covers repository mapping, coding delivery, workspace operations,
coordination, architecture, runtime integration, frontend quality, testing and
repair, independent review, release gates, documentation, focused validation,
and security review. Versioning and Plugin packaging remain staged.

# Configuration

The current CLI configuration surface is deliberately small:

```text
opensrc serve --bind <address> --database <path> \
  --provider-config <providers.json> --skills-dir <directory>
opensrc run <request> --project-root <path> --server <url> --agent <file> \
  [--provider <id> --model <name>]
opensrc execute <run-id> --provider <id> --model <name> --server <url>
opensrc tui --server <url>
opensrc status --server <url>
opensrc validate-agents <directory>
opensrc classify <request>
opensrc benchmark-local --scenarios <file> --iterations <n> [--output <file>]
```

Provider configuration is JSON; see `app/providers.example.json`. It references
credentials by environment-variable name and never embeds an API key.

Agent and Skill definitions are Markdown with YAML front matter. Organization
policy, provider pricing/rate limits, workspace defaults, MCP, Plugins, and
keyring integration do not yet have stable configuration files.

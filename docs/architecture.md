# Architecture

The authoritative architecture documents are:

- `architecture/codex-architecture.md`
- `architecture/gemini-architecture.md`
- `architecture/comparison-matrix.md`
- `architecture/target-architecture.md`
- `architecture/decision-log.md`
- `architecture/protocol-v1.md`
- `architecture/storage-schema.md`

The implementation follows a dependency-inward Rust workspace:

```text
opensrc-cli -> opensrc-server -> opensrc-runtime -> opensrc-store
      |                 \              \               /
      +------------------\--------------> opensrc-core
                          \
                           -> opensrc-providers -> opensrc-core
```

`opensrc-core` contains no HTTP, SQLite, terminal, provider-SDK, or platform
sandbox dependency. State changes are explicit and persisted. Clients operate
through protocol v1.

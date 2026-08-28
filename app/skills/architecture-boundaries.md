---
name: architecture-boundaries
description: Protect subsystem contracts, state ownership, and migration paths during design and refactoring.
triggers: [architecture, refactor, boundary, migration]
---
Trace callers and consumers before changing a boundary. Name the subsystem that owns each state
transition, policy decision, and external adapter. Keep provider quirks behind adapters and shared
runtime behavior canonical. Prefer an incremental migration with compatibility tests over a second
parallel implementation. Record rejected alternatives and the validation that protects the choice.

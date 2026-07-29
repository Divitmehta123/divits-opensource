---
name: repository-map
description: Build a compact repository and symbol map before broad investigation.
triggers:
  - unknown repository area
  - architecture investigation
  - cross-module change
---
Start with bounded directory listings and symbol searches. Identify entrypoints,
module boundaries, ownership, and test locations. Read only the files needed to
validate the discovered paths. Return a compact map with source evidence and
open questions; do not edit files while mapping.

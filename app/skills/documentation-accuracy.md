---
name: documentation-accuracy
description: Produce commands and documentation that match verified runtime behavior.
triggers: [documentation, readme, guide, docs]
---
Derive documentation from source and observed commands. Use exact executable names, flags, paths,
defaults, and platform caveats. Mark experimental or unavailable behavior honestly. Validate code
samples and commands when feasible, keep conceptual explanations aligned with runtime boundaries,
and never document an intended feature as if it already exists.

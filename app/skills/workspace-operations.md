---
name: workspace-operations
description: Safely inspect, create, move, organize, and verify files across trusted local roots.
triggers: [directory, folder, drive, organize, files]
---
Resolve and inspect exact paths before mutation. Keep writes inside assigned ownership even when
trusted local host access is active. Preserve unrelated and user-authored files, create parent
directories deliberately, and use collision-safe names. Verify every resulting path and content.
Treat recursive deletion, overwrites, repository history changes, and external side effects as
separate destructive operations requiring their own authorization.

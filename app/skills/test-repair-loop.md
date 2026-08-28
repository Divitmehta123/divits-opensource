---
name: test-repair-loop
description: Reproduce failures, repair root causes, and repeat validation until the required gates pass.
triggers: [test, debug, failure, regression]
---
Capture the exact failing command and output before changing code. Reduce the failure to the
smallest responsible boundary, add or strengthen a regression test, and fix the root cause rather
than weakening assertions. Re-run the focused test after each repair, then the broader relevant
suite. Stop only on a passing gate or a concrete external blocker with preserved evidence.

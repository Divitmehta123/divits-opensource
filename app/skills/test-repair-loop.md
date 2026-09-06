---
name: test-repair-loop
description: Reproduce failures, repair root causes, and repeat validation until the required gates pass.
triggers: [test, debug, failure, regression]
---
Capture the exact failing command and output before changing code. Reduce the failure to the
smallest responsible boundary, add or strengthen a regression test, and fix the root cause rather
than weakening assertions. Re-run the focused test after each repair, then the broader relevant
suite. Stop only on a passing gate or a concrete external blocker with preserved evidence.

Use shell.test for each required validation command, with the exact command string specified by
the contract. After repairing a failure rerun that same command so the runtime can resolve the
failed gate. Do not weaken tests or replace a failed command with an unrelated passing command.
Include a startup smoke test for generated apps; confirm an HTTP response or real CLI output and
clean up its process in a finally block. Unexecuted, skipped and unavailable checks are not passes.

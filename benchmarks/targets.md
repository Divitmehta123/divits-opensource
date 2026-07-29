# Initial performance targets

These are acceptance targets, not current measurements.

| Scenario | Target |
|---|---|
| Server warm startup | under 150 ms on reference workstation |
| Direct-mode runtime overhead | under 15 ms excluding provider |
| Focused context build for 20 files | under 250 ms |
| SQLite event append p95 | under 5 ms |
| Agent status projection p95 | under 10 ms |
| Wait notification wakeup p95 | under 25 ms |
| TUI refresh render | under 16 ms |
| Small request model round trips | one Direct; at most two Focused |
| Full-agent scheduler overhead | under 2% of total Run time |

Provider/model accuracy, latency, cost, tool correctness, patch success, test
success, and retry rate require credentialed benchmark runs and cannot be
claimed from local codec tests.

Run the local deterministic slice with:

```powershell
& 'F:\Project OpenSource\benchmarks\run-local.ps1' -Iterations 1000
```

The output records classifier acceptance and p50/p95/max in-process latency for
all 12 scenarios. It is not a substitute for credentialed end-to-end runs.

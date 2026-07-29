# Performance

Executed Runs record model/tool-call counts, provider and tool timings,
time-to-first-event, canonical usage, cached tokens, failure counts, Agent
counts, inter-agent messages, and configured cost.

SQLite persists token/timing ledgers. `GET /v1/metrics` exposes aggregate or
per-Run projections, and the TUI displays tokens, calls, latency, and cost.
Cost remains zero until provider pricing is configured. Fine-grained context
token attribution, concurrency/idle metrics, approval timing, and workspace
conflicts remain staged.

Initial local targets are in `benchmarks/targets.md`; scenario definitions are
in `benchmarks/scenarios.json`. `benchmarks/run-local.ps1` measures in-process
classifier p50/p95 latency for all 12 scenarios. Credentialed provider/model
quality and cost runs remain separate.

Focused source graphs generated during architecture reconstruction are in
`research/graphs`. Graphify measured approximately 3.9x fewer tokens per query
for the Codex Agent corpus and 10x for the Gemini Agent corpus.

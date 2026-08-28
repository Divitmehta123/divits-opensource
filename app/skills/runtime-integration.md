---
name: runtime-integration
description: Integrate agent runtime, provider, policy, persistence, and tool layers through canonical contracts.
triggers: [runtime, provider, tool, mcp, agentic]
---
Trace the request from entry point through routing, agent creation, model calls, tool normalization,
policy enforcement, persistence, events, and final synthesis. Keep provider/model behavior neutral;
isolate host or vendor differences behind declared capabilities. Test success, denial, malformed
input, cancellation, retry, and recovery paths. A shallow health response is not proof that the
complete contract works.

# Provider-neutral message protocol

The canonical protocol preserves provider-native conversation and tool
semantics. Adapters own wire serialization; tool results are never converted
into fake user prose.

## Canonical message

Each message has a typed role and ordered content blocks.

Roles:

- `system`
- `developer`
- `user`
- `assistant`
- `tool`

Content blocks:

- text
- file reference or attachment
- assistant reasoning summary
- assistant tool call
- tool result
- tool error
- approval request/result
- context summary

Tool call blocks retain provider call ID, canonical call ID, tool name and JSON
arguments. Tool result/error blocks retain both IDs, the tool name, structured
result/error, timing and approval state.

## Adapter ownership

Every provider adapter maps canonical blocks to its native request form and
maps streamed native events back to canonical events. OpenAI-compatible
adapters emit assistant `tool_calls` followed by `tool` messages keyed by
`tool_call_id`. Gemini adapters emit `functionCall` and `functionResponse`
parts. Provider-specific authentication, errors, rate limits, usage,
continuation IDs and reasoning controls remain adapter responsibilities.

Recorded fixture tests must cover text, streaming deltas, tool calls, tool
results, authentication errors and usage without storing credentials.


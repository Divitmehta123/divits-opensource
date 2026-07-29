# Providers

The canonical provider request/event model is in `opensrc-core`. Adapters
declare capabilities and own wire-specific authentication, request mapping,
response parsing, usage mapping, and error classification.

## Implemented codecs

- Configurable OpenAI-compatible `/chat/completions`, including SSE
- Native Gemini `generateContent` and `streamGenerateContent` SSE

OpenAI, Kimi/Moonshot, DeepSeek, Z.AI/GLM, and Qwen deployments can use separate
OpenAI-compatible adapter instances only when the selected endpoint actually
supports that protocol. URLs and model names are configuration, not compiled
defaults.

The setup catalog includes AICredits, Krutrim Cloud, OpenAI, OpenRouter, Gemini,
Groq, Together AI, Fireworks AI, Mistral AI, xAI, DeepSeek, Kimi, Z.AI/GLM,
Qwen, DeepInfra, NVIDIA NIM, Cerebras, SambaNova, Cohere, SiliconFlow,
Hugging Face Inference, Perplexity, Ollama, LM Studio, and local vLLM. A custom
OpenAI-compatible entry covers smaller gateways and self-hosted servers without
requiring a product release. Catalog responses shaped as OpenAI `data`, a
top-level `models` list, or a direct array are normalized into one model picker.

This list is deliberately protocol-based. A provider is not shown merely
because it sells model access: native-only authentication or request formats
need a real adapter before they can safely use the canonical agent runtime.

## AICredits

AICredits is available as a named OpenAI-compatible provider. Use the inference
base URL `https://api.aicredits.in/v1` and an `AICREDITS_API_KEY` credential.
Its model catalog is discovered from the provider's documented
`https://api.aicredits.in/api/models` endpoint, so `/models` can show the live
catalog after connection. The OpenSource runtime still owns agents, skills,
filesystem tools, approvals, and execution; the provider supplies model
inference and tool-call messages. Models that do not reliably emit tool calls
can still be used for chat, but should not be selected for tool-dependent work.

## Configuration

`app/providers.example.json` defines protocol, base URL, family, capability
flags, and `api_key_env`. A server registers only configured adapters. The
capability router rejects an adapter that cannot satisfy a request.

API keys stay inside adapter configuration and are sent only as authorization
headers. They must never enter prompts, events, or debug serialization.

## Skills and service connections

Provider choice does not control local capabilities. The runtime owns skills,
agents, filesystem/process tools, approvals, and MCP connections, so any model
that can reliably emit the canonical tool-call format sees the same capability
surface.

Users can ask the agent to install a skill from a local path or Git URL.
`skill.install` validates `SKILL.md`, rejects symbolic links and unsafe
subdirectories, installs under the current project's `.opensource/skills`,
and refreshes discovery immediately.

Users can also ask the agent to connect an MCP server. `mcp.connect` persists
either a local stdio command with environment references or a Streamable HTTP
URL with a token environment reference, tests the connection, and exposes its
tools in the same session. GitHub uses the official remote endpoint
`https://api.githubcopilot.com/mcp/` with `GITHUB_PAT` by default. Other
services use their documented MCP endpoint or local server package; no
unverified service is presented as natively integrated.

Prompt-cache details, thought signatures, previous-response continuation,
provider pricing, retry/rate-limit scheduling, and credentialed live contract
tests remain staged. A capability flag must describe what this adapter instance
actually implements, not merely what a provider markets.

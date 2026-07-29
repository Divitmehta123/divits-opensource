use crate::redaction::redact_known_secret;
use crate::sse::{SseFrame, json_frames};
use async_trait::async_trait;
use futures::StreamExt;
use opensrc_core::{
    CanonicalMessage, CanonicalModelRequest, MessageContent, MessageRole, ModelEvent,
    ModelEventStream, ProviderAdapter, ProviderCapabilities, ProviderError,
};
use reqwest::StatusCode;
use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiCompatibleFamily {
    #[serde(rename = "openai")]
    OpenAi,
    Kimi,
    #[serde(rename = "deepseek")]
    DeepSeek,
    Glm,
    Qwen,
    #[serde(rename = "openrouter")]
    OpenRouter,
    #[serde(rename = "aicredits")]
    AiCredits,
    Custom,
}

pub struct OpenAiCompatibleConfig {
    pub id: String,
    pub family: OpenAiCompatibleFamily,
    pub base_url: String,
    pub capabilities: ProviderCapabilities,
    pub extra_headers: BTreeMap<String, String>,
    api_key: String,
}

impl OpenAiCompatibleConfig {
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        family: OpenAiCompatibleFamily,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        mut capabilities: ProviderCapabilities,
    ) -> Self {
        if family == OpenAiCompatibleFamily::OpenRouter {
            // OpenRouter reports structured-output support per model. Keep the
            // provider default conservative so changing models cannot break plans.
            capabilities.supports_structured_output = false;
        }
        Self {
            id: id.into(),
            family,
            base_url: base_url.into(),
            capabilities,
            extra_headers: BTreeMap::new(),
            api_key: api_key.into(),
        }
    }
}

pub struct OpenAiCompatibleAdapter {
    client: reqwest::Client,
    config: OpenAiCompatibleConfig,
}

impl OpenAiCompatibleAdapter {
    #[must_use]
    pub fn new(config: OpenAiCompatibleConfig) -> Self {
        Self {
            client: reqwest::Client::new(),
            config,
        }
    }

    /// Return the model-catalog endpoint for the configured compatible API.
    ///
    /// `AICredits` uses standard `OpenAI` paths for inference under `/v1`, but
    /// deliberately exposes its gateway-wide catalog at `/api/models`. Keeping
    /// this exception on the named family avoids making custom compatible
    /// providers guess at non-standard catalog routes.
    fn models_url(&self) -> String {
        let base_url = self.config.base_url.trim_end_matches('/');
        if self.config.family == OpenAiCompatibleFamily::AiCredits {
            let api_root = base_url.strip_suffix("/v1").unwrap_or(base_url);
            return format!("{api_root}/api/models");
        }
        format!("{base_url}/models")
    }
}

#[allow(clippy::too_many_lines)]
#[async_trait]
impl ProviderAdapter for OpenAiCompatibleAdapter {
    fn id(&self) -> &str {
        &self.config.id
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.config.capabilities.clone()
    }

    async fn execute(
        &self,
        request: CanonicalModelRequest,
    ) -> Result<Vec<ModelEvent>, ProviderError> {
        let body = build_request(&request, self.config.family);
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let mut call = self.client.post(url).bearer_auth(&self.config.api_key);
        for (name, value) in &self.config.extra_headers {
            call = call.header(name, value);
        }
        let response = call
            .json(&body)
            .send()
            .await
            .map_err(|error| ProviderError::Transient(error.to_string()))?;
        let status = response.status();
        let retry_after_ms = retry_after_ms(response.headers());
        let payload: Value = response
            .json()
            .await
            .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
        if !status.is_success() {
            return Err(classify_error(
                status,
                &payload,
                &self.config.api_key,
                retry_after_ms,
            ));
        }
        parse_response(&payload)
    }

    async fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        let url = self.models_url();
        let mut call = self.client.get(url).bearer_auth(&self.config.api_key);
        for (name, value) in &self.config.extra_headers {
            call = call.header(name, value);
        }
        let response = call
            .send()
            .await
            .map_err(|error| ProviderError::Transient(error.to_string()))?;
        let status = response.status();
        let retry_after_ms = retry_after_ms(response.headers());
        let payload: Value = response
            .json()
            .await
            .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
        if !status.is_success() {
            return Err(classify_error(
                status,
                &payload,
                &self.config.api_key,
                retry_after_ms,
            ));
        }
        let mut models = model_ids(&payload);
        models.sort();
        models.dedup();
        Ok(models)
    }

    #[allow(clippy::collapsible_if)]
    async fn stream(
        &self,
        request: CanonicalModelRequest,
    ) -> Result<ModelEventStream, ProviderError> {
        let mut body = build_request(&request, self.config.family);
        body["stream"] = Value::Bool(true);
        body["stream_options"] = json!({"include_usage": true});
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let mut call = self.client.post(url).bearer_auth(&self.config.api_key);
        for (name, value) in &self.config.extra_headers {
            call = call.header(name, value);
        }
        let response = call
            .json(&body)
            .send()
            .await
            .map_err(|error| ProviderError::Transient(error.to_string()))?;
        let status = response.status();
        let retry_after_ms = retry_after_ms(response.headers());
        if !status.is_success() {
            let payload: Value = response
                .json()
                .await
                .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
            return Err(classify_error(
                status,
                &payload,
                &self.config.api_key,
                retry_after_ms,
            ));
        }

        let output = async_stream::try_stream! {
            let mut frames = json_frames(response);
            let mut partial_calls: BTreeMap<u64, PartialToolCall> = BTreeMap::new();
            let mut response_id = None;
            let mut completed = false;
            while let Some(frame) = frames.next().await {
                match frame? {
                    SseFrame::Json(payload) => {
                        if response_id.is_none() {
                            response_id = payload.get("id").and_then(Value::as_str).map(str::to_string);
                        }
                        if let Some(text) = payload
                            .pointer("/choices/0/delta/content")
                            .and_then(Value::as_str)
                        {
                            if !text.is_empty() {
                                yield ModelEvent::TextDelta { text: text.to_string() };
                            }
                        }
                        if let Some(calls) = payload
                            .pointer("/choices/0/delta/tool_calls")
                            .and_then(Value::as_array)
                        {
                            for call in calls {
                                let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                                let partial = partial_calls.entry(index).or_default();
                                if let Some(id) = call.get("id").and_then(Value::as_str) {
                                    partial.id.push_str(id);
                                }
                                if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                                    partial.name.push_str(name);
                                }
                                if let Some(arguments) = call
                                    .pointer("/function/arguments")
                                    .and_then(Value::as_str)
                                {
                                    partial.arguments.push_str(arguments);
                                }
                            }
                        }
                        if let Some(usage) = payload.get("usage").filter(|value| !value.is_null()) {
                            yield ModelEvent::Usage {
                                input_tokens: usage.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0),
                                output_tokens: usage.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0),
                                cached_tokens: usage
                                    .pointer("/prompt_tokens_details/cached_tokens")
                                    .and_then(Value::as_u64)
                                    .unwrap_or(0),
                            };
                        }
                    }
                    SseFrame::Done => {
                        for (_, call) in std::mem::take(&mut partial_calls) {
                            yield call.finish()?;
                        }
                        yield ModelEvent::Completed {
                            response_id: response_id.take(),
                        };
                        completed = true;
                        break;
                    }
                }
            }
            if !completed {
                for (_, call) in partial_calls {
                    yield call.finish()?;
                }
                yield ModelEvent::Completed { response_id };
            }
        };
        Ok(Box::pin(output))
    }
}

fn model_ids(payload: &Value) -> Vec<String> {
    payload
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| payload.get("models").and_then(Value::as_array))
        .or_else(|| payload.as_array())
        .into_iter()
        .flatten()
        .filter_map(|model| {
            model
                .as_str()
                .or_else(|| model.get("id").and_then(Value::as_str))
                .or_else(|| model.get("name").and_then(Value::as_str))
        })
        .map(str::to_string)
        .collect()
}

#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl PartialToolCall {
    fn finish(self) -> Result<ModelEvent, ProviderError> {
        let arguments = if self.arguments.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&self.arguments).map_err(|error| {
                ProviderError::InvalidResponse(format!(
                    "invalid streamed tool arguments for `{}`: {error}",
                    self.name
                ))
            })?
        };
        Ok(ModelEvent::ToolCall {
            id: self.id,
            name: self.name,
            arguments,
        })
    }
}

fn build_request(request: &CanonicalModelRequest, family: OpenAiCompatibleFamily) -> Value {
    let mut messages = vec![json!({"role": "system", "content": request.system})];
    messages.extend(openai_messages(&request.messages, family));
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema
                }
            })
        })
        .collect::<Vec<_>>();
    let mut body = json!({
        "model": request.model,
        "messages": messages,
        "stream": false
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if let Some(temperature) = request.temperature {
        body["temperature"] = json!(temperature);
    }
    if let Some(maximum) = request.max_output_tokens {
        body["max_tokens"] = json!(maximum);
    }
    if let Some(schema) = &request.structured_output_schema {
        body["response_format"] = json!({
            "type": "json_schema",
            "json_schema": {
                "name": "structured_output",
                "strict": true,
                "schema": schema
            }
        });
    }
    if let Some(reasoning) = &request.reasoning_level {
        if family == OpenAiCompatibleFamily::OpenRouter {
            body["reasoning"] = json!({"effort": reasoning});
        } else {
            body["reasoning_effort"] = json!(reasoning);
        }
    }
    body
}

fn openai_messages(messages: &[CanonicalMessage], family: OpenAiCompatibleFamily) -> Vec<Value> {
    let mut output = Vec::new();
    for message in messages {
        if message.role == MessageRole::Tool {
            for content in &message.content {
                match content {
                    MessageContent::ToolResult {
                        provider_call_id,
                        result,
                        ..
                    } => output.push(json!({
                        "role": "tool",
                        "tool_call_id": provider_call_id,
                        "content": serde_json::to_string(result).unwrap_or_else(|_| "null".to_string())
                    })),
                    MessageContent::ToolError {
                        provider_call_id,
                        error,
                        ..
                    } => output.push(json!({
                        "role": "tool",
                        "tool_call_id": provider_call_id,
                        "content": serde_json::to_string(&json!({"error": error}))
                            .unwrap_or_else(|_| "{\"error\":\"tool failed\"}".to_string())
                    })),
                    _ => {}
                }
            }
            continue;
        }

        let text = text_content(&message.content, family);
        let tool_calls = message
            .content
            .iter()
            .filter_map(|content| match content {
                MessageContent::ToolCall {
                    provider_call_id,
                    name,
                    arguments,
                    ..
                } => Some(json!({
                    "id": provider_call_id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": serde_json::to_string(arguments)
                            .unwrap_or_else(|_| "{}".to_string())
                    }
                })),
                _ => None,
            })
            .collect::<Vec<_>>();
        let native_content = openai_native_content(&message.content, &text, family);
        let mut value = json!({
            "role": openai_role(message.role),
            "content": native_content.unwrap_or_else(|| {
                if text.is_empty() { Value::Null } else { Value::String(text.clone()) }
            })
        });
        if !tool_calls.is_empty() {
            value["tool_calls"] = Value::Array(tool_calls);
        }
        output.push(value);
    }
    output
}

fn openai_native_content(
    content: &[MessageContent],
    text: &str,
    family: OpenAiCompatibleFamily,
) -> Option<Value> {
    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(json!({"type": "text", "text": text}));
    }
    let mut found = false;
    for item in content {
        let MessageContent::FileReference { path, mime_type } = item else {
            continue;
        };
        let Some((mime_type, data)) = crate::media::inline_media(path, mime_type.as_deref()) else {
            continue;
        };
        parts.push(json!({
            "type": "text",
            "text": format!("Attachment available to file tools at: {path}")
        }));
        if mime_type.starts_with("image/") {
            parts.push(json!({
                "type": "image_url",
                "image_url": {"url": format!("data:{mime_type};base64,{data}")}
            }));
            found = true;
        } else if matches!(mime_type.as_str(), "audio/wav" | "audio/mpeg") {
            parts.push(json!({
                "type": "input_audio",
                "input_audio": {
                    "data": data,
                    "format": if mime_type == "audio/wav" { "wav" } else { "mp3" }
                }
            }));
            found = true;
        } else if family == OpenAiCompatibleFamily::OpenRouter
            && matches!(
                mime_type.as_str(),
                "video/mp4" | "video/mpeg" | "video/quicktime" | "video/webm"
            )
        {
            parts.push(json!({
                "type": "video_url",
                "video_url": {
                    "url": format!("data:{mime_type};base64,{data}")
                }
            }));
            found = true;
        }
    }
    found.then_some(Value::Array(parts))
}

fn openai_role(role: MessageRole) -> &'static str {
    match role {
        MessageRole::System => "system",
        MessageRole::Developer => "developer",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    }
}

fn text_content(content: &[MessageContent], family: OpenAiCompatibleFamily) -> String {
    content
        .iter()
        .filter_map(|content| match content {
            MessageContent::Text { text }
            | MessageContent::ReasoningSummary { text }
            | MessageContent::ContextSummary { text } => Some(text.clone()),
            MessageContent::FileReference { path, mime_type } => {
                let native = mime_type.as_deref().is_some_and(|mime| {
                    mime.starts_with("image/")
                        || matches!(mime, "audio/wav" | "audio/mpeg")
                        || family == OpenAiCompatibleFamily::OpenRouter
                            && matches!(
                                mime,
                                "video/mp4" | "video/mpeg" | "video/quicktime" | "video/webm"
                            )
                });
                (!native).then(|| {
                    let kind = mime_type.as_deref().unwrap_or("file");
                    format!("Local attachment available to filesystem tools at `{path}` ({kind}).")
                })
            }
            MessageContent::ApprovalRequest {
                approval_id,
                summary,
                ..
            } => Some(format!("Approval requested ({approval_id}): {summary}")),
            MessageContent::ApprovalResult {
                approval_id,
                decision,
                reason,
            } => Some(format!(
                "Approval {approval_id}: {decision}{}",
                reason
                    .as_ref()
                    .map(|value| format!(" ({value})"))
                    .unwrap_or_default()
            )),
            MessageContent::ToolCall { .. }
            | MessageContent::ToolResult { .. }
            | MessageContent::ToolError { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_response(payload: &Value) -> Result<Vec<ModelEvent>, ProviderError> {
    let message = payload
        .pointer("/choices/0/message")
        .ok_or_else(|| ProviderError::InvalidResponse("missing choices[0].message".to_string()))?;
    let mut events = Vec::new();
    if let Some(text) = message.get("content").and_then(Value::as_str)
        && !text.is_empty()
    {
        events.push(ModelEvent::TextDelta {
            text: text.to_string(),
        });
    }
    if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in tool_calls {
            let arguments = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .and_then(|value| serde_json::from_str(value).ok())
                .unwrap_or(Value::Null);
            events.push(ModelEvent::ToolCall {
                id: call
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                name: call
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                arguments,
            });
        }
    }
    if let Some(usage) = payload.get("usage") {
        events.push(ModelEvent::Usage {
            input_tokens: usage
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output_tokens: usage
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            cached_tokens: usage
                .pointer("/prompt_tokens_details/cached_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        });
    }
    events.push(ModelEvent::Completed {
        response_id: payload
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string),
    });
    Ok(events)
}

fn classify_error(
    status: StatusCode,
    payload: &Value,
    api_key: &str,
    retry_after_ms: Option<u64>,
) -> ProviderError {
    let message = redact_known_secret(
        payload
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("provider request failed"),
        api_key,
    );
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        ProviderError::Authentication(message)
    } else if status == StatusCode::TOO_MANY_REQUESTS {
        ProviderError::RateLimited {
            message,
            retry_after_ms,
        }
    } else if status.is_server_error() {
        ProviderError::Transient(message)
    } else {
        ProviderError::Rejected(message)
    }
}

fn retry_after_ms(headers: &HeaderMap) -> Option<u64> {
    let milliseconds = headers
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    milliseconds.or_else(|| {
        headers
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(|seconds| seconds.saturating_mul(1_000))
    })
}

#[cfg(test)]
mod tests {
    use super::{
        OpenAiCompatibleAdapter, OpenAiCompatibleConfig, OpenAiCompatibleFamily, build_request,
        model_ids, parse_response,
    };
    use opensrc_core::{
        CanonicalMessage, CanonicalModelRequest, MessageContent, MessageRole, ProviderCapabilities,
    };
    use serde_json::json;
    use std::collections::BTreeMap;

    #[test]
    fn maps_canonical_request_and_usage() {
        let request = CanonicalModelRequest {
            model: "code-model".to_string(),
            system: "system".to_string(),
            messages: vec![
                CanonicalMessage::text(MessageRole::User, "hello"),
                CanonicalMessage {
                    role: MessageRole::Assistant,
                    content: vec![MessageContent::ToolCall {
                        provider_call_id: "provider-call-1".to_string(),
                        canonical_call_id: "canonical-call-1".to_string(),
                        name: "fs.read".to_string(),
                        arguments: json!({"path": "README.md"}),
                    }],
                },
                CanonicalMessage {
                    role: MessageRole::Tool,
                    content: vec![MessageContent::ToolResult {
                        provider_call_id: "provider-call-1".to_string(),
                        canonical_call_id: "canonical-call-1".to_string(),
                        name: "fs.read".to_string(),
                        result: json!({"content": "hello"}),
                        timing_ms: Some(2),
                        approval_state: Some("not_required".to_string()),
                    }],
                },
            ],
            tools: Vec::new(),
            structured_output_schema: None,
            reasoning_level: None,
            temperature: Some(0.2),
            max_output_tokens: Some(100),
            cache_hints: BTreeMap::new(),
        };
        let body = build_request(&request, OpenAiCompatibleFamily::Custom);
        assert_eq!(body["messages"][1]["content"], "hello");
        assert_eq!(
            body["messages"][2]["tool_calls"][0]["id"],
            "provider-call-1"
        );
        assert_eq!(
            body["messages"][2]["tool_calls"][0]["function"]["name"],
            "fs.read"
        );
        assert_eq!(body["messages"][3]["role"], "tool");
        assert_eq!(body["messages"][3]["tool_call_id"], "provider-call-1");
        assert_eq!(body["messages"][3]["content"], r#"{"content":"hello"}"#);
        let events = parse_response(&json!({
            "id": "response-1",
            "choices": [{"message": {"content": "done"}}],
            "usage": {"prompt_tokens": 4, "completion_tokens": 2}
        }))
        .expect("response");
        assert_eq!(events.len(), 3);
    }

    #[test]
    fn openrouter_uses_native_video_reasoning_and_conservative_structured_outputs() {
        let path = std::env::temp_dir().join(format!(
            "opensource-openrouter-video-{}.mp4",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&path, b"video").expect("video fixture");
        let request = CanonicalModelRequest {
            model: "google/gemini-3.6-flash".to_string(),
            system: "system".to_string(),
            messages: vec![CanonicalMessage {
                role: MessageRole::User,
                content: vec![MessageContent::FileReference {
                    path: path.to_string_lossy().into_owned(),
                    mime_type: Some("video/mp4".to_string()),
                }],
            }],
            tools: Vec::new(),
            structured_output_schema: None,
            reasoning_level: Some("high".to_string()),
            temperature: None,
            max_output_tokens: None,
            cache_hints: BTreeMap::new(),
        };

        let body = build_request(&request, OpenAiCompatibleFamily::OpenRouter);
        let content = body["messages"][1]["content"]
            .as_array()
            .expect("native content");
        assert_eq!(content[1]["type"], "video_url");
        assert!(
            content[1]["video_url"]["url"]
                .as_str()
                .is_some_and(|value| value.starts_with("data:video/mp4;base64,"))
        );
        assert_eq!(body["reasoning"]["effort"], "high");
        assert!(body.get("reasoning_effort").is_none());

        let config = OpenAiCompatibleConfig::new(
            "openrouter",
            OpenAiCompatibleFamily::OpenRouter,
            "https://openrouter.ai/api/v1",
            "secret",
            ProviderCapabilities {
                supports_structured_output: true,
                ..ProviderCapabilities::default()
            },
        );
        assert!(!config.capabilities.supports_structured_output);
        std::fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn aicredits_uses_its_gateway_catalog_without_changing_openai_requests() {
        let config = OpenAiCompatibleConfig::new(
            "aicredits",
            OpenAiCompatibleFamily::AiCredits,
            "https://api.aicredits.in/v1/",
            "secret",
            ProviderCapabilities::default(),
        );
        let adapter = OpenAiCompatibleAdapter::new(config);
        assert_eq!(adapter.models_url(), "https://api.aicredits.in/api/models");

        let request = CanonicalModelRequest {
            model: "openai/gpt-4o-mini".to_string(),
            system: "system".to_string(),
            messages: vec![CanonicalMessage::text(MessageRole::User, "use a tool")],
            tools: vec![opensrc_core::CanonicalTool {
                name: "fs.read".to_string(),
                description: "Read a file".to_string(),
                input_schema: json!({"type": "object"}),
            }],
            structured_output_schema: None,
            reasoning_level: None,
            temperature: None,
            max_output_tokens: None,
            cache_hints: BTreeMap::new(),
        };
        let body = build_request(&request, OpenAiCompatibleFamily::AiCredits);
        assert_eq!(body["model"], "openai/gpt-4o-mini");
        assert_eq!(body["tools"][0]["function"]["name"], "fs.read");
    }

    #[test]
    fn model_catalog_parser_shapes_remain_provider_neutral() {
        assert_eq!(
            model_ids(&json!({"data": [{"id": "openai-shape"}]})),
            ["openai-shape"]
        );
        assert_eq!(model_ids(&json!([{"id": "array-shape"}])), ["array-shape"]);
        assert_eq!(
            model_ids(&json!({"models": ["string-shape"]})),
            ["string-shape"]
        );
    }
}

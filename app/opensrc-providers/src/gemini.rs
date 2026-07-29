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
use serde_json::{Value, json};

pub struct GeminiConfig {
    pub id: String,
    pub base_url: String,
    pub capabilities: ProviderCapabilities,
    api_key: String,
}

impl GeminiConfig {
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        capabilities: ProviderCapabilities,
    ) -> Self {
        Self {
            id: id.into(),
            base_url: base_url.into(),
            capabilities,
            api_key: api_key.into(),
        }
    }
}

pub struct GeminiAdapter {
    client: reqwest::Client,
    config: GeminiConfig,
}

impl GeminiAdapter {
    #[must_use]
    pub fn new(config: GeminiConfig) -> Self {
        Self {
            client: reqwest::Client::new(),
            config,
        }
    }
}

#[async_trait]
impl ProviderAdapter for GeminiAdapter {
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
        let url = format!(
            "{}/models/{}:generateContent",
            self.config.base_url.trim_end_matches('/'),
            request.model
        );
        let response = self
            .client
            .post(url)
            .header("x-goog-api-key", &self.config.api_key)
            .json(&build_request(&request))
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
        let url = format!(
            "{}/models?pageSize=1000",
            self.config.base_url.trim_end_matches('/')
        );
        let response = self
            .client
            .get(url)
            .header("x-goog-api-key", &self.config.api_key)
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
        let mut models = payload
            .get("models")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|model| model.get("name").and_then(Value::as_str))
            .map(|name| name.strip_prefix("models/").unwrap_or(name).to_string())
            .collect::<Vec<_>>();
        models.sort();
        models.dedup();
        Ok(models)
    }

    #[allow(clippy::collapsible_if)]
    async fn stream(
        &self,
        request: CanonicalModelRequest,
    ) -> Result<ModelEventStream, ProviderError> {
        let url = format!(
            "{}/models/{}:streamGenerateContent?alt=sse",
            self.config.base_url.trim_end_matches('/'),
            request.model
        );
        let response = self
            .client
            .post(url)
            .header("x-goog-api-key", &self.config.api_key)
            .json(&build_request(&request))
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
            let mut response_id = None;
            let mut completed = false;
            while let Some(frame) = frames.next().await {
                match frame? {
                    SseFrame::Json(payload) => {
                        if response_id.is_none() {
                            response_id = payload
                                .get("responseId")
                                .and_then(Value::as_str)
                                .map(str::to_string);
                        }
                        if let Some(parts) = payload
                            .pointer("/candidates/0/content/parts")
                            .and_then(Value::as_array)
                        {
                            for (index, part) in parts.iter().enumerate() {
                                if let Some(text) = part.get("text").and_then(Value::as_str) {
                                    if !text.is_empty() {
                                        yield ModelEvent::TextDelta { text: text.to_string() };
                                    }
                                }
                                if let Some(call) = part.get("functionCall") {
                                    yield ModelEvent::ToolCall {
                                        id: format!("gemini-stream-call-{index}"),
                                        name: call
                                            .get("name")
                                            .and_then(Value::as_str)
                                            .unwrap_or_default()
                                            .to_string(),
                                        arguments: call.get("args").cloned().unwrap_or_else(|| json!({})),
                                    };
                                }
                            }
                        }
                        if let Some(usage) = payload.get("usageMetadata") {
                            yield ModelEvent::Usage {
                                input_tokens: usage
                                    .get("promptTokenCount")
                                    .and_then(Value::as_u64)
                                    .unwrap_or(0),
                                output_tokens: usage
                                    .get("candidatesTokenCount")
                                    .and_then(Value::as_u64)
                                    .unwrap_or(0),
                                cached_tokens: usage
                                    .get("cachedContentTokenCount")
                                    .and_then(Value::as_u64)
                                    .unwrap_or(0),
                            };
                        }
                    }
                    SseFrame::Done => {
                        yield ModelEvent::Completed {
                            response_id: response_id.take(),
                        };
                        completed = true;
                        break;
                    }
                }
            }
            if !completed {
                yield ModelEvent::Completed { response_id };
            }
        };
        Ok(Box::pin(output))
    }
}

fn build_request(request: &CanonicalModelRequest) -> Value {
    let contents = gemini_contents(&request.messages);
    let declarations = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "parametersJsonSchema": tool.input_schema
            })
        })
        .collect::<Vec<_>>();
    let mut body = json!({
        "systemInstruction": {"parts": [{"text": request.system}]},
        "contents": contents,
        "generationConfig": {}
    });
    if !declarations.is_empty() {
        body["tools"] = json!([{"functionDeclarations": declarations}]);
    }
    if let Some(temperature) = request.temperature {
        body["generationConfig"]["temperature"] = json!(temperature);
    }
    if let Some(maximum) = request.max_output_tokens {
        body["generationConfig"]["maxOutputTokens"] = json!(maximum);
    }
    if let Some(level) = request.reasoning_level.as_deref() {
        body["generationConfig"]["thinkingConfig"] = json!({"thinkingLevel": level});
    }
    if let Some(schema) = &request.structured_output_schema {
        body["generationConfig"]["responseMimeType"] = json!("application/json");
        body["generationConfig"]["responseJsonSchema"] = schema.clone();
    }
    body
}

fn gemini_contents(messages: &[CanonicalMessage]) -> Vec<Value> {
    messages
        .iter()
        .map(|message| {
            let role = if message.role == MessageRole::Assistant {
                "model"
            } else {
                "user"
            };
            let mut parts = message
                .content
                .iter()
                .map(|content| match content {
                    MessageContent::Text { text }
                    | MessageContent::ReasoningSummary { text }
                    | MessageContent::ContextSummary { text } => json!({"text": text}),
                    MessageContent::FileReference { path, mime_type } => {
                        crate::media::inline_media(path, mime_type.as_deref()).map_or_else(
                            || json!({"text": format!("@{path}")}),
                            |(mime_type, data)| {
                                json!({
                                    "inline_data": {"mime_type": mime_type, "data": data}
                                })
                            },
                        )
                    }
                    MessageContent::ToolCall {
                        name, arguments, ..
                    } => json!({
                        "functionCall": {
                            "name": name,
                            "args": arguments
                        }
                    }),
                    MessageContent::ToolResult { name, result, .. } => json!({
                        "functionResponse": {
                            "name": name,
                            "response": result
                        }
                    }),
                    MessageContent::ToolError { name, error, .. } => json!({
                        "functionResponse": {
                            "name": name,
                            "response": {"error": error}
                        }
                    }),
                    MessageContent::ApprovalRequest { summary, .. } => {
                        json!({"text": summary})
                    }
                    MessageContent::ApprovalResult {
                        decision, reason, ..
                    } => json!({
                        "text": format!(
                            "Approval: {decision}{}",
                            reason
                                .as_ref()
                                .map(|value| format!(" ({value})"))
                                .unwrap_or_default()
                        )
                    }),
                })
                .collect::<Vec<_>>();
            for content in &message.content {
                if let MessageContent::FileReference { path, mime_type } = content
                    && crate::media::inline_media(path, mime_type.as_deref()).is_some()
                {
                    parts.push(json!({
                        "text": format!("Attachment available to file tools at: {path}")
                    }));
                }
            }
            json!({"role": role, "parts": parts})
        })
        .collect()
}

fn parse_response(payload: &Value) -> Result<Vec<ModelEvent>, ProviderError> {
    let parts = payload
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ProviderError::InvalidResponse("missing candidates[0].content.parts".to_string())
        })?;
    let mut events = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            events.push(ModelEvent::TextDelta {
                text: text.to_string(),
            });
        }
        if let Some(call) = part.get("functionCall") {
            events.push(ModelEvent::ToolCall {
                id: format!("gemini-call-{index}"),
                name: call
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                arguments: call.get("args").cloned().unwrap_or(Value::Null),
            });
        }
    }
    if let Some(usage) = payload.get("usageMetadata") {
        events.push(ModelEvent::Usage {
            input_tokens: usage
                .get("promptTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output_tokens: usage
                .get("candidatesTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            cached_tokens: usage
                .get("cachedContentTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        });
    }
    events.push(ModelEvent::Completed {
        response_id: payload
            .get("responseId")
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
            .unwrap_or("Gemini request failed"),
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
    headers
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .or_else(|| {
            headers
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .map(|seconds| seconds.saturating_mul(1_000))
        })
}

#[cfg(test)]
mod tests {
    use super::{build_request, parse_response};
    use opensrc_core::{CanonicalMessage, CanonicalModelRequest, MessageContent, MessageRole};
    use serde_json::json;
    use std::collections::BTreeMap;

    #[test]
    fn maps_native_gemini_wire_format() {
        let request = CanonicalModelRequest {
            model: "gemini-model".to_string(),
            system: "system".to_string(),
            messages: vec![
                CanonicalMessage::text(MessageRole::User, "hello"),
                CanonicalMessage {
                    role: MessageRole::Assistant,
                    content: vec![MessageContent::ToolCall {
                        provider_call_id: "gemini-call-1".to_string(),
                        canonical_call_id: "canonical-call-1".to_string(),
                        name: "fs.read".to_string(),
                        arguments: json!({"path": "README.md"}),
                    }],
                },
                CanonicalMessage {
                    role: MessageRole::Tool,
                    content: vec![MessageContent::ToolResult {
                        provider_call_id: "gemini-call-1".to_string(),
                        canonical_call_id: "canonical-call-1".to_string(),
                        name: "fs.read".to_string(),
                        result: json!({"content": "hello"}),
                        timing_ms: Some(2),
                        approval_state: Some("not_required".to_string()),
                    }],
                },
            ],
            tools: Vec::new(),
            structured_output_schema: Some(json!({"type": "object"})),
            reasoning_level: Some("high".to_string()),
            temperature: None,
            max_output_tokens: None,
            cache_hints: BTreeMap::new(),
        };
        let body = build_request(&request);
        assert_eq!(
            body["generationConfig"]["responseMimeType"],
            "application/json"
        );
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "high"
        );
        assert_eq!(
            body["contents"][1]["parts"][0]["functionCall"]["name"],
            "fs.read"
        );
        assert_eq!(
            body["contents"][2]["parts"][0]["functionResponse"]["name"],
            "fs.read"
        );
        assert_eq!(
            body["contents"][2]["parts"][0]["functionResponse"]["response"]["content"],
            "hello"
        );
        let events = parse_response(&json!({
            "responseId": "g-1",
            "candidates": [{"content": {"parts": [{"text": "done"}]}}],
            "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 1}
        }))
        .expect("response");
        assert_eq!(events.len(), 3);
    }
}

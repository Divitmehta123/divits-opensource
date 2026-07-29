use crate::{MessageContent, MessageRole};
use async_trait::async_trait;
use futures::stream::{self, Stream};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::pin::Pin;
use thiserror::Error;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
// These booleans intentionally mirror independently negotiable provider features.
#[allow(clippy::struct_excessive_bools)]
pub struct ProviderCapabilities {
    pub supports_streaming: bool,
    pub supports_tool_calls: bool,
    pub supports_parallel_tool_calls: bool,
    pub supports_structured_output: bool,
    pub supports_reasoning_controls: bool,
    pub supports_prompt_caching: bool,
    pub supports_previous_response_continuation: bool,
    pub supports_context_reuse: bool,
    pub supports_native_token_counting: bool,
    pub supports_multimodal_input: bool,
    pub supports_thought_signatures: bool,
    pub supports_batch_requests: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CanonicalModelRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<CanonicalMessage>,
    pub tools: Vec<CanonicalTool>,
    pub structured_output_schema: Option<Value>,
    pub reasoning_level: Option<String>,
    pub temperature: Option<f32>,
    pub max_output_tokens: Option<u64>,
    pub cache_hints: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CanonicalMessage {
    pub role: MessageRole,
    pub content: Vec<MessageContent>,
}

impl CanonicalMessage {
    #[must_use]
    pub fn text(role: MessageRole, text: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![MessageContent::text(text)],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CanonicalTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelEvent {
    TextDelta {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cached_tokens: u64,
    },
    Completed {
        response_id: Option<String>,
    },
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("provider rejected request: {0}")]
    Rejected(String),
    #[error("provider is temporarily unavailable: {0}")]
    Transient(String),
    #[error("provider rate limit reached: {message}")]
    RateLimited {
        message: String,
        retry_after_ms: Option<u64>,
    },
    #[error("provider authentication failed: {0}")]
    Authentication(String),
    #[error("provider response was invalid: {0}")]
    InvalidResponse(String),
}

pub type ModelEventStream =
    Pin<Box<dyn Stream<Item = Result<ModelEvent, ProviderError>> + Send + 'static>>;

#[async_trait]
pub trait ProviderAdapter: Send + Sync {
    fn id(&self) -> &str;
    fn capabilities(&self) -> ProviderCapabilities;

    async fn execute(
        &self,
        request: CanonicalModelRequest,
    ) -> Result<Vec<ModelEvent>, ProviderError>;

    async fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        Ok(Vec::new())
    }

    async fn stream(
        &self,
        request: CanonicalModelRequest,
    ) -> Result<ModelEventStream, ProviderError> {
        let events = self.execute(request).await?;
        Ok(Box::pin(stream::iter(events.into_iter().map(Ok))))
    }
}

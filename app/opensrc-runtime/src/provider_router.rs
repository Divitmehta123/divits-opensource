use opensrc_core::{ProviderAdapter, ProviderCapabilities};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use thiserror::Error;

#[derive(Debug, Clone, Default)]
// These booleans describe independent request requirements, not a single state.
#[allow(clippy::struct_excessive_bools)]
pub struct RequiredCapabilities {
    pub streaming: bool,
    pub tools: bool,
    pub structured_output: bool,
    pub multimodal: bool,
}

#[derive(Debug, Error)]
pub enum RouterError {
    #[error("provider `{0}` is not registered")]
    UnknownProvider(String),
    #[error("provider `{provider}` lacks required capability `{capability}`")]
    MissingCapability {
        provider: String,
        capability: &'static str,
    },
    #[error("model discovery for provider `{provider}` failed: {message}")]
    ModelDiscovery { provider: String, message: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderDescriptor {
    pub id: String,
    pub default_model: Option<String>,
    pub capabilities: ProviderCapabilities,
}

#[derive(Clone, Default)]
pub struct ProviderRouter {
    adapters: Arc<RwLock<BTreeMap<String, Arc<dyn ProviderAdapter>>>>,
    default_models: Arc<RwLock<BTreeMap<String, String>>>,
    known_models: Arc<RwLock<BTreeMap<String, Vec<String>>>>,
}

impl ProviderRouter {
    pub fn register(&self, adapter: Arc<dyn ProviderAdapter>) {
        self.adapters
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(adapter.id().to_string(), adapter);
    }

    pub fn register_with_model(
        &self,
        adapter: Arc<dyn ProviderAdapter>,
        default_model: impl Into<String>,
    ) {
        self.register_with_models(adapter, default_model, Vec::new());
    }

    pub fn register_with_models(
        &self,
        adapter: Arc<dyn ProviderAdapter>,
        default_model: impl Into<String>,
        models: Vec<String>,
    ) {
        let id = adapter.id().to_string();
        self.register(adapter);
        let default_model = default_model.into();
        self.default_models
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone(), default_model.clone());
        let mut models = models;
        models.push(default_model);
        models.sort();
        models.dedup();
        self.known_models
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, models);
    }

    pub fn unregister(&self, provider: &str) -> bool {
        self.default_models
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(provider);
        self.known_models
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(provider);
        self.adapters
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(provider)
            .is_some()
    }

    #[must_use]
    pub fn provider_ids(&self) -> Vec<String> {
        self.adapters
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn descriptors(&self) -> Vec<ProviderDescriptor> {
        let adapters = self
            .adapters
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let models = self
            .default_models
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        adapters
            .iter()
            .map(|(id, adapter)| ProviderDescriptor {
                id: id.clone(),
                default_model: models.get(id).cloned(),
                capabilities: adapter.capabilities(),
            })
            .collect()
    }

    #[must_use]
    pub fn default_model(&self, provider: &str) -> Option<String> {
        self.default_models
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(provider)
            .cloned()
    }

    #[must_use]
    pub fn known_models(&self, provider: &str) -> Vec<String> {
        let mut models = self
            .known_models
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(provider)
            .cloned()
            .unwrap_or_default();
        if let Some(default) = self.default_model(provider) {
            models.push(default);
        }
        models.sort();
        models.dedup();
        models
    }

    #[must_use]
    pub fn model_catalog(&self) -> Vec<(String, String)> {
        self.provider_ids()
            .into_iter()
            .flat_map(|provider| {
                self.known_models(&provider)
                    .into_iter()
                    .map(move |model| (provider.clone(), model))
            })
            .collect()
    }

    pub fn resolve(
        &self,
        provider: &str,
        required: &RequiredCapabilities,
    ) -> Result<Arc<dyn ProviderAdapter>, RouterError> {
        let adapter = self
            .adapters
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(provider)
            .cloned()
            .ok_or_else(|| RouterError::UnknownProvider(provider.to_string()))?;
        validate_capabilities(provider, &adapter.capabilities(), required)?;
        Ok(adapter)
    }

    pub fn capabilities_for_model(
        &self,
        provider: &str,
        model: &str,
    ) -> Result<ProviderCapabilities, RouterError> {
        let adapter = self
            .adapters
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(provider)
            .cloned()
            .ok_or_else(|| RouterError::UnknownProvider(provider.to_string()))?;
        let mut capabilities = adapter.capabilities();
        capabilities.supports_multimodal_input = model_supports_multimodal_input(
            provider,
            model,
            capabilities.supports_multimodal_input,
        );
        capabilities.supports_tool_calls = capabilities.supports_tool_calls
            && model_supports_local_tools(provider, model)
            && model_is_chat_capable(model);
        capabilities.supports_parallel_tool_calls &= capabilities.supports_tool_calls;
        Ok(capabilities)
    }

    pub fn resolve_model(
        &self,
        provider: &str,
        model: &str,
        required: &RequiredCapabilities,
    ) -> Result<Arc<dyn ProviderAdapter>, RouterError> {
        let adapter = self
            .adapters
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(provider)
            .cloned()
            .ok_or_else(|| RouterError::UnknownProvider(provider.to_string()))?;
        let capabilities = self.capabilities_for_model(provider, model)?;
        validate_capabilities(provider, &capabilities, required)?;
        Ok(adapter)
    }

    pub async fn list_models(&self, provider: &str) -> Result<Vec<String>, RouterError> {
        let adapter = self.resolve(provider, &RequiredCapabilities::default())?;
        let mut models =
            adapter
                .list_models()
                .await
                .map_err(|error| RouterError::ModelDiscovery {
                    provider: provider.to_string(),
                    message: error.to_string(),
                })?;
        if let Some(default) = self.default_model(provider) {
            models.push(default);
        }
        models.sort();
        models.dedup();
        self.known_models
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(provider.to_string(), models.clone());
        Ok(models)
    }
}

#[must_use]
pub fn model_is_chat_capable(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    ![
        "whisper",
        "orpheus",
        "prompt-guard",
        "safeguard",
        "embedding",
        "moderation",
        "text-to-speech",
        "speech-to-text",
        "/tts",
    ]
    .iter()
    .any(|marker| model.contains(marker))
}

fn model_supports_local_tools(provider: &str, model: &str) -> bool {
    let provider = provider.to_ascii_lowercase();
    let model = model.to_ascii_lowercase();
    !(provider == "groq" && matches!(model.as_str(), "groq/compound" | "groq/compound-mini"))
}

fn model_supports_multimodal_input(provider: &str, model: &str, provider_default: bool) -> bool {
    let provider = provider.to_ascii_lowercase();
    let model = model.to_ascii_lowercase();
    if provider == "groq" {
        return model.contains("qwen3.6")
            || model.contains("qwen3-vl")
            || model.contains("qwen3_vl");
    }
    provider_default
        || [
            "vision",
            "llava",
            "pixtral",
            "qwen-vl",
            "qwen_vl",
            "qwen3-vl",
            "qwen3_vl",
            "qwen3.6",
            "gemini",
            "gpt-4o",
            "gpt-4.1",
            "gpt-5",
            "o3",
            "o4",
            "claude-3",
            "claude-sonnet-4",
            "claude-opus-4",
            "llama-4-scout",
            "llama-4-maverick",
        ]
        .iter()
        .any(|marker| model.contains(marker))
}

fn validate_capabilities(
    provider: &str,
    available: &ProviderCapabilities,
    required: &RequiredCapabilities,
) -> Result<(), RouterError> {
    for (needed, available, name) in [
        (
            required.streaming,
            available.supports_streaming,
            "streaming",
        ),
        (required.tools, available.supports_tool_calls, "tool_calls"),
        (
            required.structured_output,
            available.supports_structured_output,
            "structured_output",
        ),
        (
            required.multimodal,
            available.supports_multimodal_input,
            "multimodal_input",
        ),
    ] {
        if needed && !available {
            return Err(RouterError::MissingCapability {
                provider: provider.to_string(),
                capability: name,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        model_is_chat_capable, model_supports_local_tools, model_supports_multimodal_input,
    };

    #[test]
    fn groq_models_are_filtered_by_their_actual_task_capabilities() {
        assert!(!model_supports_multimodal_input(
            "groq",
            "llama-3.3-70b-versatile",
            true
        ));
        assert!(model_supports_multimodal_input(
            "groq",
            "qwen/qwen3.6-27b",
            false
        ));
        assert!(model_supports_local_tools(
            "groq",
            "llama-3.3-70b-versatile"
        ));
        assert!(!model_supports_local_tools("groq", "groq/compound"));
        assert!(!model_is_chat_capable("whisper-large-v3"));
    }
}

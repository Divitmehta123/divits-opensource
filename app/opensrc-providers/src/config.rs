use crate::{
    GeminiAdapter, GeminiConfig, OpenAiCompatibleAdapter, OpenAiCompatibleConfig,
    OpenAiCompatibleFamily,
};
use opensrc_core::{ProviderAdapter, ProviderCapabilities};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;

const KEYRING_SERVICE: &str = "opensource.provider";
const KEYRING_PREFIX: &str = "keyring:";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderProtocol {
    OpenaiCompatible,
    Gemini,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderFile {
    pub providers: Vec<ProviderEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderEntry {
    pub id: String,
    pub protocol: ProviderProtocol,
    pub base_url: String,
    pub api_key_env: String,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub family: Option<OpenAiCompatibleFamily>,
    #[serde(default)]
    pub capabilities: Option<ProviderCapabilities>,
}

#[derive(Debug, Error)]
pub enum ProviderConfigError {
    #[error("failed to read provider configuration {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid provider configuration {path}: {source}")]
    Invalid {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("provider id `{0}` is duplicated")]
    DuplicateId(String),
    #[error("provider `{provider}` references missing environment variable `{variable}`")]
    MissingCredential { provider: String, variable: String },
    #[error("provider `{provider}` credential store failed: {message}")]
    CredentialStore { provider: String, message: String },
    #[error("provider `{provider}` has invalid base URL `{base_url}`")]
    InvalidBaseUrl { provider: String, base_url: String },
}

pub fn load_provider_file(
    path: impl AsRef<Path>,
) -> Result<Vec<Arc<dyn ProviderAdapter>>, ProviderConfigError> {
    let document = read_provider_file(path)?;
    build_adapters(document)
}

pub fn read_provider_file(path: impl AsRef<Path>) -> Result<ProviderFile, ProviderConfigError> {
    let path = path.as_ref();
    let content = std::fs::read_to_string(path).map_err(|source| ProviderConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_str(&content).map_err(|source| ProviderConfigError::Invalid {
        path: path.to_path_buf(),
        source,
    })
}

pub fn build_adapters(
    document: ProviderFile,
) -> Result<Vec<Arc<dyn ProviderAdapter>>, ProviderConfigError> {
    let mut ids = BTreeSet::new();
    let mut adapters: Vec<Arc<dyn ProviderAdapter>> = Vec::new();
    for entry in document.providers {
        if !ids.insert(entry.id.clone()) {
            return Err(ProviderConfigError::DuplicateId(entry.id));
        }
        if !valid_base_url(&entry.base_url) {
            return Err(ProviderConfigError::InvalidBaseUrl {
                provider: entry.id,
                base_url: entry.base_url,
            });
        }
        let api_key = if is_anonymous_local_compatible(&entry.protocol, &entry.base_url)
            && entry.api_key_env.trim().is_empty()
        {
            String::new()
        } else {
            resolve_credential(&entry.id, &entry.api_key_env)?
        };
        let capabilities = entry
            .capabilities
            .unwrap_or_else(|| default_capabilities(&entry.protocol));
        match entry.protocol {
            ProviderProtocol::OpenaiCompatible => {
                let config = OpenAiCompatibleConfig::new(
                    entry.id,
                    entry.family.unwrap_or(OpenAiCompatibleFamily::Custom),
                    entry.base_url,
                    api_key,
                    capabilities,
                );
                adapters.push(Arc::new(OpenAiCompatibleAdapter::new(config)));
            }
            ProviderProtocol::Gemini => {
                let config = GeminiConfig::new(entry.id, entry.base_url, api_key, capabilities);
                adapters.push(Arc::new(GeminiAdapter::new(config)));
            }
        }
    }
    Ok(adapters)
}

#[must_use]
pub fn is_anonymous_local_compatible(protocol: &ProviderProtocol, base_url: &str) -> bool {
    if *protocol != ProviderProtocol::OpenaiCompatible {
        return false;
    }
    let base_url = base_url.to_ascii_lowercase();
    ["http://127.0.0.1:", "http://localhost:", "http://[::1]:"]
        .iter()
        .any(|prefix| base_url.starts_with(prefix))
}

pub fn store_provider_credential(
    provider: &str,
    api_key: &str,
) -> Result<String, ProviderConfigError> {
    let entry = keyring::v1::Entry::new(KEYRING_SERVICE, provider).map_err(|error| {
        ProviderConfigError::CredentialStore {
            provider: provider.to_string(),
            message: error.to_string(),
        }
    })?;
    entry
        .set_password(api_key)
        .map_err(|error| ProviderConfigError::CredentialStore {
            provider: provider.to_string(),
            message: error.to_string(),
        })?;
    Ok(format!("{KEYRING_PREFIX}{provider}"))
}

fn resolve_credential(provider: &str, reference: &str) -> Result<String, ProviderConfigError> {
    if let Some(account) = reference.strip_prefix(KEYRING_PREFIX) {
        return keyring::v1::Entry::new(KEYRING_SERVICE, account)
            .and_then(|entry| entry.get_password())
            .map_err(|error| ProviderConfigError::CredentialStore {
                provider: provider.to_string(),
                message: error.to_string(),
            });
    }
    std::env::var(reference).map_err(|_| ProviderConfigError::MissingCredential {
        provider: provider.to_string(),
        variable: reference.to_string(),
    })
}

fn default_capabilities(protocol: &ProviderProtocol) -> ProviderCapabilities {
    ProviderCapabilities {
        supports_streaming: true,
        supports_tool_calls: true,
        supports_parallel_tool_calls: true,
        supports_structured_output: true,
        supports_reasoning_controls: matches!(protocol, ProviderProtocol::OpenaiCompatible),
        supports_prompt_caching: false,
        supports_previous_response_continuation: false,
        supports_context_reuse: false,
        supports_native_token_counting: true,
        supports_multimodal_input: matches!(protocol, ProviderProtocol::Gemini),
        supports_thought_signatures: false,
        supports_batch_requests: false,
    }
}

fn valid_base_url(value: &str) -> bool {
    value.starts_with("https://")
        || value.starts_with("http://127.0.0.1")
        || value.starts_with("http://localhost")
}

#[cfg(test)]
mod tests {
    use super::{
        KEYRING_PREFIX, ProviderFile, ProviderProtocol, default_capabilities,
        is_anonymous_local_compatible, read_provider_file, valid_base_url,
    };
    use crate::OpenAiCompatibleFamily;
    use std::collections::BTreeSet;

    #[test]
    fn parses_environment_referenced_configuration_without_a_secret_field() {
        let document: ProviderFile = serde_json::from_str(
            r#"{
                "providers": [{
                    "id": "deepseek",
                    "protocol": "openai_compatible",
                    "family": "deepseek",
                    "base_url": "https://provider.example/v1",
                    "api_key_env": "DEEPSEEK_API_KEY"
                }]
            }"#,
        )
        .expect("provider config");
        assert_eq!(document.providers.len(), 1);
        assert_eq!(
            document.providers[0].protocol,
            ProviderProtocol::OpenaiCompatible
        );
        assert!(valid_base_url(&document.providers[0].base_url));
        assert!(default_capabilities(&ProviderProtocol::Gemini).supports_tool_calls);
    }

    #[test]
    fn keyring_references_are_distinct_from_environment_variables() {
        assert_eq!(format!("{KEYRING_PREFIX}gemini"), "keyring:gemini");
    }

    #[test]
    fn local_compatible_servers_can_run_without_fake_credentials() {
        for url in [
            "http://127.0.0.1:11434/v1",
            "http://localhost:1234/v1",
            "http://[::1]:8000/v1",
        ] {
            assert!(is_anonymous_local_compatible(
                &ProviderProtocol::OpenaiCompatible,
                url
            ));
        }
        assert!(!is_anonymous_local_compatible(
            &ProviderProtocol::Gemini,
            "http://localhost:1234/v1"
        ));
        assert!(!is_anonymous_local_compatible(
            &ProviderProtocol::OpenaiCompatible,
            "https://api.example.com/v1"
        ));
    }

    #[test]
    fn shipped_provider_example_contains_the_configurable_multi_llm_targets_without_secrets() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../providers.example.json");
        let document = read_provider_file(path).expect("shipped provider example");
        let ids = document
            .providers
            .iter()
            .map(|provider| provider.id.as_str())
            .collect::<BTreeSet<_>>();

        assert_eq!(
            ids,
            ["aicredits", "deepseek", "kimi", "openrouter", "zai",]
                .into_iter()
                .collect()
        );
        assert!(document.providers.iter().all(|provider| {
            provider.api_key_env.starts_with("keyring:")
                || (provider.api_key_env.ends_with("_API_KEY")
                    && !provider.api_key_env.chars().any(char::is_whitespace))
        }));
        let kimi = document
            .providers
            .iter()
            .find(|provider| provider.id == "kimi")
            .expect("Kimi example");
        assert!(kimi.models.iter().any(|model| model == "kimi-for-coding"));
        let aicredits = document
            .providers
            .iter()
            .find(|provider| provider.id == "aicredits")
            .expect("AICredits example");
        assert_eq!(aicredits.family, Some(OpenAiCompatibleFamily::AiCredits));
        assert_eq!(aicredits.base_url, "https://api.aicredits.in/v1");
    }
}

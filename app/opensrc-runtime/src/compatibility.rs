//! Explicit provider/model compatibility profiles.
//!
//! The standard profile is deliberately strict. Recovery for constrained local
//! models is opt-in by route so hosted providers never inherit local-model
//! heuristics merely because they expose the same canonical API shape.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompatibilityProfile {
    Standard,
    OllamaLocal,
    OllamaGemma,
}

impl CompatibilityProfile {
    #[must_use]
    pub fn for_route(provider: &str, model: &str) -> Self {
        if !provider.eq_ignore_ascii_case("ollama") {
            return Self::Standard;
        }
        let model = model.to_ascii_lowercase();
        if model
            .split(['/', ':', '-', '_'])
            .any(|part| part.starts_with("gemma"))
        {
            Self::OllamaGemma
        } else {
            Self::OllamaLocal
        }
    }

    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::OllamaLocal => "ollama-local",
            Self::OllamaGemma => "ollama-gemma",
        }
    }

    #[must_use]
    pub const fn repairs_tool_call_aliases(self) -> bool {
        matches!(self, Self::OllamaLocal | Self::OllamaGemma)
    }

    #[must_use]
    pub const fn materializes_filename_labeled_code(self) -> bool {
        matches!(self, Self::OllamaLocal | Self::OllamaGemma)
    }

    #[must_use]
    pub const fn forgives_redundant_invalid_calls(self) -> bool {
        matches!(self, Self::OllamaLocal | Self::OllamaGemma)
    }

    #[must_use]
    pub const fn repairs_calculator_companions(self) -> bool {
        matches!(self, Self::OllamaGemma)
    }
}

#[cfg(test)]
mod tests {
    use super::CompatibilityProfile;

    #[test]
    fn provider_matrix_keeps_local_recovery_out_of_hosted_routes() {
        for provider in [
            "openrouter",
            "aicredits",
            "openai",
            "gemini",
            "anthropic",
            "deepseek",
            "zai",
            "custom",
        ] {
            let profile = CompatibilityProfile::for_route(provider, "gemma-4");
            assert_eq!(profile, CompatibilityProfile::Standard, "{provider}");
            assert!(!profile.repairs_tool_call_aliases());
            assert!(!profile.materializes_filename_labeled_code());
            assert!(!profile.forgives_redundant_invalid_calls());
            assert!(!profile.repairs_calculator_companions());
        }
    }

    #[test]
    fn ollama_routes_select_only_the_needed_local_profile() {
        let generic = CompatibilityProfile::for_route("ollama", "llama3.3:70b");
        assert_eq!(generic, CompatibilityProfile::OllamaLocal);
        assert!(generic.repairs_tool_call_aliases());
        assert!(generic.materializes_filename_labeled_code());
        assert!(generic.forgives_redundant_invalid_calls());
        assert!(!generic.repairs_calculator_companions());

        for model in ["gemma4:e2b", "gemma-3-27b", "library/gemma_4"] {
            let profile = CompatibilityProfile::for_route("OLLAMA", model);
            assert_eq!(profile, CompatibilityProfile::OllamaGemma, "{model}");
            assert!(profile.repairs_calculator_companions());
        }
    }
}

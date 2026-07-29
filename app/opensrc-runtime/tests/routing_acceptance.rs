use async_trait::async_trait;
use futures::stream;
use opensrc_core::{
    CanonicalModelRequest, ContextInheritance, ContextPolicy, ExecutionMode, MessageContent,
    MessageRole, ModelEvent, ModelEventStream, ProviderAdapter, ProviderCapabilities,
    ProviderError,
};
use opensrc_runtime::{
    CostClass, ExecutionEngine, ModeClassifier, ProviderRouter, RoleExecutionKind,
    RoutingPolicyRegistry, ThinkingMode, ToolExecutor, ToolProfile,
};
use opensrc_store::Store;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct CapturingProvider {
    id: &'static str,
    requests: Arc<Mutex<Vec<CanonicalModelRequest>>>,
}

impl CapturingProvider {
    fn new(id: &'static str) -> Self {
        Self {
            id,
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn requests(&self) -> Vec<CanonicalModelRequest> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl ProviderAdapter for CapturingProvider {
    fn id(&self) -> &str {
        self.id
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }

    async fn execute(
        &self,
        request: CanonicalModelRequest,
    ) -> Result<Vec<ModelEvent>, ProviderError> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request);
        Ok(vec![
            ModelEvent::TextDelta {
                text: "ok".to_string(),
            },
            ModelEvent::Completed {
                response_id: Some("routing-acceptance".to_string()),
            },
        ])
    }

    async fn stream(
        &self,
        _request: CanonicalModelRequest,
    ) -> Result<ModelEventStream, ProviderError> {
        Ok(Box::pin(stream::empty()))
    }
}

fn provider_router() -> ProviderRouter {
    let providers = ProviderRouter::default();
    providers.register_with_model(
        Arc::new(CapturingProvider::new("deepseek")),
        "deepseek-v4-pro",
    );
    providers.register_with_models(
        Arc::new(CapturingProvider::new("kimi")),
        "kimi-k2.7-code",
        vec!["kimi-for-coding".to_string()],
    );
    providers.register_with_model(Arc::new(CapturingProvider::new("zai")), "glm-4.5");
    providers
}

#[test]
fn simple_question_stays_direct_and_uses_glm_instead_of_the_deepseek_planner() {
    let request = "Explain Rust ownership in one paragraph";
    assert_eq!(
        ModeClassifier::classify(request).mode,
        ExecutionMode::Direct
    );

    let assignment = RoutingPolicyRegistry::default()
        .resolve_for_role("generalist", request, &provider_router())
        .expect("generalist routing policy should resolve")
        .expect("generalist is an LLM-backed role");

    assert_eq!(assignment.alias, "glm-agent");
    assert_eq!(assignment.provider, "zai");
    assert_eq!(assignment.model, "glm-4.5");
    assert_ne!(assignment.alias, "deepseek-pro");
}

#[test]
fn small_edit_is_focused_and_routes_to_kimi_code() {
    let request = "Fix the small one-line bug in src/main.rs";
    assert_eq!(
        ModeClassifier::classify(request).mode,
        ExecutionMode::Focused
    );

    let registry = RoutingPolicyRegistry::default();
    let dynamic_assignment = registry
        .resolve_for_role("generalist", request, &provider_router())
        .expect("generalist routing policy should resolve")
        .expect("generalist is an LLM-backed role");
    let implementer_assignment = registry
        .resolve_for_role("implementer", request, &provider_router())
        .expect("implementer routing policy should resolve")
        .expect("implementer is an LLM-backed role");

    assert_eq!(dynamic_assignment.alias, "kimi-code");
    assert_eq!(implementer_assignment.alias, "kimi-code");
    assert_eq!(implementer_assignment.model, "kimi-k2.7-code");
}

#[test]
fn frontend_work_routes_to_glm_with_a_bounded_writer_profile() {
    let registry = RoutingPolicyRegistry::default();
    let assignment = registry
        .resolve_for_role(
            "frontend",
            "Build the settings popup and improve its layout",
            &provider_router(),
        )
        .expect("frontend alias should resolve")
        .expect("frontend is an LLM-backed role");
    let policy = registry
        .role("frontend")
        .expect("frontend alias should resolve to a role policy");

    assert_eq!(assignment.alias, "glm-agent");
    assert_eq!(assignment.provider, "zai");
    assert_eq!(policy.tool_profile, ToolProfile::BoundedWriter);
    assert!(policy.tool_profile.writes());
    assert_eq!(policy.writable_paths, ["<task-owned>"]);
}

#[test]
fn architecture_work_uses_deepseek_with_max_reasoning() {
    let request = "Redesign the architecture across the entire repository";
    assert_eq!(
        ModeClassifier::classify(request).mode,
        ExecutionMode::Agentic
    );

    let registry = RoutingPolicyRegistry::default();
    let assignment = registry
        .resolve_for_role("plan", request, &provider_router())
        .expect("plan alias should resolve")
        .expect("architect is an LLM-backed role");
    let policy = registry
        .role("plan")
        .expect("plan alias should resolve to architect");

    assert_eq!(policy.role, "architect");
    assert_eq!(assignment.alias, "deepseek-pro");
    assert_eq!(assignment.model, "deepseek-v4-pro");
    assert_eq!(policy.thinking, ThinkingMode::Enabled);
    assert_eq!(policy.reasoning_effort.as_deref(), Some("max"));
}

#[test]
fn visual_browser_validation_uses_multimodal_kimi_and_browser_tools() {
    let request = "Compare this screenshot with the rendered browser page";
    let registry = RoutingPolicyRegistry::default();
    let assignment = registry
        .resolve_for_role("browser", request, &provider_router())
        .expect("browser alias should resolve")
        .expect("browser validation is an LLM-backed role");
    let policy = registry
        .role("browser")
        .expect("browser alias should resolve to browser validation");
    let snapshot = registry.snapshot();
    let model = &snapshot.models[&assignment.alias];

    assert_eq!(assignment.alias, "kimi-code");
    assert_eq!(assignment.provider, "kimi");
    assert!(model.multimodal);
    assert!(model.always_thinking);
    assert_eq!(policy.tool_profile, ToolProfile::BrowserReadOnly);
}

#[test]
fn awaiter_is_a_zero_cost_deterministic_service_with_no_primary_model() {
    let registry = RoutingPolicyRegistry::default();
    let policy = registry.role("awaiter").expect("awaiter policy");
    let assignment = registry
        .resolve_for_role("awaiter", "Wait for the test process", &provider_router())
        .expect("awaiter routing should be valid");

    assert_eq!(policy.execution, RoleExecutionKind::Deterministic);
    assert_eq!(policy.cost_class, CostClass::Zero);
    assert!(policy.primary_model.is_none());
    assert!(policy.fallback_models.is_empty());
    assert!(!policy.deterministic_services.is_empty());
    assert!(assignment.is_none());
}

#[test]
fn explicit_model_aliases_and_normalized_role_aliases_resolve() {
    let providers = provider_router();
    let registry = RoutingPolicyRegistry::default();

    let explicit = registry
        .resolve_alias("kimi-code", &providers)
        .expect("explicit Kimi alias");
    let normalized = registry
        .resolve_for_role("review", "Review this patch", &providers)
        .expect("review role should resolve")
        .expect("review role is LLM-backed");
    let fallbacks = registry.fallback_assignments("frontend", &providers);

    assert_eq!(explicit.display_name, "Kimi K2.7 Code");
    assert_eq!(explicit.provider, "kimi");
    assert_eq!(normalized.alias, "deepseek-pro");
    assert_eq!(
        fallbacks
            .iter()
            .map(|fallback| fallback.alias.as_str())
            .collect::<Vec<_>>(),
        ["kimi-code", "deepseek-pro"]
    );

    let gateway_only = ProviderRouter::default();
    gateway_only.register_with_model(
        Arc::new(CapturingProvider::new("openrouter")),
        "deepseek-v4-pro",
    );
    let gateway = registry
        .resolve_alias("deepseek-pro", &gateway_only)
        .expect("OpenRouter should satisfy the explicit DeepSeek alias");
    assert_eq!(gateway.provider, "openrouter");
}

#[test]
fn built_in_roles_expose_bounded_context_inheritance_policies() {
    let registry = RoutingPolicyRegistry::default();
    let generalist = registry.role("generalist").expect("generalist policy");
    let architect = registry.role("architect").expect("architect policy");
    let awaiter = registry.role("awaiter").expect("awaiter policy");

    assert_eq!(
        generalist.context_policy.inheritance,
        ContextInheritance::SummaryOnly
    );
    assert_eq!(generalist.context_policy.max_tokens, Some(64_000));
    assert_eq!(
        architect.context_policy.inheritance,
        ContextInheritance::SelectedItems
    );
    assert_eq!(architect.context_policy.max_tokens, Some(100_000));
    assert_eq!(awaiter.context_policy.inheritance, ContextInheritance::None);
}

#[tokio::test]
async fn configured_last_n_turns_context_is_applied_before_the_model_call() {
    let store = Store::in_memory().expect("in-memory store");
    let conversation = store
        .create_conversation("F:\\routing-acceptance", Some("context".to_string()))
        .expect("conversation");
    for (role, text) in [
        (MessageRole::User, "old user turn"),
        (MessageRole::Assistant, "old assistant turn"),
        (MessageRole::User, "recent user turn"),
        (MessageRole::Assistant, "recent assistant turn"),
    ] {
        store
            .append_message(
                conversation.id,
                None,
                role,
                vec![MessageContent::text(text)],
                None,
                None,
                None,
            )
            .expect("history message");
    }
    let run = store
        .create_run(conversation.id, "latest user turn", ExecutionMode::Direct)
        .expect("run");

    let capture = CapturingProvider::new("zai");
    let providers = ProviderRouter::default();
    providers.register_with_model(Arc::new(capture.clone()), "glm-4.5");
    let policies = RoutingPolicyRegistry::default();
    let mut generalist = policies.role("generalist").expect("generalist policy");
    generalist.context_policy = ContextPolicy {
        inheritance: ContextInheritance::LastNTurns,
        last_n_turns: Some(2),
        selected_items: Vec::new(),
        max_tokens: None,
    };
    policies
        .upsert_role(generalist)
        .expect("updated context policy");

    let engine = ExecutionEngine::new(store, Arc::new(providers), ToolExecutor::default())
        .with_routing_policy_registry(policies);
    let result = engine
        .execute_run_with_policy(run.id, "zai", "glm-4.5", None, true)
        .await
        .expect("direct execution");

    assert_eq!(result.provider.as_deref(), Some("zai"));
    assert_eq!(result.model.as_deref(), Some("glm-4.5"));
    let requests = capture.requests();
    assert_eq!(requests.len(), 1);
    let texts = requests[0]
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            MessageContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        texts,
        [
            "recent user turn",
            "recent assistant turn",
            "latest user turn"
        ]
    );
    assert!(!texts.contains(&"old user turn"));
    assert!(!texts.contains(&"old assistant turn"));
}

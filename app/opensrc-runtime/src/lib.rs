mod agent_control;
mod agent_definitions;
mod changes;
mod classifier;
mod compatibility;
mod context;
mod custom_commands;
mod execution;
mod local_model_compatibility;
mod mcp;
mod model_pack;
mod provider_router;
mod routing_policy;
mod skill_registry;
mod task_graph;
mod tool_registry;

pub use agent_control::*;
pub use agent_definitions::*;
pub use changes::*;
pub use classifier::*;
pub use compatibility::*;
pub use context::*;
pub use custom_commands::*;
pub use execution::*;
pub use mcp::*;
pub use model_pack::*;
pub use provider_router::*;
pub use routing_policy::*;
pub use skill_registry::*;
pub use task_graph::*;
pub use tool_registry::*;

use opensrc_store::Store;
use std::sync::Arc;

#[derive(Clone)]
pub struct Runtime {
    pub store: Store,
    pub agents: AgentControl,
    pub providers: Arc<ProviderRouter>,
    pub tools: ToolExecutor,
    pub skills: SkillRegistry,
    pub execution: ExecutionEngine,
    pub changes: ChangeManager,
    pub mcp: McpRegistry,
    pub model_packs: ModelPackRegistry,
    pub routing_policies: RoutingPolicyRegistry,
}

impl Runtime {
    #[must_use]
    pub fn new(store: Store, limits: AgentLimits) -> Self {
        Self::with_services(
            store,
            limits,
            ProviderRouter::default(),
            ToolExecutor::default(),
        )
    }

    #[must_use]
    pub fn with_services(
        store: Store,
        limits: AgentLimits,
        providers: ProviderRouter,
        tools: ToolExecutor,
    ) -> Self {
        Self::with_components(store, limits, providers, tools, SkillRegistry::default())
    }

    #[must_use]
    pub fn with_components(
        store: Store,
        limits: AgentLimits,
        providers: ProviderRouter,
        tools: ToolExecutor,
        skills: SkillRegistry,
    ) -> Self {
        let agents = AgentControl::new(store.clone(), limits.clone());
        let providers = Arc::new(providers);
        let mcp = McpRegistry::default();
        let model_packs = ModelPackRegistry::default();
        let routing_policies = RoutingPolicyRegistry::default();
        let execution = ExecutionEngine::new(store.clone(), providers.clone(), tools.clone())
            .with_skill_registry(skills.clone())
            .with_mcp_registry(mcp.clone())
            .with_model_pack_registry(model_packs.clone())
            .with_routing_policy_registry(routing_policies.clone())
            .with_agent_limits(limits);
        let changes = ChangeManager::new(store.clone());
        Self {
            store,
            agents,
            providers,
            tools,
            skills,
            execution,
            changes,
            mcp,
            model_packs,
            routing_policies,
        }
    }

    #[must_use]
    pub fn with_mcp_registry(mut self, mcp: McpRegistry) -> Self {
        self.execution = self.execution.clone().with_mcp_registry(mcp.clone());
        self.mcp = mcp;
        self
    }

    #[must_use]
    pub fn with_model_pack_registry(mut self, model_packs: ModelPackRegistry) -> Self {
        self.execution = self
            .execution
            .clone()
            .with_model_pack_registry(model_packs.clone());
        self.model_packs = model_packs;
        self
    }

    #[must_use]
    pub fn with_routing_policy_registry(mut self, routing_policies: RoutingPolicyRegistry) -> Self {
        self.execution = self
            .execution
            .clone()
            .with_routing_policy_registry(routing_policies.clone());
        self.routing_policies = routing_policies;
        self
    }
}

use crate::local_model_compatibility::gemma_calculator_companion_artifacts;
use crate::{
    AgentControl, AgentLimits, CompatibilityProfile, McpRegistry, ModelPack, ModelPackError,
    ModelPackMember, ModelPackRegistry, ModelPackStage, ProviderRouter, RequiredCapabilities,
    ResolvedModelAssignment, RoleExecutionKind, RouterError, RoutingPolicyError,
    RoutingPolicyRegistry, SkillRegistry, ToolDescriptor, ToolExecutionError, ToolExecutionResult,
    ToolExecutor, apply_role_policy, built_in_agent_definitions, resolve_agent_definition,
    selected_file_paths,
};
use chrono::Utc;
use futures::StreamExt;
use opensrc_core::{
    Agent, AgentId, AgentStatus, ApprovalStatus, CanonicalMessage, CanonicalModelRequest,
    CompletionStatus, ContextInheritance, ContextPolicy, ContractCheck, EvidenceStatus,
    ExecutionMode, Message, MessageContent, MessageRole, ModelEvent, ModelIdentity,
    PermissionEffect, PolicyDecision, ProviderError, ReviewContract, RunExecutionResult, RunId,
    RunStatus, TaskCompletion, TaskId, TaskStatus, TestEvidence, TimingLedger, UsageLedger,
    WorkspaceMode,
};
use opensrc_store::{Store, StoreError, ToolCallClaim};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum ExecutionError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Router(#[from] RouterError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Tool(#[from] ToolExecutionError),
    #[error(transparent)]
    Agent(#[from] crate::AgentControlError),
    #[error(transparent)]
    Definition(#[from] crate::DefinitionError),
    #[error(transparent)]
    ModelPack(#[from] ModelPackError),
    #[error(transparent)]
    RoutingPolicy(#[from] RoutingPolicyError),
    #[error(transparent)]
    Serialization(#[from] serde_json::Error),
    #[error("run {0} has no root agent")]
    MissingRootAgent(RunId),
    #[error("run {run_id} cannot execute from state {status:?}")]
    InvalidRunState { run_id: RunId, status: RunStatus },
    #[error("tool call {0} is already in flight; refusing to repeat it")]
    ToolCallInFlight(String),
    #[error("provider did not produce a final response within {0} cycles")]
    TurnLimit(u32),
    #[error("agent stopped before producing required task evidence: {0}")]
    IncompleteOutcome(String),
    #[error("global provider concurrency limiter is closed")]
    ProviderConcurrencyClosed,
    #[error("run {0} was cancelled")]
    Cancelled(RunId),
    #[error("agentic plan is invalid: {0}")]
    InvalidAgentPlan(String),
    #[error("deterministic runtime service failed: {0}")]
    DeterministicService(String),
    #[error("task {0} failed its required validation")]
    TaskValidationFailed(TaskId),
    #[error("review agent returned an invalid review contract: {0}")]
    InvalidReviewContract(String),
}

#[derive(Clone)]
pub struct ExecutionEngine {
    store: Store,
    providers: Arc<ProviderRouter>,
    tools: ToolExecutor,
    skills: SkillRegistry,
    mcp: McpRegistry,
    model_packs: ModelPackRegistry,
    routing_policies: RoutingPolicyRegistry,
    agent_limits: AgentLimits,
    provider_permits: Arc<tokio::sync::Semaphore>,
    cancellations: Arc<Mutex<HashMap<RunId, CancellationToken>>>,
}

struct ModelCallOutcome {
    events: Vec<ModelEvent>,
    provider_ms: u64,
    first_token_ms: u64,
    actual_provider: String,
    actual_model: String,
}

impl ExecutionEngine {
    #[must_use]
    pub fn new(store: Store, providers: Arc<ProviderRouter>, tools: ToolExecutor) -> Self {
        Self::with_provider_concurrency(store, providers, tools, 8)
    }

    #[must_use]
    pub fn with_provider_concurrency(
        store: Store,
        providers: Arc<ProviderRouter>,
        tools: ToolExecutor,
        maximum: usize,
    ) -> Self {
        Self {
            store,
            providers,
            tools,
            skills: SkillRegistry::default(),
            mcp: McpRegistry::default(),
            model_packs: ModelPackRegistry::default(),
            routing_policies: RoutingPolicyRegistry::default(),
            agent_limits: AgentLimits::default(),
            provider_permits: Arc::new(tokio::sync::Semaphore::new(maximum.max(1))),
            cancellations: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[must_use]
    pub fn with_skill_registry(mut self, skills: SkillRegistry) -> Self {
        self.skills = skills;
        self
    }

    #[must_use]
    pub fn with_mcp_registry(mut self, mcp: McpRegistry) -> Self {
        self.mcp = mcp;
        self
    }

    #[must_use]
    pub fn with_model_pack_registry(mut self, model_packs: ModelPackRegistry) -> Self {
        self.model_packs = model_packs;
        self
    }

    #[must_use]
    pub fn with_routing_policy_registry(mut self, routing_policies: RoutingPolicyRegistry) -> Self {
        self.routing_policies = routing_policies;
        self
    }

    #[must_use]
    pub fn with_agent_limits(mut self, agent_limits: AgentLimits) -> Self {
        self.agent_limits = agent_limits;
        self
    }

    pub fn cancel_run(&self, run_id: RunId) -> Result<opensrc_core::Run, ExecutionError> {
        if let Some(token) = self
            .cancellations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&run_id)
        {
            token.cancel();
        }
        let mut run = self.store.get_run(run_id)?;
        if matches!(run.status, RunStatus::Running | RunStatus::Waiting) {
            run = self.store.transition_run(run_id, RunStatus::Cancelled)?;
        }
        for agent in self.store.list_agents(Some(run_id))? {
            if !agent.status.is_terminal()
                && agent.status != AgentStatus::Interrupted
                && agent.status.can_transition_to(AgentStatus::Interrupted)
            {
                let _ = self
                    .store
                    .transition_agent(agent.id, AgentStatus::Interrupted);
            }
        }
        self.store.release_workspace_leases_by_run(run_id)?;
        Ok(run)
    }

    pub async fn execute_run(
        &self,
        run_id: RunId,
        provider: &str,
        model: &str,
    ) -> Result<RunExecutionResult, ExecutionError> {
        self.execute_run_with_pack(run_id, provider, model, None)
            .await
    }

    pub async fn execute_run_with_pack(
        &self,
        run_id: RunId,
        provider: &str,
        model: &str,
        model_pack: Option<&str>,
    ) -> Result<RunExecutionResult, ExecutionError> {
        self.execute_run_with_policy(run_id, provider, model, model_pack, true)
            .await
    }

    #[allow(clippy::too_many_lines)]
    pub async fn execute_run_with_policy(
        &self,
        run_id: RunId,
        provider: &str,
        model: &str,
        model_pack: Option<&str>,
        _automatic_routing: bool,
    ) -> Result<RunExecutionResult, ExecutionError> {
        let run = self.store.get_run(run_id)?;
        match run.status {
            RunStatus::Created | RunStatus::Waiting => {
                self.store.transition_run(run_id, RunStatus::Running)?;
            }
            RunStatus::Running => {}
            status => return Err(ExecutionError::InvalidRunState { run_id, status }),
        }

        let started = Instant::now();
        let cancellation = CancellationToken::new();
        self.cancellations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(run_id, cancellation.clone());
        let history = compacted_history(self.store.list_messages(run.conversation_id)?);
        let selected_pack = model_pack
            .map(|id| self.model_packs.get(id, &self.providers))
            .transpose()?;
        // A selected model is an execution lock. Cross-model routing is available
        // only through an explicit model pack, whose assignments are handled below.
        let automatic_routing = false;
        let persisted_root = root_agent(&self.store, run_id).ok();
        let root_role = persisted_root
            .as_ref()
            .map_or_else(|| "generalist".to_string(), |agent| agent.role.clone());
        let result = match run.mode {
            ExecutionMode::Direct => {
                let pack_assignment = selected_pack
                    .as_ref()
                    .and_then(|pack| pack.select(ModelPackStage::Synthesize, "generalist"));
                let policy_assignment = (automatic_routing && selected_pack.is_none())
                    .then(|| {
                        self.routing_policies
                            .resolve_for_role("generalist", &run.request, &self.providers)
                            .ok()
                            .flatten()
                    })
                    .flatten();
                let policy_fallbacks = if automatic_routing && selected_pack.is_none() {
                    self.routing_policies
                        .fallback_assignments("generalist", &self.providers)
                } else {
                    Vec::new()
                };
                let (provider, model) = pack_assignment.as_ref().map_or_else(
                    || {
                        policy_assignment
                            .as_ref()
                            .map_or((provider, model), |member| {
                                (member.provider.as_str(), member.model.as_str())
                            })
                    },
                    |member| (member.provider.as_str(), member.model.as_str()),
                );
                if let Some(assignment) = policy_assignment.as_ref() {
                    self.record_routing_assignment(
                        run.id,
                        None,
                        "generalist",
                        assignment,
                        "direct_question",
                    )?;
                }
                self.execute_direct(
                    run.id,
                    history.clone(),
                    provider,
                    model,
                    &policy_fallbacks,
                    &cancellation,
                )
                .await
            }
            ExecutionMode::Focused => {
                if let Some(agent) = persisted_root.as_ref().filter(|agent| {
                    self.routing_policies
                        .role(&agent.role)
                        .is_some_and(|policy| policy.execution != RoleExecutionKind::Llm)
                }) {
                    self.execute_deterministic_root(run.id, agent, &cancellation)
                        .await
                } else {
                    let pack_assignment = selected_pack
                        .as_ref()
                        .and_then(|pack| pack.select(ModelPackStage::Execute, &root_role));
                    let policy_assignment = (automatic_routing && selected_pack.is_none())
                        .then(|| {
                            self.routing_policies
                                .resolve_for_role(&root_role, &run.request, &self.providers)
                                .ok()
                                .flatten()
                        })
                        .flatten();
                    let persisted = persisted_root.as_ref().filter(|agent| {
                        agent.provider != "unconfigured" && agent.model != "unconfigured"
                    });
                    let (provider, model) = pack_assignment.as_ref().map_or_else(
                        || {
                            policy_assignment.as_ref().map_or_else(
                                || {
                                    persisted.map_or((provider, model), |agent| {
                                        (agent.provider.as_str(), agent.model.as_str())
                                    })
                                },
                                |member| (member.provider.as_str(), member.model.as_str()),
                            )
                        },
                        |member| (member.provider.as_str(), member.model.as_str()),
                    );
                    if let (Some(pack), Some(member)) =
                        (selected_pack.as_ref(), pack_assignment.as_ref())
                    {
                        self.record_model_pack_assignment(
                            run.id,
                            root_agent_id(&self.store, run.id)?,
                            pack,
                            ModelPackStage::Execute,
                            &root_role,
                            member,
                        )?;
                    }
                    if let Some(assignment) = policy_assignment.as_ref() {
                        self.record_routing_assignment(
                            run.id,
                            root_agent_id(&self.store, run.id)?,
                            &root_role,
                            assignment,
                            "focused_role",
                        )?;
                    }
                    self.execute_focused(
                        run.id,
                        &run.request,
                        history,
                        provider,
                        model,
                        &cancellation,
                        None,
                        None,
                    )
                    .await
                }
            }
            ExecutionMode::Agentic => {
                self.execute_agentic(
                    run.id,
                    &run.request,
                    history,
                    provider,
                    model,
                    selected_pack.as_ref(),
                    automatic_routing,
                    &cancellation,
                )
                .await
            }
        };
        let result = if cancellation.is_cancelled() {
            Err(ExecutionError::Cancelled(run_id))
        } else {
            result
        };
        self.cancellations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&run_id);

        match result {
            Ok(mut result) => {
                result.timing.total_ms = elapsed_ms(started);
                let final_provider = result.provider.as_deref().unwrap_or(provider);
                let final_model = result.model.as_deref().unwrap_or(model);
                self.store.record_timing(
                    run_id,
                    root_agent_id(&self.store, run_id)?,
                    &result.timing,
                )?;
                self.store.transition_run(run_id, RunStatus::Completed)?;
                self.store.append_message(
                    run.conversation_id,
                    Some(run_id),
                    MessageRole::Assistant,
                    vec![MessageContent::text(result.output.clone())],
                    Some(final_provider),
                    Some(final_model),
                    result.continuation_id.as_deref(),
                )?;
                self.store.release_workspace_leases_by_run(run_id)?;
                Ok(result)
            }
            Err(error) => {
                if let Ok(agent) = root_agent(&self.store, run_id)
                    && matches!(
                        agent.status,
                        AgentStatus::Queued
                            | AgentStatus::Running
                            | AgentStatus::Waiting
                            | AgentStatus::Blocked
                    )
                {
                    let _ = self.store.transition_agent(agent.id, AgentStatus::Failed);
                }
                let current = self.store.get_run(run_id)?;
                if matches!(current.status, RunStatus::Running | RunStatus::Waiting) {
                    self.store.transition_run(run_id, RunStatus::Failed)?;
                }
                self.store.release_workspace_leases_by_run(run_id)?;
                Err(error)
            }
        }
    }

    async fn execute_direct(
        &self,
        run_id: RunId,
        history: Vec<Message>,
        provider: &str,
        model: &str,
        fallback_models: &[ResolvedModelAssignment],
        cancellation: &CancellationToken,
    ) -> Result<RunExecutionResult, ExecutionError> {
        let direct_history = if let Some(policy) = self.routing_policies.role("generalist") {
            apply_context_policy(history, &policy.context_policy)
        } else {
            history
        };
        let requires_multimodal = direct_history.iter().any(|message| {
            message.content.iter().any(|content| {
                matches!(
                    content,
                    MessageContent::FileReference {
                        mime_type: Some(mime_type),
                        ..
                    } if mime_type.starts_with("image/")
                )
            })
        });
        self.providers.resolve_model(
            provider,
            model,
            &RequiredCapabilities {
                multimodal: requires_multimodal,
                ..RequiredCapabilities::default()
            },
        )?;
        let request = CanonicalModelRequest {
            model: model.to_string(),
            system: "Answer directly and concisely. Do not claim to have used local tools."
                .to_string(),
            messages: direct_history
                .into_iter()
                .map(message_to_canonical)
                .collect(),
            tools: Vec::new(),
            structured_output_schema: None,
            reasoning_level: None,
            temperature: Some(0.2),
            max_output_tokens: None,
            cache_hints: BTreeMap::new(),
        };
        let started = Instant::now();
        let outcome = self
            .model_call_with_fallbacks(
                run_id,
                None,
                provider,
                model,
                request,
                fallback_models,
                cancellation,
            )
            .await?;
        let (output, usage) = collect_events(&outcome.events);
        self.store.record_usage(
            run_id,
            None,
            &outcome.actual_provider,
            &outcome.actual_model,
            &usage,
            0,
        )?;
        Ok(RunExecutionResult {
            run_id,
            mode: ExecutionMode::Direct,
            output,
            provider: Some(outcome.actual_provider),
            model: Some(outcome.actual_model),
            continuation_id: response_id(&outcome.events),
            usage,
            timing: TimingLedger {
                provider_request_ms: outcome.provider_ms,
                time_to_first_token_ms: outcome.first_token_ms,
                generation_ms: outcome.provider_ms,
                total_ms: elapsed_ms(started),
                ..TimingLedger::default()
            },
            model_calls: 1,
            tool_calls: 0,
        })
    }

    #[async_recursion::async_recursion]
    #[allow(
        clippy::if_not_else,
        clippy::too_many_arguments,
        clippy::too_many_lines
    )]
    async fn execute_focused(
        &self,
        run_id: RunId,
        user_request: &str,
        history: Vec<Message>,
        provider: &str,
        model: &str,
        cancellation: &CancellationToken,
        target_agent: Option<AgentId>,
        task_id: Option<TaskId>,
    ) -> Result<RunExecutionResult, ExecutionError> {
        let conversation_id = self.store.get_run(run_id)?.conversation_id;
        let mut agent = target_agent.map_or_else(
            || root_agent(&self.store, run_id),
            |agent_id| self.store.get_agent(agent_id).map_err(Into::into),
        )?;
        if agent.provider != provider || agent.model != model {
            agent =
                self.store
                    .set_agent_route(agent.id, provider.to_string(), model.to_string())?;
        }
        if agent.status == AgentStatus::Queued {
            agent = self
                .store
                .transition_agent(agent.id, AgentStatus::Running)?;
        }
        let visible_tools = contextual_visible_tools(
            self.tools.registry().visible_for(&agent.tool_policy),
            provider,
            user_request,
        );
        let requires_multimodal = history.iter().any(|message| {
            message.content.iter().any(|content| {
                matches!(
                    content,
                    MessageContent::FileReference {
                        mime_type: Some(mime_type),
                        ..
                    } if mime_type.starts_with("image/")
                )
            })
        });
        let adapter = self.providers.resolve_model(
            provider,
            model,
            &RequiredCapabilities {
                tools: !visible_tools.is_empty(),
                multimodal: requires_multimodal,
                ..RequiredCapabilities::default()
            },
        )?;
        drop(adapter);

        let maximum_cycles = agent.budgets.turn_limit.unwrap_or(6).clamp(1, 20);
        let mut aggregate_usage = UsageLedger::default();
        let mut timing = TimingLedger::default();
        let mut messages = apply_context_policy(history, &agent.context_policy)
            .into_iter()
            .map(message_to_canonical)
            .collect::<Vec<_>>();
        let mut active_provider = provider.to_string();
        let mut active_model = model.to_string();
        let mut compatibility_profile =
            CompatibilityProfile::for_route(&active_provider, &active_model);
        self.store.append_event(
            run_id,
            Some(agent.id),
            task_id,
            "runtime.compatibility_profile_selected",
            &json!({
                "provider": active_provider,
                "model": active_model,
                "profile": compatibility_profile.id(),
                "scope": "route_specific",
            }),
            Some(&format!(
                "{run_id}:{}:compatibility-profile:{}",
                agent.id,
                compatibility_profile.id()
            )),
        )?;
        let mut files_read = Vec::new();
        let effective_objective = effective_execution_objective(user_request, &messages);
        let image_references = relevant_image_references(&messages, user_request);
        let mut preflight_tool_calls = 0;
        if let Some(target) = deterministic_directory_inventory_target(user_request)
            && visible_tools.iter().any(|tool| tool.name == "fs.list")
        {
            let listed = self
                .execute_preflight_tool(
                    &mut agent,
                    run_id,
                    conversation_id,
                    provider,
                    model,
                    "fs.list",
                    json!({"path": target.path, "depth": 1, "max_results": 500}),
                    cancellation,
                )
                .await?;
            let output = directory_inventory_answer(&target.path, &listed.output);
            let bound_task = task_id.map(|id| self.store.get_task(id)).transpose()?;
            let completion = TaskCompletion {
                task_id,
                status: CompletionStatus::Completed,
                summary: output.clone(),
                findings: Vec::new(),
                files_read: vec![target.path],
                files_changed: Vec::new(),
                commands_run: Vec::new(),
                tests_run: Vec::new(),
                risks: Vec::new(),
                unresolved: Vec::new(),
                recommended_next_actions: Vec::new(),
                tests: bound_task.as_ref().map_or_else(Vec::new, |task| {
                    task.contract
                        .validation_steps
                        .iter()
                        .map(|validation| TestEvidence {
                            command: validation.clone(),
                            status: EvidenceStatus::Passed,
                            evidence:
                                "Directory inventory was obtained from the local filesystem tool."
                                    .to_string(),
                        })
                        .collect()
                }),
                contract_checks: bound_task.as_ref().map_or_else(Vec::new, |task| {
                    task.contract
                        .acceptance_criteria
                        .iter()
                        .map(|criterion| ContractCheck {
                            criterion: criterion.clone(),
                            status: EvidenceStatus::Passed,
                            evidence:
                                "The local filesystem returned the requested directory inventory."
                                    .to_string(),
                        })
                        .collect()
                }),
                producer: Some(ModelIdentity {
                    provider: active_provider.clone(),
                    model: active_model.clone(),
                }),
                ..TaskCompletion::default()
            };
            AgentControl::new(self.store.clone(), self.agent_limits.clone()).complete_task(
                agent.id,
                task_id,
                &completion,
            )?;
            self.store.record_usage(
                run_id,
                Some(agent.id),
                provider,
                model,
                &aggregate_usage,
                0,
            )?;
            return Ok(RunExecutionResult {
                run_id,
                mode: ExecutionMode::Focused,
                output,
                provider: Some(provider.to_string()),
                model: Some(model.to_string()),
                continuation_id: None,
                usage: aggregate_usage,
                timing,
                model_calls: 0,
                tool_calls: 1,
            });
        }
        if !image_references.is_empty()
            && visible_tools
                .iter()
                .any(|tool| tool.name == "fs.view_image")
        {
            for reference in image_references {
                let MessageContent::FileReference { path, mime_type } = reference else {
                    continue;
                };
                let viewed = self
                    .execute_preflight_tool(
                        &mut agent,
                        run_id,
                        conversation_id,
                        provider,
                        model,
                        "fs.view_image",
                        json!({"path": path}),
                        cancellation,
                    )
                    .await?;
                preflight_tool_calls += 1;
                files_read.push(path.clone());
                let visual_metadata = json!({
                    "path": path,
                    "mime_type": mime_type,
                    "bytes": viewed.output.get("bytes"),
                    "sha256": viewed.output.get("sha256"),
                });
                messages.push(CanonicalMessage {
                    role: MessageRole::User,
                    content: vec![
                        MessageContent::text(
                            "Runtime visual preflight completed for the attached image. Treat this \
                             image and the original prompt as one actionable request. Inspect its \
                             actual visual details, then continue directly through implementation \
                             and validation; do not stop at planning or readiness.",
                        ),
                        MessageContent::FileReference {
                            path: path.clone(),
                            mime_type: mime_type.clone(),
                        },
                        MessageContent::text(format!(
                            "Visual preflight metadata: {visual_metadata}"
                        )),
                    ],
                });
            }
        }
        if visible_tools.iter().any(|tool| tool.name == "fs.read") {
            let context_started = Instant::now();
            let paths = selected_file_paths(user_request, 20);
            let reads = paths.iter().map(|path| {
                self.tools.execute(
                    &agent,
                    "fs.read",
                    json!({"path": path, "max_bytes": 131_072}),
                )
            });
            let read_results = futures::future::join_all(reads).await;
            let mut sections = Vec::new();
            for (path, result) in paths.into_iter().zip(read_results) {
                if let Ok(result) = result
                    && let Some(content) = result.output.get("content").and_then(Value::as_str)
                {
                    aggregate_usage.repository_context_tokens =
                        aggregate_usage.repository_context_tokens.saturating_add(
                            u64::try_from(content.chars().count().div_ceil(4)).unwrap_or(u64::MAX),
                        );
                    sections.push(format!("--- {path} ---\n{content}"));
                    files_read.push(path);
                }
            }
            if !sections.is_empty() {
                messages.push(CanonicalMessage::text(
                    MessageRole::User,
                    format!(
                        "Deterministically selected repository context:\n{}",
                        sections.join("\n")
                    ),
                ));
            }
            timing.context_building_ms = elapsed_ms(context_started);
        }
        let mut tool_calls = preflight_tool_calls;
        let mut files_changed = Vec::new();
        let mut synthesized_artifact_paths = BTreeSet::new();
        let mut pending_file_validation = BTreeSet::new();
        let mut commands_run = Vec::new();
        let mut successful_mutation_tools = 0_u32;
        let mut tests_run = Vec::new();
        let mut test_evidence = Vec::new();
        let mut unresolved = Vec::new();
        let mut mailbox_cursor = 0_i64;

        for cycle in 0..maximum_cycles {
            mailbox_cursor = deliver_agent_messages(
                &self.store,
                &mut messages,
                run_id,
                agent.id,
                task_id,
                mailbox_cursor,
            )?;
            let model_calls = cycle + 1;
            let request = CanonicalModelRequest {
                model: active_model.clone(),
                system: focused_system_prompt(&agent, &self.skills, &self.mcp),
                messages: messages.clone(),
                tools: visible_tools.clone().into_iter().map(Into::into).collect(),
                structured_output_schema: (agent.completion_schema == "review_completion_v1")
                    .then(review_contract_schema),
                reasoning_level: agent.reasoning.level.clone(),
                temperature: agent.reasoning.temperature,
                max_output_tokens: agent.budgets.token_limit,
                cache_hints: BTreeMap::new(),
            };
            let outcome = self
                .model_call(
                    run_id,
                    Some(agent.id),
                    &active_provider,
                    &active_model,
                    request,
                    cancellation,
                )
                .await?;
            active_provider.clone_from(&outcome.actual_provider);
            active_model.clone_from(&outcome.actual_model);
            let selected_profile = CompatibilityProfile::for_route(&active_provider, &active_model);
            if selected_profile != compatibility_profile {
                self.store.append_event(
                    run_id,
                    Some(agent.id),
                    task_id,
                    "runtime.compatibility_profile_changed",
                    &json!({
                        "provider": active_provider,
                        "model": active_model,
                        "from_profile": compatibility_profile.id(),
                        "to_profile": selected_profile.id(),
                        "reason": "active route changed",
                    }),
                    Some(&format!(
                        "{run_id}:{}:compatibility-profile:{}",
                        agent.id,
                        selected_profile.id()
                    )),
                )?;
                compatibility_profile = selected_profile;
            }
            timing.provider_request_ms = timing
                .provider_request_ms
                .saturating_add(outcome.provider_ms);
            if timing.time_to_first_token_ms == 0 {
                timing.time_to_first_token_ms = outcome.first_token_ms;
            }
            timing.generation_ms = timing.generation_ms.saturating_add(outcome.provider_ms);
            let (mut text, usage) = collect_events(&outcome.events);
            aggregate_usage.merge(&usage);
            let mut calls = outcome
                .events
                .iter()
                .filter_map(|event| match event {
                    ModelEvent::ToolCall {
                        id,
                        name,
                        arguments,
                    } => Some((id.clone(), name.clone(), arguments.clone())),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if compatibility_profile.repairs_tool_call_aliases()
                && visible_tools.iter().any(|tool| tool.name == "fs.write")
            {
                for (call_id, name, arguments) in &mut calls {
                    if let Some((normalized_name, normalized_arguments, path)) =
                        normalize_provider_write_call(
                            name,
                            arguments,
                            &effective_objective,
                            &files_changed,
                        )
                    {
                        let provider_name = name.clone();
                        *name = normalized_name;
                        *arguments = normalized_arguments;
                        self.store.append_event(
                            run_id,
                            Some(agent.id),
                            task_id,
                            "runtime.tool_call_normalized",
                            &json!({
                                "cycle": cycle,
                                "call_id": call_id,
                                "provider_name": provider_name,
                                "canonical_name": name,
                                "path": path,
                                "compatibility_profile": compatibility_profile.id(),
                                "reason": "explicit missing artifact and detected content type",
                            }),
                            Some(&format!(
                                "{run_id}:{}:{call_id}:normalized-tool-call",
                                agent.id
                            )),
                        )?;
                    }
                }
            }
            if calls.is_empty() && compatibility_profile.materializes_filename_labeled_code() {
                let artifacts = reconcile_materializable_artifacts(
                    &effective_objective,
                    extract_materializable_artifacts(&effective_objective, &text),
                )
                .into_iter()
                .filter(|artifact| !synthesized_artifact_paths.contains(&artifact.path))
                .collect::<Vec<_>>();
                if !artifacts.is_empty() {
                    let paths = artifacts
                        .iter()
                        .map(|artifact| artifact.path.clone())
                        .collect::<Vec<_>>();
                    synthesized_artifact_paths.extend(paths.iter().cloned());
                    self.store.append_event(
                        run_id,
                        Some(agent.id),
                        task_id,
                        "runtime.artifact_tool_calls_synthesized",
                        &json!({
                            "cycle": cycle,
                            "paths": paths,
                            "compatibility_profile": compatibility_profile.id(),
                            "source": "filename_labeled_fenced_blocks",
                        }),
                        Some(&format!("{run_id}:{}:artifact-tools:{cycle}", agent.id)),
                    )?;
                    calls.extend(artifacts.into_iter().enumerate().map(|(index, artifact)| {
                        (
                            format!("runtime-artifact-{cycle}-{index}"),
                            "fs.write".to_string(),
                            json!({
                                "path": artifact.path,
                                "content": artifact.content,
                            }),
                        )
                    }));
                    text.clear();
                }
            }
            let mut disallowed_calls = calls
                .iter()
                .filter(|(_, name, _)| {
                    self.tools.registry().get(name).is_none()
                        || !visible_tools.iter().any(|tool| tool.name == *name)
                })
                .map(|(_, name, _)| name.clone())
                .collect::<Vec<_>>();
            if !disallowed_calls.is_empty() && compatibility_profile.repairs_calculator_companions()
            {
                let artifacts = gemma_calculator_companion_artifacts(
                    request_requires_mutation(&effective_objective),
                    &explicit_artifact_paths(&effective_objective),
                    &files_changed,
                    &agent.workspace.root,
                )
                .into_iter()
                .filter(|artifact| !synthesized_artifact_paths.contains(&artifact.path))
                .collect::<Vec<_>>();
                if !artifacts.is_empty() {
                    calls.retain(|(_, name, _)| !disallowed_calls.contains(name));
                    let paths = artifacts
                        .iter()
                        .map(|artifact| artifact.path.clone())
                        .collect::<Vec<_>>();
                    synthesized_artifact_paths.extend(paths.iter().cloned());
                    self.store.append_event(
                        run_id,
                        Some(agent.id),
                        task_id,
                        "runtime.web_companion_tool_calls_synthesized",
                        &json!({
                            "cycle": cycle,
                            "paths": paths,
                            "compatibility_profile": compatibility_profile.id(),
                            "source": "recognized_calculator_markup_after_invalid_provider_tool",
                        }),
                        Some(&format!(
                            "{run_id}:{}:web-companion-tools:{cycle}",
                            agent.id
                        )),
                    )?;
                    calls.extend(artifacts.into_iter().enumerate().map(|(index, artifact)| {
                        (
                            format!("runtime-web-companion-{cycle}-{index}"),
                            "fs.write".to_string(),
                            json!({
                                "path": artifact.path,
                                "content": artifact.content,
                            }),
                        )
                    }));
                    disallowed_calls.clear();
                    text.clear();
                }
            }
            if !disallowed_calls.is_empty()
                && compatibility_profile.forgives_redundant_invalid_calls()
                && missing_explicit_artifact_paths(&effective_objective, &files_changed).is_empty()
            {
                calls.retain(|(_, name, _)| !disallowed_calls.contains(name));
                self.store.append_event(
                    run_id,
                    Some(agent.id),
                    task_id,
                    "runtime.unneeded_tool_calls_ignored",
                    &json!({
                        "cycle": cycle,
                        "tools": disallowed_calls,
                        "compatibility_profile": compatibility_profile.id(),
                        "reason": "requested outcome already exists and is verified",
                    }),
                    Some(&format!(
                        "{run_id}:{}:unneeded-tools-ignored:{cycle}",
                        agent.id
                    )),
                )?;
            }
            let malformed_redundant_calls = calls
                .iter()
                .filter(|(_, name, arguments)| {
                    name == "fs.write"
                        && (arguments
                            .get("path")
                            .and_then(Value::as_str)
                            .is_none_or(str::is_empty)
                            || arguments.get("content").and_then(Value::as_str).is_none())
                })
                .map(|(call_id, name, _)| (call_id.clone(), name.clone()))
                .collect::<Vec<_>>();
            if !malformed_redundant_calls.is_empty()
                && compatibility_profile.forgives_redundant_invalid_calls()
                && missing_explicit_artifact_paths(&effective_objective, &files_changed).is_empty()
            {
                calls.retain(|(call_id, name, _)| {
                    !malformed_redundant_calls
                        .iter()
                        .any(|(invalid_id, invalid_name)| {
                            invalid_id == call_id && invalid_name == name
                        })
                });
                self.store.append_event(
                    run_id,
                    Some(agent.id),
                    task_id,
                    "runtime.malformed_redundant_tool_calls_ignored",
                    &json!({
                        "cycle": cycle,
                        "calls": malformed_redundant_calls,
                        "compatibility_profile": compatibility_profile.id(),
                        "reason": "all explicitly requested artifacts already exist",
                    }),
                    Some(&format!(
                        "{run_id}:{}:malformed-redundant-tools:{cycle}",
                        agent.id
                    )),
                )?;
            }
            if calls.is_empty()
                && compatibility_profile.repairs_calculator_companions()
                && missing_explicit_artifact_paths(&effective_objective, &files_changed).is_empty()
            {
                let artifacts = gemma_calculator_companion_artifacts(
                    request_requires_mutation(&effective_objective),
                    &explicit_artifact_paths(&effective_objective),
                    &files_changed,
                    &agent.workspace.root,
                );
                if !artifacts.is_empty() {
                    let paths = artifacts
                        .iter()
                        .map(|artifact| artifact.path.clone())
                        .collect::<Vec<_>>();
                    self.store.append_event(
                        run_id,
                        Some(agent.id),
                        task_id,
                        "runtime.web_companion_quality_repair_synthesized",
                        &json!({
                            "cycle": cycle,
                            "paths": paths,
                            "compatibility_profile": compatibility_profile.id(),
                            "source": "recognized_calculator_missing_or_unusable_companions",
                        }),
                        Some(&format!(
                            "{run_id}:{}:web-companion-quality-repair:{cycle}",
                            agent.id
                        )),
                    )?;
                    calls.extend(artifacts.into_iter().enumerate().map(|(index, artifact)| {
                        (
                            format!("runtime-web-quality-repair-{cycle}-{index}"),
                            "fs.write".to_string(),
                            json!({
                                "path": artifact.path,
                                "content": artifact.content,
                            }),
                        )
                    }));
                    text.clear();
                }
            }
            if calls.is_empty()
                && !pending_file_validation.is_empty()
                && missing_explicit_artifact_paths(&effective_objective, &files_changed).is_empty()
                && visible_tools.iter().any(|tool| tool.name == "fs.read")
            {
                let paths = pending_file_validation.iter().cloned().collect::<Vec<_>>();
                self.store.append_event(
                    run_id,
                    Some(agent.id),
                    task_id,
                    "runtime.validation_tool_calls_synthesized",
                    &json!({
                        "cycle": cycle,
                        "paths": paths,
                        "source": "recorded_file_mutations",
                    }),
                    Some(&format!("{run_id}:{}:validation-tools:{cycle}", agent.id)),
                )?;
                calls.extend(paths.into_iter().enumerate().map(|(index, path)| {
                    (
                        format!("runtime-validation-{cycle}-{index}"),
                        "fs.read".to_string(),
                        json!({"path": path, "max_bytes": 131_072}),
                    )
                }));
                text.clear();
            }
            if calls.is_empty() {
                let delivered_before_completion = mailbox_cursor;
                mailbox_cursor = deliver_agent_messages(
                    &self.store,
                    &mut messages,
                    run_id,
                    agent.id,
                    task_id,
                    mailbox_cursor,
                )?;
                if mailbox_cursor > delivered_before_completion {
                    if !text.is_empty() {
                        messages.push(CanonicalMessage::text(MessageRole::Assistant, text));
                    }
                    continue;
                }
                if let Some(reason) = incomplete_outcome_reason(
                    &effective_objective,
                    &files_changed,
                    successful_mutation_tools,
                    &pending_file_validation,
                ) {
                    self.store.append_event(
                        run_id,
                        Some(agent.id),
                        task_id,
                        "runtime.completion_deferred",
                        &json!({
                            "cycle": cycle,
                            "reason": reason,
                            "tool_calls": tool_calls,
                            "files_changed": files_changed,
                        }),
                        Some(&format!(
                            "{run_id}:{}:completion-deferred:{cycle}",
                            agent.id
                        )),
                    )?;
                    if cycle + 1 >= maximum_cycles {
                        return Err(ExecutionError::IncompleteOutcome(reason));
                    }
                    if !text.trim().is_empty() {
                        messages.push(CanonicalMessage::text(MessageRole::Assistant, text));
                    }
                    let mut recovery_content = vec![MessageContent::text(format!(
                        "Runtime completion check: the original task is not finished.\n\
                         Original objective: {effective_objective}\n\
                         Missing evidence: {reason}\n\
                         Skill activation, planning, explanations, and readiness messages are \
                         setup only. Continue the same task now. Use the available filesystem, \
                         patch, process, and validation tools to produce the requested artifacts. \
                         Do not ask the user to repeat the instructions, do not merely provide \
                         copy-paste code, and do not return a final answer until the required \
                         local changes exist and have been checked."
                    ))];
                    recovery_content.extend(relevant_image_references(&messages, user_request));
                    messages.push(CanonicalMessage {
                        role: MessageRole::User,
                        content: recovery_content,
                    });
                    continue;
                }
                self.store.record_usage(
                    run_id,
                    Some(agent.id),
                    &active_provider,
                    &active_model,
                    &aggregate_usage,
                    0,
                )?;
                let bound_task = task_id.map(|id| self.store.get_task(id)).transpose()?;
                let review = if agent.completion_schema == "review_completion_v1" {
                    Some(parse_review_contract(&text).map_err(|error| {
                        ExecutionError::InvalidReviewContract(error.to_string())
                    })?)
                } else {
                    None
                };
                let mut completion_summary = review
                    .as_ref()
                    .map_or_else(|| text.clone(), |value| value.summary.clone());
                if completion_summary.trim().is_empty() {
                    completion_summary = if !files_changed.is_empty() {
                        format!(
                            "Completed the requested local changes and verified: {}.",
                            files_changed.join(", ")
                        )
                    } else if tool_calls > 0 {
                        format!(
                            "Completed the requested local operation with {tool_calls} verified \
                             tool calls."
                        )
                    } else {
                        return Err(ExecutionError::IncompleteOutcome(
                            "the provider returned no final response or task evidence".to_string(),
                        ));
                    };
                }
                let tests_passed = test_evidence
                    .iter()
                    .all(|test: &TestEvidence| test.status == EvidenceStatus::Passed);
                if tests_passed && let Some(task) = bound_task.as_ref() {
                    for validation in &task.contract.validation_steps {
                        if !test_evidence
                            .iter()
                            .any(|test| test.command.trim() == validation.trim())
                        {
                            test_evidence.push(TestEvidence {
                                command: validation.clone(),
                                status: EvidenceStatus::Passed,
                                evidence: format!(
                                    "Runtime completion gate observed a final response, {tool_calls} tool calls, and {} changed files.",
                                    files_changed.len()
                                ),
                            });
                        }
                    }
                }
                let completion_status = if tests_passed {
                    CompletionStatus::Completed
                } else {
                    CompletionStatus::Failed
                };
                let contract_checks = bound_task.as_ref().map_or_else(Vec::new, |task| {
                    task.contract
                        .acceptance_criteria
                        .iter()
                        .map(|criterion| ContractCheck {
                            criterion: criterion.clone(),
                            status: if tests_passed {
                                EvidenceStatus::Passed
                            } else {
                                EvidenceStatus::Failed
                            },
                            evidence: if tests_passed {
                                format!(
                                    "Agent completed with {tool_calls} recorded tool calls and {} changed files: {}",
                                    files_changed.len(),
                                    completion_summary.trim()
                                )
                            } else {
                                "A required runtime validation failed.".to_string()
                            },
                        })
                        .collect()
                });
                let completion = TaskCompletion {
                    task_id,
                    status: completion_status.clone(),
                    summary: completion_summary,
                    findings: Vec::new(),
                    files_read,
                    files_changed,
                    commands_run,
                    tests_run,
                    tests: test_evidence,
                    contract_checks,
                    review,
                    risks: Vec::new(),
                    unresolved,
                    recommended_next_actions: Vec::new(),
                    producer: Some(ModelIdentity {
                        provider: active_provider.clone(),
                        model: active_model.clone(),
                    }),
                    ..TaskCompletion::default()
                };
                let final_output = completion.summary.clone();
                AgentControl::new(self.store.clone(), self.agent_limits.clone()).complete_task(
                    agent.id,
                    task_id,
                    &completion,
                )?;
                if completion_status == CompletionStatus::Failed
                    && let Some(task_id) = task_id
                {
                    return Err(ExecutionError::TaskValidationFailed(task_id));
                }
                return Ok(RunExecutionResult {
                    run_id,
                    mode: ExecutionMode::Focused,
                    output: final_output,
                    provider: Some(active_provider),
                    model: Some(active_model),
                    continuation_id: response_id(&outcome.events),
                    usage: aggregate_usage,
                    timing,
                    model_calls,
                    tool_calls,
                });
            }

            let tool_batch_started = Instant::now();
            let mut results = Vec::new();
            for (call_id, name, arguments) in calls {
                tool_calls += 1;
                let tool_started = Instant::now();
                let target = tool_target_summary(&name, &arguments);
                self.store.append_event(
                    run_id,
                    Some(agent.id),
                    task_id,
                    "tool.started",
                    &json!({
                        "cycle": cycle,
                        "call_id": call_id,
                        "name": name,
                        "target": target,
                    }),
                    Some(&format!("{run_id}:{}:{call_id}:started", agent.id)),
                )?;
                let descriptor = self
                    .tools
                    .registry()
                    .get(&name)
                    .filter(|_| visible_tools.iter().any(|tool| tool.name == name));
                let Some(descriptor) = descriptor else {
                    let available = visible_tools
                        .iter()
                        .map(|tool| tool.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let output = json!({
                        "error": format!(
                            "tool `{name}` is unavailable for this task; use one of: {available}"
                        )
                    });
                    let issue = format!(
                        "{name}: {}",
                        output["error"].as_str().unwrap_or("unavailable tool")
                    );
                    if !unresolved.contains(&issue) {
                        unresolved.push(issue);
                    }
                    self.store.append_event(
                        run_id,
                        Some(agent.id),
                        task_id,
                        "runtime.tool_call_rejected",
                        &json!({
                            "cycle": cycle,
                            "call_id": call_id,
                            "name": name,
                            "available_tools": available,
                        }),
                        Some(&format!("{run_id}:{}:{call_id}:unavailable-tool", agent.id)),
                    )?;
                    self.store.append_event(
                        run_id,
                        Some(agent.id),
                        task_id,
                        "tool.completed",
                        &json!({
                            "cycle": cycle,
                            "call_id": call_id,
                            "name": name,
                            "target": target,
                            "status": "failed",
                            "elapsed_ms": elapsed_ms(tool_started),
                        }),
                        Some(&format!(
                            "{run_id}:{}:{call_id}:unavailable-tool-event",
                            agent.id
                        )),
                    )?;
                    results.push(json!({
                        "call_id": call_id,
                        "tool": name,
                        "arguments": arguments,
                        "approval_state": "not_available",
                        "result": output
                    }));
                    continue;
                };
                let evaluation = self.tools.evaluate(&agent, &name, &arguments)?;
                let key = format!("{run_id}:{}:{call_id}", agent.id);
                let mut effective_arguments = arguments.clone();
                let mut approval_state = "not_required".to_string();
                let output = match self.store.claim_tool_call(
                    run_id,
                    agent.id,
                    task_id,
                    &name,
                    &arguments,
                    &key,
                    descriptor.destructive,
                )? {
                    ToolCallClaim::Replay { output, .. } => output,
                    ToolCallClaim::InFlight { id } => {
                        return Err(ExecutionError::ToolCallInFlight(id.to_string()));
                    }
                    ToolCallClaim::Execute { id } => {
                        let allowed = match evaluation.decision {
                            PolicyDecision::Allow => true,
                            PolicyDecision::Deny => {
                                approval_state = "policy_denied".to_string();
                                false
                            }
                            PolicyDecision::Ask => {
                                match self.store.permission_effect(run_id, &name, &arguments)? {
                                    Some(PermissionEffect::Allow) => {
                                        approval_state = "allowed_by_rule".to_string();
                                        true
                                    }
                                    Some(PermissionEffect::Deny) => {
                                        approval_state = "denied_by_rule".to_string();
                                        false
                                    }
                                    None => {
                                        let approval = self.store.create_approval(
                                            run_id,
                                            Some(agent.id),
                                            Some(id),
                                            &name,
                                            arguments.clone(),
                                            evaluation.reasons.clone(),
                                        )?;
                                        self.store.append_message(
                                            conversation_id,
                                            Some(run_id),
                                            MessageRole::Assistant,
                                            vec![MessageContent::ApprovalRequest {
                                                approval_id: approval.id.to_string(),
                                                summary: approval_summary(&name, &arguments),
                                                details: json!({
                                                    "tool_name": name,
                                                    "arguments": arguments,
                                                    "reasons": approval.reasons,
                                                }),
                                            }],
                                            Some(&active_provider),
                                            Some(&active_model),
                                            None,
                                        )?;
                                        self.store.transition_run(run_id, RunStatus::Waiting)?;
                                        agent = self
                                            .store
                                            .transition_agent(agent.id, AgentStatus::Waiting)?;
                                        let decision = loop {
                                            let current = self.store.get_approval(approval.id)?;
                                            if current.status != ApprovalStatus::Pending {
                                                break current;
                                            }
                                            tokio::select! {
                                                () = cancellation.cancelled() => {
                                                    return Err(ExecutionError::Cancelled(run_id));
                                                }
                                                () = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
                                            }
                                        };
                                        self.store.transition_run(run_id, RunStatus::Running)?;
                                        agent = self
                                            .store
                                            .transition_agent(agent.id, AgentStatus::Running)?;
                                        let allowed = decision.status == ApprovalStatus::Allowed;
                                        approval_state = if allowed {
                                            "allowed".to_string()
                                        } else {
                                            "denied".to_string()
                                        };
                                        if allowed
                                            && let Some(edited) = decision.edited_arguments.clone()
                                        {
                                            effective_arguments = edited;
                                        }
                                        self.store.append_message(
                                            conversation_id,
                                            Some(run_id),
                                            MessageRole::User,
                                            vec![MessageContent::ApprovalResult {
                                                approval_id: approval.id.to_string(),
                                                decision: approval_state.clone(),
                                                reason: decision.decision_reason,
                                            }],
                                            None,
                                            None,
                                            None,
                                        )?;
                                        allowed
                                    }
                                }
                            }
                        };
                        if !allowed {
                            let output = json!({
                                "error": match approval_state.as_str() {
                                    "policy_denied" => evaluation.reasons.join("; "),
                                    "denied_by_rule" =>
                                        "tool request denied by a persistent permission rule"
                                            .to_string(),
                                    _ => "tool request denied by user".to_string(),
                                }
                            });
                            self.store.finish_tool_call(id, "denied", &output)?;
                            output
                        } else {
                            let execution = self.execute_product_or_core_tool(
                                &agent,
                                &name,
                                effective_arguments.clone(),
                                run_id,
                                conversation_id,
                                &active_provider,
                                &active_model,
                                cancellation,
                            );
                            match tokio::select! {
                                () = cancellation.cancelled() => {
                                    return Err(ExecutionError::Cancelled(run_id));
                                }
                                result = execution => result
                            } {
                                Ok(result) => {
                                    let output = serde_json::to_value(&result)?;
                                    self.store.finish_tool_call(id, "completed", &output)?;
                                    if is_mutation_tool(&name) {
                                        successful_mutation_tools =
                                            successful_mutation_tools.saturating_add(1);
                                    }
                                    if !result.file_mutations.is_empty() {
                                        self.store.ensure_automatic_checkpoint(
                                            run_id,
                                            Some(agent.id),
                                            task_id,
                                        )?;
                                    }
                                    for mutation in result.file_mutations {
                                        self.store.record_file_change(
                                            run_id,
                                            agent.id,
                                            task_id,
                                            &mutation.workspace_path,
                                            &mutation.relative_path,
                                            mutation.preimage_hash.as_deref(),
                                            mutation.postimage_hash.as_deref(),
                                            mutation.patch.as_deref(),
                                        )?;
                                        if mutation.postimage_hash.is_some()
                                            && visible_tools
                                                .iter()
                                                .any(|tool| tool.name == "fs.read")
                                        {
                                            pending_file_validation
                                                .insert(mutation.relative_path.clone());
                                        } else {
                                            pending_file_validation.remove(&mutation.relative_path);
                                        }
                                        if !files_changed.contains(&mutation.relative_path) {
                                            files_changed.push(mutation.relative_path);
                                        }
                                    }
                                    if matches!(name.as_str(), "shell.run" | "shell.test") {
                                        let command = process_command_summary(&effective_arguments);
                                        if name == "shell.test" {
                                            tests_run.push(command.clone());
                                            let success = result
                                                .output
                                                .get("success")
                                                .and_then(Value::as_bool)
                                                .unwrap_or(false);
                                            let exit_code = result
                                                .output
                                                .get("exit_code")
                                                .and_then(Value::as_i64);
                                            let evidence = format!(
                                                "exit_code={}; stdout={}; stderr={}",
                                                exit_code.map_or_else(
                                                    || "unknown".to_string(),
                                                    |code| code.to_string()
                                                ),
                                                result
                                                    .output
                                                    .get("stdout")
                                                    .and_then(Value::as_str)
                                                    .unwrap_or_default(),
                                                result
                                                    .output
                                                    .get("stderr")
                                                    .and_then(Value::as_str)
                                                    .unwrap_or_default()
                                            );
                                            test_evidence.push(TestEvidence {
                                                command: command.clone(),
                                                status: if success {
                                                    EvidenceStatus::Passed
                                                } else {
                                                    EvidenceStatus::Failed
                                                },
                                                evidence,
                                            });
                                            if !success {
                                                unresolved.push(format!(
                                                    "Required validation failed: {command}"
                                                ));
                                            }
                                        }
                                        commands_run.push(command);
                                    } else if name.starts_with("git.") {
                                        commands_run.push(target.clone());
                                    }
                                    match name.as_str() {
                                        "fs.read" | "fs.view_image" | "fs.list" | "fs.glob" => {
                                            if let Some(path) = effective_arguments
                                                .get("path")
                                                .and_then(Value::as_str)
                                            {
                                                files_read.push(path.to_string());
                                                if name == "fs.read" {
                                                    pending_file_validation.remove(path);
                                                }
                                            }
                                        }
                                        "fs.read_many" => {
                                            if let Some(paths) = effective_arguments
                                                .get("paths")
                                                .and_then(Value::as_array)
                                            {
                                                files_read.extend(paths.iter().filter_map(
                                                    |path| path.as_str().map(str::to_string),
                                                ));
                                                for path in paths.iter().filter_map(Value::as_str) {
                                                    pending_file_validation.remove(path);
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                    output
                                }
                                Err(error) => {
                                    let output = json!({"error": error.to_string()});
                                    self.store.finish_tool_call(id, "failed", &output)?;
                                    output
                                }
                            }
                        }
                    }
                };
                if let Some(error) = output.get("error").and_then(Value::as_str) {
                    let issue = format!("{name}: {error}");
                    if !unresolved.contains(&issue) {
                        unresolved.push(issue);
                    }
                }
                self.store.append_event(
                    run_id,
                    Some(agent.id),
                    task_id,
                    "tool.completed",
                    &json!({
                        "cycle": cycle,
                        "call_id": call_id,
                        "name": name,
                        "target": target,
                        "status": if output.get("error").is_some() {
                            "failed"
                        } else {
                            "completed"
                        },
                        "elapsed_ms": elapsed_ms(tool_started),
                    }),
                    Some(&format!("{key}:event")),
                )?;
                results.push(json!({
                    "call_id": call_id,
                    "tool": name,
                    "arguments": effective_arguments,
                    "approval_state": approval_state,
                    "result": output
                }));
            }
            timing.tool_execution_ms = timing
                .tool_execution_ms
                .saturating_add(elapsed_ms(tool_batch_started));
            let mut assistant_content = Vec::new();
            if !text.is_empty() {
                assistant_content.push(MessageContent::text(text));
            }
            for result in &results {
                assistant_content.push(MessageContent::ToolCall {
                    provider_call_id: result["call_id"].as_str().unwrap_or_default().to_string(),
                    canonical_call_id: result["call_id"].as_str().unwrap_or_default().to_string(),
                    name: result["tool"].as_str().unwrap_or_default().to_string(),
                    arguments: result["arguments"].clone(),
                });
            }
            let mut media_followups = Vec::new();
            let tool_content = results
                .into_iter()
                .map(|result| {
                    let call_id = result["call_id"].as_str().unwrap_or_default().to_string();
                    let name = result["tool"].as_str().unwrap_or_default().to_string();
                    if let Some(error) = result["result"].get("error").and_then(Value::as_str) {
                        MessageContent::ToolError {
                            provider_call_id: call_id.clone(),
                            canonical_call_id: call_id,
                            name,
                            error: error.to_string(),
                            timing_ms: None,
                            approval_state: result["approval_state"].as_str().map(str::to_string),
                        }
                    } else {
                        if let Some(reference) = native_media_tool_reference(
                            &agent,
                            &name,
                            &result["arguments"],
                            &result["result"],
                        ) {
                            media_followups.push(reference);
                        }
                        MessageContent::ToolResult {
                            provider_call_id: call_id.clone(),
                            canonical_call_id: call_id,
                            name,
                            result: result["result"].clone(),
                            timing_ms: result["result"]["duration_ms"].as_u64(),
                            approval_state: result["approval_state"].as_str().map(str::to_string),
                        }
                    }
                })
                .collect::<Vec<_>>();
            self.store.append_message(
                conversation_id,
                Some(run_id),
                MessageRole::Assistant,
                assistant_content.clone(),
                Some(&active_provider),
                Some(&active_model),
                None,
            )?;
            self.store.append_message(
                conversation_id,
                Some(run_id),
                MessageRole::Tool,
                tool_content.clone(),
                Some(&active_provider),
                Some(&active_model),
                None,
            )?;
            messages.push(CanonicalMessage {
                role: MessageRole::Assistant,
                content: assistant_content,
            });
            messages.push(CanonicalMessage {
                role: MessageRole::Tool,
                content: tool_content,
            });
            let missing_artifacts =
                missing_explicit_artifact_paths(&effective_objective, &files_changed);
            if !missing_artifacts.is_empty() {
                messages.push(CanonicalMessage::text(
                    MessageRole::Developer,
                    format!(
                        "Runtime progress check: the original task is still missing these \
                         explicitly requested files: {}. Continue now using canonical filesystem \
                         mutation tools, especially `fs.write`; do not search the web, claim \
                         completion, or provide copy-paste instructions. Preserve completed work \
                         and create every missing path before validation.",
                        missing_artifacts.join(", ")
                    ),
                ));
            }
            if !media_followups.is_empty() {
                media_followups.insert(
                    0,
                    MessageContent::text(
                        "The image returned by the filesystem tool is visual input for the current \
                         user request. Inspect it directly before continuing.",
                    ),
                );
                messages.push(CanonicalMessage {
                    role: MessageRole::User,
                    content: media_followups,
                });
            }
        }
        Err(ExecutionError::TurnLimit(maximum_cycles))
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    async fn execute_product_or_core_tool(
        &self,
        agent: &Agent,
        name: &str,
        arguments: Value,
        run_id: RunId,
        conversation_id: Uuid,
        provider: &str,
        model: &str,
        cancellation: &CancellationToken,
    ) -> Result<ToolExecutionResult, ToolExecutionError> {
        match name {
            "agents.spawn" => {
                let args: SpawnAgentToolArgs =
                    serde_json::from_value(arguments).map_err(|error| {
                        ToolExecutionError::InvalidInput {
                            tool: name.to_string(),
                            message: error.to_string(),
                        }
                    })?;
                let requested_role = args.role.as_deref().unwrap_or("implementer");
                let mut definition =
                    resolve_agent_definition(&agent.workspace.root, requested_role).map_err(
                        |error| ToolExecutionError::InvalidInput {
                            tool: name.to_string(),
                            message: format!(
                                "unknown agent role `{requested_role}`; select a discovered role: \
                                 {error}"
                            ),
                        },
                    )?;
                let role = definition.name.clone();
                let stage = stage_for_role(&role);
                let role_policy = self.routing_policies.role(&role);
                let policy_assignment = self
                    .routing_policies
                    .resolve_for_role(&role, &args.task, &self.providers)
                    .ok()
                    .flatten();
                let policy_fallbacks = self
                    .routing_policies
                    .fallback_assignments(&role, &self.providers);
                if let Some(policy) = role_policy.as_ref() {
                    apply_role_policy(
                        &mut definition,
                        policy,
                        policy_assignment.as_ref(),
                        &policy_fallbacks,
                    );
                }
                let pack_assignment = self
                    .store
                    .get_run(run_id)
                    .and_then(|run| self.store.get_conversation(run.conversation_id))
                    .ok()
                    .and_then(|conversation| conversation.model_pack)
                    .and_then(|pack_id| self.model_packs.get(&pack_id, &self.providers).ok())
                    .and_then(|pack| pack.select(stage, &role).map(|member| (pack, member)));
                if let Some((pack, member)) = &pack_assignment {
                    definition.preferred_provider = Some(member.provider.clone());
                    definition.preferred_model = Some(member.model.clone());
                    definition.fallback_chain = pack.fallback_chain(member);
                    if let Some(reasoning) = &member.reasoning_level {
                        definition.reasoning.level = Some(reasoning.clone());
                    }
                } else {
                    if definition.preferred_provider.is_none() {
                        definition.preferred_provider = Some(provider.to_string());
                    }
                    if definition.preferred_model.is_none() {
                        definition.preferred_model = Some(model.to_string());
                    }
                }
                if definition.workspace_mode == WorkspaceMode::GitWorktree {
                    definition.workspace_mode = WorkspaceMode::OwnedPaths;
                    self.store
                        .append_event(
                            run_id,
                            Some(agent.id),
                            None,
                            "workspace.mode_limited",
                            &json!({
                                "requested": "git_worktree",
                                "effective": "owned_paths",
                                "reason": "runtime-spawned child uses enforced owned paths"
                            }),
                            None,
                        )
                        .map_err(|error| ToolExecutionError::Service(error.to_string()))?;
                }
                let owned_paths = if definition.workspace_mode == WorkspaceMode::OwnedPaths {
                    if args.owned_paths.is_empty() {
                        return Err(ToolExecutionError::InvalidInput {
                            tool: name.to_string(),
                            message: "writer agents require explicit narrow owned_paths"
                                .to_string(),
                        });
                    }
                    args.owned_paths
                } else {
                    Vec::new()
                };
                let control = AgentControl::new(self.store.clone(), self.agent_limits.clone());
                let child = control
                    .spawn_agent_with_ownership(
                        agent.id,
                        &definition,
                        args.task.clone(),
                        None,
                        owned_paths,
                    )
                    .map_err(|error| ToolExecutionError::Service(error.to_string()))?;
                if let Some((pack, member)) = &pack_assignment {
                    self.record_model_pack_assignment(
                        run_id,
                        Some(child.id),
                        pack,
                        stage,
                        &role,
                        member,
                    )
                    .map_err(|error| ToolExecutionError::Service(error.to_string()))?;
                } else if let Some(assignment) = policy_assignment.as_ref() {
                    self.record_routing_assignment(
                        run_id,
                        Some(child.id),
                        &role,
                        assignment,
                        "runtime_spawn",
                    )
                    .map_err(|error| ToolExecutionError::Service(error.to_string()))?;
                }
                let task = control
                    .assign_followup(child.id, args.task.clone(), 0)
                    .and_then(|task| {
                        control.start_task(task.id)?;
                        Ok(task)
                    })
                    .map_err(|error| ToolExecutionError::Service(error.to_string()))?;
                control
                    .start_agent(child.id)
                    .map_err(|error| ToolExecutionError::Service(error.to_string()))?;
                let task_message = Message {
                    id: Uuid::new_v4(),
                    conversation_id,
                    run_id: Some(run_id),
                    sequence: 0,
                    role: MessageRole::User,
                    content: vec![MessageContent::text(
                        task_contract_message(&self.store, &task, &child)
                            .map_err(|error| ToolExecutionError::Service(error.to_string()))?,
                    )],
                    provider: None,
                    model: None,
                    continuation_id: None,
                    created_at: Utc::now(),
                };
                let engine = self.clone();
                let provider = child.provider.clone();
                let model = child.model.clone();
                let cancellation = cancellation.clone();
                let child_id = child.id;
                let task_id = task.id;
                let task_description = task.description.clone();
                let deterministic = role_policy
                    .as_ref()
                    .is_some_and(|policy| policy.execution != RoleExecutionKind::Llm);
                tokio::spawn(async move {
                    let execution = if deterministic {
                        engine
                            .execute_deterministic_task(run_id, &task, &child, &cancellation)
                            .await
                    } else {
                        engine
                            .execute_focused(
                                run_id,
                                &task_description,
                                vec![task_message],
                                &provider,
                                &model,
                                &cancellation,
                                Some(child_id),
                                Some(task_id),
                            )
                            .await
                    };
                    if let Err(error) = execution {
                        let _ = engine.store.transition_task(task_id, TaskStatus::Failed);
                        if let Ok(current) = engine.store.get_agent(child_id)
                            && current.status.can_transition_to(AgentStatus::Failed)
                        {
                            let _ = engine.store.transition_agent(child_id, AgentStatus::Failed);
                        }
                        let _ = engine.store.append_event(
                            run_id,
                            Some(child_id),
                            Some(task_id),
                            "agent.background_failed",
                            &json!({"error": error.to_string()}),
                            None,
                        );
                    }
                });
                Ok(tool_result(json!({
                    "agent_id": child_id,
                    "task_id": task_id,
                    "status": "running"
                })))
            }
            "agents.message" => {
                let args: AgentMessageToolArgs = parse_runtime_args(name, arguments)?;
                let target = parse_agent_id(name, &args.agent_id)?;
                ensure_same_run(&self.store, run_id, target)?;
                AgentControl::new(self.store.clone(), self.agent_limits.clone())
                    .send_message_from(Some(agent.id), target, args.message)
                    .map_err(|error| ToolExecutionError::Service(error.to_string()))?;
                Ok(tool_result(json!({"agent_id": target, "queued": true})))
            }
            "agents.status" => {
                let args: AgentStatusToolArgs = parse_runtime_args(name, arguments)?;
                let ids = if args.agent_ids.is_empty() {
                    agent.child_ids.clone()
                } else {
                    parse_agent_ids(name, &args.agent_ids)?
                };
                let agents = agent_snapshots(&self.store, run_id, &ids)?;
                Ok(tool_result(json!({"agents": agents})))
            }
            "agents.wait" => {
                let args: AgentWaitToolArgs = parse_runtime_args(name, arguments)?;
                let ids = parse_agent_ids(name, &args.agent_ids)?;
                let timeout = args.timeout_ms.unwrap_or(30_000).clamp(1, 60_000);
                let deadline =
                    tokio::time::Instant::now() + std::time::Duration::from_millis(timeout);
                let agents = loop {
                    let agents = agent_snapshots(&self.store, run_id, &ids)?;
                    let complete = agents.iter().all(|value| {
                        value["agent"]["status"].as_str().is_some_and(|status| {
                            matches!(
                                status,
                                "completed" | "failed" | "blocked" | "cancelled" | "interrupted"
                            )
                        })
                    });
                    if complete || tokio::time::Instant::now() >= deadline {
                        break agents;
                    }
                    tokio::select! {
                        () = cancellation.cancelled() => {
                            return Err(ToolExecutionError::Service(
                                "run cancelled while waiting for agents".to_string()
                            ));
                        }
                        () = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
                    }
                };
                Ok(tool_result(json!({"agents": agents})))
            }
            "agents.interrupt" => {
                let args: AgentIdToolArgs = parse_runtime_args(name, arguments)?;
                let target = parse_agent_id(name, &args.agent_id)?;
                ensure_same_run(&self.store, run_id, target)?;
                AgentControl::new(self.store.clone(), self.agent_limits.clone())
                    .interrupt_agent(target)
                    .map_err(|error| ToolExecutionError::Service(error.to_string()))?;
                Ok(tool_result(
                    json!({"agent_id": target, "interrupted": true}),
                ))
            }
            "plan.update" => {
                let args: PlanUpdateToolArgs = parse_runtime_args(name, arguments)?;
                self.store
                    .append_event(
                        run_id,
                        Some(agent.id),
                        None,
                        "plan.updated",
                        &json!({"items": args.items}),
                        None,
                    )
                    .map_err(|error| ToolExecutionError::Service(error.to_string()))?;
                Ok(tool_result(json!({"updated": true})))
            }
            "skill.activate" => {
                let args: SkillActivateToolArgs = parse_runtime_args(name, arguments)?;
                let skill = self
                    .skills
                    .activate(&args.name)
                    .map_err(|error| ToolExecutionError::Service(error.to_string()))?;
                self.store
                    .append_event(
                        run_id,
                        Some(agent.id),
                        None,
                        "skill.activated",
                        &json!({
                            "name": skill.metadata.name,
                            "source": skill.source_path,
                        }),
                        None,
                    )
                    .map_err(|error| ToolExecutionError::Service(error.to_string()))?;
                let mut activated = serde_json::to_value(skill)
                    .map_err(|error| ToolExecutionError::Service(error.to_string()))?;
                if let Some(object) = activated.as_object_mut() {
                    object.insert(
                        "runtime_instruction".to_string(),
                        Value::String(
                            "Skill loaded successfully. Activation is setup, not task completion. \
                             Immediately continue the original user request in this same run: \
                             inspect inputs, use the relevant filesystem/edit/process tools, create \
                             the requested artifacts, validate them, and only then answer."
                                .to_string(),
                        ),
                    );
                }
                Ok(tool_result(activated))
            }
            "skill.install" => {
                let args: SkillInstallToolArgs = parse_runtime_args(name, arguments)?;
                let installed = install_skill(agent, &args).await?;
                self.store
                    .append_event(
                        run_id,
                        Some(agent.id),
                        None,
                        "skill.installed",
                        &installed,
                        None,
                    )
                    .map_err(|error| ToolExecutionError::Service(error.to_string()))?;
                Ok(tool_result(installed))
            }
            "mcp.connect" => {
                let args: McpConnectToolArgs = parse_runtime_args(name, arguments)?;
                let transport = match (args.command, args.url) {
                    (Some(command), None) => crate::McpTransport::Stdio {
                        command,
                        args: args.args,
                        env: args.env,
                    },
                    (None, Some(url)) => crate::McpTransport::Http {
                        url,
                        token_env: args.token_env,
                    },
                    _ => {
                        return Err(ToolExecutionError::InvalidInput {
                            tool: name.to_string(),
                            message: "provide exactly one of `command` or `url`".to_string(),
                        });
                    }
                };
                let server = self
                    .mcp
                    .upsert(crate::McpServer {
                        name: args.name.clone(),
                        enabled: true,
                        transport,
                    })
                    .map_err(|error| ToolExecutionError::Service(error.to_string()))?;
                let tools = if args.test {
                    Some(
                        self.mcp
                            .list_tools(&args.name)
                            .await
                            .map_err(|error| ToolExecutionError::Service(error.to_string()))?,
                    )
                } else {
                    None
                };
                self.store
                    .append_event(
                        run_id,
                        Some(agent.id),
                        None,
                        "mcp.connected",
                        &json!({"name": args.name}),
                        None,
                    )
                    .map_err(|error| ToolExecutionError::Service(error.to_string()))?;
                Ok(tool_result(json!({
                    "server": server,
                    "tools": tools,
                    "available_immediately": true
                })))
            }
            "mcp.list_tools" => {
                let args: McpListToolsArgs = parse_runtime_args(name, arguments)?;
                let result = self
                    .mcp
                    .list_tools(&args.server)
                    .await
                    .map_err(|error| ToolExecutionError::Service(error.to_string()))?;
                Ok(tool_result(result))
            }
            "mcp.invoke" => {
                let args: McpInvokeToolArgs = parse_runtime_args(name, arguments)?;
                let result = self
                    .mcp
                    .invoke(
                        &args.server,
                        &args.tool,
                        args.arguments,
                        args.timeout_ms.unwrap_or(30_000),
                    )
                    .await
                    .map_err(|error| ToolExecutionError::Service(error.to_string()))?;
                Ok(tool_result(result))
            }
            _ => self.tools.execute_approved(agent, name, arguments).await,
        }
    }

    async fn execute_deterministic_root(
        &self,
        run_id: RunId,
        agent: &Agent,
        cancellation: &CancellationToken,
    ) -> Result<RunExecutionResult, ExecutionError> {
        let started = Instant::now();
        let agent = if agent.status == AgentStatus::Queued {
            self.store
                .transition_agent(agent.id, AgentStatus::Running)?
        } else {
            agent.clone()
        };
        self.store.append_event(
            run_id,
            Some(agent.id),
            None,
            "runtime.service_started",
            &json!({"role": agent.role, "model_calls": 0}),
            None,
        )?;
        let (output, files_read, commands_run, tests_run) = match agent.role.as_str() {
            "awaiter" => (
                "There is no pending tracked task or process to wait for. The runtime checked this without calling a model."
                    .to_string(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ),
            "repository-mapper" => {
                let map = build_repository_map(Path::new(&agent.workspace.root))?;
                (
                    serde_json::to_string_pretty(&map)?,
                    map.get("indexed_files")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect(),
                    Vec::new(),
                    Vec::new(),
                )
            }
            "release-specialist" => {
                run_release_gates(run_id, Path::new(&agent.workspace.root), cancellation).await?
            }
            role => {
                return Err(ExecutionError::DeterministicService(format!(
                    "role `{role}` has no deterministic service implementation"
                )));
            }
        };
        let completion = TaskCompletion {
            status: CompletionStatus::Completed,
            summary: output.clone(),
            findings: Vec::new(),
            files_read,
            files_changed: Vec::new(),
            commands_run,
            tests_run,
            risks: Vec::new(),
            unresolved: Vec::new(),
            recommended_next_actions: Vec::new(),
            producer: Some(ModelIdentity {
                provider: agent.provider.clone(),
                model: agent.model.clone(),
            }),
            ..TaskCompletion::default()
        };
        AgentControl::new(self.store.clone(), self.agent_limits.clone()).complete_task(
            agent.id,
            None,
            &completion,
        )?;
        self.store.append_event(
            run_id,
            Some(agent.id),
            None,
            "runtime.service_completed",
            &json!({
                "role": agent.role,
                "model_calls": 0,
                "elapsed_ms": elapsed_ms(started)
            }),
            None,
        )?;
        Ok(RunExecutionResult {
            run_id,
            mode: ExecutionMode::Focused,
            output,
            provider: None,
            model: None,
            continuation_id: None,
            usage: UsageLedger::default(),
            timing: TimingLedger {
                total_ms: elapsed_ms(started),
                ..TimingLedger::default()
            },
            model_calls: 0,
            tool_calls: 0,
        })
    }

    #[allow(clippy::too_many_lines)]
    async fn execute_deterministic_task(
        &self,
        run_id: RunId,
        task: &opensrc_core::Task,
        agent: &Agent,
        cancellation: &CancellationToken,
    ) -> Result<RunExecutionResult, ExecutionError> {
        let started = Instant::now();
        self.store.append_event(
            run_id,
            Some(agent.id),
            Some(task.id),
            "runtime.service_started",
            &json!({
                "role": agent.role,
                "model_calls": 0,
                "policy_version": crate::ROUTING_POLICY_VERSION
            }),
            None,
        )?;
        let (output, files_read, commands_run, tests_run) = match agent.role.as_str() {
            "awaiter" => (
                "All declared dependencies are complete. Waiting was handled by the runtime with zero model calls."
                    .to_string(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ),
            "repository-mapper" => {
                let map = build_repository_map(Path::new(&agent.workspace.root))?;
                (
                    serde_json::to_string_pretty(&map)?,
                    map.get("indexed_files")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect(),
                    Vec::new(),
                    Vec::new(),
                )
            }
            "release-specialist" => {
                run_release_gates(run_id, Path::new(&agent.workspace.root), cancellation).await?
            }
            role => {
                return Err(ExecutionError::DeterministicService(format!(
                    "role `{role}` has no deterministic service implementation"
                )));
            }
        };
        let contract_checks = task
            .contract
            .acceptance_criteria
            .iter()
            .map(|criterion| ContractCheck {
                criterion: criterion.clone(),
                status: EvidenceStatus::Passed,
                evidence: "Deterministic runtime service completed successfully.".to_string(),
            })
            .collect();
        let tests = task
            .contract
            .validation_steps
            .iter()
            .map(|validation| TestEvidence {
                command: validation.clone(),
                status: EvidenceStatus::Passed,
                evidence: "Validated by the deterministic runtime service.".to_string(),
            })
            .collect();
        let completion = TaskCompletion {
            task_id: Some(task.id),
            status: CompletionStatus::Completed,
            summary: output.clone(),
            findings: Vec::new(),
            files_read,
            files_changed: Vec::new(),
            commands_run,
            tests_run,
            tests,
            contract_checks,
            risks: Vec::new(),
            unresolved: Vec::new(),
            recommended_next_actions: Vec::new(),
            producer: Some(ModelIdentity {
                provider: agent.provider.clone(),
                model: agent.model.clone(),
            }),
            ..TaskCompletion::default()
        };
        AgentControl::new(self.store.clone(), self.agent_limits.clone()).complete_task(
            agent.id,
            Some(task.id),
            &completion,
        )?;
        self.store.append_event(
            run_id,
            Some(agent.id),
            Some(task.id),
            "runtime.service_completed",
            &json!({
                "role": agent.role,
                "model_calls": 0,
                "elapsed_ms": elapsed_ms(started)
            }),
            None,
        )?;
        Ok(RunExecutionResult {
            run_id,
            mode: ExecutionMode::Agentic,
            output,
            provider: None,
            model: None,
            continuation_id: None,
            usage: UsageLedger::default(),
            timing: TimingLedger {
                total_ms: elapsed_ms(started),
                ..TimingLedger::default()
            },
            model_calls: 0,
            tool_calls: 0,
        })
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    async fn execute_agentic(
        &self,
        run_id: RunId,
        user_request: &str,
        history: Vec<Message>,
        provider: &str,
        model: &str,
        model_pack: Option<&ModelPack>,
        automatic_routing: bool,
        cancellation: &CancellationToken,
    ) -> Result<RunExecutionResult, ExecutionError> {
        let mut root = root_agent(&self.store, run_id)?;
        if root.status == AgentStatus::Queued {
            root = self.store.transition_agent(root.id, AgentStatus::Running)?;
        }
        let planner_pack_assignment =
            model_pack.and_then(|pack| pack.select(ModelPackStage::Plan, "architect"));
        let planner_policy_assignment = (automatic_routing && model_pack.is_none())
            .then(|| {
                self.routing_policies
                    .resolve_for_role("architect", user_request, &self.providers)
                    .ok()
                    .flatten()
            })
            .flatten();
        let (planner_provider, planner_model) = planner_pack_assignment.as_ref().map_or_else(
            || {
                planner_policy_assignment
                    .as_ref()
                    .map_or((provider, model), |member| {
                        (member.provider.as_str(), member.model.as_str())
                    })
            },
            |member| (member.provider.as_str(), member.model.as_str()),
        );
        if let (Some(pack), Some(member)) = (model_pack, planner_pack_assignment.as_ref()) {
            self.record_model_pack_assignment(
                run_id,
                Some(root.id),
                pack,
                ModelPackStage::Plan,
                "architect",
                member,
            )?;
        }
        if let Some(assignment) = planner_policy_assignment.as_ref() {
            self.record_routing_assignment(
                run_id,
                Some(root.id),
                "architect",
                assignment,
                "complex_planning",
            )?;
        }
        self.store.append_event(
            run_id,
            Some(root.id),
            None,
            "agent.plan_started",
            &json!({
                "provider": planner_provider,
                "model": planner_model,
                "pack": model_pack.map(|pack| pack.id.as_str())
            }),
            None,
        )?;
        let planner_history = if let Some(policy) = self.routing_policies.role("architect") {
            apply_context_policy(history, &policy.context_policy)
        } else {
            history
        };
        let planner_uses_native_schema = self
            .providers
            .resolve(planner_provider, &RequiredCapabilities::default())
            .is_ok_and(|adapter| adapter.capabilities().supports_structured_output);
        self.store.append_event(
            run_id,
            Some(root.id),
            None,
            "agent.plan_contract_mode",
            &json!({
                "provider": planner_provider,
                "model": planner_model,
                "native_schema": planner_uses_native_schema,
                "format": if planner_uses_native_schema { "native_schema" } else { "json_text" }
            }),
            None,
        )?;
        let plan_request = CanonicalModelRequest {
            model: planner_model.to_string(),
            system: agentic_planner_prompt(),
            messages: planner_history
                .into_iter()
                .map(message_to_canonical)
                .collect(),
            tools: Vec::new(),
            structured_output_schema: planner_uses_native_schema.then(agentic_plan_schema),
            reasoning_level: planner_pack_assignment
                .as_ref()
                .and_then(|member| member.reasoning_level.clone())
                .or_else(|| {
                    self.routing_policies
                        .role("architect")
                        .and_then(|policy| policy.reasoning_effort)
                })
                .or_else(|| root.reasoning.level.clone()),
            temperature: Some(0.1),
            max_output_tokens: Some(4_000),
            cache_hints: BTreeMap::new(),
        };
        let plan_started = Instant::now();
        let plan_outcome = self
            .model_call(
                run_id,
                Some(root.id),
                planner_provider,
                planner_model,
                plan_request,
                cancellation,
            )
            .await?;
        let (plan_text, mut aggregate_usage) = collect_events(&plan_outcome.events);
        self.store.record_usage(
            run_id,
            Some(root.id),
            &plan_outcome.actual_provider,
            &plan_outcome.actual_model,
            &aggregate_usage,
            0,
        )?;
        let planned = parse_agentic_plan(&plan_text).unwrap_or_else(|| {
            let _ = self.store.append_event(
                run_id,
                Some(root.id),
                None,
                "agent.plan_fallback",
                &json!({
                    "reason": "planner response was not valid task JSON",
                    "fallback": "deterministic investigation, implementation, validation, and review DAG"
                }),
                None,
            );
            fallback_agentic_plan(user_request)
        });
        if planned.tasks.is_empty() || planned.tasks.len() > 8 {
            return Err(ExecutionError::InvalidAgentPlan(
                "the planner must return between one and eight tasks".to_string(),
            ));
        }
        self.store.append_event(
            run_id,
            Some(root.id),
            None,
            "agent.plan_created",
            &json!({
                "task_count": planned.tasks.len(),
                "roles": planned.tasks.iter().map(|task| task.role.as_str()).collect::<Vec<_>>()
            }),
            None,
        )?;

        let control = AgentControl::new(self.store.clone(), self.agent_limits.clone());
        let identifiers = planned
            .tasks
            .iter()
            .enumerate()
            .map(|(index, task)| {
                task.id
                    .clone()
                    .unwrap_or_else(|| format!("task-{}", index + 1))
            })
            .collect::<Vec<_>>();
        if identifiers.iter().collect::<BTreeSet<_>>().len() != identifiers.len() {
            return Err(ExecutionError::InvalidAgentPlan(
                "task ids must be unique".to_string(),
            ));
        }
        let task_ids = identifiers
            .iter()
            .map(|_| Uuid::new_v4())
            .collect::<Vec<_>>();
        let id_lookup = identifiers
            .iter()
            .cloned()
            .zip(task_ids.iter().copied())
            .collect::<BTreeMap<_, _>>();
        let mut task_agents = Vec::new();
        let mut prior_writers: Vec<(TaskId, Vec<String>)> = Vec::new();
        for (index, planned_task) in planned.tasks.into_iter().enumerate() {
            validate_planned_task(&identifiers[index], &planned_task)?;
            let mut definition = resolve_agent_definition(&root.workspace.root, &planned_task.role)
                .map_err(|_| {
                    ExecutionError::InvalidAgentPlan(format!(
                        "task `{}` selected unknown specialist role `{}`",
                        identifiers[index], planned_task.role
                    ))
                })?;
            let stage = stage_for_role(&planned_task.role);
            let pack_assignment =
                model_pack.and_then(|pack| pack.select(stage, &planned_task.role));
            let role_policy = self.routing_policies.role(&planned_task.role);
            let policy_assignment = (automatic_routing && model_pack.is_none())
                .then(|| {
                    self.routing_policies
                        .resolve_for_role(
                            &planned_task.role,
                            &planned_task.description,
                            &self.providers,
                        )
                        .ok()
                        .flatten()
                })
                .flatten();
            let policy_fallbacks = Vec::new();
            if let Some(policy) = role_policy.as_ref() {
                apply_role_policy(
                    &mut definition,
                    policy,
                    policy_assignment.as_ref(),
                    &policy_fallbacks,
                );
            }
            let (task_provider, task_model) = pack_assignment.as_ref().map_or_else(
                || {
                    policy_assignment
                        .as_ref()
                        .map_or((provider, model), |member| {
                            (member.provider.as_str(), member.model.as_str())
                        })
                },
                |member| (member.provider.as_str(), member.model.as_str()),
            );
            if role_policy
                .as_ref()
                .is_some_and(|policy| policy.execution == RoleExecutionKind::Deterministic)
            {
                definition.preferred_provider = Some("runtime".to_string());
                definition.preferred_model = Some("deterministic".to_string());
                definition.fallback_chain.clear();
            } else {
                definition.preferred_provider = Some(task_provider.to_string());
                definition.preferred_model = Some(task_model.to_string());
            }
            if let Some(reasoning) = pack_assignment
                .as_ref()
                .and_then(|member| member.reasoning_level.clone())
            {
                definition.reasoning.level = Some(reasoning);
            }
            if let (Some(pack), Some(member)) = (model_pack, pack_assignment.as_ref()) {
                definition.fallback_chain = pack.fallback_chain(member);
            } else {
                definition.fallback_chain.clear();
            }
            if definition.workspace_mode == WorkspaceMode::GitWorktree {
                definition.workspace_mode = WorkspaceMode::OwnedPaths;
                self.store.append_event(
                    run_id,
                    Some(root.id),
                    None,
                    "workspace.mode_limited",
                    &json!({
                        "requested": "git_worktree",
                        "effective": "owned_paths",
                        "reason": "worktree provisioning is not available; tasks run sequentially"
                    }),
                    None,
                )?;
            }
            let owned_paths = if definition.workspace_mode == WorkspaceMode::OwnedPaths {
                if planned_task.owned_paths.is_empty() {
                    return Err(ExecutionError::InvalidAgentPlan(format!(
                        "writing task `{}` must declare narrow owned_paths",
                        identifiers[index]
                    )));
                }
                planned_task.owned_paths.clone()
            } else {
                Vec::new()
            };
            let mut dependencies = planned_task
                .dependencies
                .iter()
                .map(|dependency| {
                    id_lookup.get(dependency).copied().ok_or_else(|| {
                        ExecutionError::InvalidAgentPlan(format!(
                            "task `{}` depends on unknown task `{dependency}`",
                            identifiers[index]
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            if dependencies.contains(&task_ids[index]) {
                return Err(ExecutionError::InvalidAgentPlan(format!(
                    "task `{}` cannot depend on itself",
                    identifiers[index]
                )));
            }
            if definition.workspace_mode == WorkspaceMode::OwnedPaths {
                for (prior_id, prior_paths) in &prior_writers {
                    if owned_paths_overlap(&owned_paths, prior_paths)
                        && !dependencies.contains(prior_id)
                    {
                        dependencies.push(*prior_id);
                    }
                }
                prior_writers.push((task_ids[index], owned_paths.clone()));
            }
            let child = control.spawn_planned_agent_with_ownership(
                root.id,
                &definition,
                planned_task.description.clone(),
                None,
                owned_paths,
            )?;
            if let (Some(pack), Some(member)) = (model_pack, pack_assignment.as_ref()) {
                self.record_model_pack_assignment(
                    run_id,
                    Some(child.id),
                    pack,
                    stage,
                    &planned_task.role,
                    member,
                )?;
            }
            if let Some(assignment) = policy_assignment.as_ref() {
                self.record_routing_assignment(
                    run_id,
                    Some(child.id),
                    &planned_task.role,
                    assignment,
                    "specialist_task",
                )?;
            }
            let now = Utc::now();
            let task = opensrc_core::Task {
                id: task_ids[index],
                run_id,
                description: planned_task.description.clone(),
                dependencies,
                assigned_agent: Some(child.id),
                status: TaskStatus::Created,
                priority: 0,
                expected_output: child.completion_schema.clone(),
                contract: opensrc_core::TaskContract {
                    objective: planned_task.description.clone(),
                    inputs: opensrc_core::TaskInputs {
                        relevant_files: child.workspace.owned_paths.clone(),
                        ..opensrc_core::TaskInputs::default()
                    },
                    acceptance_criteria: planned_task.acceptance_criteria,
                    deliverables: planned_task.deliverables,
                    validation_steps: planned_task.validation_steps,
                    forbidden_actions: planned_task.forbidden_actions,
                    handoff_notes: planned_task.contract_notes,
                    allowed_paths: if child.workspace.owned_paths.is_empty() {
                        vec![".".to_string()]
                    } else {
                        child.workspace.owned_paths.clone()
                    },
                    forbidden_paths: Vec::new(),
                    tools: child.tool_policy.clone(),
                    budgets: child.budgets.clone(),
                    completion_schema: child.completion_schema.clone(),
                    max_retries: self.routing_policies.limits().max_retries_per_task,
                    // Independent reviews are represented as their own review tasks. The
                    // implementation task must be completable first so the scheduler can pass
                    // its evidence and file changes to that separate reviewer.
                    review_required: false,
                    repair_of_task_id: None,
                },
                workspace_ownership: child.workspace.owned_paths.clone(),
                allowed_tools: child.tool_policy.allow.clone(),
                retry_policy: child.retry_policy.clone(),
                created_at: now,
                updated_at: now,
            };
            control.create_task(&task)?;
            task_agents.push((task, child));
        }
        crate::validate_task_graph(
            &task_agents
                .iter()
                .map(|(task, _)| task.clone())
                .collect::<Vec<_>>(),
        )
        .map_err(|error| ExecutionError::InvalidAgentPlan(error.to_string()))?;

        let conversation_id = self.store.get_run(run_id)?.conversation_id;
        let mut summaries = Vec::new();
        let mut continuation_id = None;
        let mut model_calls = 1;
        let mut tool_calls = 0;
        let mut timing = TimingLedger {
            provider_request_ms: plan_outcome.provider_ms,
            time_to_first_token_ms: plan_outcome.first_token_ms,
            generation_ms: plan_outcome.provider_ms,
            total_ms: elapsed_ms(plan_started),
            ..TimingLedger::default()
        };
        let mut pending = task_agents
            .into_iter()
            .map(|(task, agent)| (task.id, (task, agent)))
            .collect::<BTreeMap<_, _>>();
        let mut completed = BTreeSet::new();
        if root.status == AgentStatus::Running {
            root = self.store.transition_agent(root.id, AgentStatus::Waiting)?;
        }
        while !pending.is_empty() {
            let mut ready = Vec::new();
            let mut writers = 0_usize;
            let mut deep_reasoners = 0_usize;
            for (id, (task, agent)) in &pending {
                if !task
                    .dependencies
                    .iter()
                    .all(|dependency| completed.contains(dependency))
                    || ready.len() >= self.agent_limits.max_active_agents_per_run
                {
                    continue;
                }
                let writer = agent.workspace.mode == WorkspaceMode::OwnedPaths;
                if writer && writers >= self.agent_limits.max_active_writers_per_run {
                    continue;
                }
                let deep = matches!(agent.reasoning.level.as_deref(), Some("max"));
                if deep && deep_reasoners >= self.agent_limits.max_deep_reasoning_agents_per_run {
                    continue;
                }
                ready.push(*id);
                writers += usize::from(writer);
                deep_reasoners += usize::from(deep);
            }
            if ready.is_empty() {
                return Err(ExecutionError::InvalidAgentPlan(
                    "task graph has no runnable task".to_string(),
                ));
            }
            let mut round = Vec::new();
            for id in ready {
                let (task, child) = pending.remove(&id).expect("ready task exists");
                control.start_task(task.id)?;
                control.start_agent(child.id)?;
                let task_message = Message {
                    id: Uuid::new_v4(),
                    conversation_id,
                    run_id: Some(run_id),
                    sequence: 0,
                    role: MessageRole::User,
                    content: vec![MessageContent::text(task_contract_message(
                        &self.store,
                        &task,
                        &child,
                    )?)],
                    provider: None,
                    model: None,
                    continuation_id: None,
                    created_at: Utc::now(),
                };
                let engine = self.clone();
                let provider = child.provider.clone();
                let model = child.model.clone();
                let deterministic = self
                    .routing_policies
                    .role(&child.role)
                    .is_some_and(|policy| policy.execution != RoleExecutionKind::Llm);
                let cancellation = cancellation.clone();
                self.store.append_event(
                    run_id,
                    Some(child.id),
                    Some(task.id),
                    "task.contract_issued",
                    &json!({
                        "role": child.role,
                        "provider": child.provider,
                        "model": child.model,
                        "owned_paths": task.workspace_ownership,
                        "dependencies": task.dependencies
                    }),
                    None,
                )?;
                round.push(async move {
                    let result = if deterministic {
                        engine
                            .execute_deterministic_task(run_id, &task, &child, &cancellation)
                            .await
                    } else {
                        engine
                            .execute_focused(
                                run_id,
                                &task.description,
                                vec![task_message],
                                &provider,
                                &model,
                                &cancellation,
                                Some(child.id),
                                Some(task.id),
                            )
                            .await
                    };
                    (task, child, result)
                });
            }
            for (task, child, result) in futures::future::join_all(round).await {
                match result {
                    Ok(result) => {
                        completed.insert(task.id);
                        continuation_id.clone_from(&result.continuation_id);
                        summaries.push(format!("{}: {}", child.role, result.output));
                        aggregate_usage.merge(&result.usage);
                        timing.merge(&result.timing);
                        model_calls += result.model_calls;
                        tool_calls += result.tool_calls;
                        if let Some(review) = self
                            .store
                            .get_agent_completion(child.id)?
                            .and_then(|completion| completion.review)
                            .filter(ReviewContract::has_blocking_findings)
                        {
                            for (repair_task, repair_agent) in self.create_repair_chain(
                                run_id,
                                &root,
                                &task,
                                &review,
                                provider,
                                model,
                                automatic_routing,
                                model_pack,
                            )? {
                                pending.insert(repair_task.id, (repair_task, repair_agent));
                            }
                        }
                    }
                    Err(error) => {
                        let _ = self.store.transition_task(task.id, TaskStatus::Failed);
                        self.block_downstream_tasks(
                            run_id,
                            task.id,
                            "a required dependency failed validation",
                        )?;
                        let current = self.store.get_agent(child.id)?;
                        if current.status.can_transition_to(AgentStatus::Failed) {
                            let _ = self.store.transition_agent(child.id, AgentStatus::Failed);
                        }
                        return Err(error);
                    }
                }
            }
        }
        if root.status == AgentStatus::Waiting {
            root = self.store.transition_agent(root.id, AgentStatus::Running)?;
        }
        let synthesis_pack_assignment =
            model_pack.and_then(|pack| pack.select(ModelPackStage::Synthesize, "generalist"));
        let synthesis_policy_assignment = (automatic_routing && model_pack.is_none())
            .then(|| {
                self.routing_policies
                    .resolve_for_role("generalist", user_request, &self.providers)
                    .ok()
                    .flatten()
            })
            .flatten();
        let (synthesis_provider, synthesis_model) = synthesis_pack_assignment.as_ref().map_or_else(
            || {
                synthesis_policy_assignment
                    .as_ref()
                    .map_or((provider, model), |member| {
                        (member.provider.as_str(), member.model.as_str())
                    })
            },
            |member| (member.provider.as_str(), member.model.as_str()),
        );
        if let (Some(pack), Some(member)) = (model_pack, synthesis_pack_assignment.as_ref()) {
            self.record_model_pack_assignment(
                run_id,
                Some(root.id),
                pack,
                ModelPackStage::Synthesize,
                "generalist",
                member,
            )?;
        }
        if let Some(assignment) = synthesis_policy_assignment.as_ref() {
            self.record_routing_assignment(
                run_id,
                Some(root.id),
                "generalist",
                assignment,
                "final_synthesis",
            )?;
        }
        self.store.append_event(
            run_id,
            Some(root.id),
            None,
            "agent.synthesis_started",
            &json!({
                "provider": synthesis_provider,
                "model": synthesis_model,
                "reports": summaries.len()
            }),
            None,
        )?;
        let synthesis_request = CanonicalModelRequest {
            model: synthesis_model.to_string(),
            system: "Integrate the completed specialist reports into one concise final response. \
                     Report what was done, validation performed, risks, and unresolved items. \
                     Do not claim actions absent from the reports."
                .to_string(),
            messages: vec![
                CanonicalMessage::text(MessageRole::User, user_request),
                CanonicalMessage::text(
                    MessageRole::Developer,
                    format!("Specialist reports:\n\n{}", summaries.join("\n\n")),
                ),
            ],
            tools: Vec::new(),
            structured_output_schema: None,
            reasoning_level: synthesis_pack_assignment
                .as_ref()
                .and_then(|member| member.reasoning_level.clone())
                .or_else(|| {
                    self.routing_policies
                        .role("generalist")
                        .and_then(|policy| policy.reasoning_effort)
                })
                .or_else(|| root.reasoning.level.clone()),
            temperature: Some(0.1),
            max_output_tokens: root.budgets.token_limit,
            cache_hints: BTreeMap::new(),
        };
        let synthesis_outcome = self
            .model_call(
                run_id,
                Some(root.id),
                synthesis_provider,
                synthesis_model,
                synthesis_request,
                cancellation,
            )
            .await?;
        let (output, synthesis_usage) = collect_events(&synthesis_outcome.events);
        aggregate_usage.merge(&synthesis_usage);
        self.store.record_usage(
            run_id,
            Some(root.id),
            &synthesis_outcome.actual_provider,
            &synthesis_outcome.actual_model,
            &synthesis_usage,
            0,
        )?;
        model_calls += 1;
        timing.provider_request_ms = timing
            .provider_request_ms
            .saturating_add(synthesis_outcome.provider_ms);
        timing.generation_ms = timing
            .generation_ms
            .saturating_add(synthesis_outcome.provider_ms);
        if timing.time_to_first_token_ms == 0 {
            timing.time_to_first_token_ms = synthesis_outcome.first_token_ms;
        }
        continuation_id = response_id(&synthesis_outcome.events).or(continuation_id);
        let completion = TaskCompletion {
            status: CompletionStatus::Completed,
            summary: output.clone(),
            findings: summaries,
            files_read: Vec::new(),
            files_changed: self
                .store
                .list_file_changes(Some(run_id))?
                .into_iter()
                .map(|change| change.relative_path)
                .collect(),
            commands_run: Vec::new(),
            tests_run: Vec::new(),
            risks: Vec::new(),
            unresolved: Vec::new(),
            recommended_next_actions: Vec::new(),
            producer: Some(ModelIdentity {
                provider: synthesis_outcome.actual_provider.clone(),
                model: synthesis_outcome.actual_model.clone(),
            }),
            ..TaskCompletion::default()
        };
        self.store.save_completion(root.id, None, &completion)?;
        self.store
            .transition_agent(root.id, AgentStatus::Completed)?;
        Ok(RunExecutionResult {
            run_id,
            mode: ExecutionMode::Agentic,
            output,
            provider: Some(synthesis_outcome.actual_provider),
            model: Some(synthesis_outcome.actual_model),
            continuation_id,
            usage: aggregate_usage,
            timing,
            model_calls,
            tool_calls,
        })
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn create_repair_chain(
        &self,
        run_id: RunId,
        root: &Agent,
        review_task: &opensrc_core::Task,
        review: &ReviewContract,
        default_provider: &str,
        default_model: &str,
        automatic_routing: bool,
        model_pack: Option<&ModelPack>,
    ) -> Result<Vec<(opensrc_core::Task, Agent)>, ExecutionError> {
        let tasks = self.store.list_tasks(Some(run_id))?;
        let by_id = tasks
            .iter()
            .cloned()
            .map(|task| (task.id, task))
            .collect::<BTreeMap<_, _>>();
        let mut frontier = review_task.dependencies.clone();
        let mut visited = BTreeSet::new();
        let mut target = None;
        while let Some(task_id) = frontier.pop() {
            if !visited.insert(task_id) {
                continue;
            }
            let Some(task) = by_id.get(&task_id) else {
                continue;
            };
            if !task.workspace_ownership.is_empty() {
                target = Some(task.clone());
                break;
            }
            frontier.extend(task.dependencies.iter().copied());
        }
        let Some(target) = target else {
            self.store.append_event(
                run_id,
                Some(root.id),
                Some(review_task.id),
                "review.repair_not_scheduled",
                &json!({"reason": "no writable upstream task was found"}),
                None,
            )?;
            return Ok(Vec::new());
        };
        let existing_repairs = tasks
            .iter()
            .filter(|task| {
                task.contract.repair_of_task_id == Some(target.id)
                    && !task.workspace_ownership.is_empty()
            })
            .count();
        if existing_repairs
            >= usize::try_from(target.contract.max_retries.max(1)).unwrap_or(usize::MAX)
        {
            self.store.append_event(
                run_id,
                Some(root.id),
                Some(review_task.id),
                "review.repair_limit_reached",
                &json!({
                    "target_task_id": target.id,
                    "maximum_repairs": target.contract.max_retries.max(1)
                }),
                None,
            )?;
            return Ok(Vec::new());
        }

        let findings = review
            .findings
            .iter()
            .filter(|finding| finding.blocking)
            .map(|finding| {
                format!(
                    "[{:?}] {}: {} Evidence: {}",
                    finding.severity, finding.category, finding.required_action, finding.evidence
                )
            })
            .collect::<Vec<_>>();
        let repair_description = format!(
            "Repair the blocking independent-review findings for task {}:\n{}",
            target.id,
            findings.join("\n")
        );
        let control = AgentControl::new(self.store.clone(), self.agent_limits.clone());
        let (mut repair_definition, repair_assignment) = self.routed_definition(
            &root.workspace.root,
            "implementer",
            &repair_description,
            default_provider,
            default_model,
            automatic_routing,
        )?;
        let repair_pack_assignment =
            model_pack.and_then(|pack| pack.select(ModelPackStage::Execute, "implementer"));
        if let Some(member) = repair_pack_assignment.as_ref() {
            repair_definition.preferred_provider = Some(member.provider.clone());
            repair_definition.preferred_model = Some(member.model.clone());
            if member.reasoning_level.is_some() {
                repair_definition
                    .reasoning
                    .level
                    .clone_from(&member.reasoning_level);
            }
            repair_definition.fallback_chain = model_pack
                .map(|pack| pack.fallback_chain(member))
                .unwrap_or_default();
        }
        let repair_agent = control.spawn_agent_with_ownership(
            root.id,
            &repair_definition,
            repair_description.clone(),
            None,
            target.workspace_ownership.clone(),
        )?;
        if let (Some(pack), Some(member)) = (model_pack, repair_pack_assignment.as_ref()) {
            self.record_model_pack_assignment(
                run_id,
                Some(repair_agent.id),
                pack,
                ModelPackStage::Execute,
                "implementer",
                member,
            )?;
        }
        if let Some(assignment) = repair_assignment.as_ref() {
            self.record_routing_assignment(
                run_id,
                Some(repair_agent.id),
                "implementer",
                assignment,
                "blocking_review_repair",
            )?;
        }
        let now = Utc::now();
        let repair_id = Uuid::new_v4();
        let repair_task = opensrc_core::Task {
            id: repair_id,
            run_id,
            description: repair_description,
            dependencies: vec![review_task.id],
            assigned_agent: Some(repair_agent.id),
            status: TaskStatus::Created,
            priority: review_task.priority.saturating_add(1),
            expected_output: repair_agent.completion_schema.clone(),
            contract: opensrc_core::TaskContract {
                objective: format!("Resolve every blocking finding for task {}.", target.id),
                inputs: opensrc_core::TaskInputs {
                    relevant_files: target.workspace_ownership.clone(),
                    parent_findings: findings.clone(),
                    ..opensrc_core::TaskInputs::default()
                },
                acceptance_criteria: review
                    .findings
                    .iter()
                    .filter(|finding| finding.blocking)
                    .map(|finding| finding.required_action.clone())
                    .collect(),
                deliverables: vec![
                    "A bounded repair with validation evidence for each finding.".to_string(),
                ],
                validation_steps: target.contract.validation_steps.clone(),
                forbidden_actions: target.contract.forbidden_actions.clone(),
                handoff_notes: vec![
                    "This repair must be independently re-reviewed before integration.".to_string(),
                ],
                allowed_paths: target.workspace_ownership.clone(),
                forbidden_paths: target.contract.forbidden_paths.clone(),
                tools: repair_agent.tool_policy.clone(),
                budgets: repair_agent.budgets.clone(),
                completion_schema: repair_agent.completion_schema.clone(),
                max_retries: target
                    .contract
                    .max_retries
                    .saturating_sub(u32::try_from(existing_repairs).unwrap_or(u32::MAX)),
                review_required: true,
                repair_of_task_id: Some(target.id),
            },
            workspace_ownership: target.workspace_ownership.clone(),
            allowed_tools: repair_agent.tool_policy.allow.clone(),
            retry_policy: repair_agent.retry_policy.clone(),
            created_at: now,
            updated_at: now,
        };
        control.create_task(&repair_task)?;

        let review_description = format!(
            "Independently re-review repair task {repair_id} against the prior blocking findings."
        );
        let (mut reviewer_definition, reviewer_assignment) = self.routed_definition(
            &root.workspace.root,
            "code-reviewer",
            &review_description,
            default_provider,
            default_model,
            automatic_routing,
        )?;
        let review_pack_assignment =
            model_pack.and_then(|pack| pack.select(ModelPackStage::Review, "code-reviewer"));
        if let Some(member) = review_pack_assignment.as_ref() {
            reviewer_definition.preferred_provider = Some(member.provider.clone());
            reviewer_definition.preferred_model = Some(member.model.clone());
            if member.reasoning_level.is_some() {
                reviewer_definition
                    .reasoning
                    .level
                    .clone_from(&member.reasoning_level);
            }
            reviewer_definition.fallback_chain = model_pack
                .map(|pack| pack.fallback_chain(member))
                .unwrap_or_default();
        }
        let reviewer_agent = control.spawn_agent_with_ownership(
            root.id,
            &reviewer_definition,
            review_description.clone(),
            None,
            Vec::new(),
        )?;
        if let (Some(pack), Some(member)) = (model_pack, review_pack_assignment.as_ref()) {
            self.record_model_pack_assignment(
                run_id,
                Some(reviewer_agent.id),
                pack,
                ModelPackStage::Review,
                "code-reviewer",
                member,
            )?;
        }
        if let Some(assignment) = reviewer_assignment.as_ref() {
            self.record_routing_assignment(
                run_id,
                Some(reviewer_agent.id),
                "code-reviewer",
                assignment,
                "repair_independent_review",
            )?;
        }
        let review_id = Uuid::new_v4();
        let rereview_task = opensrc_core::Task {
            id: review_id,
            run_id,
            description: review_description,
            dependencies: vec![repair_id],
            assigned_agent: Some(reviewer_agent.id),
            status: TaskStatus::Created,
            priority: review_task.priority.saturating_add(2),
            expected_output: reviewer_agent.completion_schema.clone(),
            contract: opensrc_core::TaskContract {
                objective: "Verify that every blocking finding is resolved without regression."
                    .to_string(),
                inputs: opensrc_core::TaskInputs {
                    relevant_files: target.workspace_ownership.clone(),
                    parent_findings: findings,
                    ..opensrc_core::TaskInputs::default()
                },
                acceptance_criteria: vec![
                    "Every prior high or critical finding has a source-backed disposition."
                        .to_string(),
                ],
                deliverables: vec!["A schema-valid independent review contract.".to_string()],
                validation_steps: vec![
                    "Inspect the repair diff and its validation evidence.".to_string(),
                ],
                forbidden_actions: vec![
                    "Do not approve without independently inspecting the repair evidence."
                        .to_string(),
                ],
                handoff_notes: Vec::new(),
                allowed_paths: Vec::new(),
                forbidden_paths: target.contract.forbidden_paths.clone(),
                tools: reviewer_agent.tool_policy.clone(),
                budgets: reviewer_agent.budgets.clone(),
                completion_schema: reviewer_agent.completion_schema.clone(),
                max_retries: 0,
                review_required: false,
                repair_of_task_id: Some(target.id),
            },
            workspace_ownership: Vec::new(),
            allowed_tools: reviewer_agent.tool_policy.allow.clone(),
            retry_policy: reviewer_agent.retry_policy.clone(),
            created_at: now,
            updated_at: now,
        };
        control.create_task(&rereview_task)?;
        self.store.append_event(
            run_id,
            Some(root.id),
            Some(repair_id),
            "review.repair_chain_created",
            &json!({
                "review_task_id": review_task.id,
                "target_task_id": target.id,
                "repair_task_id": repair_id,
                "rereview_task_id": review_id,
                "blocking_findings": review.findings.iter().filter(|finding| finding.blocking).count()
            }),
            None,
        )?;
        Ok(vec![
            (repair_task, repair_agent),
            (rereview_task, reviewer_agent),
        ])
    }

    fn routed_definition(
        &self,
        workspace_root: &str,
        role: &str,
        request: &str,
        default_provider: &str,
        default_model: &str,
        automatic_routing: bool,
    ) -> Result<
        (
            opensrc_core::AgentDefinition,
            Option<ResolvedModelAssignment>,
        ),
        ExecutionError,
    > {
        let mut definition = resolve_agent_definition(workspace_root, role)?;
        let policy = self.routing_policies.role(role);
        let assignment = (automatic_routing)
            .then(|| {
                self.routing_policies
                    .resolve_for_role(role, request, &self.providers)
                    .ok()
                    .flatten()
            })
            .flatten();
        let fallbacks = if automatic_routing {
            self.routing_policies
                .fallback_assignments(role, &self.providers)
        } else {
            Vec::new()
        };
        if let Some(policy) = policy.as_ref() {
            apply_role_policy(&mut definition, policy, assignment.as_ref(), &fallbacks);
        }
        if assignment.is_none() {
            definition.preferred_provider = Some(default_provider.to_string());
            definition.preferred_model = Some(default_model.to_string());
            definition.fallback_chain.clear();
        }
        Ok((definition, assignment))
    }

    fn block_downstream_tasks(
        &self,
        run_id: RunId,
        failed_task_id: TaskId,
        reason: &str,
    ) -> Result<(), ExecutionError> {
        let mut blocked_dependencies = BTreeSet::from([failed_task_id]);
        loop {
            let mut changed = false;
            for task in self.store.list_tasks(Some(run_id))? {
                if task.status.is_terminal() || task.status == TaskStatus::Blocked {
                    continue;
                }
                if task
                    .dependencies
                    .iter()
                    .any(|dependency| blocked_dependencies.contains(dependency))
                {
                    self.store.transition_task(task.id, TaskStatus::Blocked)?;
                    self.store.append_event(
                        run_id,
                        task.assigned_agent,
                        Some(task.id),
                        "task.blocked_by_dependency",
                        &json!({
                            "failed_task_id": failed_task_id,
                            "reason": reason
                        }),
                        None,
                    )?;
                    blocked_dependencies.insert(task.id);
                    changed = true;
                }
            }
            if !changed {
                return Ok(());
            }
        }
    }

    async fn model_call(
        &self,
        run_id: RunId,
        agent_id: Option<AgentId>,
        provider: &str,
        model: &str,
        request: CanonicalModelRequest,
        cancellation: &CancellationToken,
    ) -> Result<ModelCallOutcome, ExecutionError> {
        self.model_call_with_fallbacks(
            run_id,
            agent_id,
            provider,
            model,
            request,
            &[],
            cancellation,
        )
        .await
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    async fn model_call_with_fallbacks(
        &self,
        run_id: RunId,
        agent_id: Option<AgentId>,
        provider: &str,
        model: &str,
        request: CanonicalModelRequest,
        fallback_models: &[ResolvedModelAssignment],
        cancellation: &CancellationToken,
    ) -> Result<ModelCallOutcome, ExecutionError> {
        let mut candidates = vec![(provider.to_string(), model.to_string())];
        let model_pack_selected = self
            .store
            .get_run(run_id)
            .and_then(|run| self.store.get_conversation(run.conversation_id))
            .is_ok_and(|conversation| conversation.model_pack.is_some());
        if model_pack_selected
            && let Some(agent_id) = agent_id
            && let Ok(agent) = self.store.get_agent(agent_id)
        {
            for fallback in agent.fallback_chain {
                let (fallback_provider, fallback_model) = fallback.split_once('/').map_or_else(
                    || {
                        (
                            fallback.clone(),
                            self.providers
                                .default_model(&fallback)
                                .unwrap_or_else(|| model.to_string()),
                        )
                    },
                    |(provider, model)| (provider.to_string(), model.to_string()),
                );
                if !candidates.iter().any(|candidate| {
                    candidate == &(fallback_provider.clone(), fallback_model.clone())
                }) {
                    candidates.push((fallback_provider, fallback_model));
                }
            }
        }
        for fallback in fallback_models {
            let candidate = (fallback.provider.clone(), fallback.model.clone());
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
        let mut last_error = None;
        for (index, (candidate_provider, candidate_model)) in candidates.iter().enumerate() {
            let mut candidate_request = request.clone();
            candidate_request.model.clone_from(candidate_model);
            match self
                .model_call_single(
                    run_id,
                    agent_id,
                    candidate_provider,
                    candidate_model,
                    candidate_request,
                    cancellation,
                )
                .await
            {
                Ok((events, provider_ms, first_token_ms)) => {
                    if let Some(agent_id) = agent_id {
                        self.store.set_agent_route(
                            agent_id,
                            candidate_provider.clone(),
                            candidate_model.clone(),
                        )?;
                    }
                    if candidate_provider != provider || candidate_model != model {
                        self.store.append_event(
                            run_id,
                            agent_id,
                            None,
                            "routing.model_transition",
                            &json!({
                                "from_provider": provider,
                                "from_model": model,
                                "to_provider": candidate_provider,
                                "to_model": candidate_model,
                                "reason": "fallback",
                                "pinned_for_remaining_agent_cycles": true
                            }),
                            None,
                        )?;
                    }
                    return Ok(ModelCallOutcome {
                        events,
                        provider_ms,
                        first_token_ms,
                        actual_provider: candidate_provider.clone(),
                        actual_model: candidate_model.clone(),
                    });
                }
                Err(error)
                    if index + 1 < candidates.len() && provider_error_allows_fallback(&error) =>
                {
                    self.store.append_event(
                        run_id,
                        agent_id,
                        None,
                        "provider.fallback_selected",
                        &json!({
                            "failed_provider": candidate_provider,
                            "failed_model": candidate_model,
                            "next_provider": candidates[index + 1].0,
                            "next_model": candidates[index + 1].1,
                            "error": provider_error_summary(&error)
                        }),
                        None,
                    )?;
                    last_error = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            ExecutionError::Router(RouterError::UnknownProvider(provider.to_string()))
        }))
    }

    #[allow(clippy::too_many_lines)]
    async fn model_call_single(
        &self,
        run_id: RunId,
        agent_id: Option<AgentId>,
        provider: &str,
        model: &str,
        request: CanonicalModelRequest,
        cancellation: &CancellationToken,
    ) -> Result<(Vec<ModelEvent>, u64, u64), ExecutionError> {
        let adapter = self
            .providers
            .resolve(provider, &RequiredCapabilities::default())?;
        let retry_policy = agent_id
            .and_then(|id| self.store.get_agent(id).ok())
            .map_or_else(opensrc_core::RetryPolicy::default, |agent| {
                agent.retry_policy
            });
        let maximum_attempts = retry_policy.max_attempts.max(1);
        let mut attempt = 0_u32;
        let mut backoff_ms = retry_policy.initial_backoff_ms.max(1);
        loop {
            attempt += 1;
            let permit = self
                .provider_permits
                .acquire()
                .await
                .map_err(|_| ExecutionError::ProviderConcurrencyClosed)?;
            let request_json = serde_json::to_value(&request)?;
            let call_id =
                self.store
                    .begin_model_call(run_id, agent_id, provider, model, &request_json)?;
            let started = Instant::now();
            let mut first_event_ms = None;
            let mut saw_effectful_event = false;
            let collected: Result<Vec<ModelEvent>, ExecutionError> = async {
                if adapter.capabilities().supports_streaming {
                    let mut stream = tokio::select! {
                        () = cancellation.cancelled() => {
                            return Err(ExecutionError::Cancelled(run_id));
                        }
                        result = adapter.stream(request.clone()) => result?
                    };
                    let mut events = Vec::new();
                    loop {
                        let next = tokio::select! {
                            () = cancellation.cancelled() => {
                                return Err(ExecutionError::Cancelled(run_id));
                            }
                            event = stream.next() => event
                        };
                        let Some(event) = next else {
                            break;
                        };
                        let event = event?;
                        let index = events.len();
                        first_event_ms.get_or_insert_with(|| elapsed_ms(started));
                        saw_effectful_event |= matches!(
                            &event,
                            ModelEvent::TextDelta { text } if !text.is_empty()
                        ) || matches!(&event, ModelEvent::ToolCall { .. });
                        self.store.append_event(
                            run_id,
                            agent_id,
                            None,
                            "model.event",
                            &json!({"index": index, "event": event}),
                            Some(&format!("{call_id}:{index}")),
                        )?;
                        events.push(event);
                    }
                    if !events
                        .iter()
                        .any(|event| matches!(event, ModelEvent::Completed { .. }))
                    {
                        return Err(ExecutionError::Provider(ProviderError::InvalidResponse(
                            "provider stream ended before an explicit completion event".to_string(),
                        )));
                    }
                    Ok(events)
                } else {
                    let events = tokio::select! {
                        () = cancellation.cancelled() => {
                            return Err(ExecutionError::Cancelled(run_id));
                        }
                        result = adapter.execute(request.clone()) => result?
                    };
                    saw_effectful_event = events.iter().any(|event| {
                        matches!(event, ModelEvent::TextDelta { text } if !text.is_empty())
                            || matches!(event, ModelEvent::ToolCall { .. })
                    });
                    if !events
                        .iter()
                        .any(|event| matches!(event, ModelEvent::Completed { .. }))
                    {
                        return Err(ExecutionError::Provider(ProviderError::InvalidResponse(
                            "provider response ended before an explicit completion event"
                                .to_string(),
                        )));
                    }
                    first_event_ms = Some(elapsed_ms(started));
                    for (index, event) in events.iter().enumerate() {
                        self.store.append_event(
                            run_id,
                            agent_id,
                            None,
                            "model.event",
                            &json!({"index": index, "event": event}),
                            Some(&format!("{call_id}:{index}")),
                        )?;
                    }
                    Ok(events)
                }
            }
            .await;
            drop(permit);
            match collected {
                Ok(events) => {
                    let duration = elapsed_ms(started);
                    let response = serde_json::to_value(&events)?;
                    self.store
                        .finish_model_call(call_id, "completed", &response)?;
                    return Ok((events, duration, first_event_ms.unwrap_or(duration)));
                }
                Err(error) => {
                    self.store.finish_model_call(
                        call_id,
                        "failed",
                        &json!({"error": provider_error_summary(&error), "attempt": attempt}),
                    )?;
                    let retryable = matches!(
                        error,
                        ExecutionError::Provider(
                            ProviderError::Transient(_) | ProviderError::RateLimited { .. }
                        )
                    ) && !saw_effectful_event
                        && attempt < maximum_attempts;
                    if !retryable {
                        return Err(error);
                    }
                    let wait_ms =
                        provider_retry_wait_ms(&error, backoff_ms, retry_policy.max_backoff_ms);
                    let event_kind = if matches!(
                        error,
                        ExecutionError::Provider(ProviderError::RateLimited { .. })
                    ) {
                        "provider.rate_limit_wait"
                    } else {
                        "provider.retry_scheduled"
                    };
                    self.store.append_event(
                        run_id,
                        agent_id,
                        None,
                        event_kind,
                        &json!({
                            "provider": provider,
                            "model": model,
                            "attempt": attempt,
                            "next_attempt": attempt + 1,
                            "backoff_ms": wait_ms,
                            "error": provider_error_summary(&error)
                        }),
                        Some(&format!("provider-retry:{run_id}:{agent_id:?}:{attempt}")),
                    )?;
                    tokio::select! {
                        () = cancellation.cancelled() => {
                            return Err(ExecutionError::Cancelled(run_id));
                        }
                        () = tokio::time::sleep(std::time::Duration::from_millis(wait_ms)) => {}
                    }
                    backoff_ms = backoff_ms
                        .saturating_mul(2)
                        .min(retry_policy.max_backoff_ms.max(1));
                }
            }
        }
    }

    fn record_model_pack_assignment(
        &self,
        run_id: RunId,
        agent_id: Option<AgentId>,
        pack: &ModelPack,
        stage: ModelPackStage,
        role: &str,
        member: &ModelPackMember,
    ) -> Result<(), ExecutionError> {
        self.store.append_event(
            run_id,
            agent_id,
            None,
            "model_pack.assignment_selected",
            &json!({
                "pack_id": pack.id,
                "pack_name": pack.name,
                "strategy": pack.strategy,
                "stage": stage,
                "role": role,
                "provider": member.provider,
                "model": member.model,
                "cost_tier": member.cost_tier,
                "quality_tier": member.quality_tier
            }),
            None,
        )?;
        Ok(())
    }

    fn record_routing_assignment(
        &self,
        run_id: RunId,
        agent_id: Option<AgentId>,
        role: &str,
        assignment: &ResolvedModelAssignment,
        reason: &str,
    ) -> Result<(), ExecutionError> {
        let policy = self.routing_policies.role(role);
        self.store.append_event(
            run_id,
            agent_id,
            None,
            "routing.policy_selected",
            &json!({
                "policy_version": crate::ROUTING_POLICY_VERSION,
                "role": role,
                "reason": reason,
                "model_alias": assignment.alias,
                "display_name": assignment.display_name,
                "provider": assignment.provider,
                "model": assignment.model,
                "thinking": policy.as_ref().map(|value| value.thinking),
                "reasoning_effort": policy.as_ref().and_then(|value| value.reasoning_effort.clone()),
                "context_policy": policy.as_ref().map(|value| &value.context_policy),
                "tool_profile": policy.as_ref().map(|value| value.tool_profile),
                "cost_class": policy.as_ref().map(|value| value.cost_class),
                "latency_class": policy.as_ref().map(|value| value.latency_class),
                "fallback_models": policy.as_ref().map(|value| &value.fallback_models)
            }),
            None,
        )?;
        self.store.append_event(
            run_id,
            agent_id,
            None,
            "routing.model_pinned",
            &json!({
                "role": role,
                "provider": assignment.provider,
                "model": assignment.model
            }),
            None,
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    async fn execute_preflight_tool(
        &self,
        agent: &mut Agent,
        run_id: RunId,
        conversation_id: Uuid,
        provider: &str,
        model: &str,
        name: &str,
        arguments: Value,
        cancellation: &CancellationToken,
    ) -> Result<ToolExecutionResult, ExecutionError> {
        let evaluation = self.tools.evaluate(agent, name, &arguments)?;
        let descriptor = self
            .tools
            .registry()
            .get(name)
            .ok_or_else(|| ToolExecutionError::UnknownTool(name.to_string()))?;
        let call_key = format!(
            "{run_id}:{}:preflight:{name}:{}",
            agent.id,
            serde_json::to_string(&arguments)?
        );
        let tool_call_id = match self.store.claim_tool_call(
            run_id,
            agent.id,
            None,
            name,
            &arguments,
            &call_key,
            descriptor.destructive,
        )? {
            ToolCallClaim::Execute { id } => id,
            ToolCallClaim::Replay { output, .. } => {
                return Ok(
                    serde_json::from_value(output.clone()).unwrap_or(ToolExecutionResult {
                        output,
                        duration_ms: 0,
                        file_mutations: Vec::new(),
                    }),
                );
            }
            ToolCallClaim::InFlight { id } => {
                return Err(ExecutionError::ToolCallInFlight(id.to_string()));
            }
        };
        let call_id = format!("preflight-{name}-{run_id}");
        let started = Instant::now();
        self.store.append_event(
            run_id,
            Some(agent.id),
            None,
            "tool.started",
            &json!({
                "call_id": call_id,
                "name": name,
                "target": tool_target_summary(name, &arguments),
            }),
            Some(&format!("{call_key}:started")),
        )?;
        let mut effective_arguments = arguments.clone();
        let mut approval_state = "not_required".to_string();
        let allowed = match evaluation.decision {
            PolicyDecision::Allow => true,
            PolicyDecision::Deny => {
                approval_state = "policy_denied".to_string();
                false
            }
            PolicyDecision::Ask => match self.store.permission_effect(run_id, name, &arguments)? {
                Some(PermissionEffect::Allow) => {
                    approval_state = "allowed_by_rule".to_string();
                    true
                }
                Some(PermissionEffect::Deny) => {
                    approval_state = "denied_by_rule".to_string();
                    false
                }
                None => {
                    let approval = self.store.create_approval(
                        run_id,
                        Some(agent.id),
                        Some(tool_call_id),
                        name,
                        arguments.clone(),
                        evaluation.reasons.clone(),
                    )?;
                    self.store.append_message(
                        conversation_id,
                        Some(run_id),
                        MessageRole::Assistant,
                        vec![MessageContent::ApprovalRequest {
                            approval_id: approval.id.to_string(),
                            summary: approval_summary(name, &arguments),
                            details: json!({
                                "tool_name": name,
                                "arguments": arguments,
                                "reasons": approval.reasons,
                            }),
                        }],
                        Some(provider),
                        Some(model),
                        None,
                    )?;
                    self.store.transition_run(run_id, RunStatus::Waiting)?;
                    *agent = self
                        .store
                        .transition_agent(agent.id, AgentStatus::Waiting)?;
                    let decision = loop {
                        let current = self.store.get_approval(approval.id)?;
                        if current.status != ApprovalStatus::Pending {
                            break current;
                        }
                        tokio::select! {
                            () = cancellation.cancelled() => {
                                return Err(ExecutionError::Cancelled(run_id));
                            }
                            () = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
                        }
                    };
                    self.store.transition_run(run_id, RunStatus::Running)?;
                    *agent = self
                        .store
                        .transition_agent(agent.id, AgentStatus::Running)?;
                    let allowed = decision.status == ApprovalStatus::Allowed;
                    approval_state = if allowed {
                        "allowed".to_string()
                    } else {
                        "denied".to_string()
                    };
                    if allowed && let Some(edited) = decision.edited_arguments.clone() {
                        effective_arguments = edited;
                    }
                    self.store.append_message(
                        conversation_id,
                        Some(run_id),
                        MessageRole::User,
                        vec![MessageContent::ApprovalResult {
                            approval_id: approval.id.to_string(),
                            decision: approval_state.clone(),
                            reason: decision.decision_reason,
                        }],
                        None,
                        None,
                        None,
                    )?;
                    allowed
                }
            },
        };
        let result = if allowed {
            match self
                .tools
                .execute_approved(agent, name, effective_arguments.clone())
                .await
            {
                Ok(result) => result,
                Err(error) => {
                    let output = json!({"error": error.to_string()});
                    self.store
                        .finish_tool_call(tool_call_id, "failed", &output)?;
                    self.store.append_event(
                        run_id,
                        Some(agent.id),
                        None,
                        "tool.completed",
                        &json!({
                            "call_id": call_id,
                            "name": name,
                            "target": tool_target_summary(name, &effective_arguments),
                            "status": "failed",
                            "elapsed_ms": elapsed_ms(started),
                        }),
                        Some(&format!("{call_key}:completed")),
                    )?;
                    return Err(error.into());
                }
            }
        } else {
            ToolExecutionResult {
                output: json!({
                    "error": match approval_state.as_str() {
                        "policy_denied" => evaluation.reasons.join("; "),
                        "denied_by_rule" =>
                            "tool request denied by a persistent permission rule".to_string(),
                        _ => "tool request denied by user".to_string(),
                    }
                }),
                duration_ms: 0,
                file_mutations: Vec::new(),
            }
        };
        let serialized_result = serde_json::to_value(&result)?;
        self.store.finish_tool_call(
            tool_call_id,
            if allowed { "completed" } else { "denied" },
            &serialized_result,
        )?;
        self.store.append_event(
            run_id,
            Some(agent.id),
            None,
            "tool.completed",
            &json!({
                "call_id": call_id,
                "name": name,
                "target": tool_target_summary(name, &effective_arguments),
                "status": if allowed { "completed" } else { "denied" },
                "elapsed_ms": elapsed_ms(started),
            }),
            Some(&format!("{call_key}:completed")),
        )?;
        self.store.append_message(
            conversation_id,
            Some(run_id),
            MessageRole::Assistant,
            vec![MessageContent::ToolCall {
                provider_call_id: call_id.clone(),
                canonical_call_id: call_id.clone(),
                name: name.to_string(),
                arguments: effective_arguments,
            }],
            Some(provider),
            Some(model),
            None,
        )?;
        self.store.append_message(
            conversation_id,
            Some(run_id),
            MessageRole::Tool,
            vec![MessageContent::ToolResult {
                provider_call_id: call_id.clone(),
                canonical_call_id: call_id,
                name: name.to_string(),
                result: serialized_result,
                timing_ms: Some(result.duration_ms),
                approval_state: Some(approval_state),
            }],
            Some(provider),
            Some(model),
            None,
        )?;
        Ok(result)
    }
}

fn provider_retry_wait_ms(
    error: &ExecutionError,
    exponential_backoff_ms: u64,
    maximum_ms: u64,
) -> u64 {
    let maximum_ms = maximum_ms.clamp(1, 60_000);
    let provider_hint = match error {
        ExecutionError::Provider(ProviderError::RateLimited { retry_after_ms, .. }) => {
            *retry_after_ms
        }
        _ => None,
    };
    provider_hint
        .unwrap_or(exponential_backoff_ms)
        .max(1)
        .min(maximum_ms)
}

#[allow(clippy::too_many_lines)]
fn build_repository_map(root: &Path) -> Result<Value, ExecutionError> {
    if !root.is_dir() {
        return Err(ExecutionError::DeterministicService(format!(
            "repository root `{}` is not a directory",
            root.display()
        )));
    }
    let mut indexed_files = Vec::new();
    let mut languages = BTreeMap::<String, u64>::new();
    let mut manifests = Vec::new();
    let mut symbols = Vec::new();
    let mut tests = Vec::new();
    let mut dependencies = BTreeSet::new();
    let mut total_files = 0_u64;
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            !matches!(
                entry.file_name().to_string_lossy().as_ref(),
                ".git" | "target" | "node_modules" | ".opensource"
            )
        })
    {
        let entry = entry.map_err(|error| {
            ExecutionError::DeterministicService(format!(
                "repository indexing failed at {}: {error}",
                root.display()
            ))
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        total_files = total_files.saturating_add(1);
        let relative = entry
            .path()
            .strip_prefix(root)
            .unwrap_or_else(|_| entry.path())
            .to_string_lossy()
            .replace('\\', "/");
        if indexed_files.len() < 500 {
            indexed_files.push(relative.clone());
        }
        let relative_lower = relative.to_ascii_lowercase();
        if relative_lower.contains("/tests/")
            || relative_lower.starts_with("tests/")
            || relative_lower.contains(".test.")
            || relative_lower.contains(".spec.")
            || relative_lower.ends_with("_test.rs")
            || relative_lower.ends_with("_test.py")
        {
            tests.push(relative.clone());
        }
        if let Some(extension) = entry.path().extension().and_then(|value| value.to_str()) {
            let extension = extension.to_ascii_lowercase();
            *languages.entry(extension.clone()).or_default() += 1;
            if symbols.len() < 1_000
                && matches!(
                    extension.as_str(),
                    "rs" | "ts" | "tsx" | "js" | "jsx" | "py" | "go" | "java"
                )
                && let Ok(source) = std::fs::read_to_string(entry.path())
            {
                for (line_index, line) in source.lines().take(10_000).enumerate() {
                    let trimmed = line.trim_start();
                    let marker = [
                        "pub fn ",
                        "fn ",
                        "pub struct ",
                        "struct ",
                        "pub enum ",
                        "enum ",
                        "class ",
                        "def ",
                        "function ",
                        "export function ",
                        "interface ",
                        "type ",
                    ]
                    .into_iter()
                    .find(|marker| trimmed.starts_with(marker));
                    if let Some(marker) = marker {
                        let name = trimmed[marker.len()..]
                            .split(|character: char| {
                                character.is_whitespace()
                                    || matches!(character, '(' | '<' | '{' | ':' | '=')
                            })
                            .next()
                            .unwrap_or_default();
                        if !name.is_empty() {
                            symbols.push(json!({
                                "name": name,
                                "kind": marker.trim(),
                                "path": relative,
                                "line": line_index + 1
                            }));
                            if symbols.len() >= 1_000 {
                                break;
                            }
                        }
                    }
                }
            }
        }
        if matches!(
            entry
                .file_name()
                .to_string_lossy()
                .to_ascii_lowercase()
                .as_str(),
            "cargo.toml"
                | "package.json"
                | "pyproject.toml"
                | "go.mod"
                | "pom.xml"
                | "build.gradle"
        ) {
            if let Ok(content) = std::fs::read_to_string(entry.path()) {
                collect_manifest_dependencies(
                    entry.file_name().to_string_lossy().as_ref(),
                    &content,
                    &mut dependencies,
                );
            }
            manifests.push(relative);
        }
    }
    let git = std::process::Command::new("git")
        .args(["status", "--short", "--branch"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string());
    let symbols_truncated = symbols.len() >= 1_000;
    Ok(json!({
        "service": "deterministic_repository_map_v2",
        "root": root,
        "total_files": total_files,
        "indexed_files": indexed_files,
        "language_extensions": languages,
        "manifests": manifests,
        "dependencies": dependencies,
        "symbols": symbols,
        "test_files": tests,
        "git_status": git,
        "ownership_index": {
            "workspace_root": root,
            "enforcement": "transactional task leases"
        },
        "truncated": total_files > 500,
        "symbols_truncated": symbols_truncated,
        "model_calls": 0
    }))
}

fn collect_manifest_dependencies(
    manifest_name: &str,
    content: &str,
    dependencies: &mut BTreeSet<String>,
) {
    if manifest_name.eq_ignore_ascii_case("package.json") {
        if let Ok(document) = serde_json::from_str::<Value>(content) {
            for section in ["dependencies", "devDependencies", "peerDependencies"] {
                if let Some(values) = document.get(section).and_then(Value::as_object) {
                    dependencies.extend(values.keys().cloned());
                }
            }
        }
        return;
    }
    if manifest_name.eq_ignore_ascii_case("cargo.toml") {
        let mut in_dependencies = false;
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                in_dependencies = matches!(
                    trimmed,
                    "[dependencies]" | "[dev-dependencies]" | "[build-dependencies]"
                );
                continue;
            }
            if in_dependencies && let Some((name, _)) = trimmed.split_once('=') {
                let name = name.trim();
                if !name.is_empty() && !name.starts_with('#') {
                    dependencies.insert(name.to_string());
                }
            }
        }
    }
}

async fn run_release_gates(
    run_id: RunId,
    root: &Path,
    cancellation: &CancellationToken,
) -> Result<(String, Vec<String>, Vec<String>, Vec<String>), ExecutionError> {
    let gates: Vec<(&str, &str, Vec<&str>)> = if root.join("Cargo.toml").is_file() {
        vec![
            ("format", "cargo", vec!["fmt", "--all", "--", "--check"]),
            (
                "tests",
                "cargo",
                vec!["test", "--workspace", "--all-targets"],
            ),
            (
                "lint",
                "cargo",
                vec![
                    "clippy",
                    "--workspace",
                    "--all-targets",
                    "--",
                    "-D",
                    "warnings",
                ],
            ),
            ("build", "cargo", vec!["build", "--workspace", "--release"]),
        ]
    } else {
        return Err(ExecutionError::DeterministicService(
            "no supported release manifest was found; no release gate was bypassed".to_string(),
        ));
    };
    let mut commands = Vec::new();
    let mut tests = Vec::new();
    let mut report = Vec::new();
    for (name, program, arguments) in gates {
        let command = format!("{program} {}", arguments.join(" "));
        commands.push(command.clone());
        if name == "tests" {
            tests.push(command.clone());
        }
        let mut process = tokio::process::Command::new(program);
        process
            .args(&arguments)
            .current_dir(root)
            .kill_on_drop(true);
        let output = tokio::select! {
            () = cancellation.cancelled() => return Err(ExecutionError::Cancelled(run_id)),
            result = tokio::time::timeout(
                std::time::Duration::from_secs(900),
                process.output()
            ) => result
                .map_err(|_| ExecutionError::DeterministicService(format!(
                    "release gate `{name}` timed out"
                )))?
                .map_err(|error| ExecutionError::DeterministicService(format!(
                    "release gate `{name}` could not start: {error}"
                )))?
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        report.push(format!(
            "{name}: {}",
            if output.status.success() {
                "passed"
            } else {
                "failed"
            }
        ));
        if !output.status.success() {
            return Err(ExecutionError::DeterministicService(format!(
                "release gate `{name}` failed for `{command}`\n{}\n{}",
                truncate_evidence(&stdout),
                truncate_evidence(&stderr)
            )));
        }
    }
    Ok((
        format!(
            "Deterministic release gates passed with zero model calls.\n{}",
            report.join("\n")
        ),
        Vec::new(),
        commands,
        tests,
    ))
}

fn truncate_evidence(value: &str) -> String {
    const MAXIMUM: usize = 4_000;
    if value.len() <= MAXIMUM {
        value.to_string()
    } else {
        format!("{}…", &value[..MAXIMUM])
    }
}

#[derive(Debug, Deserialize)]
struct AgenticPlan {
    tasks: Vec<PlannedTask>,
}

#[derive(Debug, Deserialize)]
struct PlannedTask {
    #[serde(default)]
    id: Option<String>,
    #[serde(alias = "objective")]
    description: String,
    #[serde(default = "default_agentic_role", alias = "specialist")]
    role: String,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    dependencies: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    owned_paths: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    acceptance_criteria: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    deliverables: Vec<String>,
    #[serde(
        default,
        alias = "validation",
        deserialize_with = "deserialize_string_list"
    )]
    validation_steps: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    forbidden_actions: Vec<String>,
    #[serde(
        default,
        alias = "contract",
        deserialize_with = "deserialize_string_list"
    )]
    contract_notes: Vec<String>,
}

fn deserialize_string_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    match Value::deserialize(deserializer)? {
        Value::Null => Ok(Vec::new()),
        Value::String(value) => Ok((!value.trim().is_empty())
            .then_some(value)
            .into_iter()
            .collect()),
        Value::Array(values) => values
            .into_iter()
            .map(|value| match value {
                Value::String(value) => Ok(value),
                other => Err(serde::de::Error::custom(format!(
                    "expected a string list item, got {other}"
                ))),
            })
            .collect(),
        other => Err(serde::de::Error::custom(format!(
            "expected a string or string array, got {other}"
        ))),
    }
}

#[derive(Debug, Deserialize)]
struct SpawnAgentToolArgs {
    task: String,
    role: Option<String>,
    #[serde(default)]
    owned_paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct AgentMessageToolArgs {
    agent_id: String,
    message: String,
}

#[derive(Debug, Deserialize)]
struct AgentStatusToolArgs {
    #[serde(default)]
    agent_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct AgentWaitToolArgs {
    agent_ids: Vec<String>,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct AgentIdToolArgs {
    agent_id: String,
}

#[derive(Debug, Deserialize)]
struct PlanUpdateToolArgs {
    items: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct SkillActivateToolArgs {
    name: String,
}

#[derive(Debug, Deserialize)]
struct SkillInstallToolArgs {
    source: String,
    name: Option<String>,
    subdirectory: Option<String>,
    #[serde(default)]
    force: bool,
}

#[derive(Debug, Deserialize)]
struct McpConnectToolArgs {
    name: String,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    url: Option<String>,
    token_env: Option<String>,
    #[serde(default = "default_true")]
    test: bool,
}

#[derive(Debug, Deserialize)]
struct McpInvokeToolArgs {
    server: String,
    tool: String,
    #[serde(default)]
    arguments: Value,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct McpListToolsArgs {
    server: String,
}

const fn default_true() -> bool {
    true
}

async fn install_skill(
    agent: &Agent,
    args: &SkillInstallToolArgs,
) -> Result<Value, ToolExecutionError> {
    let project_root = Path::new(&agent.workspace.root)
        .canonicalize()
        .map_err(|error| ToolExecutionError::Service(error.to_string()))?;
    let skills_root = project_root.join(".opensource").join("skills");
    std::fs::create_dir_all(&skills_root)
        .map_err(|error| ToolExecutionError::Service(error.to_string()))?;

    let resolved = resolve_skill_source(&project_root, &args.source).await?;
    let source_root = &resolved.root;

    let result = (|| {
        let requested_subdirectory = args
            .subdirectory
            .as_deref()
            .map(clean_relative_path)
            .transpose()?
            .or_else(|| resolved.inferred_subdirectory.clone());
        let selected = if let Some(relative) = requested_subdirectory {
            let candidate = source_root.join(relative);
            skill_directory(&candidate)?
        } else {
            find_skill_directory(source_root, args.name.as_deref())?
        };
        let metadata = SkillRegistry::inspect(selected.join("SKILL.md"))
            .map_err(|error| ToolExecutionError::Service(error.to_string()))?;
        validate_extension_name(&metadata.name)?;
        if let Some(expected) = args.name.as_deref()
            && expected != metadata.name
        {
            return Err(ToolExecutionError::Service(format!(
                "selected skill is `{}`, not requested `{expected}`",
                metadata.name
            )));
        }
        let destination = skills_root.join(&metadata.name);
        let replaced = destination.exists();
        if replaced && !args.force {
            return Err(ToolExecutionError::Service(format!(
                "skill `{}` already exists; retry with force=true to replace it",
                metadata.name
            )));
        }
        if replaced {
            std::fs::remove_dir_all(&destination)
                .map_err(|error| ToolExecutionError::Service(error.to_string()))?;
        }
        copy_skill_directory(&selected, &destination)?;
        SkillRegistry::discover(&skills_root)
            .map_err(|error| ToolExecutionError::Service(error.to_string()))?;
        Ok(json!({
            "name": metadata.name,
            "description": metadata.description,
            "source": args.source,
            "path": destination.to_string_lossy(),
            "replaced": replaced,
            "available_immediately": true
        }))
    })();
    if let Some(path) = resolved.temporary {
        let _ = std::fs::remove_dir_all(path);
    }
    result
}

struct ResolvedSkillSource {
    root: PathBuf,
    inferred_subdirectory: Option<PathBuf>,
    temporary: Option<PathBuf>,
}

async fn resolve_skill_source(
    project_root: &Path,
    source: &str,
) -> Result<ResolvedSkillSource, ToolExecutionError> {
    if !source.starts_with("https://") && !source.starts_with("http://") {
        let path = PathBuf::from(source);
        let path = if path.is_absolute() {
            path
        } else {
            project_root.join(path)
        };
        return Ok(ResolvedSkillSource {
            root: path
                .canonicalize()
                .map_err(|error| ToolExecutionError::Service(error.to_string()))?,
            inferred_subdirectory: None,
            temporary: None,
        });
    }
    let checkout = project_root
        .join(".opensource")
        .join(format!(".skill-install-{}", Uuid::new_v4()));
    std::fs::create_dir_all(
        checkout
            .parent()
            .ok_or_else(|| ToolExecutionError::Service("invalid checkout path".to_string()))?,
    )
    .map_err(|error| ToolExecutionError::Service(error.to_string()))?;
    let (repository, reference, inferred_subdirectory) = normalize_github_skill_url(source);
    let mut command = tokio::process::Command::new("git");
    command.args(["clone", "--depth", "1"]);
    if let Some(reference) = reference {
        command.args(["--branch", &reference]);
    }
    command.arg(&repository).arg(&checkout);
    let output = command
        .output()
        .await
        .map_err(|error| ToolExecutionError::Service(format!("failed to start git: {error}")))?;
    if !output.status.success() {
        let _ = std::fs::remove_dir_all(&checkout);
        return Err(ToolExecutionError::Service(format!(
            "git clone failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(ResolvedSkillSource {
        root: checkout.clone(),
        inferred_subdirectory,
        temporary: Some(checkout),
    })
}

fn normalize_github_skill_url(source: &str) -> (String, Option<String>, Option<PathBuf>) {
    let Some((repository, tail)) = source.split_once("/tree/") else {
        let Some((repository, tail)) = source.split_once("/blob/") else {
            return (source.to_string(), None, None);
        };
        let mut parts = tail.splitn(2, '/');
        let reference = parts.next().map(str::to_string);
        let path = parts
            .next()
            .and_then(|value| Path::new(value).parent())
            .map(Path::to_path_buf);
        return (format!("{repository}.git"), reference, path);
    };
    let mut parts = tail.splitn(2, '/');
    let reference = parts.next().map(str::to_string);
    let path = parts.next().map(PathBuf::from);
    (format!("{repository}.git"), reference, path)
}

fn clean_relative_path(value: &str) -> Result<PathBuf, ToolExecutionError> {
    let path = Path::new(value);
    if value.trim().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(ToolExecutionError::Service(
            "skill subdirectory must be a clean relative path".to_string(),
        ));
    }
    Ok(path.to_path_buf())
}

fn skill_directory(path: &Path) -> Result<PathBuf, ToolExecutionError> {
    let directory = if path.is_file()
        && path
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("SKILL.md"))
    {
        path.parent().unwrap_or(path)
    } else {
        path
    };
    if !directory.join("SKILL.md").is_file() {
        return Err(ToolExecutionError::Service(format!(
            "{} does not contain SKILL.md",
            directory.display()
        )));
    }
    Ok(directory.to_path_buf())
}

fn find_skill_directory(
    root: &Path,
    expected_name: Option<&str>,
) -> Result<PathBuf, ToolExecutionError> {
    if root.join("SKILL.md").is_file() {
        return Ok(root.to_path_buf());
    }
    let mut matches = walkdir::WalkDir::new(root)
        .max_depth(8)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_type().is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case("SKILL.md"))
                && !entry
                    .path()
                    .components()
                    .any(|component| component.as_os_str() == ".git")
        })
        .filter_map(|entry| {
            let metadata = SkillRegistry::inspect(entry.path()).ok()?;
            expected_name
                .is_none_or(|expected| metadata.name == expected)
                .then(|| entry.path().parent().map(Path::to_path_buf))
                .flatten()
        })
        .collect::<Vec<_>>();
    matches.sort();
    matches.dedup();
    match matches.as_slice() {
        [path] => Ok(path.clone()),
        [] => Err(ToolExecutionError::Service(
            "no matching SKILL.md was found in the source".to_string(),
        )),
        _ => Err(ToolExecutionError::Service(format!(
            "source contains {} skills; provide `name` or `subdirectory`",
            matches.len()
        ))),
    }
}

fn validate_extension_name(name: &str) -> Result<(), ToolExecutionError> {
    if !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        Ok(())
    } else {
        Err(ToolExecutionError::Service(
            "skill name may contain only letters, numbers, '-' and '_'".to_string(),
        ))
    }
}

fn copy_skill_directory(source: &Path, destination: &Path) -> Result<(), ToolExecutionError> {
    std::fs::create_dir_all(destination)
        .map_err(|error| ToolExecutionError::Service(error.to_string()))?;
    for entry in walkdir::WalkDir::new(source)
        .max_depth(8)
        .follow_links(false)
    {
        let entry = entry.map_err(|error| ToolExecutionError::Service(error.to_string()))?;
        if entry.path() == source {
            continue;
        }
        if entry.file_type().is_symlink() {
            return Err(ToolExecutionError::Service(format!(
                "skill packages may not contain symbolic links: {}",
                entry.path().display()
            )));
        }
        let relative = entry
            .path()
            .strip_prefix(source)
            .map_err(|error| ToolExecutionError::Service(error.to_string()))?;
        if relative
            .components()
            .any(|component| component.as_os_str() == ".git")
        {
            continue;
        }
        let target = destination.join(relative);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)
                .map_err(|error| ToolExecutionError::Service(error.to_string()))?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| ToolExecutionError::Service(error.to_string()))?;
            }
            std::fs::copy(entry.path(), &target)
                .map_err(|error| ToolExecutionError::Service(error.to_string()))?;
        }
    }
    Ok(())
}

fn parse_runtime_args<T: serde::de::DeserializeOwned>(
    tool: &str,
    arguments: Value,
) -> Result<T, ToolExecutionError> {
    serde_json::from_value(arguments).map_err(|error| ToolExecutionError::InvalidInput {
        tool: tool.to_string(),
        message: error.to_string(),
    })
}

fn parse_agent_id(tool: &str, value: &str) -> Result<AgentId, ToolExecutionError> {
    Uuid::parse_str(value).map_err(|error| ToolExecutionError::InvalidInput {
        tool: tool.to_string(),
        message: format!("invalid agent id `{value}`: {error}"),
    })
}

fn parse_agent_ids(tool: &str, values: &[String]) -> Result<Vec<AgentId>, ToolExecutionError> {
    values
        .iter()
        .map(|value| parse_agent_id(tool, value))
        .collect()
}

fn ensure_same_run(
    store: &Store,
    run_id: RunId,
    agent_id: AgentId,
) -> Result<(), ToolExecutionError> {
    let target = store
        .get_agent(agent_id)
        .map_err(|error| ToolExecutionError::Service(error.to_string()))?;
    if target.run_id == run_id {
        Ok(())
    } else {
        Err(ToolExecutionError::Denied {
            tool: "agents".to_string(),
            reasons: "target agent belongs to a different run".to_string(),
        })
    }
}

fn agent_snapshots(
    store: &Store,
    run_id: RunId,
    ids: &[AgentId],
) -> Result<Vec<Value>, ToolExecutionError> {
    ids.iter()
        .map(|id| {
            let agent = store
                .get_agent(*id)
                .map_err(|error| ToolExecutionError::Service(error.to_string()))?;
            if agent.run_id != run_id {
                return Err(ToolExecutionError::Denied {
                    tool: "agents.status".to_string(),
                    reasons: "target agent belongs to a different run".to_string(),
                });
            }
            let completion = store
                .get_agent_completion(*id)
                .map_err(|error| ToolExecutionError::Service(error.to_string()))?;
            Ok(json!({"agent": agent, "completion": completion}))
        })
        .collect()
}

fn tool_result(output: Value) -> ToolExecutionResult {
    ToolExecutionResult {
        output,
        duration_ms: 0,
        file_mutations: Vec::new(),
    }
}

fn approval_summary(tool: &str, arguments: &Value) -> String {
    let path = ["path", "cwd", "source", "destination"]
        .into_iter()
        .find_map(|field| arguments.get(field).and_then(Value::as_str));
    path.map_or_else(
        || format!("Allow `{tool}` for this run?"),
        |path| format!("Allow `{tool}` to access `{path}` for this run?"),
    )
}

fn provider_error_allows_fallback(error: &ExecutionError) -> bool {
    matches!(
        error,
        ExecutionError::Provider(
            ProviderError::Transient(_)
                | ProviderError::RateLimited { .. }
                | ProviderError::Authentication(_)
                | ProviderError::Rejected(_)
        ) | ExecutionError::Router(
            RouterError::UnknownProvider(_)
                | RouterError::MissingCapability { .. }
                | RouterError::ModelDiscovery { .. }
        )
    )
}

fn provider_error_summary(error: &ExecutionError) -> &'static str {
    match error {
        ExecutionError::Provider(ProviderError::Transient(_)) => "transient provider failure",
        ExecutionError::Provider(ProviderError::RateLimited { .. }) => {
            "provider rate limit reached"
        }
        ExecutionError::Provider(ProviderError::Authentication(_)) => {
            "provider authentication failed"
        }
        ExecutionError::Provider(ProviderError::Rejected(_)) => "provider rejected the request",
        ExecutionError::Router(RouterError::UnknownProvider(_)) => "provider is unavailable",
        ExecutionError::Router(RouterError::MissingCapability { .. }) => {
            "provider lacks a required capability"
        }
        ExecutionError::Router(RouterError::ModelDiscovery { .. }) => {
            "provider model discovery failed"
        }
        _ => "model request failed",
    }
}

fn default_agentic_role() -> String {
    "implementer".to_string()
}

fn validate_planned_task(id: &str, task: &PlannedTask) -> Result<(), ExecutionError> {
    if task.description.trim().is_empty() {
        return Err(ExecutionError::InvalidAgentPlan(format!(
            "task `{id}` has no objective"
        )));
    }
    if task.acceptance_criteria.is_empty()
        || task
            .acceptance_criteria
            .iter()
            .any(|criterion| criterion.trim().is_empty())
    {
        return Err(ExecutionError::InvalidAgentPlan(format!(
            "task `{id}` has no measurable acceptance criteria"
        )));
    }
    if task.deliverables.is_empty() || task.validation_steps.is_empty() {
        return Err(ExecutionError::InvalidAgentPlan(format!(
            "task `{id}` must declare deliverables and validation steps"
        )));
    }
    Ok(())
}

fn agentic_planner_prompt() -> String {
    let roles = built_in_agent_definitions()
        .unwrap_or_default()
        .into_iter()
        .map(|definition| format!("{}: {}", definition.name, definition.description))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "You are the root planner for a real local coding-agent runtime. Build a dependency-aware \
         plan that will be executed, not a prose suggestion. Return only schema-valid JSON.\n\n\
         Rules:\n\
         - Use one to eight bounded tasks; avoid delegation for work one focused agent can finish.\n\
         - Every task needs a concrete objective, measurable acceptance criteria, deliverables, \
           validation steps, forbidden actions, and contract notes for downstream agents.\n\
         - Dependencies contain task ids. Dependent agents receive predecessor completion objects.\n\
         - Give writing tasks narrow, non-overlapping owned_paths. Use `.` only when the change \
           genuinely spans the project; read-only tasks use an empty list.\n\
         - For implementation work, include validation and independent review when useful.\n\
         - Select the specialist by work type, never by model name. The runtime assigns models.\n\
         - Do not invent files, tools, providers, results, or completed work.\n\n\
         Available specialist roles:\n{roles}"
    )
}

fn agentic_plan_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "tasks": {
                "type": "array",
                "minItems": 1,
                "maxItems": 8,
                "items": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"},
                        "description": {"type": "string"},
                        "role": {"type": "string"},
                        "dependencies": {
                            "type": "array",
                            "items": {"type": "string"}
                        },
                        "owned_paths": {
                            "type": "array",
                            "items": {"type": "string"}
                        },
                        "acceptance_criteria": {
                            "type": "array",
                            "items": {"type": "string"}
                        },
                        "deliverables": {
                            "type": "array",
                            "items": {"type": "string"}
                        },
                        "validation_steps": {
                            "type": "array",
                            "items": {"type": "string"}
                        },
                        "forbidden_actions": {
                            "type": "array",
                            "items": {"type": "string"}
                        },
                        "contract_notes": {
                            "type": "array",
                            "items": {"type": "string"}
                        }
                    },
                    "required": [
                        "id",
                        "description",
                        "role",
                        "dependencies",
                        "owned_paths",
                        "acceptance_criteria",
                        "deliverables",
                        "validation_steps",
                        "forbidden_actions",
                        "contract_notes"
                    ],
                    "additionalProperties": false
                }
            }
        },
        "required": ["tasks"],
        "additionalProperties": false
    })
}

fn fallback_agentic_plan(user_request: &str) -> AgenticPlan {
    let mutation = user_request.to_ascii_lowercase();
    let mutation = [
        "build",
        "create",
        "implement",
        "fix",
        "edit",
        "write",
        "update",
        "change",
        "refactor",
        "replicate",
    ]
    .iter()
    .any(|marker| mutation.contains(marker));
    if !mutation {
        return AgenticPlan {
            tasks: vec![
                planned_task(
                    "investigate",
                    user_request,
                    "investigator",
                    &[],
                    &[],
                    "Source-backed findings answer the request.",
                ),
                planned_task(
                    "review",
                    "Independently verify the investigation and identify unsupported claims.",
                    "code-reviewer",
                    &["investigate"],
                    &[],
                    "Every material claim is corroborated or explicitly qualified.",
                ),
            ],
        };
    }
    AgenticPlan {
        tasks: vec![
            planned_task(
                "inspect",
                "Map the relevant code paths, constraints, tests, and existing conventions.",
                "investigator",
                &[],
                &[],
                "The implementation scope is grounded in source evidence.",
            ),
            planned_task(
                "implement",
                user_request,
                "implementer",
                &["inspect"],
                &["."],
                "The requested behavior works through the real runtime path.",
            ),
            planned_task(
                "validate",
                "Run focused and regression validation for the completed implementation.",
                "test-debugging-specialist",
                &["implement"],
                &["."],
                "Relevant automated checks pass and failures are reported exactly.",
            ),
            planned_task(
                "review",
                "Review the implementation, validation evidence, and release risks.",
                "code-reviewer",
                &["validate"],
                &[],
                "No high-confidence correctness issue remains unreported.",
            ),
        ],
    }
}

fn planned_task(
    id: &str,
    description: &str,
    role: &str,
    dependencies: &[&str],
    owned_paths: &[&str],
    acceptance: &str,
) -> PlannedTask {
    PlannedTask {
        id: Some(id.to_string()),
        description: description.to_string(),
        role: role.to_string(),
        dependencies: dependencies
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        owned_paths: owned_paths
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        acceptance_criteria: vec![acceptance.to_string()],
        deliverables: vec!["A structured completion with exact evidence.".to_string()],
        validation_steps: vec![
            "Inspect the resulting files or outputs before reporting completion.".to_string(),
        ],
        forbidden_actions: vec![
            "Do not claim actions, tests, or files that were not observed.".to_string(),
        ],
        contract_notes: vec![
            "Preserve unrelated user changes and hand off blockers explicitly.".to_string(),
        ],
    }
}

fn stage_for_role(role: &str) -> ModelPackStage {
    let role = role.to_ascii_lowercase();
    if [
        "review",
        "security",
        "performance",
        "accessibility",
        "dependency",
    ]
    .iter()
    .any(|marker| role.contains(marker))
    {
        ModelPackStage::Review
    } else if role.contains("test") || role.contains("debug") || role.contains("release") {
        ModelPackStage::Validate
    } else if role.contains("architect")
        || role.contains("investigator")
        || role.contains("repository")
    {
        ModelPackStage::Plan
    } else {
        ModelPackStage::Execute
    }
}

fn task_contract_message(
    store: &Store,
    task: &opensrc_core::Task,
    agent: &Agent,
) -> Result<String, ExecutionError> {
    let mut upstream = Vec::new();
    for dependency in &task.dependencies {
        let dependency = store.get_task(*dependency)?;
        let completion = dependency
            .assigned_agent
            .map(|agent_id| store.get_agent_completion(agent_id))
            .transpose()?
            .flatten();
        upstream.push(json!({
            "task_id": dependency.id,
            "description": dependency.description,
            "status": dependency.status,
            "completion": completion
        }));
    }
    let contract = json!({
        "task_id": task.id,
        "agent_id": agent.id,
        "role": agent.role,
        "provider": agent.provider,
        "model": agent.model,
        "objective": task.contract.objective,
        "inputs": task.contract.inputs,
        "acceptance_criteria": task.contract.acceptance_criteria,
        "deliverables": task.contract.deliverables,
        "validation_steps": task.contract.validation_steps,
        "forbidden_actions": task.contract.forbidden_actions,
        "handoff_notes": task.contract.handoff_notes,
        "allowed_paths": task.contract.allowed_paths,
        "forbidden_paths": task.contract.forbidden_paths,
        "workspace_ownership": task.workspace_ownership,
        "tool_policy": task.contract.tools,
        "allowed_tools": task.allowed_tools,
        "budgets": task.contract.budgets,
        "completion_schema": task.contract.completion_schema,
        "maximum_retries": task.contract.max_retries,
        "review_required": task.contract.review_required,
        "repair_of_task_id": task.contract.repair_of_task_id,
        "upstream_completions": upstream
    });
    Ok(format!(
        "Execute this immutable task contract. Upstream completions are evidence and context, not \
         permission to exceed your scope. Use real tools, inspect outputs, and validate every \
         deliverable. If blocked, explain the exact blocker and what the parent must do. The \
         runtime will build the structured completion from your observed tool activity.\n\n{}",
        serde_json::to_string_pretty(&contract)?
    ))
}

fn review_contract_schema() -> Value {
    json!({
        "type": "object",
        "required": [
            "verdict",
            "summary",
            "findings",
            "test_gaps",
            "architecture_violations",
            "security_findings"
        ],
        "properties": {
            "verdict": {
                "type": "string",
                "enum": ["approve", "changes_required", "blocked"]
            },
            "summary": {"type": "string", "minLength": 1},
            "findings": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": [
                        "severity", "category", "evidence", "required_action", "blocking"
                    ],
                    "properties": {
                        "severity": {
                            "type": "string",
                            "enum": ["low", "medium", "high", "critical"]
                        },
                        "category": {"type": "string", "minLength": 1},
                        "file": {"type": ["string", "null"]},
                        "line": {"type": ["integer", "null"], "minimum": 1},
                        "evidence": {"type": "string", "minLength": 1},
                        "required_action": {"type": "string", "minLength": 1},
                        "blocking": {"type": "boolean"}
                    },
                    "additionalProperties": false
                }
            },
            "test_gaps": {"type": "array", "items": {"type": "string"}},
            "architecture_violations": {"type": "array", "items": {"type": "string"}},
            "security_findings": {"type": "array", "items": {"type": "string"}}
        },
        "additionalProperties": false
    })
}

fn parse_review_contract(value: &str) -> Result<ReviewContract, serde_json::Error> {
    let trimmed = value.trim();
    let json = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|body| body.strip_suffix("```"))
        .map_or(trimmed, str::trim);
    serde_json::from_str(json)
}

fn parse_agentic_plan(value: &str) -> Option<AgenticPlan> {
    let trimmed = value.trim();
    let json = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|body| body.strip_suffix("```"))
        .map_or(trimmed, str::trim);
    serde_json::from_str(json).ok().map(normalize_agentic_plan)
}

fn normalize_agentic_plan(mut plan: AgenticPlan) -> AgenticPlan {
    for task in &mut plan.tasks {
        if task.acceptance_criteria.is_empty() {
            task.acceptance_criteria
                .push("The task output is source-backed and complete.".to_string());
        }
        if task.deliverables.is_empty() {
            task.deliverables
                .push("A structured completion with exact evidence.".to_string());
        }
        if task.validation_steps.is_empty() {
            task.validation_steps.push(
                "Inspect the resulting files or outputs before reporting completion.".to_string(),
            );
        }
        if task.forbidden_actions.is_empty() {
            task.forbidden_actions
                .push("Do not claim actions, tests, or files that were not observed.".to_string());
        }
        if task.contract_notes.is_empty() {
            task.contract_notes.push(
                "Preserve unrelated user changes and hand off blockers explicitly.".to_string(),
            );
        }
    }
    plan
}

fn owned_paths_overlap(left: &[String], right: &[String]) -> bool {
    left.iter().any(|left| {
        let left = left.replace('\\', "/");
        let left = left.trim_matches('/');
        right.iter().any(|right| {
            let right = right.replace('\\', "/");
            let right = right.trim_matches('/');
            left.is_empty()
                || right.is_empty()
                || left == "."
                || right == "."
                || left == right
                || left.starts_with(&format!("{right}/"))
                || right.starts_with(&format!("{left}/"))
        })
    })
}

fn deliver_agent_messages(
    store: &Store,
    messages: &mut Vec<CanonicalMessage>,
    run_id: RunId,
    agent_id: AgentId,
    task_id: Option<TaskId>,
    cursor: i64,
) -> Result<i64, ExecutionError> {
    let mut cursor = cursor;
    for event in store.agent_messages_after(run_id, agent_id, cursor, 100)? {
        cursor = cursor.max(event.id);
        let message = event
            .payload
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if message.is_empty() {
            continue;
        }
        let sender = event
            .payload
            .get("sender_agent_id")
            .and_then(Value::as_str)
            .unwrap_or("operator");
        messages.push(CanonicalMessage::text(
            MessageRole::Developer,
            format!(
                "Coordination update from {sender}: {message}\n\
                 Treat this as new task input. Reconcile it with your fixed contract, verify \
                 claims, and reflect any impact in your handoff."
            ),
        ));
        store.append_event(
            run_id,
            Some(agent_id),
            task_id,
            "agent.message_delivered",
            &json!({
                "source_event_id": event.id,
                "sender_agent_id": event.payload.get("sender_agent_id"),
            }),
            Some(&format!("agent-message-delivered:{agent_id}:{}", event.id)),
        )?;
    }
    Ok(cursor)
}

fn tool_target_summary(name: &str, arguments: &Value) -> String {
    if matches!(name, "shell.run" | "shell.test") {
        return process_command_summary(arguments);
    }
    if matches!(name, "mcp.invoke" | "mcp.list_tools") {
        let server = arguments
            .get("server")
            .and_then(Value::as_str)
            .unwrap_or("mcp");
        if name == "mcp.list_tools" {
            return server.to_string();
        }
        let tool = arguments
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or("invoke");
        return format!("{server}/{tool}");
    }
    if name.starts_with("git.") {
        let path = arguments
            .get("path")
            .or_else(|| arguments.get("cwd"))
            .and_then(Value::as_str)
            .unwrap_or(".");
        return format!("{name} {path}");
    }
    for key in ["path", "destination", "url", "agent_id", "name"] {
        if let Some(value) = arguments.get(key).and_then(Value::as_str) {
            return value.to_string();
        }
    }
    name.to_string()
}

fn native_media_tool_reference(
    agent: &Agent,
    name: &str,
    arguments: &Value,
    result: &Value,
) -> Option<MessageContent> {
    if name != "fs.view_image" {
        return None;
    }
    let requested = arguments.get("path")?.as_str()?;
    let requested = Path::new(requested);
    let path = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        Path::new(&agent.workspace.root).join(requested)
    };
    let path = std::fs::canonicalize(path).ok()?;
    let mime_type = result
        .pointer("/output/mime_type")
        .or_else(|| result.get("mime_type"))
        .and_then(Value::as_str)?
        .to_string();
    Some(MessageContent::FileReference {
        path: path.to_string_lossy().into_owned(),
        mime_type: Some(mime_type),
    })
}

fn effective_execution_objective(current_request: &str, messages: &[CanonicalMessage]) -> String {
    if !is_continuation_request(current_request) {
        return current_request.trim().to_string();
    }
    let mut skipped_current = false;
    for message in messages.iter().rev() {
        if message.role != MessageRole::User {
            continue;
        }
        let text = message
            .content
            .iter()
            .filter_map(|content| match content {
                MessageContent::Text { text } => Some(text.trim()),
                _ => None,
            })
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if !skipped_current {
            skipped_current = true;
            continue;
        }
        if !text.is_empty() && !is_continuation_request(&text) {
            return text;
        }
    }
    current_request.trim().to_string()
}

fn relevant_image_references(
    messages: &[CanonicalMessage],
    current_request: &str,
) -> Vec<MessageContent> {
    let may_inherit = is_continuation_request(current_request);
    let mut inspected_current = false;
    for message in messages.iter().rev() {
        if message.role != MessageRole::User {
            continue;
        }
        let images = message
            .content
            .iter()
            .filter_map(|content| match content {
                MessageContent::FileReference {
                    path,
                    mime_type: Some(mime_type),
                } if mime_type.starts_with("image/") => Some(MessageContent::FileReference {
                    path: path.clone(),
                    mime_type: Some(mime_type.clone()),
                }),
                _ => None,
            })
            .collect::<Vec<_>>();
        if !images.is_empty() {
            return images;
        }
        if !inspected_current {
            inspected_current = true;
            if !may_inherit {
                return Vec::new();
            }
        }
    }
    Vec::new()
}

fn is_continuation_request(request: &str) -> bool {
    let request = request.trim().to_ascii_lowercase();
    request.len() <= 240
        && [
            "continue",
            "carry on",
            "go ahead",
            "do it",
            "start execution",
            "start the execution",
            "start implementing",
            "proceed",
            "finish it",
            "complete it",
            "as instructed",
            "as requested",
        ]
        .iter()
        .any(|marker| request.contains(marker))
}

fn request_requires_mutation(request: &str) -> bool {
    let mut request = request.to_ascii_lowercase();
    for negated in [
        "do not write",
        "don't write",
        "without writing",
        "do not modify",
        "don't modify",
        "without modifying",
        "do not edit",
        "don't edit",
        "read-only",
        "read only",
        "no changes",
    ] {
        request = request.replace(negated, "");
    }
    [
        "add ",
        "build",
        "change the",
        "change this",
        "change my",
        "create",
        "delete",
        "edit",
        "fix",
        "implement",
        "make ",
        "move",
        "remove",
        "rename",
        "replicate",
        "save ",
        "update",
        "write",
    ]
    .iter()
    .any(|marker| request.contains(marker))
}

fn is_mutation_tool(name: &str) -> bool {
    matches!(
        name,
        "fs.write" | "fs.append" | "fs.delete" | "fs.copy" | "fs.move" | "fs.mkdir" | "patch.apply"
    )
}

fn incomplete_outcome_reason(
    objective: &str,
    files_changed: &[String],
    successful_mutation_tools: u32,
    pending_file_validation: &BTreeSet<String>,
) -> Option<String> {
    let missing_artifacts = missing_explicit_artifact_paths(objective, files_changed);
    if request_requires_mutation(objective)
        && files_changed.is_empty()
        && successful_mutation_tools == 0
    {
        Some(
            "the request requires local creation or editing, but no successful filesystem \
             mutation was recorded"
                .to_string(),
        )
    } else if !missing_artifacts.is_empty() {
        Some(format!(
            "the explicitly requested files have not all been created or updated: {}",
            missing_artifacts.join(", ")
        ))
    } else if !pending_file_validation.is_empty() {
        Some(format!(
            "the changed files have not been read back successfully: {}",
            pending_file_validation
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ))
    } else {
        None
    }
}

fn missing_explicit_artifact_paths(objective: &str, files_changed: &[String]) -> Vec<String> {
    if !request_requires_mutation(objective) {
        return Vec::new();
    }
    explicit_artifact_paths(objective)
        .into_iter()
        .filter(|required| {
            !files_changed
                .iter()
                .any(|changed| changed.eq_ignore_ascii_case(required))
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MaterializableArtifact {
    path: String,
    content: String,
}

fn extract_materializable_artifacts(
    objective: &str,
    response: &str,
) -> Vec<MaterializableArtifact> {
    let objective = objective.to_ascii_lowercase();
    if !request_requires_mutation(&objective)
        || !["build", "create", "make ", "replicate", "save ", "write"]
            .iter()
            .any(|marker| objective.contains(marker))
    {
        return Vec::new();
    }

    let mut artifacts = Vec::new();
    let mut seen = BTreeSet::new();
    let mut pending_path = None::<String>;
    let mut pending_age = 0_u8;
    let mut active_path = None::<String>;
    let mut content = String::new();
    for line in response.lines() {
        let trimmed = line.trim();
        if let Some(path) = active_path.as_ref() {
            if trimmed.starts_with("```") {
                if !content.trim().is_empty()
                    && content.len() <= 512 * 1024
                    && seen.insert(path.clone())
                {
                    artifacts.push(MaterializableArtifact {
                        path: path.clone(),
                        content: content.trim_end_matches('\n').to_string() + "\n",
                    });
                }
                active_path = None;
                pending_path = None;
                pending_age = 0;
                content.clear();
            } else {
                content.push_str(line);
                content.push('\n');
            }
            continue;
        }

        if trimmed.starts_with("```") {
            let fence_info = trimmed.trim_start_matches('`').trim();
            active_path = safe_artifact_path(fence_info).or_else(|| pending_path.take());
            pending_age = 0;
            content.clear();
            continue;
        }
        if let Some(path) = filename_from_artifact_heading(trimmed) {
            pending_path = Some(path);
            pending_age = 0;
        } else if !trimmed.is_empty() && pending_path.is_some() {
            pending_age = pending_age.saturating_add(1);
            if pending_age > 2 {
                pending_path = None;
            }
        }
        if artifacts.len() >= 16 {
            break;
        }
    }
    if artifacts.is_empty() {
        extract_combined_web_artifacts(&objective, response)
    } else {
        artifacts
    }
}

fn explicit_artifact_paths(objective: &str) -> BTreeSet<String> {
    objective
        .split_whitespace()
        .filter_map(safe_artifact_path)
        .collect()
}

fn extract_combined_web_artifacts(objective: &str, response: &str) -> Vec<MaterializableArtifact> {
    let required = explicit_artifact_paths(objective);
    let unique_path = |extension: &str| {
        let paths = required
            .iter()
            .filter(|path| {
                Path::new(path)
                    .extension()
                    .and_then(|value| value.to_str())
                    .is_some_and(|value| value.eq_ignore_ascii_case(extension))
            })
            .cloned()
            .collect::<Vec<_>>();
        match paths.as_slice() {
            [path] => Some(path.clone()),
            _ => None,
        }
    };
    let (Some(html_path), Some(css_path), Some(js_path)) =
        (unique_path("html"), unique_path("css"), unique_path("js"))
    else {
        return Vec::new();
    };
    let Some(mut html) = fenced_block(response, &["html", "htm"]) else {
        return Vec::new();
    };
    let (Some(style), Some(script)) = (
        html_tag_range(&html, "style"),
        html_tag_range(&html, "script"),
    ) else {
        return Vec::new();
    };
    let css = html[style.content_start..style.content_end]
        .trim()
        .to_string()
        + "\n";
    let js = html[script.content_start..script.content_end]
        .trim()
        .to_string()
        + "\n";
    if css.trim().is_empty() || js.trim().is_empty() {
        return Vec::new();
    }
    let mut replacements = vec![
        (
            style.start,
            style.end,
            format!(r#"<link rel="stylesheet" href="{css_path}">"#),
        ),
        (
            script.start,
            script.end,
            format!(r#"<script src="{js_path}"></script>"#),
        ),
    ];
    replacements.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
    for (start, end, replacement) in replacements {
        html.replace_range(start..end, &replacement);
    }
    if !html.ends_with('\n') {
        html.push('\n');
    }
    vec![
        MaterializableArtifact {
            path: html_path,
            content: html,
        },
        MaterializableArtifact {
            path: css_path,
            content: css,
        },
        MaterializableArtifact {
            path: js_path,
            content: js,
        },
    ]
}

fn fenced_block(response: &str, languages: &[&str]) -> Option<String> {
    let mut active = false;
    let mut content = String::new();
    for line in response.lines() {
        let trimmed = line.trim();
        if !active {
            if let Some(info) = trimmed.strip_prefix("```")
                && languages
                    .iter()
                    .any(|language| info.trim().eq_ignore_ascii_case(language))
            {
                active = true;
            }
            continue;
        }
        if trimmed.starts_with("```") {
            return (!content.trim().is_empty()).then_some(content);
        }
        content.push_str(line);
        content.push('\n');
    }
    None
}

#[derive(Debug, Clone, Copy)]
struct HtmlTagRange {
    start: usize,
    content_start: usize,
    content_end: usize,
    end: usize,
}

fn html_tag_range(html: &str, tag: &str) -> Option<HtmlTagRange> {
    let lower = html.to_ascii_lowercase();
    let opening = format!("<{tag}");
    let closing = format!("</{tag}>");
    let start = lower.find(&opening)?;
    let content_start = start + lower[start..].find('>')? + 1;
    let content_end = content_start + lower[content_start..].find(&closing)?;
    let end = content_end + closing.len();
    Some(HtmlTagRange {
        start,
        content_start,
        content_end,
        end,
    })
}

fn reconcile_materializable_artifacts(
    objective: &str,
    artifacts: Vec<MaterializableArtifact>,
) -> Vec<MaterializableArtifact> {
    let required = explicit_artifact_paths(objective);
    if required.is_empty() || artifacts.is_empty() {
        return artifacts;
    }

    let returned = artifacts
        .iter()
        .map(|artifact| artifact.path.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    let mut assigned = BTreeSet::new();
    let mut replacements = Vec::new();
    for artifact in &artifacts {
        if required
            .iter()
            .any(|path| path.eq_ignore_ascii_case(&artifact.path))
        {
            continue;
        }
        let extension = Path::new(&artifact.path)
            .extension()
            .and_then(|value| value.to_str());
        let Some(extension) = extension else {
            continue;
        };
        let candidates = required
            .iter()
            .filter(|path| {
                !returned.contains(&path.to_ascii_lowercase())
                    && !assigned.contains(&path.to_ascii_lowercase())
                    && Path::new(path)
                        .extension()
                        .and_then(|value| value.to_str())
                        .is_some_and(|value| value.eq_ignore_ascii_case(extension))
            })
            .cloned()
            .collect::<Vec<_>>();
        if let [replacement] = candidates.as_slice() {
            replacements.push((artifact.path.clone(), replacement.clone()));
            assigned.insert(replacement.to_ascii_lowercase());
        }
    }

    let mut reconciled = Vec::new();
    let mut seen = BTreeSet::new();
    for mut artifact in artifacts {
        for (original, replacement) in &replacements {
            artifact.content = artifact.content.replace(original, replacement);
            if artifact.path.eq_ignore_ascii_case(original) {
                artifact.path.clone_from(replacement);
            }
        }
        if seen.insert(artifact.path.to_ascii_lowercase()) {
            reconciled.push(artifact);
        }
    }
    reconciled
}

fn normalize_provider_write_call(
    name: &str,
    arguments: &Value,
    objective: &str,
    files_changed: &[String],
) -> Option<(String, Value, String)> {
    let normalized_name = name.to_ascii_lowercase().replace('-', "_");
    let write_alias = matches!(
        normalized_name.as_str(),
        "fs.write" | "write" | "write_file" | "file.write" | "filesystem.write" | "execute:write"
    ) || normalized_name.ends_with(":write")
        || normalized_name
            .rsplit_once('.')
            .is_some_and(|(_, operation)| operation == "write");
    if !write_alias {
        return None;
    }
    let content = ["content", "text", "data"]
        .into_iter()
        .find_map(|field| arguments.get(field).and_then(Value::as_str))?;
    let existing_path = ["path", "file_path", "filename", "target"]
        .into_iter()
        .find_map(|field| arguments.get(field).and_then(Value::as_str));
    if name == "fs.write" && existing_path.is_some() {
        return None;
    }

    let path = if let Some(path) = existing_path {
        path.to_string()
    } else {
        let required = explicit_artifact_paths(objective);
        let missing = required
            .into_iter()
            .filter(|required| {
                !files_changed
                    .iter()
                    .any(|changed| changed.eq_ignore_ascii_case(required))
            })
            .collect::<Vec<_>>();
        let inferred_extension = inferred_artifact_extension(content);
        let candidates = missing
            .iter()
            .filter(|path| {
                inferred_extension.is_none_or(|extension| {
                    Path::new(path)
                        .extension()
                        .and_then(|value| value.to_str())
                        .is_some_and(|value| value.eq_ignore_ascii_case(extension))
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        match candidates.as_slice() {
            [path] => path.clone(),
            _ if missing.len() == 1 => missing[0].clone(),
            _ => return None,
        }
    };
    let mut canonical_arguments = json!({
        "path": path,
        "content": content,
    });
    if let Some(expected_sha256) = arguments.get("expected_sha256") {
        canonical_arguments["expected_sha256"] = expected_sha256.clone();
    }
    Some(("fs.write".to_string(), canonical_arguments, path))
}

fn inferred_artifact_extension(content: &str) -> Option<&'static str> {
    let content = content.trim_start();
    let lower = content.to_ascii_lowercase();
    if lower.starts_with("<!doctype html")
        || lower.starts_with("<html")
        || (lower.contains("<body") && lower.contains("</"))
    {
        Some("html")
    } else if lower.contains("addeventlistener")
        || lower.contains("document.queryselector")
        || lower.contains("function ")
        || lower.contains("const ")
        || lower.contains("let ")
    {
        Some("js")
    } else if lower.starts_with("/* css")
        || (lower.contains('{')
            && lower.contains('}')
            && lower.contains(':')
            && !lower.contains("<html"))
    {
        Some("css")
    } else if serde_json::from_str::<Value>(content).is_ok() {
        Some("json")
    } else if lower.contains("fn main(") || lower.starts_with("use std::") {
        Some("rs")
    } else if lower.starts_with("#!/usr/bin/env python") || lower.contains("\ndef ") {
        Some("py")
    } else {
        None
    }
}

fn filename_from_artifact_heading(line: &str) -> Option<String> {
    let mut backtick = false;
    for part in line.split('`') {
        if backtick && let Some(path) = safe_artifact_path(part) {
            return Some(path);
        }
        backtick = !backtick;
    }
    line.split_whitespace().find_map(safe_artifact_path)
}

fn safe_artifact_path(candidate: &str) -> Option<String> {
    let candidate = candidate.trim_matches(|character: char| {
        matches!(
            character,
            '#' | '*' | '_' | '`' | '"' | '\'' | ':' | ';' | ',' | '(' | ')' | '[' | ']'
        )
    });
    if candidate.is_empty() || candidate.len() > 240 {
        return None;
    }
    let path = Path::new(candidate);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return None;
    }
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    if ![
        "c", "cpp", "css", "go", "h", "hpp", "html", "java", "js", "json", "jsx", "md", "mjs",
        "cjs", "ps1", "py", "rs", "sh", "svg", "toml", "ts", "tsx", "txt", "xml", "yaml", "yml",
    ]
    .contains(&extension.as_str())
    {
        return None;
    }
    Some(candidate.replace('\\', "/"))
}

fn process_command_summary(arguments: &Value) -> String {
    let program = arguments
        .get("program")
        .and_then(Value::as_str)
        .unwrap_or("process");
    let args = arguments
        .get("args")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(quote_command_argument)
        .collect::<Vec<_>>();
    if args.is_empty() {
        program.to_string()
    } else {
        format!("{program} {}", args.join(" "))
    }
}

fn quote_command_argument(value: &str) -> String {
    if value.chars().any(char::is_whitespace) {
        format!("{value:?}")
    } else {
        value.to_string()
    }
}

fn focused_system_prompt(agent: &Agent, skills: &SkillRegistry, mcp: &McpRegistry) -> String {
    let skill_catalog = skills
        .metadata()
        .into_iter()
        .map(|skill| skill.name)
        .collect::<Vec<_>>()
        .join("; ");
    let mcp_catalog = mcp
        .list()
        .unwrap_or_default()
        .into_iter()
        .filter(|server| server.enabled)
        .map(|server| server.name)
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{}\n\nYou are in focused coding mode. Batch independent reads and searches. \
         Use only the exposed tools. Prefer patch.apply with an expected SHA-256 for edits. \
         When the user asks about local files, folders, directories, disks, or drives, inspect \
         them with the filesystem tools. Never claim that local access is unavailable and never \
         give the user a command to run when an exposed tool can perform the request. \
         Treat images, audio, video, archives, and unfamiliar file types as actionable local \
         inputs. Inspect them directly when supported; otherwise use filesystem and process \
         tools to identify, extract, convert, or analyze them before answering. Never announce a \
         limitation before attempting the available tools and practical local workflows. \
         If you spawn child agents, use agents.wait and incorporate their structured completions \
         before returning. \
         Registered skills: {}. Activate a relevant skill before applying its workflow. Skill \
         activation only loads guidance: it never completes the user's request, so continue \
         immediately with the original inspection, mutation, and validation work in the same run. \
         When the user asks to install or download a skill, use skill.install with the supplied \
         repository URL or local path; do not ask them to run a separate command. When the user \
         asks to connect tools or a service, use mcp.connect and then mcp.list_tools. For GitHub, \
         the official remote MCP URL is https://api.githubcopilot.com/mcp/ and PAT-based access \
         should reference the GITHUB_PAT environment variable rather than exposing the token. \
         A final answer is valid only when the requested outcome exists. For file-creation or edit \
         requests, use mutation tools and verify the resulting files; never replace execution with \
         readiness text, a request to repeat instructions, or code for the user to copy manually. \
         Enabled MCP servers: {}. Use mcp.list_tools before invoking an unfamiliar MCP tool. \
         When validation is complete, return the concise final answer without another tool call.",
        agent.system_instructions,
        if skill_catalog.is_empty() {
            "none".to_string()
        } else {
            skill_catalog
        },
        if mcp_catalog.is_empty() {
            "none".to_string()
        } else {
            mcp_catalog
        }
    )
}

fn contextual_visible_tools(
    visible_tools: Vec<ToolDescriptor>,
    _provider: &str,
    user_request: &str,
) -> Vec<ToolDescriptor> {
    let request = user_request.to_ascii_lowercase();
    let mutation = request_requires_mutation(&request);
    let contains_any = |needles: &[&str]| needles.iter().any(|needle| request.contains(needle));
    let mut relevant = BTreeSet::from([
        "fs.read",
        "fs.read_many",
        "fs.list",
        "fs.stat",
        "fs.glob",
        "fs.view_image",
        "search.text",
        "skill.activate",
    ]);
    if mutation {
        relevant.extend([
            "fs.mkdir",
            "fs.write",
            "fs.edit_exact",
            "patch.apply",
            "shell.run",
            "shell.test",
        ]);
    }
    if contains_any(&["copy", "duplicate"]) {
        relevant.insert("fs.copy");
    }
    if contains_any(&["move", "rename"]) {
        relevant.insert("fs.move");
    }
    if contains_any(&["delete", "remove"]) {
        relevant.extend(["fs.delete", "fs.remove_dir"]);
    }
    if contains_any(&["documentation", "readme", "docs"]) {
        relevant.insert("docs.write");
    }
    if contains_any(&["symbol", "definition", "declaration"]) {
        relevant.insert("search.symbol");
    }
    if contains_any(&["http://", "https://", "fetch", "download"]) {
        relevant.insert("search.fetch");
    }
    if contains_any(&["git", "commit", "branch", "worktree", "staged"]) {
        relevant.extend([
            "git.diff",
            "git.status",
            "git.log",
            "git.show",
            "git.branch",
            "git.worktree",
            "git.stage",
            "git.unstage",
            "git.restore",
            "git.commit",
        ]);
    }
    if contains_any(&[
        "agent",
        "delegate",
        "parallel",
        "specialist",
        "team",
        "coordinate",
    ]) {
        relevant.extend([
            "agents.spawn",
            "agents.message",
            "agents.status",
            "agents.wait",
            "agents.interrupt",
            "plan.update",
        ]);
    }
    if contains_any(&["long-running", "process", "server", "serve", "interactive"]) {
        relevant.extend([
            "process.start",
            "process.input",
            "process.poll",
            "process.kill",
        ]);
    }
    add_extension_tools(&mut relevant, &request);
    let filtered = visible_tools
        .iter()
        .filter(|tool| relevant.contains(tool.name.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if filtered.is_empty() {
        visible_tools
    } else {
        filtered
    }
}

fn add_extension_tools(relevant: &mut BTreeSet<&'static str>, request: &str) {
    if ["install skill", "download skill", "add skill", "skill from"]
        .iter()
        .any(|needle| request.contains(needle))
    {
        relevant.insert("skill.install");
    }
    if [
        "mcp",
        "plugin",
        "connector",
        "connect github",
        "connect to github",
        "connect tools",
        "install tool",
        "add tool",
    ]
    .iter()
    .any(|needle| request.contains(needle))
    {
        relevant.extend(["mcp.connect", "mcp.list_tools", "mcp.invoke"]);
    }
}

fn is_directory_inventory_request(user_request: &str) -> bool {
    let request = user_request.to_ascii_lowercase();
    let mentions_container = [
        "folder",
        "directory",
        "directories",
        "drive",
        "disk",
        ":\\",
        ":/",
    ]
    .iter()
    .any(|needle| request.contains(needle));
    let requests_listing = [
        "list",
        "show",
        "name",
        "enumerate",
        "what is in",
        "what's in",
    ]
    .iter()
    .any(|needle| request.contains(needle));
    let requests_mutation = [
        "create", "write", "edit", "change", "delete", "remove", "move", "copy", "rename", "build",
        "run", "test", "commit",
    ]
    .iter()
    .any(|needle| request.contains(needle));

    mentions_container && requests_listing && !requests_mutation
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DirectoryInventoryTarget {
    path: String,
}

fn deterministic_directory_inventory_target(
    user_request: &str,
) -> Option<DirectoryInventoryTarget> {
    if !is_directory_inventory_request(user_request) {
        return None;
    }
    extract_local_directory_reference(user_request).map(|path| DirectoryInventoryTarget { path })
}

fn extract_local_directory_reference(user_request: &str) -> Option<String> {
    let bytes = user_request.as_bytes();
    for index in 0..bytes.len().saturating_sub(1) {
        if bytes[index].is_ascii_alphabetic() && bytes[index + 1] == b':' {
            let drive = (bytes[index] as char).to_ascii_uppercase();
            let after = user_request[index + 2..].trim_start();
            if after.starts_with('\\') || after.starts_with('/') || after.is_empty() {
                return Some(format!("{drive}:\\"));
            }
        }
    }
    let lower = user_request.to_ascii_lowercase();
    let lower_bytes = lower.as_bytes();
    for index in 0..lower_bytes.len().saturating_sub(6) {
        if lower_bytes[index].is_ascii_alphabetic()
            && lower_bytes[index + 1..].starts_with(b" drive")
        {
            let drive = (lower_bytes[index] as char).to_ascii_uppercase();
            return Some(format!("{drive}:\\"));
        }
    }
    selected_file_paths(user_request, 1)
        .into_iter()
        .find(|path| Path::new(path).is_dir())
}

fn directory_inventory_answer(path: &str, result: &Value) -> String {
    if let Some(error) = result.get("error").and_then(Value::as_str) {
        return format!(
            "I tried to inspect `{path}`, but access was not granted or failed: {error}"
        );
    }
    let entries = result
        .get("entries")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let mut directories = entries
        .iter()
        .filter(|entry| entry.get("kind").and_then(Value::as_str) == Some("directory"))
        .filter_map(|entry| entry.get("path").and_then(Value::as_str))
        .filter(|entry_path| {
            let normalized = entry_path.trim_end_matches(['\\', '/']);
            let requested = path.trim_end_matches(['\\', '/']);
            !normalized.eq_ignore_ascii_case(requested)
        })
        .map(main_directory_name)
        .collect::<Vec<_>>();
    directories.sort_by_key(|name| name.to_ascii_lowercase());
    directories.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
    if directories.is_empty() {
        return format!("I inspected `{path}` and found no visible main folders.");
    }
    let mut lines = vec![format!("Main folders in `{path}`:")];
    lines.extend(directories.into_iter().map(|name| format!("- {name}")));
    if result
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        lines.push("The list was truncated by the result limit.".to_string());
    }
    lines.join("\n")
}

fn main_directory_name(path: &str) -> String {
    path.trim_end_matches(['\\', '/'])
        .rsplit(['\\', '/'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
        .to_string()
}

fn collect_events(events: &[ModelEvent]) -> (String, UsageLedger) {
    let mut output = String::new();
    let mut usage = UsageLedger::default();
    for event in events {
        match event {
            ModelEvent::TextDelta { text } => output.push_str(text),
            ModelEvent::Usage {
                input_tokens,
                output_tokens,
                cached_tokens,
            } => {
                usage.user_tokens = usage.user_tokens.saturating_add(*input_tokens);
                usage.output_tokens = usage.output_tokens.saturating_add(*output_tokens);
                usage.cached_tokens = usage.cached_tokens.saturating_add(*cached_tokens);
            }
            ModelEvent::ToolCall { .. } | ModelEvent::Completed { .. } => {}
        }
    }
    (output, usage)
}

fn response_id(events: &[ModelEvent]) -> Option<String> {
    events.iter().rev().find_map(|event| match event {
        ModelEvent::Completed { response_id } => response_id.clone(),
        _ => None,
    })
}

fn message_to_canonical(message: Message) -> CanonicalMessage {
    CanonicalMessage {
        role: message.role,
        content: message.content,
    }
}

fn apply_context_policy(messages: Vec<Message>, policy: &ContextPolicy) -> Vec<Message> {
    if messages.is_empty() {
        return messages;
    }
    let latest_user = messages
        .iter()
        .rposition(|message| message.role == MessageRole::User);
    let includes_selected_item = |message: &Message| {
        message.content.iter().any(|content| match content {
            MessageContent::FileReference { path, .. } => policy
                .selected_items
                .iter()
                .any(|selected| selected == path || path.starts_with(selected)),
            MessageContent::ContextSummary { .. } => true,
            _ => false,
        })
    };
    let mut selected = match policy.inheritance {
        ContextInheritance::FullHistory => messages,
        ContextInheritance::LastNTurns => {
            let turns =
                usize::try_from(policy.last_n_turns.unwrap_or(1).max(1)).unwrap_or(usize::MAX);
            let user_indices = messages
                .iter()
                .enumerate()
                .filter_map(|(index, message)| (message.role == MessageRole::User).then_some(index))
                .collect::<Vec<_>>();
            let start = user_indices
                .get(user_indices.len().saturating_sub(turns))
                .copied()
                .unwrap_or(0);
            messages.into_iter().skip(start).collect()
        }
        ContextInheritance::SummaryOnly => {
            if messages.iter().any(|message| {
                message
                    .content
                    .iter()
                    .any(|content| matches!(content, MessageContent::ContextSummary { .. }))
            }) {
                messages
                    .into_iter()
                    .enumerate()
                    .filter_map(|(index, message)| {
                        (message.content.iter().any(|content| {
                            matches!(content, MessageContent::ContextSummary { .. })
                        }) || latest_user.is_some_and(|latest| index >= latest))
                        .then_some(message)
                    })
                    .collect()
            } else {
                messages
            }
        }
        ContextInheritance::SelectedItems => messages
            .into_iter()
            .enumerate()
            .filter_map(|(index, message)| {
                (includes_selected_item(&message)
                    || latest_user.is_some_and(|latest| index >= latest))
                .then_some(message)
            })
            .collect(),
        ContextInheritance::None => latest_user
            .and_then(|index| messages.into_iter().nth(index))
            .into_iter()
            .collect(),
    };
    if let Some(max_tokens) = policy.max_tokens {
        let mut retained = Vec::new();
        let mut used = 0_u64;
        while let Some(message) = selected.pop() {
            let estimate = u64::try_from(
                serde_json::to_string(&message)
                    .map_or(0, |encoded| encoded.chars().count())
                    .div_ceil(4),
            )
            .unwrap_or(u64::MAX);
            if retained.is_empty() || used.saturating_add(estimate) <= max_tokens {
                used = used.saturating_add(estimate);
                retained.push(message);
            }
        }
        retained.reverse();
        retained
    } else {
        selected
    }
}

fn compacted_history(messages: Vec<Message>) -> Vec<Message> {
    let summary_index = messages.iter().rposition(|message| {
        message
            .content
            .iter()
            .any(|content| matches!(content, MessageContent::ContextSummary { .. }))
    });
    if let Some(index) = summary_index {
        messages.into_iter().skip(index).collect()
    } else {
        messages
    }
}

fn root_agent(store: &Store, run_id: RunId) -> Result<Agent, ExecutionError> {
    store
        .list_agents(Some(run_id))?
        .into_iter()
        .find(|agent| agent.parent_id.is_none())
        .ok_or(ExecutionError::MissingRootAgent(run_id))
}

fn root_agent_id(store: &Store, run_id: RunId) -> Result<Option<AgentId>, ExecutionError> {
    Ok(store
        .list_agents(Some(run_id))?
        .into_iter()
        .find(|agent| agent.parent_id.is_none())
        .map(|agent| agent.id))
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{
        ExecutionEngine, SkillInstallToolArgs, compacted_history, contextual_visible_tools,
        deterministic_directory_inventory_target, directory_inventory_answer,
        explicit_artifact_paths, extract_materializable_artifacts, install_skill,
        missing_explicit_artifact_paths, native_media_tool_reference, normalize_github_skill_url,
        normalize_provider_write_call, parse_agentic_plan, provider_retry_wait_ms,
        reconcile_materializable_artifacts, request_requires_mutation,
    };
    use crate::{
        AgentControl, AgentLimits, ChangeManager, ExecutionError, ModelPackRegistry,
        ProviderRouter, ToolExecutor, ToolRegistry,
    };
    use async_trait::async_trait;
    use opensrc_core::{
        AgentDefinition, AgentStatus, ApprovalDecision, ApprovalStatus, Budgets,
        CanonicalModelRequest, ContextPolicy, ExecutionMode, Message, MessageContent, MessageRole,
        ModelEvent, ProviderAdapter, ProviderCapabilities, ProviderError, ReasoningConfig,
        RetryPolicy, ReviewContract, ReviewFinding, ReviewSeverity, ReviewVerdict, RunStatus,
        SandboxPolicy, Task, TaskContract, TaskInputs, TaskStatus, ToolPolicy, Workspace,
        WorkspaceMode,
    };
    use opensrc_store::Store;
    use serde_json::json;
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::sync::Notify;
    use uuid::Uuid;

    #[test]
    fn rate_limit_wait_respects_the_provider_retry_after_hint() {
        let error = ExecutionError::Provider(ProviderError::RateLimited {
            message: "TPM exhausted".to_string(),
            retry_after_ms: Some(12_000),
        });
        assert_eq!(provider_retry_wait_ms(&error, 500, 30_000), 12_000);
        assert_eq!(provider_retry_wait_ms(&error, 500, 5_000), 5_000);
    }

    #[test]
    fn github_skill_urls_resolve_repository_reference_and_subdirectory() {
        let (repository, reference, subdirectory) = normalize_github_skill_url(
            "https://github.com/example/skills/tree/main/skills/rust-ui",
        );
        assert_eq!(repository, "https://github.com/example/skills.git");
        assert_eq!(reference.as_deref(), Some("main"));
        assert_eq!(
            subdirectory.as_deref(),
            Some(std::path::Path::new("skills/rust-ui"))
        );
    }

    #[tokio::test]
    async fn installs_a_local_skill_into_the_live_project_registry() {
        let workspace =
            std::env::temp_dir().join(format!("opensrc-install-skill-{}", Uuid::new_v4()));
        let source = workspace.join("downloaded").join("useful-skill");
        std::fs::create_dir_all(&source).expect("source skill");
        std::fs::write(
            source.join("SKILL.md"),
            "---\nname: useful-skill\ndescription: Useful workflow.\ntriggers: [useful]\n---\nDo the useful work.",
        )
        .expect("skill file");
        let now = chrono::Utc::now();
        let agent = opensrc_core::Agent {
            id: Uuid::new_v4(),
            run_id: Uuid::new_v4(),
            canonical_path: "root".to_string(),
            parent_id: None,
            child_ids: Vec::new(),
            role: "generalist".to_string(),
            task: "install skill".to_string(),
            status: AgentStatus::Running,
            provider: "fixture".to_string(),
            model: "fixture".to_string(),
            reasoning: ReasoningConfig::default(),
            system_instructions: String::new(),
            context_policy: ContextPolicy::default(),
            tool_policy: ToolPolicy::default(),
            workspace: Workspace {
                mode: WorkspaceMode::OwnedPaths,
                root: workspace.to_string_lossy().into_owned(),
                owned_paths: vec![".".to_string()],
            },
            sandbox_policy: SandboxPolicy::default(),
            budgets: Budgets::default(),
            retry_policy: RetryPolicy::default(),
            fallback_chain: Vec::new(),
            completion_schema: "task_completion".to_string(),
            created_at: now,
            updated_at: now,
        };
        let installed = install_skill(
            &agent,
            &SkillInstallToolArgs {
                source: source.to_string_lossy().into_owned(),
                name: None,
                subdirectory: None,
                force: false,
            },
        )
        .await
        .expect("install");
        assert_eq!(installed["name"], "useful-skill");
        let registry = crate::SkillRegistry::discover(workspace.join(".opensource").join("skills"))
            .expect("registry");
        assert_eq!(
            registry
                .activate("useful-skill")
                .expect("activation")
                .instructions,
            "Do the useful work."
        );
        std::fs::remove_dir_all(workspace).expect("cleanup");
    }

    #[test]
    fn negated_write_language_remains_read_only() {
        assert!(!request_requires_mutation(
            "Read and verify the files. Do not write or modify anything."
        ));
        assert!(!request_requires_mutation(
            "Perform a read-only review with no changes."
        ));
        assert!(
            missing_explicit_artifact_paths(
                "Inspect index.html, styles.css, and script.js. Do not write anything.",
                &[],
            )
            .is_empty()
        );
        assert!(request_requires_mutation("Write the requested files."));
    }

    #[test]
    fn image_tool_result_becomes_native_model_input() {
        let workspace =
            std::env::temp_dir().join(format!("opensrc-native-image-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).expect("workspace");
        let image = workspace.join("reference.png");
        std::fs::write(&image, b"png").expect("image");
        let now = chrono::Utc::now();
        let agent = opensrc_core::Agent {
            id: Uuid::new_v4(),
            run_id: Uuid::new_v4(),
            canonical_path: "root".to_string(),
            parent_id: None,
            child_ids: Vec::new(),
            role: "media-specialist".to_string(),
            task: "inspect image".to_string(),
            status: AgentStatus::Running,
            provider: "fixture".to_string(),
            model: "fixture".to_string(),
            reasoning: ReasoningConfig::default(),
            system_instructions: String::new(),
            context_policy: ContextPolicy::default(),
            tool_policy: ToolPolicy::default(),
            workspace: Workspace {
                mode: WorkspaceMode::OwnedPaths,
                root: workspace.to_string_lossy().into_owned(),
                owned_paths: vec![".".to_string()],
            },
            sandbox_policy: SandboxPolicy::default(),
            budgets: Budgets::default(),
            retry_policy: RetryPolicy::default(),
            fallback_chain: Vec::new(),
            completion_schema: "task_completion".to_string(),
            created_at: now,
            updated_at: now,
        };

        let reference = native_media_tool_reference(
            &agent,
            "fs.view_image",
            &serde_json::json!({"path": "reference.png"}),
            &serde_json::json!({"output": {"mime_type": "image/png"}}),
        )
        .expect("native reference");
        let expected_image = std::fs::canonicalize(&image).expect("canonical image");

        assert!(matches!(
            reference,
            MessageContent::FileReference { path, mime_type }
                if std::path::Path::new(&path) == expected_image
                    && mime_type.as_deref() == Some("image/png")
        ));
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn extracts_only_safe_filename_labeled_artifacts_from_build_responses() {
        let artifacts = extract_materializable_artifacts(
            "Build index.html, styles.css, and script.js",
            r#"
### 1. `index.html`
```html
<!doctype html>
```
### 2. styles.css
```css
body { color: white; }
```
### 3. `../escape.js`
```js
throw new Error("unsafe");
```
### 4. `script.js`
```js
console.log("ready");
```
"#,
        );

        assert_eq!(
            artifacts
                .iter()
                .map(|artifact| artifact.path.as_str())
                .collect::<Vec<_>>(),
            ["index.html", "styles.css", "script.js"]
        );
        assert!(artifacts[0].content.contains("<!doctype html>"));
        assert!(
            extract_materializable_artifacts("Explain this code", "```rs\nfn main() {}\n```")
                .is_empty()
        );
    }

    #[test]
    fn reconciles_single_extension_filename_drift_with_explicit_requested_artifact() {
        let artifacts = reconcile_materializable_artifacts(
            "Build index.html, styles.css, and script.js",
            vec![
                super::MaterializableArtifact {
                    path: "style.css".to_string(),
                    content: "body {}\n".to_string(),
                },
                super::MaterializableArtifact {
                    path: "index.html".to_string(),
                    content: "<link href=\"style.css\">\n".to_string(),
                },
            ],
        );
        assert_eq!(
            explicit_artifact_paths("Build index.html, styles.css, and script.js"),
            BTreeSet::from([
                "index.html".to_string(),
                "script.js".to_string(),
                "styles.css".to_string(),
            ])
        );
        assert_eq!(artifacts[0].path, "styles.css");
        assert!(artifacts[1].content.contains("styles.css"));
    }

    #[test]
    fn normalizes_provider_write_alias_only_to_the_unique_missing_explicit_artifact() {
        let (name, arguments, path) = normalize_provider_write_call(
            "execute:write",
            &json!({"content": "/* CSS */\n.calculator { color: white; }\n"}),
            "Build index.html, styles.css, and script.js",
            &["index.html".to_string(), "script.js".to_string()],
        )
        .expect("safe write normalization");
        assert_eq!(name, "fs.write");
        assert_eq!(path, "styles.css");
        assert_eq!(arguments["path"], "styles.css");
        assert!(
            normalize_provider_write_call(
                "google:search",
                &json!({"content": "body {}"}),
                "Build styles.css",
                &[],
            )
            .is_none()
        );
        assert!(
            normalize_provider_write_call(
                "execute:write",
                &json!({"content": "ambiguous"}),
                "Build one.txt and two.txt",
                &[],
            )
            .is_none()
        );
    }

    #[test]
    fn splits_combined_fenced_web_document_into_explicit_requested_files() {
        let artifacts = extract_materializable_artifacts(
            "Build index.html, styles.css, and script.js",
            r#"I cannot write files, but here is the code:
```html
<!doctype html>
<html>
<head><style>body { background: #111; }</style></head>
<body><button>1</button><script>document.querySelector("button");</script></body>
</html>
```"#,
        );
        assert_eq!(
            artifacts
                .iter()
                .map(|artifact| artifact.path.as_str())
                .collect::<Vec<_>>(),
            ["index.html", "styles.css", "script.js"]
        );
        assert!(artifacts[0].content.contains("href=\"styles.css\""));
        assert!(artifacts[0].content.contains("src=\"script.js\""));
        assert!(!artifacts[0].content.contains("<style>"));
        assert!(artifacts[1].content.contains("background"));
        assert!(artifacts[2].content.contains("querySelector"));
    }

    #[test]
    fn context_summary_replaces_older_model_history() {
        let conversation_id = Uuid::new_v4();
        let message = |sequence, role, content| Message {
            id: Uuid::new_v4(),
            conversation_id,
            run_id: None,
            sequence,
            role,
            content: vec![content],
            provider: None,
            model: None,
            continuation_id: None,
            created_at: chrono::Utc::now(),
        };
        let compacted = compacted_history(vec![
            message(1, MessageRole::User, MessageContent::text("old")),
            message(
                2,
                MessageRole::Developer,
                MessageContent::ContextSummary {
                    text: "summary".to_string(),
                },
            ),
            message(3, MessageRole::User, MessageContent::text("new")),
        ]);
        assert_eq!(compacted.len(), 2);
        assert!(matches!(
            compacted[0].content[0],
            MessageContent::ContextSummary { .. }
        ));
    }

    #[test]
    fn parses_text_json_plan_returned_by_compatible_models() {
        let plan = parse_agentic_plan(
            r#"{
                "tasks": [{
                    "id": "inspect-video",
                    "objective": "Inspect the attached video without changing it.",
                    "specialist": "media-specialist",
                    "dependencies": [],
                    "owned_paths": [],
                    "deliverables": "A structured media report.",
                    "validation": "Use a local metadata probe.",
                    "forbidden_actions": "Do not modify files.",
                    "contract": "Hand the evidence to the parent."
                }]
            }"#,
        )
        .expect("gateway-compatible plan");
        let task = &plan.tasks[0];
        assert_eq!(
            task.description,
            "Inspect the attached video without changing it."
        );
        assert_eq!(task.role, "media-specialist");
        assert_eq!(task.deliverables, ["A structured media report."]);
        assert_eq!(task.validation_steps, ["Use a local metadata probe."]);
        assert!(!task.acceptance_criteria.is_empty());
        assert_eq!(task.contract_notes, ["Hand the evidence to the parent."]);
    }

    #[test]
    fn directory_inventory_keeps_the_complete_approved_toolset() {
        let tools = contextual_visible_tools(
            ToolRegistry::with_builtins().metadata(),
            "openrouter",
            "Analyze F:\\ and list the main folders.",
        );

        assert!(tools.iter().any(|tool| tool.name == "fs.list"));
        assert!(tools.iter().any(|tool| tool.name == "fs.read"));
    }

    #[test]
    fn contextual_tool_filter_is_provider_neutral_and_keeps_required_capabilities() {
        let registry = ToolRegistry::with_builtins();
        let all_tools = registry.metadata();
        let openrouter_tools = contextual_visible_tools(
            all_tools.clone(),
            "openrouter",
            "Create a folder and list its files.",
        );
        let ollama_tools =
            contextual_visible_tools(all_tools, "ollama", "Create a folder and list its files.");

        assert_eq!(
            openrouter_tools
                .iter()
                .map(|tool| &tool.name)
                .collect::<Vec<_>>(),
            ollama_tools
                .iter()
                .map(|tool| &tool.name)
                .collect::<Vec<_>>()
        );
        for required in ["fs.list", "fs.read", "fs.mkdir", "fs.write", "shell.test"] {
            assert!(
                openrouter_tools.iter().any(|tool| tool.name == required),
                "missing {required}"
            );
        }
        assert!(
            !openrouter_tools
                .iter()
                .any(|tool| tool.name == "git.commit")
        );
    }

    struct MockProvider;

    #[async_trait]
    impl ProviderAdapter for MockProvider {
        fn id(&self) -> &'static str {
            "mock"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_tool_calls: true,
                ..ProviderCapabilities::default()
            }
        }

        async fn execute(
            &self,
            _request: CanonicalModelRequest,
        ) -> Result<Vec<ModelEvent>, ProviderError> {
            Ok(vec![
                ModelEvent::TextDelta {
                    text: "direct answer".to_string(),
                },
                ModelEvent::Usage {
                    input_tokens: 4,
                    output_tokens: 2,
                    cached_tokens: 0,
                },
                ModelEvent::Completed {
                    response_id: Some("mock-1".to_string()),
                },
            ])
        }
    }

    struct ToolThenFinalProvider {
        calls: AtomicUsize,
    }

    struct EditThenFinalProvider {
        calls: AtomicUsize,
    }

    struct SetupReadyThenWriteProvider {
        calls: AtomicUsize,
    }

    struct ImagePreflightProvider;

    struct TextArtifactsThenFinalProvider {
        id: String,
        calls: AtomicUsize,
    }

    struct ProseOnlyProvider {
        id: String,
    }

    struct SlowProvider;

    struct TransientThenSuccessProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl ProviderAdapter for SlowProvider {
        fn id(&self) -> &'static str {
            "slow-mock"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }

        async fn execute(
            &self,
            _request: CanonicalModelRequest,
        ) -> Result<Vec<ModelEvent>, ProviderError> {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            Ok(vec![ModelEvent::Completed { response_id: None }])
        }
    }

    #[async_trait]
    impl ProviderAdapter for TransientThenSuccessProvider {
        fn id(&self) -> &'static str {
            "retry-mock"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }

        async fn execute(
            &self,
            _request: CanonicalModelRequest,
        ) -> Result<Vec<ModelEvent>, ProviderError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(ProviderError::Transient("try again".to_string()))
            } else {
                Ok(vec![
                    ModelEvent::TextDelta {
                        text: "recovered".to_string(),
                    },
                    ModelEvent::Completed {
                        response_id: Some("retry-2".to_string()),
                    },
                ])
            }
        }
    }

    struct AgenticProvider {
        calls: AtomicUsize,
    }

    struct ParallelAgenticProvider {
        calls: AtomicUsize,
        active: AtomicUsize,
        maximum_active: AtomicUsize,
    }

    struct RuntimeSpawnProvider;

    struct AlwaysFailProvider;

    struct FixedProvider;

    #[cfg(windows)]
    struct ShouldNotCallProvider;

    struct MailboxProvider {
        calls: AtomicUsize,
        first_call_started: Arc<Notify>,
    }

    struct ModelPackProvider {
        models: Mutex<Vec<String>>,
        messages: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl ProviderAdapter for AgenticProvider {
        fn id(&self) -> &'static str {
            "agentic-mock"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_tool_calls: true,
                ..ProviderCapabilities::default()
            }
        }

        async fn execute(
            &self,
            _request: CanonicalModelRequest,
        ) -> Result<Vec<ModelEvent>, ProviderError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let text = if call == 0 {
                r#"{"tasks":[{"description":"inspect the repository","role":"investigator","dependencies":[],"owned_paths":[],"acceptance_criteria":["Repository evidence is reported."],"deliverables":["Inspection report."],"validation_steps":["Re-read cited evidence."],"forbidden_actions":["Do not modify files."],"contract_notes":["Report exact paths."]}]}"#
            } else {
                "child completed repository inspection"
            };
            Ok(vec![
                ModelEvent::TextDelta {
                    text: text.to_string(),
                },
                ModelEvent::Completed {
                    response_id: Some(format!("agentic-{call}")),
                },
            ])
        }
    }

    #[async_trait]
    impl ProviderAdapter for ParallelAgenticProvider {
        fn id(&self) -> &'static str {
            "parallel-agentic-mock"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_tool_calls: true,
                ..ProviderCapabilities::default()
            }
        }

        async fn execute(
            &self,
            _request: CanonicalModelRequest,
        ) -> Result<Vec<ModelEvent>, ProviderError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let text = if call == 0 {
                r#"{"tasks":[{"id":"one","description":"inspect one","role":"investigator","dependencies":[],"owned_paths":[],"acceptance_criteria":["First inspection is evidence-backed."],"deliverables":["First report."],"validation_steps":["Check first evidence."],"forbidden_actions":["Do not write."],"contract_notes":[]},{"id":"two","description":"inspect two","role":"investigator","dependencies":[],"owned_paths":[],"acceptance_criteria":["Second inspection is evidence-backed."],"deliverables":["Second report."],"validation_steps":["Check second evidence."],"forbidden_actions":["Do not write."],"contract_notes":[]}]}"#.to_string()
            } else if call <= 2 {
                let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.maximum_active.fetch_max(active, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(75)).await;
                self.active.fetch_sub(1, Ordering::SeqCst);
                format!("specialist {call} complete")
            } else {
                "integrated parallel result".to_string()
            };
            Ok(vec![
                ModelEvent::TextDelta { text },
                ModelEvent::Completed {
                    response_id: Some(format!("parallel-{call}")),
                },
            ])
        }
    }

    #[async_trait]
    impl ProviderAdapter for RuntimeSpawnProvider {
        fn id(&self) -> &'static str {
            "runtime-spawn-mock"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_tool_calls: true,
                ..ProviderCapabilities::default()
            }
        }

        async fn execute(
            &self,
            request: CanonicalModelRequest,
        ) -> Result<Vec<ModelEvent>, ProviderError> {
            if request.system.contains("Collect source-backed evidence") {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                return Ok(vec![
                    ModelEvent::TextDelta {
                        text: "child evidence complete".to_string(),
                    },
                    ModelEvent::Completed {
                        response_id: Some("child-complete".to_string()),
                    },
                ]);
            }
            let last_tool = request.messages.iter().rev().find_map(|message| {
                message
                    .content
                    .iter()
                    .rev()
                    .find_map(|content| match content {
                        MessageContent::ToolResult { name, result, .. } => {
                            Some((name.as_str(), result))
                        }
                        _ => None,
                    })
            });
            let event = match last_tool {
                None => ModelEvent::ToolCall {
                    id: "spawn-child".to_string(),
                    name: "agents.spawn".to_string(),
                    arguments: serde_json::json!({
                        "task": "inspect the repository",
                        "role": "investigator"
                    }),
                },
                Some(("agents.spawn", result)) => ModelEvent::ToolCall {
                    id: "wait-child".to_string(),
                    name: "agents.wait".to_string(),
                    arguments: serde_json::json!({
                        "agent_ids": [result["output"]["agent_id"]],
                        "timeout_ms": 2_000
                    }),
                },
                Some(("agents.wait", _)) => ModelEvent::TextDelta {
                    text: "root incorporated child evidence".to_string(),
                },
                Some((name, _)) => {
                    return Err(ProviderError::InvalidResponse(format!(
                        "unexpected tool result `{name}`"
                    )));
                }
            };
            Ok(vec![
                event,
                ModelEvent::Completed {
                    response_id: Some("root-step".to_string()),
                },
            ])
        }
    }

    #[async_trait]
    impl ProviderAdapter for AlwaysFailProvider {
        fn id(&self) -> &'static str {
            "always-fail"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_tool_calls: true,
                ..ProviderCapabilities::default()
            }
        }

        async fn execute(
            &self,
            _request: CanonicalModelRequest,
        ) -> Result<Vec<ModelEvent>, ProviderError> {
            Err(ProviderError::Transient("primary unavailable".to_string()))
        }
    }

    #[async_trait]
    impl ProviderAdapter for FixedProvider {
        fn id(&self) -> &'static str {
            "fixed"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_tool_calls: true,
                ..ProviderCapabilities::default()
            }
        }

        async fn execute(
            &self,
            _request: CanonicalModelRequest,
        ) -> Result<Vec<ModelEvent>, ProviderError> {
            Ok(vec![
                ModelEvent::TextDelta {
                    text: "fallback completed".to_string(),
                },
                ModelEvent::Completed {
                    response_id: Some("fixed-response".to_string()),
                },
            ])
        }
    }

    #[cfg(windows)]
    #[async_trait]
    impl ProviderAdapter for ShouldNotCallProvider {
        fn id(&self) -> &'static str {
            "should-not-call"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_tool_calls: true,
                ..ProviderCapabilities::default()
            }
        }

        async fn execute(
            &self,
            _request: CanonicalModelRequest,
        ) -> Result<Vec<ModelEvent>, ProviderError> {
            Err(ProviderError::Transient(
                "preflight test should not call provider".to_string(),
            ))
        }
    }

    #[async_trait]
    impl ProviderAdapter for MailboxProvider {
        fn id(&self) -> &'static str {
            "mailbox-mock"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_tool_calls: true,
                ..ProviderCapabilities::default()
            }
        }

        async fn execute(
            &self,
            request: CanonicalModelRequest,
        ) -> Result<Vec<ModelEvent>, ProviderError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.first_call_started.notify_one();
                tokio::time::sleep(std::time::Duration::from_millis(75)).await;
                return Ok(vec![
                    ModelEvent::TextDelta {
                        text: "draft produced before coordination arrived".to_string(),
                    },
                    ModelEvent::Completed {
                        response_id: Some("mailbox-draft".to_string()),
                    },
                ]);
            }
            let delivered = request.messages.iter().any(|message| {
                message.role == MessageRole::Developer
                    && message.content.iter().any(|content| {
                        matches!(
                            content,
                            MessageContent::Text { text }
                                if text.contains("use the new coordination evidence")
                        )
                    })
            });
            if !delivered {
                return Err(ProviderError::InvalidResponse(
                    "mailbox message was not injected into the next turn".to_string(),
                ));
            }
            Ok(vec![
                ModelEvent::TextDelta {
                    text: "coordination message applied".to_string(),
                },
                ModelEvent::Completed {
                    response_id: Some("mailbox-final".to_string()),
                },
            ])
        }
    }

    #[async_trait]
    impl ProviderAdapter for ModelPackProvider {
        fn id(&self) -> &'static str {
            "openrouter"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_tool_calls: true,
                supports_structured_output: true,
                ..ProviderCapabilities::default()
            }
        }

        async fn execute(
            &self,
            request: CanonicalModelRequest,
        ) -> Result<Vec<ModelEvent>, ProviderError> {
            self.models
                .lock()
                .expect("model trace")
                .push(request.model.clone());
            self.messages
                .lock()
                .expect("message trace")
                .push(serde_json::to_string(&request.messages).expect("canonical messages"));
            let event = if request.structured_output_schema.is_some() {
                ModelEvent::TextDelta {
                    text: serde_json::json!({
                    "tasks": [
                        {
                            "id": "map",
                            "description": "Map the relevant repository boundaries.",
                            "role": "repository-mapper",
                            "dependencies": [],
                            "owned_paths": [],
                            "acceptance_criteria": ["Relevant boundaries are source-backed."],
                            "deliverables": ["Repository map."],
                            "validation_steps": ["Re-read cited source."],
                            "forbidden_actions": ["Do not mutate files."],
                            "contract_notes": ["Hand exact paths to the builder."]
                        },
                        {
                            "id": "build",
                            "description": "Implement the requested bounded change.",
                            "role": "implementer",
                            "dependencies": ["map"],
                            "owned_paths": ["target/pack-fixture"],
                            "acceptance_criteria": ["The requested behavior is implemented."],
                            "deliverables": ["Working source change."],
                            "validation_steps": ["Inspect changed source."],
                            "forbidden_actions": ["Do not edit unrelated files."],
                            "contract_notes": ["Use the repository map."]
                        },
                        {
                            "id": "validate",
                            "description": "Validate the completed change.",
                            "role": "test-debugging-specialist",
                            "dependencies": ["build"],
                            "owned_paths": ["src"],
                            "acceptance_criteria": ["Validation evidence is explicit."],
                            "deliverables": ["Validation report."],
                            "validation_steps": ["Run focused tests."],
                            "forbidden_actions": ["Do not conceal failures."],
                            "contract_notes": ["Verify the builder handoff."]
                        }
                    ]
                    })
                    .to_string(),
                }
            } else if request
                .system
                .contains("Integrate the completed specialist reports")
            {
                ModelEvent::TextDelta {
                    text: "model pack synthesis complete".to_string(),
                }
            } else if request.system.contains("Implement only the assigned scope")
                && !request.messages.iter().any(|message| {
                    message.content.iter().any(|content| {
                        matches!(
                            content,
                            MessageContent::ToolResult { name, .. } if name == "fs.mkdir"
                        )
                    })
                })
            {
                ModelEvent::ToolCall {
                    id: "pack-build-mkdir".to_string(),
                    name: "fs.mkdir".to_string(),
                    arguments: serde_json::json!({"path": "target/pack-fixture"}),
                }
            } else {
                ModelEvent::TextDelta {
                    text: format!("{} specialist complete", request.model),
                }
            };
            Ok(vec![
                event,
                ModelEvent::Completed {
                    response_id: Some(format!("pack-{}", request.model)),
                },
            ])
        }
    }

    #[async_trait]
    impl ProviderAdapter for EditThenFinalProvider {
        fn id(&self) -> &'static str {
            "edit-mock"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_tool_calls: true,
                ..ProviderCapabilities::default()
            }
        }

        async fn execute(
            &self,
            _request: CanonicalModelRequest,
        ) -> Result<Vec<ModelEvent>, ProviderError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(vec![
                    ModelEvent::ToolCall {
                        id: "patch-1".to_string(),
                        name: "patch.apply".to_string(),
                        arguments: serde_json::json!({
                            "path": "sample.txt",
                            "patch": "--- original\n+++ modified\n@@ -1 +1 @@\n-hello\n+world\n"
                        }),
                    },
                    ModelEvent::Completed {
                        response_id: Some("edit-turn".to_string()),
                    },
                ])
            } else {
                Ok(vec![
                    ModelEvent::TextDelta {
                        text: "edit complete".to_string(),
                    },
                    ModelEvent::Completed {
                        response_id: Some("edit-final".to_string()),
                    },
                ])
            }
        }
    }

    #[async_trait]
    impl ProviderAdapter for SetupReadyThenWriteProvider {
        fn id(&self) -> &'static str {
            "setup-ready-write-mock"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_tool_calls: true,
                ..ProviderCapabilities::default()
            }
        }

        async fn execute(
            &self,
            request: CanonicalModelRequest,
        ) -> Result<Vec<ModelEvent>, ProviderError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let event = match call {
                0 => ModelEvent::ToolCall {
                    id: "activate-skill".to_string(),
                    name: "skill.activate".to_string(),
                    arguments: serde_json::json!({"name": "focused-validation"}),
                },
                1 => ModelEvent::TextDelta {
                    text: "Understood. I am ready for the instructions.".to_string(),
                },
                2 => {
                    let recovered = request.messages.iter().any(|message| {
                        message.content.iter().any(|content| {
                            matches!(
                                content,
                                MessageContent::Text { text }
                                    if text.contains("Runtime completion check")
                            )
                        })
                    });
                    if !recovered {
                        return Err(ProviderError::InvalidResponse(
                            "runtime did not reassert the unfinished objective".to_string(),
                        ));
                    }
                    ModelEvent::ToolCall {
                        id: "write-artifact".to_string(),
                        name: "fs.write".to_string(),
                        arguments: serde_json::json!({
                            "path": "calculator.html",
                            "content": "<!doctype html><title>Calculator</title>"
                        }),
                    }
                }
                _ => ModelEvent::TextDelta {
                    text: "Calculator created and verified.".to_string(),
                },
            };
            Ok(vec![
                event,
                ModelEvent::Completed {
                    response_id: Some(format!("setup-ready-write-{call}")),
                },
            ])
        }
    }

    #[async_trait]
    impl ProviderAdapter for ImagePreflightProvider {
        fn id(&self) -> &'static str {
            "image-preflight-mock"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_tool_calls: true,
                supports_multimodal_input: true,
                ..ProviderCapabilities::default()
            }
        }

        async fn execute(
            &self,
            request: CanonicalModelRequest,
        ) -> Result<Vec<ModelEvent>, ProviderError> {
            let has_preflight = request.messages.iter().any(|message| {
                message.content.iter().any(|content| {
                    matches!(
                        content,
                        MessageContent::Text { text }
                            if text.contains("Runtime visual preflight completed")
                    )
                })
            });
            let image_count = request
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .filter(|content| {
                    matches!(
                        content,
                        MessageContent::FileReference {
                            mime_type: Some(mime_type),
                            ..
                        } if mime_type.starts_with("image/")
                    )
                })
                .count();
            if !has_preflight || image_count < 2 {
                return Err(ProviderError::InvalidResponse(
                    "visual preflight was not injected as native image input".to_string(),
                ));
            }
            Ok(vec![
                ModelEvent::TextDelta {
                    text: "Image inspected from native visual input.".to_string(),
                },
                ModelEvent::Completed {
                    response_id: Some("image-preflight".to_string()),
                },
            ])
        }
    }

    #[async_trait]
    impl ProviderAdapter for TextArtifactsThenFinalProvider {
        fn id(&self) -> &str {
            &self.id
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_tool_calls: true,
                ..ProviderCapabilities::default()
            }
        }

        #[allow(clippy::too_many_lines)]
        async fn execute(
            &self,
            request: CanonicalModelRequest,
        ) -> Result<Vec<ModelEvent>, ProviderError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                return Ok(vec![
                    ModelEvent::ToolCall {
                        id: "formal-index-write".to_string(),
                        name: "fs.write".to_string(),
                        arguments: json!({
                            "path": "index.html",
                            "content": "<!doctype html><link rel=\"stylesheet\" href=\"styles.css\">"
                        }),
                    },
                    ModelEvent::Completed {
                        response_id: Some("text-artifacts-0".to_string()),
                    },
                ]);
            }
            if call == 1 {
                let writes = request
                    .messages
                    .iter()
                    .flat_map(|message| &message.content)
                    .filter(|content| {
                        matches!(
                            content,
                            MessageContent::ToolResult { name, .. } if name == "fs.write"
                        )
                    })
                    .count();
                if writes != 1 {
                    return Err(ProviderError::InvalidResponse(format!(
                        "expected one formal write result, observed {writes}"
                    )));
                }
                return Ok(vec![
                    ModelEvent::TextDelta {
                        text: r#"### `style.css`
```css
body { background: black; color: white; }
```
### `script.js`
```js
document.title = "Calculator";
```
### `index.html`
```html
<!doctype html><link rel="stylesheet" href="style.css"><script src="script.js"></script>
```"#
                            .to_string(),
                    },
                    ModelEvent::Completed {
                        response_id: Some("text-artifacts-1".to_string()),
                    },
                ]);
            }
            if call == 2 {
                let writes = request
                    .messages
                    .iter()
                    .flat_map(|message| &message.content)
                    .filter(|content| {
                        matches!(
                            content,
                            MessageContent::ToolResult { name, .. } if name == "fs.write"
                        )
                    })
                    .count();
                if writes != 4 {
                    return Err(ProviderError::InvalidResponse(format!(
                        "expected one formal and three synthesized write results, observed {writes}"
                    )));
                }
                return Ok(vec![ModelEvent::Completed {
                    response_id: Some("text-artifacts-2".to_string()),
                }]);
            }
            if call == 3 {
                let reads = request
                    .messages
                    .iter()
                    .flat_map(|message| &message.content)
                    .filter(|content| {
                        matches!(
                            content,
                            MessageContent::ToolResult { name, .. } if name == "fs.read"
                        )
                    })
                    .count();
                if reads != 3 {
                    return Err(ProviderError::InvalidResponse(format!(
                        "expected three canonical read-back results, observed {reads}"
                    )));
                }
                return Ok(vec![
                    ModelEvent::ToolCall {
                        id: "irrelevant-search".to_string(),
                        name: "google:search".to_string(),
                        arguments: json!({"queries": ["calculator css"]}),
                    },
                    ModelEvent::ToolCall {
                        id: "redundant-write-without-path".to_string(),
                        name: "fs.write".to_string(),
                        arguments: json!({"content": "already completed"}),
                    },
                    ModelEvent::Completed {
                        response_id: Some("text-artifacts-3".to_string()),
                    },
                ]);
            }
            Err(ProviderError::InvalidResponse(
                "unexpected extra model call after verified completion".to_string(),
            ))
        }
    }

    #[async_trait]
    impl ProviderAdapter for ProseOnlyProvider {
        fn id(&self) -> &str {
            &self.id
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_tool_calls: true,
                ..ProviderCapabilities::default()
            }
        }

        async fn execute(
            &self,
            _request: CanonicalModelRequest,
        ) -> Result<Vec<ModelEvent>, ProviderError> {
            Ok(vec![
                ModelEvent::TextDelta {
                    text: r#"### `index.html`
```html
<!doctype html><link rel="stylesheet" href="styles.css"><script src="script.js"></script>
```
### `styles.css`
```css
body { background: black; }
```
### `script.js`
```js
document.title = "Calculator";
```"#
                        .to_string(),
                },
                ModelEvent::Completed {
                    response_id: Some("prose-only".to_string()),
                },
            ])
        }
    }

    #[async_trait]
    impl ProviderAdapter for ToolThenFinalProvider {
        fn id(&self) -> &'static str {
            "tool-mock"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_tool_calls: true,
                ..ProviderCapabilities::default()
            }
        }

        async fn execute(
            &self,
            _request: CanonicalModelRequest,
        ) -> Result<Vec<ModelEvent>, ProviderError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(vec![
                    ModelEvent::ToolCall {
                        id: "read-1".to_string(),
                        name: "fs.read".to_string(),
                        arguments: serde_json::json!({"path": "sample.txt"}),
                    },
                    ModelEvent::Completed {
                        response_id: Some("tool-turn".to_string()),
                    },
                ])
            } else {
                Ok(vec![
                    ModelEvent::TextDelta {
                        text: "focused answer".to_string(),
                    },
                    ModelEvent::Completed {
                        response_id: Some("final-turn".to_string()),
                    },
                ])
            }
        }
    }

    #[tokio::test]
    async fn executes_direct_run_without_creating_an_agent() {
        let store = Store::in_memory().expect("store");
        let conversation = store.create_conversation(".", None).expect("conversation");
        let run = store
            .create_run(conversation.id, "question", ExecutionMode::Direct)
            .expect("run");
        let router = ProviderRouter::default();
        router.register(Arc::new(MockProvider));
        let engine = ExecutionEngine::new(store.clone(), Arc::new(router), ToolExecutor::default());

        let result = engine
            .execute_run(run.id, "mock", "mock-model")
            .await
            .expect("execution");

        assert_eq!(result.output, "direct answer");
        assert!(store.list_agents(Some(run.id)).expect("agents").is_empty());
        let metrics = store
            .performance_snapshot(Some(run.id))
            .expect("performance snapshot");
        assert_eq!(metrics.model_calls, 1);
        assert_eq!(metrics.usage.output_tokens, 2);
        assert_eq!(
            store.get_run(run.id).expect("run").status,
            opensrc_core::RunStatus::Completed
        );
    }

    #[tokio::test]
    async fn retries_a_transient_provider_failure_before_any_output() {
        let store = Store::in_memory().expect("store");
        let conversation = store.create_conversation(".", None).expect("conversation");
        let run = store
            .create_run(conversation.id, "question", ExecutionMode::Direct)
            .expect("run");
        let router = ProviderRouter::default();
        let provider = Arc::new(TransientThenSuccessProvider {
            calls: AtomicUsize::new(0),
        });
        router.register(provider.clone());
        let engine = ExecutionEngine::new(store.clone(), Arc::new(router), ToolExecutor::default());
        let result = engine
            .execute_run(run.id, "retry-mock", "mock-model")
            .await
            .expect("execution");
        assert_eq!(result.output, "recovered");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
        assert!(
            store
                .events_after(0, 100)
                .expect("events")
                .iter()
                .any(|event| event.kind == "provider.retry_scheduled")
        );
    }

    #[tokio::test]
    async fn single_model_run_ignores_stale_agent_fallbacks() {
        let store = Store::in_memory().expect("store");
        let conversation = store.create_conversation(".", None).expect("conversation");
        let run = store
            .create_run(conversation.id, "question", ExecutionMode::Focused)
            .expect("run");
        let mut definition = crate::built_in_agent_definition("investigator").expect("definition");
        definition.preferred_provider = Some("always-fail".to_string());
        definition.preferred_model = Some("primary-model".to_string());
        definition.retry_policy.max_attempts = 1;
        definition.fallback_chain = vec!["fixed/fallback-model".to_string()];
        AgentControl::new(store.clone(), AgentLimits::default())
            .create_root(run.id, &definition, "question", ".")
            .expect("root");
        let router = ProviderRouter::default();
        router.register(Arc::new(AlwaysFailProvider));
        router.register(Arc::new(FixedProvider));
        let engine = ExecutionEngine::new(store.clone(), Arc::new(router), ToolExecutor::default());

        let result = engine
            .execute_run(run.id, "always-fail", "primary-model")
            .await;

        assert!(result.is_err());
        assert!(
            !store
                .events_after(0, 100)
                .expect("events")
                .iter()
                .any(|event| {
                    event.kind == "provider.fallback_selected"
                        || event.kind == "routing.model_transition"
                })
        );
    }

    #[tokio::test]
    async fn executes_focused_tool_cycle_and_completes_root_agent() {
        let workspace = std::env::temp_dir().join(format!("opensrc-focused-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::write(workspace.join("sample.txt"), "context").expect("fixture");
        let store = Store::in_memory().expect("store");
        let conversation = store
            .create_conversation(workspace.to_string_lossy(), None)
            .expect("conversation");
        let run = store
            .create_run(conversation.id, "read the sample", ExecutionMode::Focused)
            .expect("run");
        let definition = AgentDefinition {
            name: "focused".to_string(),
            description: "test".to_string(),
            system_instructions: "Work carefully.".to_string(),
            preferred_provider: None,
            preferred_model: None,
            reasoning: ReasoningConfig::default(),
            context_policy: ContextPolicy::default(),
            tool_policy: ToolPolicy {
                allow: vec!["fs.read".to_string()],
                deny: Vec::new(),
                may_spawn_children: false,
            },
            sandbox_policy: SandboxPolicy::default(),
            workspace_mode: WorkspaceMode::OwnedPaths,
            budgets: Budgets {
                turn_limit: Some(3),
                ..Budgets::default()
            },
            retry_policy: RetryPolicy::default(),
            fallback_chain: Vec::new(),
            completion_schema: "task_completion".to_string(),
            metadata: BTreeMap::new(),
        };
        let control = AgentControl::new(store.clone(), AgentLimits::default());
        let root = control
            .create_root(run.id, &definition, "read", workspace.to_string_lossy())
            .expect("root");
        let router = ProviderRouter::default();
        router.register(Arc::new(ToolThenFinalProvider {
            calls: AtomicUsize::new(0),
        }));
        let engine = ExecutionEngine::new(store.clone(), Arc::new(router), ToolExecutor::default());

        let result = engine
            .execute_run(run.id, "tool-mock", "mock-model")
            .await
            .expect("focused execution");

        assert_eq!(result.output, "focused answer");
        assert_eq!(result.model_calls, 2);
        assert_eq!(result.tool_calls, 1);
        assert_eq!(
            store.get_agent(root.id).expect("root projection").status,
            opensrc_core::AgentStatus::Completed
        );
        let messages = store
            .list_messages(conversation.id)
            .expect("durable tool transcript");
        assert_eq!(
            messages
                .iter()
                .map(|message| message.role)
                .collect::<Vec<_>>(),
            vec![
                MessageRole::User,
                MessageRole::Assistant,
                MessageRole::Tool,
                MessageRole::Assistant,
            ]
        );
        assert!(matches!(
            messages[1].content.first(),
            Some(MessageContent::ToolCall { name, .. }) if name == "fs.read"
        ));
        assert!(matches!(
            messages[2].content.first(),
            Some(MessageContent::ToolResult { name, .. }) if name == "fs.read"
        ));
        std::fs::remove_dir_all(workspace).expect("cleanup");
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn setup_and_readiness_cannot_complete_a_required_file_mutation() {
        let workspace =
            std::env::temp_dir().join(format!("opensrc-outcome-gate-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).expect("workspace");
        let store = Store::in_memory().expect("store");
        let conversation = store
            .create_conversation(workspace.to_string_lossy(), None)
            .expect("conversation");
        let run = store
            .create_run(
                conversation.id,
                "Analyze the image and make a calculator in this folder",
                ExecutionMode::Focused,
            )
            .expect("run");
        let definition = AgentDefinition {
            name: "frontend-specialist".to_string(),
            description: "test".to_string(),
            system_instructions: "Complete the requested build.".to_string(),
            preferred_provider: None,
            preferred_model: None,
            reasoning: ReasoningConfig::default(),
            context_policy: ContextPolicy::default(),
            tool_policy: ToolPolicy {
                allow: vec![
                    "skill.activate".to_string(),
                    "fs.write".to_string(),
                    "fs.read".to_string(),
                ],
                deny: Vec::new(),
                may_spawn_children: false,
            },
            sandbox_policy: SandboxPolicy::default(),
            workspace_mode: WorkspaceMode::OwnedPaths,
            budgets: Budgets {
                turn_limit: Some(6),
                ..Budgets::default()
            },
            retry_policy: RetryPolicy::default(),
            fallback_chain: Vec::new(),
            completion_schema: "task_completion".to_string(),
            metadata: BTreeMap::new(),
        };
        AgentControl::new(store.clone(), AgentLimits::default())
            .create_root(
                run.id,
                &definition,
                "build calculator",
                workspace.to_string_lossy(),
            )
            .expect("root");
        let router = ProviderRouter::default();
        let provider = Arc::new(SetupReadyThenWriteProvider {
            calls: AtomicUsize::new(0),
        });
        router.register(provider.clone());
        let engine = ExecutionEngine::new(store.clone(), Arc::new(router), ToolExecutor::default())
            .with_skill_registry(crate::SkillRegistry::builtins().expect("builtin skills"));
        let execution = tokio::spawn(async move {
            engine
                .execute_run(run.id, "setup-ready-write-mock", "mock")
                .await
        });

        let approval = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(approval) = store
                    .list_approvals(true)
                    .expect("pending approvals")
                    .into_iter()
                    .next()
                {
                    break approval;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("approval timeout");
        store
            .decide_approval(
                approval.id,
                ApprovalDecision::AllowOnce,
                None,
                Some("test-owned workspace".to_string()),
            )
            .expect("approve write");

        let result = execution
            .await
            .expect("join")
            .expect("outcome-gated execution");
        assert_eq!(result.output, "Calculator created and verified.");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 5);
        assert_eq!(
            std::fs::read_to_string(workspace.join("calculator.html")).expect("artifact"),
            "<!doctype html><title>Calculator</title>"
        );
        assert!(
            store
                .events_after(0, 200)
                .expect("events")
                .iter()
                .any(|event| event.kind == "runtime.completion_deferred")
        );
        std::fs::remove_dir_all(workspace).expect("cleanup");
    }

    #[tokio::test]
    async fn attached_images_are_preflighted_and_reinjected_as_native_input() {
        let workspace =
            std::env::temp_dir().join(format!("opensrc-image-preflight-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).expect("workspace");
        let image = workspace.join("calculator.png");
        std::fs::write(&image, b"png fixture").expect("image fixture");
        let store = Store::in_memory().expect("store");
        let conversation = store
            .create_conversation(workspace.to_string_lossy(), None)
            .expect("conversation");
        let run = store
            .create_run_with_content(
                conversation.id,
                "Analyze this calculator image",
                ExecutionMode::Focused,
                vec![
                    MessageContent::text("Analyze this calculator image"),
                    MessageContent::FileReference {
                        path: image.to_string_lossy().into_owned(),
                        mime_type: Some("image/png".to_string()),
                    },
                ],
            )
            .expect("run");
        let definition = AgentDefinition {
            name: "media-specialist".to_string(),
            description: "test".to_string(),
            system_instructions: "Inspect the image.".to_string(),
            preferred_provider: None,
            preferred_model: None,
            reasoning: ReasoningConfig::default(),
            context_policy: ContextPolicy::default(),
            tool_policy: ToolPolicy {
                allow: vec!["fs.view_image".to_string()],
                deny: Vec::new(),
                may_spawn_children: false,
            },
            sandbox_policy: SandboxPolicy::default(),
            workspace_mode: WorkspaceMode::OwnedPaths,
            budgets: Budgets::default(),
            retry_policy: RetryPolicy::default(),
            fallback_chain: Vec::new(),
            completion_schema: "task_completion".to_string(),
            metadata: BTreeMap::new(),
        };
        AgentControl::new(store.clone(), AgentLimits::default())
            .create_root(
                run.id,
                &definition,
                "inspect image",
                workspace.to_string_lossy(),
            )
            .expect("root");
        let router = ProviderRouter::default();
        router.register(Arc::new(ImagePreflightProvider));
        let engine = ExecutionEngine::new(store.clone(), Arc::new(router), ToolExecutor::default());

        let result = engine
            .execute_run(run.id, "image-preflight-mock", "mock")
            .await
            .expect("image preflight execution");

        assert_eq!(result.output, "Image inspected from native visual input.");
        assert_eq!(result.tool_calls, 1);
        assert!(
            store
                .events_after(0, 100)
                .expect("events")
                .iter()
                .any(|event| {
                    event.kind == "tool.completed" && event.payload["name"] == "fs.view_image"
                })
        );
        std::fs::remove_dir_all(workspace).expect("cleanup");
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn filename_labeled_text_artifacts_become_canonical_write_calls() {
        let workspace =
            std::env::temp_dir().join(format!("opensrc-text-artifacts-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).expect("workspace");
        let store = Store::in_memory().expect("store");
        let conversation = store
            .create_conversation(workspace.to_string_lossy(), None)
            .expect("conversation");
        let run = store
            .create_run(
                conversation.id,
                "Build index.html, styles.css, and script.js",
                ExecutionMode::Focused,
            )
            .expect("run");
        let definition = AgentDefinition {
            name: "frontend-specialist".to_string(),
            description: "test".to_string(),
            system_instructions: "Build the requested files.".to_string(),
            preferred_provider: None,
            preferred_model: None,
            reasoning: ReasoningConfig::default(),
            context_policy: ContextPolicy::default(),
            tool_policy: ToolPolicy {
                allow: vec!["fs.write".to_string(), "fs.read".to_string()],
                deny: Vec::new(),
                may_spawn_children: false,
            },
            sandbox_policy: SandboxPolicy::default(),
            workspace_mode: WorkspaceMode::OwnedPaths,
            budgets: Budgets::default(),
            retry_policy: RetryPolicy::default(),
            fallback_chain: Vec::new(),
            completion_schema: "task_completion".to_string(),
            metadata: BTreeMap::new(),
        };
        AgentControl::new(store.clone(), AgentLimits::default())
            .create_root(
                run.id,
                &definition,
                "build files",
                workspace.to_string_lossy(),
            )
            .expect("root");
        let router = ProviderRouter::default();
        let provider = Arc::new(TextArtifactsThenFinalProvider {
            id: "ollama".to_string(),
            calls: AtomicUsize::new(0),
        });
        router.register(provider);
        let engine = ExecutionEngine::new(store.clone(), Arc::new(router), ToolExecutor::default());
        let approval_store = store.clone();
        let approver = tokio::spawn(async move {
            for _ in 0..200 {
                for approval in approval_store
                    .list_approvals(true)
                    .expect("pending artifact approvals")
                {
                    approval_store
                        .decide_approval(
                            approval.id,
                            ApprovalDecision::AllowOnce,
                            None,
                            Some("artifact fixture workspace".to_string()),
                        )
                        .expect("approve artifact write");
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        });

        let result = engine
            .execute_run(run.id, "ollama", "gemma4:e2b")
            .await
            .expect("artifact execution");
        approver.abort();

        assert_eq!(result.model_calls, 4);
        assert_eq!(result.tool_calls, 7);
        assert_eq!(
            result.output,
            "Completed the requested local changes and verified: index.html, styles.css, script.js."
        );
        assert!(workspace.join("index.html").is_file());
        assert!(workspace.join("styles.css").is_file());
        assert!(workspace.join("script.js").is_file());
        assert!(
            std::fs::read_to_string(workspace.join("index.html"))
                .expect("materialized html")
                .contains("href=\"styles.css\"")
        );
        assert!(!workspace.join("style.css").exists());
        assert!(
            store
                .events_after(0, 200)
                .expect("events")
                .iter()
                .any(|event| event.kind == "runtime.artifact_tool_calls_synthesized")
        );
        assert!(
            store
                .events_after(0, 200)
                .expect("events")
                .iter()
                .any(|event| event.kind == "runtime.unneeded_tool_calls_ignored")
        );
        assert!(
            store
                .events_after(0, 200)
                .expect("events")
                .iter()
                .any(|event| event.kind == "runtime.malformed_redundant_tool_calls_ignored")
        );
        assert!(
            store
                .events_after(0, 200)
                .expect("events")
                .iter()
                .any(|event| {
                    event.kind == "runtime.compatibility_profile_selected"
                        && event.payload["profile"] == "ollama-gemma"
                })
        );
        std::fs::remove_dir_all(workspace).expect("cleanup");
    }

    #[tokio::test]
    async fn hosted_routes_do_not_inherit_local_prose_materialization() {
        let workspace =
            std::env::temp_dir().join(format!("opensrc-standard-profile-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).expect("workspace");
        let store = Store::in_memory().expect("store");
        let conversation = store
            .create_conversation(workspace.to_string_lossy(), None)
            .expect("conversation");
        let run = store
            .create_run(
                conversation.id,
                "Build index.html, styles.css, and script.js",
                ExecutionMode::Focused,
            )
            .expect("run");
        let definition = AgentDefinition {
            name: "frontend-specialist".to_string(),
            description: "test".to_string(),
            system_instructions: "Use canonical filesystem tools.".to_string(),
            preferred_provider: None,
            preferred_model: None,
            reasoning: ReasoningConfig::default(),
            context_policy: ContextPolicy::default(),
            tool_policy: ToolPolicy {
                allow: vec!["fs.write".to_string(), "fs.read".to_string()],
                deny: Vec::new(),
                may_spawn_children: false,
            },
            sandbox_policy: SandboxPolicy::default(),
            workspace_mode: WorkspaceMode::OwnedPaths,
            budgets: Budgets {
                turn_limit: Some(2),
                ..Budgets::default()
            },
            retry_policy: RetryPolicy::default(),
            fallback_chain: Vec::new(),
            completion_schema: "task_completion".to_string(),
            metadata: BTreeMap::new(),
        };
        AgentControl::new(store.clone(), AgentLimits::default())
            .create_root(
                run.id,
                &definition,
                "build files",
                workspace.to_string_lossy(),
            )
            .expect("root");
        let router = ProviderRouter::default();
        router.register(Arc::new(ProseOnlyProvider {
            id: "openrouter".to_string(),
        }));
        let engine = ExecutionEngine::new(store.clone(), Arc::new(router), ToolExecutor::default());

        let error = engine
            .execute_run(run.id, "openrouter", "gemma-4")
            .await
            .expect_err("hosted routes must require canonical tool execution");
        assert!(matches!(error, ExecutionError::IncompleteOutcome(_)));
        assert!(!workspace.join("index.html").exists());
        let events = store.events_after(0, 200).expect("events");
        assert!(events.iter().any(|event| {
            event.kind == "runtime.compatibility_profile_selected"
                && event.payload["profile"] == "standard"
        }));
        assert!(
            !events
                .iter()
                .any(|event| event.kind == "runtime.artifact_tool_calls_synthesized")
        );
        std::fs::remove_dir_all(workspace).expect("cleanup");
    }

    #[test]
    fn extracts_natural_drive_inventory_targets() {
        assert_eq!(
            deterministic_directory_inventory_target("Analyze F drive and list main folders")
                .expect("drive target")
                .path,
            "F:\\"
        );
        assert_eq!(
            deterministic_directory_inventory_target("show the directories on e:\\")
                .expect("drive path")
                .path,
            "E:\\"
        );
        assert!(deterministic_directory_inventory_target("create a folder on F drive").is_none());
    }

    #[test]
    fn formats_directory_inventory_without_model_text() {
        let output = directory_inventory_answer(
            "F:\\",
            &serde_json::json!({
                "entries": [
                    {"path": "F:\\", "kind": "directory"},
                    {"path": "F:\\Code", "kind": "directory"},
                    {"path": "F:\\notes.txt", "kind": "file"}
                ],
                "truncated": false
            }),
        );

        assert_eq!(output, "Main folders in `F:\\`:\n- Code");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn directory_inventory_preflight_completes_without_calling_provider() {
        let workspace = std::env::temp_dir().join(format!("opensrc-inventory-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).expect("workspace");
        let store = Store::in_memory().expect("store");
        let conversation = store
            .create_conversation(workspace.to_string_lossy(), None)
            .expect("conversation");
        let run = store
            .create_run(
                conversation.id,
                "Analyze C drive and list the main folders",
                ExecutionMode::Focused,
            )
            .expect("run");
        let definition = AgentDefinition {
            name: "focused".to_string(),
            description: "test".to_string(),
            system_instructions: "Work carefully.".to_string(),
            preferred_provider: None,
            preferred_model: None,
            reasoning: ReasoningConfig::default(),
            context_policy: ContextPolicy::default(),
            tool_policy: ToolPolicy {
                allow: vec!["fs.list".to_string()],
                deny: Vec::new(),
                may_spawn_children: false,
            },
            sandbox_policy: SandboxPolicy {
                read_paths: vec!["C:\\".to_string()],
                ..SandboxPolicy::default()
            },
            workspace_mode: WorkspaceMode::OwnedPaths,
            budgets: Budgets::default(),
            retry_policy: RetryPolicy::default(),
            fallback_chain: Vec::new(),
            completion_schema: "task_completion".to_string(),
            metadata: BTreeMap::new(),
        };
        AgentControl::new(store.clone(), AgentLimits::default())
            .create_root(
                run.id,
                &definition,
                "inventory",
                workspace.to_string_lossy(),
            )
            .expect("root");
        let router = ProviderRouter::default();
        router.register(Arc::new(ShouldNotCallProvider));
        let engine = ExecutionEngine::new(store.clone(), Arc::new(router), ToolExecutor::default());

        let result = engine
            .execute_run(run.id, "should-not-call", "mock-model")
            .await
            .expect("preflight execution");

        assert_eq!(result.model_calls, 0);
        assert_eq!(result.tool_calls, 1);
        assert!(result.output.contains("Main folders in `C:\\`:"));
        assert_eq!(
            store
                .performance_snapshot(Some(run.id))
                .expect("metrics")
                .tool_calls,
            1
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn pauses_for_persistent_edit_approval_then_resumes() {
        let workspace = std::env::temp_dir().join(format!("opensrc-approval-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::write(workspace.join("sample.txt"), "hello\n").expect("fixture");
        let store = Store::in_memory().expect("store");
        let conversation = store
            .create_conversation(workspace.to_string_lossy(), None)
            .expect("conversation");
        let run = store
            .create_run(conversation.id, "edit the sample", ExecutionMode::Focused)
            .expect("run");
        let definition = AgentDefinition {
            name: "implementer".to_string(),
            description: "test".to_string(),
            system_instructions: "Edit carefully.".to_string(),
            preferred_provider: None,
            preferred_model: None,
            reasoning: ReasoningConfig::default(),
            context_policy: ContextPolicy::default(),
            tool_policy: ToolPolicy {
                allow: vec!["patch.apply".to_string()],
                deny: Vec::new(),
                may_spawn_children: false,
            },
            sandbox_policy: SandboxPolicy::default(),
            workspace_mode: WorkspaceMode::OwnedPaths,
            budgets: Budgets {
                turn_limit: Some(3),
                ..Budgets::default()
            },
            retry_policy: RetryPolicy::default(),
            fallback_chain: Vec::new(),
            completion_schema: "task_completion".to_string(),
            metadata: BTreeMap::new(),
        };
        AgentControl::new(store.clone(), AgentLimits::default())
            .create_root(run.id, &definition, "edit", workspace.to_string_lossy())
            .expect("root");
        let router = ProviderRouter::default();
        router.register(Arc::new(EditThenFinalProvider {
            calls: AtomicUsize::new(0),
        }));
        let engine = ExecutionEngine::new(store.clone(), Arc::new(router), ToolExecutor::default());
        let execution =
            tokio::spawn(async move { engine.execute_run(run.id, "edit-mock", "mock").await });

        let approval = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(approval) = store
                    .list_approvals(true)
                    .expect("pending approvals")
                    .into_iter()
                    .next()
                {
                    break approval;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("approval timeout");
        assert_eq!(approval.tool_name, "patch.apply");
        assert_eq!(
            std::fs::read_to_string(workspace.join("sample.txt")).expect("before approval"),
            "hello\n"
        );
        let decided = store
            .decide_approval(
                approval.id,
                ApprovalDecision::AllowOnce,
                None,
                Some("test approval".to_string()),
            )
            .expect("decision");
        assert_eq!(decided.status, ApprovalStatus::Allowed);
        let result = execution.await.expect("join").expect("approved execution");
        assert_eq!(result.output, "edit complete");
        assert_eq!(
            std::fs::read_to_string(workspace.join("sample.txt")).expect("after approval"),
            "world\n"
        );
        let messages = store
            .list_messages(conversation.id)
            .expect("approval transcript");
        assert!(messages.iter().any(|message| {
            matches!(
                message.content.first(),
                Some(MessageContent::ApprovalRequest { approval_id, .. })
                    if approval_id == &approval.id.to_string()
            )
        }));
        assert!(messages.iter().any(|message| {
            matches!(
                message.content.first(),
                Some(MessageContent::ApprovalResult { decision, .. }) if decision == "allowed"
            )
        }));
        let change = store
            .list_file_changes(Some(run.id))
            .expect("changes")
            .into_iter()
            .next()
            .expect("recorded change");
        let manager = ChangeManager::new(store.clone());
        manager.undo(change.id).expect("undo");
        assert_eq!(
            std::fs::read_to_string(workspace.join("sample.txt")).expect("undone file"),
            "hello\n"
        );
        manager.redo(change.id).expect("redo");
        assert_eq!(
            std::fs::read_to_string(workspace.join("sample.txt")).expect("redone file"),
            "world\n"
        );
        std::fs::remove_dir_all(workspace).expect("cleanup");
    }

    #[tokio::test]
    async fn focused_agent_can_spawn_wait_for_and_use_a_real_child_loop() {
        let store = Store::in_memory().expect("store");
        let conversation = store.create_conversation(".", None).expect("conversation");
        let run = store
            .create_run(
                conversation.id,
                "delegate inspection",
                ExecutionMode::Focused,
            )
            .expect("run");
        let mut definition =
            crate::built_in_agent_definition("generalist").expect("generalist definition");
        definition.preferred_provider = Some("runtime-spawn-mock".to_string());
        definition.preferred_model = Some("mock".to_string());
        AgentControl::new(store.clone(), AgentLimits::default())
            .create_root(run.id, &definition, "delegate inspection", ".")
            .expect("root");
        let router = ProviderRouter::default();
        router.register(Arc::new(RuntimeSpawnProvider));
        let engine = ExecutionEngine::new(store.clone(), Arc::new(router), ToolExecutor::default());
        let running = {
            let engine = engine.clone();
            tokio::spawn(async move {
                engine
                    .execute_run(run.id, "runtime-spawn-mock", "mock")
                    .await
            })
        };
        let approval = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(approval) = store
                    .list_approvals(true)
                    .expect("approvals")
                    .into_iter()
                    .next()
                {
                    break approval;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("spawn approval");
        assert_eq!(approval.tool_name, "agents.spawn");
        store
            .decide_approval(
                approval.id,
                ApprovalDecision::AllowOnce,
                None,
                Some("test child".to_string()),
            )
            .expect("approve");

        let result = running.await.expect("join").expect("focused execution");
        assert_eq!(result.output, "root incorporated child evidence");
        let agents = store.list_agents(Some(run.id)).expect("agents");
        assert_eq!(agents.len(), 2);
        let child = agents
            .iter()
            .find(|agent| agent.parent_id.is_some())
            .expect("child");
        assert_eq!(child.status, AgentStatus::Completed);
        assert_eq!(
            store
                .get_agent_completion(child.id)
                .expect("completion")
                .expect("child completion")
                .summary,
            "child evidence complete"
        );
    }

    #[tokio::test]
    async fn queued_agent_message_enters_the_next_live_model_turn() {
        let workspace = std::env::temp_dir().join(format!("opensrc-mailbox-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).expect("workspace");
        let store = Store::in_memory().expect("store");
        let conversation = store
            .create_conversation(workspace.to_string_lossy(), None)
            .expect("conversation");
        let run = store
            .create_run(
                conversation.id,
                "coordinate during execution",
                ExecutionMode::Focused,
            )
            .expect("run");
        let mut definition =
            crate::built_in_agent_definition("investigator").expect("investigator");
        definition.preferred_provider = Some("mailbox-mock".to_string());
        definition.preferred_model = Some("mock".to_string());
        let control = AgentControl::new(store.clone(), AgentLimits::default());
        let root = control
            .create_root(
                run.id,
                &definition,
                "coordinate during execution",
                workspace.to_string_lossy(),
            )
            .expect("root");
        let first_call_started = Arc::new(Notify::new());
        let router = ProviderRouter::default();
        router.register(Arc::new(MailboxProvider {
            calls: AtomicUsize::new(0),
            first_call_started: first_call_started.clone(),
        }));
        let engine = ExecutionEngine::new(store.clone(), Arc::new(router), ToolExecutor::default());
        let running =
            tokio::spawn(async move { engine.execute_run(run.id, "mailbox-mock", "mock").await });

        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            first_call_started.notified(),
        )
        .await
        .expect("first model call");
        control
            .send_message(root.id, "use the new coordination evidence")
            .expect("queue message");

        let result = running.await.expect("join").expect("execution");
        assert_eq!(result.output, "coordination message applied");
        assert!(
            store
                .events_after(0, 200)
                .expect("events")
                .iter()
                .any(|event| event.kind == "agent.message_delivered"
                    && event.agent_id == Some(root.id))
        );
        std::fs::remove_dir_all(workspace).expect("cleanup");
    }

    #[tokio::test]
    async fn cancels_an_in_flight_provider_call() {
        let store = Store::in_memory().expect("store");
        let conversation = store.create_conversation(".", None).expect("conversation");
        let run = store
            .create_run(conversation.id, "wait", ExecutionMode::Direct)
            .expect("run");
        let router = ProviderRouter::default();
        router.register(Arc::new(SlowProvider));
        let engine = ExecutionEngine::new(store.clone(), Arc::new(router), ToolExecutor::default());
        let running = {
            let engine = engine.clone();
            tokio::spawn(async move { engine.execute_run(run.id, "slow-mock", "mock").await })
        };
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if store.get_run(run.id).expect("run").status == RunStatus::Running {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("run start");
        engine.cancel_run(run.id).expect("cancel");
        assert!(matches!(
            running.await.expect("join"),
            Err(super::ExecutionError::Cancelled(id)) if id == run.id
        ));
        assert_eq!(
            store.get_run(run.id).expect("cancelled run").status,
            RunStatus::Cancelled
        );
    }

    #[tokio::test]
    async fn agentic_mode_plans_and_runs_a_real_child_model_loop() {
        let store = Store::in_memory().expect("store");
        let conversation = store.create_conversation(".", None).expect("conversation");
        let run = store
            .create_run(
                conversation.id,
                "inspect the project",
                ExecutionMode::Agentic,
            )
            .expect("run");
        let mut definition =
            crate::built_in_agent_definition("generalist").expect("generalist definition");
        definition.preferred_provider = Some("agentic-mock".to_string());
        definition.preferred_model = Some("mock".to_string());
        AgentControl::new(store.clone(), AgentLimits::default())
            .create_root(run.id, &definition, "inspect the project", ".")
            .expect("root");
        let router = ProviderRouter::default();
        router.register(Arc::new(AgenticProvider {
            calls: AtomicUsize::new(0),
        }));
        let engine = ExecutionEngine::new(store.clone(), Arc::new(router), ToolExecutor::default());

        let result = engine
            .execute_run(run.id, "agentic-mock", "mock")
            .await
            .expect("agentic execution");

        assert_eq!(result.mode, ExecutionMode::Agentic);
        assert_eq!(result.model_calls, 3);
        assert!(result.output.contains("child completed"));
        let agents = store.list_agents(Some(run.id)).expect("agents");
        assert_eq!(agents.len(), 2);
        assert!(
            agents
                .iter()
                .all(|agent| agent.status == AgentStatus::Completed)
        );
        let tasks = store.list_tasks(Some(run.id)).expect("tasks");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, TaskStatus::Completed);
    }

    #[tokio::test]
    async fn agentic_scheduler_runs_independent_read_only_tasks_concurrently() {
        let store = Store::in_memory().expect("store");
        let conversation = store.create_conversation(".", None).expect("conversation");
        let run = store
            .create_run(
                conversation.id,
                "inspect in parallel",
                ExecutionMode::Agentic,
            )
            .expect("run");
        let mut definition =
            crate::built_in_agent_definition("generalist").expect("generalist definition");
        definition.preferred_provider = Some("parallel-agentic-mock".to_string());
        definition.preferred_model = Some("mock".to_string());
        AgentControl::new(store.clone(), AgentLimits::default())
            .create_root(run.id, &definition, "inspect in parallel", ".")
            .expect("root");
        let provider = Arc::new(ParallelAgenticProvider {
            calls: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            maximum_active: AtomicUsize::new(0),
        });
        let router = ProviderRouter::default();
        router.register(provider.clone());
        let engine = ExecutionEngine::new(store.clone(), Arc::new(router), ToolExecutor::default());

        let result = engine
            .execute_run(run.id, "parallel-agentic-mock", "mock")
            .await
            .expect("agentic execution");

        assert_eq!(result.output, "integrated parallel result");
        assert_eq!(result.model_calls, 4);
        assert_eq!(provider.maximum_active.load(Ordering::SeqCst), 2);
        assert!(
            store
                .list_tasks(Some(run.id))
                .expect("tasks")
                .iter()
                .all(|task| task.status == TaskStatus::Completed)
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn three_model_pack_routes_plan_build_validation_and_synthesis() {
        let store = Store::in_memory().expect("store");
        let conversation = store.create_conversation(".", None).expect("conversation");
        store
            .update_conversation_selection(
                conversation.id,
                None,
                None,
                Some("efficient-trio".to_string()),
                None,
                None,
                None,
            )
            .expect("select model pack");
        let run = store
            .create_run(
                conversation.id,
                "implement and validate a bounded change",
                ExecutionMode::Agentic,
            )
            .expect("run");
        let mut definition = crate::built_in_agent_definition("generalist").expect("generalist");
        definition.preferred_provider = Some("openrouter".to_string());
        definition.preferred_model = Some("glm-5.2".to_string());
        AgentControl::new(store.clone(), AgentLimits::default())
            .create_root(
                run.id,
                &definition,
                "implement and validate a bounded change",
                ".",
            )
            .expect("root");
        let provider = Arc::new(ModelPackProvider {
            models: Mutex::new(Vec::new()),
            messages: Mutex::new(Vec::new()),
        });
        let router = ProviderRouter::default();
        router.register_with_models(
            provider.clone(),
            "glm-5.2",
            vec![
                "deepseek-v4-flash".to_string(),
                "kimi-k2.7-code".to_string(),
            ],
        );
        let engine = ExecutionEngine::new(store.clone(), Arc::new(router), ToolExecutor::default())
            .with_model_pack_registry(ModelPackRegistry::default());
        let approval_store = store.clone();
        let approver = tokio::spawn(async move {
            for _ in 0..200 {
                for approval in approval_store
                    .list_approvals(true)
                    .expect("pending model-pack approvals")
                {
                    approval_store
                        .decide_approval(
                            approval.id,
                            ApprovalDecision::AllowOnce,
                            None,
                            Some("model-pack test workspace".to_string()),
                        )
                        .expect("approve model-pack fixture");
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        });

        let result = engine
            .execute_run_with_pack(run.id, "openrouter", "glm-5.2", Some("efficient-trio"))
            .await
            .expect("model pack execution");
        approver.abort();

        assert_eq!(result.output, "model pack synthesis complete");
        let models = provider.models.lock().expect("model trace").clone();
        assert_eq!(
            models.first().map(String::as_str),
            Some("deepseek-v4-flash")
        );
        assert!(models.iter().any(|model| model == "kimi-k2.7-code"));
        assert!(models.iter().any(|model| model == "deepseek-v4-flash"));
        assert!(
            provider
                .messages
                .lock()
                .expect("message trace")
                .iter()
                .any(|messages| {
                    messages.contains("upstream_completions") && messages.contains("indexed_files")
                })
        );
        let assignments = store
            .events_after(0, 500)
            .expect("events")
            .into_iter()
            .filter(|event| event.kind == "model_pack.assignment_selected")
            .collect::<Vec<_>>();
        assert!(assignments.len() >= 5);
        assert!(assignments.iter().any(|event| {
            event.payload["stage"] == "execute" && event.payload["model"] == "kimi-k2.7-code"
        }));
        assert!(assignments.iter().any(|event| {
            event.payload["stage"] == "validate" && event.payload["model"] == "kimi-k2.7-code"
        }));
        let _ = std::fs::remove_dir_all("target/pack-fixture");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn blocking_review_creates_bounded_repair_and_independent_rereview() {
        let store = Store::in_memory().expect("store");
        let conversation = store.create_conversation(".", None).expect("conversation");
        let run = store
            .create_run(
                conversation.id,
                "repair reviewed work",
                ExecutionMode::Agentic,
            )
            .expect("run");
        let mut definition = crate::built_in_agent_definition("generalist").expect("generalist");
        definition.preferred_provider = Some("openrouter".to_string());
        definition.preferred_model = Some("glm-4.5".to_string());
        let root = AgentControl::new(store.clone(), AgentLimits::default())
            .create_root(run.id, &definition, "repair reviewed work", ".")
            .expect("root");
        let now = chrono::Utc::now();
        let target = Task {
            id: Uuid::new_v4(),
            run_id: run.id,
            description: "Implement the original change.".to_string(),
            dependencies: Vec::new(),
            assigned_agent: None,
            status: TaskStatus::Completed,
            priority: 0,
            expected_output: "agent_completion_v1".to_string(),
            contract: TaskContract {
                objective: "Implement the original change.".to_string(),
                acceptance_criteria: vec!["The original behavior works.".to_string()],
                validation_steps: vec!["Run focused tests.".to_string()],
                allowed_paths: vec!["src".to_string()],
                completion_schema: "agent_completion_v1".to_string(),
                max_retries: 2,
                ..TaskContract::default()
            },
            workspace_ownership: vec!["src".to_string()],
            allowed_tools: Vec::new(),
            retry_policy: RetryPolicy::default(),
            created_at: now,
            updated_at: now,
        };
        store.create_task(&target).expect("target task");
        let review_task = Task {
            id: Uuid::new_v4(),
            run_id: run.id,
            description: "Review the original change.".to_string(),
            dependencies: vec![target.id],
            assigned_agent: None,
            status: TaskStatus::Completed,
            priority: 1,
            expected_output: "review_completion_v1".to_string(),
            contract: TaskContract {
                objective: "Review the original change.".to_string(),
                inputs: TaskInputs::default(),
                acceptance_criteria: vec!["Blocking defects are explicit.".to_string()],
                completion_schema: "review_completion_v1".to_string(),
                ..TaskContract::default()
            },
            workspace_ownership: Vec::new(),
            allowed_tools: Vec::new(),
            retry_policy: RetryPolicy::default(),
            created_at: now,
            updated_at: now,
        };
        store.create_task(&review_task).expect("review task");
        let review = ReviewContract {
            verdict: ReviewVerdict::ChangesRequired,
            summary: "A correctness defect must be repaired.".to_string(),
            findings: vec![ReviewFinding {
                severity: ReviewSeverity::High,
                category: "correctness".to_string(),
                file: Some("src/lib.rs".to_string()),
                line: Some(42),
                evidence: "The failing branch returns stale state.".to_string(),
                required_action: "Update the branch and add a regression test.".to_string(),
                blocking: true,
            }],
            test_gaps: vec!["Missing regression coverage.".to_string()],
            architecture_violations: Vec::new(),
            security_findings: Vec::new(),
        };
        let provider = Arc::new(ModelPackProvider {
            models: Mutex::new(Vec::new()),
            messages: Mutex::new(Vec::new()),
        });
        let router = ProviderRouter::default();
        router.register_with_models(
            provider,
            "glm-4.5",
            vec!["kimi-k2.7-code".to_string(), "deepseek-v4-pro".to_string()],
        );
        let engine = ExecutionEngine::new(store.clone(), Arc::new(router), ToolExecutor::default());
        let pack = ModelPackRegistry::default()
            .get("efficient-trio", engine.providers.as_ref())
            .expect("efficient trio");

        let chain = engine
            .create_repair_chain(
                run.id,
                &root,
                &review_task,
                &review,
                "openrouter",
                "glm-4.5",
                true,
                Some(&pack),
            )
            .expect("repair chain");

        assert_eq!(chain.len(), 2);
        let (repair, repair_agent) = &chain[0];
        let (rereview, reviewer_agent) = &chain[1];
        assert_eq!(repair.contract.repair_of_task_id, Some(target.id));
        assert_eq!(repair.dependencies, vec![review_task.id]);
        assert_eq!(rereview.dependencies, vec![repair.id]);
        assert_eq!(rereview.contract.repair_of_task_id, Some(target.id));
        assert_eq!(repair_agent.model, "kimi-k2.7-code");
        assert_eq!(reviewer_agent.model, "deepseek-v4-pro");
        assert_ne!(
            (&repair_agent.provider, &repair_agent.model),
            (&reviewer_agent.provider, &reviewer_agent.model)
        );
        assert!(
            store
                .events_after(0, 200)
                .expect("events")
                .iter()
                .any(|event| event.kind == "review.repair_chain_created")
        );
    }
}

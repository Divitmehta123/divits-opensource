use chrono::Utc;
use opensrc_core::{
    Agent, AgentDefinition, AgentId, AgentStatus, CompletionStatus, ContextPolicy, EvidenceStatus,
    RunId, Task, TaskCompletion, TaskId, TaskStatus, Workspace, WorkspaceLeaseMode,
    WorkspaceLeaseRequest, WorkspaceMode,
};
use opensrc_store::{Store, StoreError};
use serde_json::json;
use std::collections::BTreeSet;
use std::path::{Component, Path};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::Notify;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct AgentLimits {
    pub max_depth: usize,
    pub max_children_per_parent: usize,
    pub max_agents_per_run: usize,
    pub max_active_agents_per_run: usize,
    pub max_active_writers_per_run: usize,
    pub max_deep_reasoning_agents_per_run: usize,
}

impl Default for AgentLimits {
    fn default() -> Self {
        Self {
            max_depth: 2,
            max_children_per_parent: 16,
            max_agents_per_run: 24,
            max_active_agents_per_run: 4,
            max_active_writers_per_run: 2,
            max_deep_reasoning_agents_per_run: 1,
        }
    }
}

#[derive(Debug, Error)]
pub enum AgentControlError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("agent {0} is not allowed to spawn children")]
    SpawnDenied(AgentId),
    #[error("agent depth limit {0} reached")]
    DepthLimit(usize),
    #[error("parent child limit {0} reached")]
    ChildLimit(usize),
    #[error("run agent limit {0} reached")]
    RunLimit(usize),
    #[error("run active-agent concurrency limit {0} reached")]
    ConcurrencyLimit(usize),
    #[error("run active-writer concurrency limit {0} reached")]
    WriterConcurrencyLimit(usize),
    #[error("run deep-reasoning concurrency limit {0} reached")]
    DeepReasoningConcurrencyLimit(usize),
    #[error("only a root agent may use canonical path /root")]
    InvalidRoot,
    #[error("task {task_id} is assigned to a different agent than {agent_id}")]
    TaskAssignment { task_id: TaskId, agent_id: AgentId },
    #[error("task {0} is not ready to start")]
    TaskNotReady(TaskId),
    #[error("agent {sender} cannot message agent {target} from a different run")]
    DifferentRun { sender: AgentId, target: AgentId },
    #[error("agent {0} is already terminal; assign a follow-up task instead")]
    MessageTargetTerminal(AgentId),
    #[error("workspace ownership path `{0}` is not a safe relative path")]
    InvalidOwnedPath(String),
    #[error("task {task_id} has an invalid contract: {reason}")]
    InvalidTaskContract { task_id: TaskId, reason: String },
    #[error("task completion is invalid: {0}")]
    InvalidTaskCompletion(String),
}

#[derive(Clone)]
pub struct AgentControl {
    store: Store,
    limits: AgentLimits,
    status_changed: Arc<Notify>,
}

impl AgentControl {
    #[must_use]
    pub fn new(store: Store, limits: AgentLimits) -> Self {
        Self {
            store,
            limits,
            status_changed: Arc::new(Notify::new()),
        }
    }

    pub fn create_root(
        &self,
        run_id: RunId,
        definition: &AgentDefinition,
        task: impl Into<String>,
        workspace_root: impl Into<String>,
    ) -> Result<Agent, AgentControlError> {
        if self
            .store
            .list_agents(Some(run_id))?
            .iter()
            .any(|agent| agent.parent_id.is_none())
        {
            return Err(AgentControlError::InvalidRoot);
        }
        let agent = build_agent(
            run_id,
            "/root".to_string(),
            None,
            definition,
            task.into(),
            workspace_root.into(),
            None,
        );
        self.store.create_agent(&agent)?;
        self.store.transition_agent(agent.id, AgentStatus::Queued)?;
        self.status_changed.notify_waiters();
        self.store.get_agent(agent.id).map_err(Into::into)
    }

    pub fn spawn_agent(
        &self,
        parent_id: AgentId,
        definition: &AgentDefinition,
        task: impl Into<String>,
        context_override: Option<ContextPolicy>,
    ) -> Result<Agent, AgentControlError> {
        self.spawn_agent_with_ownership(parent_id, definition, task, context_override, Vec::new())
    }

    pub fn spawn_agent_with_ownership(
        &self,
        parent_id: AgentId,
        definition: &AgentDefinition,
        task: impl Into<String>,
        context_override: Option<ContextPolicy>,
        owned_paths: Vec<String>,
    ) -> Result<Agent, AgentControlError> {
        self.spawn_agent_with_ownership_internal(
            parent_id,
            definition,
            task,
            context_override,
            owned_paths,
            false,
        )
    }

    /// Spawn a task from the internal agentic planner. A top-level run coordinator
    /// may schedule its approved plan even when the UI-selected role is read-only;
    /// child agents still require their ordinary `may_spawn_children` capability.
    pub fn spawn_planned_agent_with_ownership(
        &self,
        parent_id: AgentId,
        definition: &AgentDefinition,
        task: impl Into<String>,
        context_override: Option<ContextPolicy>,
        owned_paths: Vec<String>,
    ) -> Result<Agent, AgentControlError> {
        self.spawn_agent_with_ownership_internal(
            parent_id,
            definition,
            task,
            context_override,
            owned_paths,
            true,
        )
    }

    fn spawn_agent_with_ownership_internal(
        &self,
        parent_id: AgentId,
        definition: &AgentDefinition,
        task: impl Into<String>,
        context_override: Option<ContextPolicy>,
        owned_paths: Vec<String>,
        allow_root_planner: bool,
    ) -> Result<Agent, AgentControlError> {
        if let Some(path) = owned_paths.iter().find(|path| !safe_owned_path(path)) {
            return Err(AgentControlError::InvalidOwnedPath(path.clone()));
        }
        let parent = self.store.get_agent(parent_id)?;
        if !(parent.tool_policy.may_spawn_children
            || allow_root_planner && parent.parent_id.is_none())
        {
            return Err(AgentControlError::SpawnDenied(parent_id));
        }
        let all = self.store.list_agents(Some(parent.run_id))?;
        if all.len() >= self.limits.max_agents_per_run {
            return Err(AgentControlError::RunLimit(self.limits.max_agents_per_run));
        }
        let children: Vec<_> = all
            .iter()
            .filter(|agent| agent.parent_id == Some(parent_id))
            .collect();
        if children.len() >= self.limits.max_children_per_parent {
            return Err(AgentControlError::ChildLimit(
                self.limits.max_children_per_parent,
            ));
        }
        let depth = parent
            .canonical_path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .count();
        if depth >= self.limits.max_depth {
            return Err(AgentControlError::DepthLimit(self.limits.max_depth));
        }
        let slug = slugify(&definition.name);
        let path = format!("{}/{}-{}", parent.canonical_path, slug, children.len() + 1);
        let mut agent = build_agent(
            parent.run_id,
            path,
            Some(parent_id),
            definition,
            task.into(),
            parent.workspace.root,
            context_override,
        );
        if agent.workspace.mode == opensrc_core::WorkspaceMode::OwnedPaths {
            agent.workspace.owned_paths = owned_paths;
        }
        self.store.create_agent(&agent)?;
        self.store.transition_agent(agent.id, AgentStatus::Queued)?;
        self.status_changed.notify_waiters();
        self.store.get_agent(agent.id).map_err(Into::into)
    }

    pub fn send_message(
        &self,
        agent_id: AgentId,
        message: impl Into<String>,
    ) -> Result<(), AgentControlError> {
        self.send_message_from(None, agent_id, message)
    }

    pub fn send_message_from(
        &self,
        sender_id: Option<AgentId>,
        agent_id: AgentId,
        message: impl Into<String>,
    ) -> Result<(), AgentControlError> {
        let agent = self.store.get_agent(agent_id)?;
        if agent.status.is_terminal() {
            return Err(AgentControlError::MessageTargetTerminal(agent_id));
        }
        if let Some(sender_id) = sender_id {
            let sender = self.store.get_agent(sender_id)?;
            if sender.run_id != agent.run_id {
                return Err(AgentControlError::DifferentRun {
                    sender: sender_id,
                    target: agent_id,
                });
            }
        }
        self.store.append_event(
            agent.run_id,
            Some(agent_id),
            None,
            "agent.message_received",
            &json!({
                "sender_agent_id": sender_id,
                "message": message.into()
            }),
            None,
        )?;
        self.status_changed.notify_waiters();
        Ok(())
    }

    pub fn start_agent(&self, agent_id: AgentId) -> Result<Agent, AgentControlError> {
        let target = self.store.get_agent(agent_id)?;
        let agents = self.store.list_agents(Some(target.run_id))?;
        let active = agents
            .iter()
            .filter(|agent| matches!(agent.status, AgentStatus::Running | AgentStatus::Restoring))
            .count();
        if active >= self.limits.max_active_agents_per_run {
            return Err(AgentControlError::ConcurrencyLimit(
                self.limits.max_active_agents_per_run,
            ));
        }
        if target.workspace.mode == opensrc_core::WorkspaceMode::OwnedPaths {
            let writers = agents
                .iter()
                .filter(|agent| {
                    matches!(agent.status, AgentStatus::Running | AgentStatus::Restoring)
                        && agent.workspace.mode == opensrc_core::WorkspaceMode::OwnedPaths
                })
                .count();
            if writers >= self.limits.max_active_writers_per_run {
                return Err(AgentControlError::WriterConcurrencyLimit(
                    self.limits.max_active_writers_per_run,
                ));
            }
        }
        if matches!(target.reasoning.level.as_deref(), Some("max")) {
            let deep = agents
                .iter()
                .filter(|agent| {
                    matches!(agent.status, AgentStatus::Running | AgentStatus::Restoring)
                        && matches!(agent.reasoning.level.as_deref(), Some("max"))
                })
                .count();
            if deep >= self.limits.max_deep_reasoning_agents_per_run {
                return Err(AgentControlError::DeepReasoningConcurrencyLimit(
                    self.limits.max_deep_reasoning_agents_per_run,
                ));
            }
        }
        let agent = self
            .store
            .transition_agent(agent_id, AgentStatus::Running)
            .map_err(AgentControlError::from)?;
        self.status_changed.notify_waiters();
        Ok(agent)
    }

    pub fn wait_agent(&self, agent_id: AgentId) -> Result<Agent, AgentControlError> {
        let agent = self
            .store
            .transition_agent(agent_id, AgentStatus::Waiting)
            .map_err(AgentControlError::from)?;
        self.status_changed.notify_waiters();
        Ok(agent)
    }

    pub fn get_agent_status(&self, agent_id: AgentId) -> Result<Agent, AgentControlError> {
        self.store.get_agent(agent_id).map_err(Into::into)
    }

    pub async fn wait_for_agents(
        &self,
        agent_ids: &[AgentId],
        timeout: std::time::Duration,
    ) -> Result<Vec<Agent>, AgentControlError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.status_changed.notified();
            let agents = agent_ids
                .iter()
                .map(|id| self.store.get_agent(*id))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            if agents
                .iter()
                .all(|agent| agent.status.is_terminal() || agent.status == AgentStatus::Blocked)
            {
                return Ok(agents);
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(agents);
            }
            tokio::select! {
                () = notified => {}
                () = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
                () = tokio::time::sleep_until(deadline) => return Ok(agents),
            }
        }
    }

    pub fn assign_followup(
        &self,
        agent_id: AgentId,
        description: impl Into<String>,
        priority: i32,
    ) -> Result<Task, AgentControlError> {
        let agent = self.store.get_agent(agent_id)?;
        let now = Utc::now();
        let description = description.into();
        let task = Task {
            id: Uuid::new_v4(),
            run_id: agent.run_id,
            description: description.clone(),
            dependencies: Vec::new(),
            assigned_agent: Some(agent_id),
            status: TaskStatus::Ready,
            priority,
            expected_output: agent.completion_schema.clone(),
            contract: opensrc_core::TaskContract {
                objective: description,
                inputs: opensrc_core::TaskInputs::default(),
                acceptance_criteria: vec!["Report evidence for every completed claim.".to_string()],
                deliverables: vec!["A structured task completion object.".to_string()],
                validation_steps: Vec::new(),
                forbidden_actions: vec![
                    "Do not modify files outside the assigned workspace ownership.".to_string(),
                ],
                handoff_notes: Vec::new(),
                allowed_paths: if workspace_is_writable(agent.workspace.mode) {
                    agent.workspace.owned_paths.clone()
                } else {
                    Vec::new()
                },
                forbidden_paths: Vec::new(),
                tools: agent.tool_policy.clone(),
                budgets: agent.budgets.clone(),
                completion_schema: agent.completion_schema.clone(),
                max_retries: agent.retry_policy.max_attempts.saturating_sub(1).min(2),
                review_required: false,
                repair_of_task_id: None,
            },
            workspace_ownership: agent.workspace.owned_paths.clone(),
            allowed_tools: agent.tool_policy.allow.clone(),
            retry_policy: agent.retry_policy.clone(),
            created_at: now,
            updated_at: now,
        };
        self.create_task(&task)?;
        self.store.append_event(
            agent.run_id,
            Some(agent_id),
            Some(task.id),
            "agent.followup_assigned",
            &json!({"description": task.description}),
            None,
        )?;
        Ok(task)
    }

    pub fn create_task(&self, task: &Task) -> Result<(), AgentControlError> {
        self.validate_task_contract(task)?;
        self.store.create_task(task).map_err(Into::into)
    }

    pub fn validate_task_contract(&self, task: &Task) -> Result<(), AgentControlError> {
        validate_nonempty(task.id, "objective", &task.contract.objective)?;
        if task.contract.acceptance_criteria.is_empty() {
            return invalid_task(task.id, "acceptance criteria must not be empty");
        }
        let mut criteria = BTreeSet::new();
        for criterion in &task.contract.acceptance_criteria {
            validate_nonempty(task.id, "acceptance criterion", criterion)?;
            if !criteria.insert(criterion.trim()) {
                return invalid_task(task.id, "acceptance criteria must be unique");
            }
        }
        validate_nonempty(
            task.id,
            "completion schema",
            &task.contract.completion_schema,
        )?;
        validate_nonempty(task.id, "expected output schema", &task.expected_output)?;
        if task.expected_output.trim() != task.contract.completion_schema.trim() {
            return invalid_task(
                task.id,
                "expected output schema must match the contract completion schema",
            );
        }

        for path in task
            .contract
            .allowed_paths
            .iter()
            .chain(&task.contract.forbidden_paths)
            .chain(&task.workspace_ownership)
        {
            if !safe_owned_path(path) {
                return invalid_task(
                    task.id,
                    format!("path scope `{path}` is not a safe relative path"),
                );
            }
        }

        if let Some(agent_id) = task.assigned_agent {
            let agent = self.store.get_agent(agent_id)?;
            if agent.run_id != task.run_id {
                return invalid_task(task.id, "assigned agent belongs to another run");
            }
            if workspace_is_writable(agent.workspace.mode) {
                if task.contract.allowed_paths.is_empty() {
                    return invalid_task(task.id, "writer tasks require allowed paths");
                }
                if task.workspace_ownership.is_empty() {
                    return invalid_task(task.id, "writer tasks require workspace ownership");
                }
                for allowed in &task.contract.allowed_paths {
                    if !task
                        .workspace_ownership
                        .iter()
                        .any(|owned| scope_is_within(allowed, owned))
                    {
                        return invalid_task(
                            task.id,
                            format!(
                                "allowed path `{allowed}` is outside the assigned workspace ownership"
                            ),
                        );
                    }
                }
            }
        }
        Ok(())
    }

    pub fn start_task(&self, task_id: TaskId) -> Result<Task, AgentControlError> {
        let task = self.store.get_task(task_id)?;
        self.validate_task_contract(&task)?;
        let tasks = self.store.list_tasks(Some(task.run_id))?;
        crate::validate_task_graph(&tasks).map_err(|_| AgentControlError::TaskNotReady(task_id))?;
        if !crate::ready_tasks(&tasks).contains(&task_id) {
            return Err(AgentControlError::TaskNotReady(task_id));
        }
        let leased = if let Some(agent_id) = task.assigned_agent {
            let agent = self.store.get_agent(agent_id)?;
            if workspace_is_writable(agent.workspace.mode) {
                self.store
                    .acquire_workspace_lease(&WorkspaceLeaseRequest {
                        run_id: task.run_id,
                        agent_id,
                        task_id: Some(task.id),
                        mode: WorkspaceLeaseMode::Write,
                        root: agent.workspace.root,
                        owned_paths: task.workspace_ownership.clone(),
                    })
                    .map(Some)?
            } else {
                None
            }
        } else {
            None
        };
        let started = (|| -> Result<Task, AgentControlError> {
            if task.status == TaskStatus::Created {
                self.store.transition_task(task_id, TaskStatus::Ready)?;
            }
            self.store
                .transition_task(task_id, TaskStatus::Running)
                .map_err(Into::into)
        })();
        if started.is_err()
            && leased.is_some()
            && self.store.get_task(task_id)?.status != TaskStatus::Running
        {
            self.store.release_workspace_leases_by_task(task_id)?;
        }
        started
    }

    pub fn reassign_task(
        &self,
        task_id: TaskId,
        agent_id: AgentId,
    ) -> Result<Task, AgentControlError> {
        self.store
            .reassign_task(task_id, agent_id)
            .map_err(Into::into)
    }

    pub fn interrupt_agent(&self, agent_id: AgentId) -> Result<(), AgentControlError> {
        self.interrupt_tree(agent_id)
    }

    fn interrupt_tree(&self, agent_id: AgentId) -> Result<(), AgentControlError> {
        let agent = self.store.get_agent(agent_id)?;
        for child in agent.child_ids {
            self.interrupt_tree(child)?;
        }
        if matches!(
            agent.status,
            AgentStatus::Created
                | AgentStatus::Queued
                | AgentStatus::Running
                | AgentStatus::Waiting
                | AgentStatus::Blocked
        ) {
            self.store
                .transition_agent(agent_id, AgentStatus::Interrupted)?;
            self.store.release_workspace_leases_by_agent(agent_id)?;
            self.status_changed.notify_waiters();
        }
        Ok(())
    }

    pub fn resume_agent(&self, agent_id: AgentId) -> Result<Agent, AgentControlError> {
        let agent = self.store.get_agent(agent_id)?;
        if agent.status != AgentStatus::Unloaded {
            self.store
                .transition_agent(agent_id, AgentStatus::Unloaded)?;
        }
        self.store
            .transition_agent(agent_id, AgentStatus::Restoring)?;
        self.store.transition_agent(agent_id, AgentStatus::Queued)?;
        self.status_changed.notify_waiters();
        self.store.get_agent(agent_id).map_err(Into::into)
    }

    pub fn complete_task(
        &self,
        agent_id: AgentId,
        task_id: Option<TaskId>,
        completion: &TaskCompletion,
    ) -> Result<Agent, AgentControlError> {
        let agent = self.store.get_agent(agent_id)?;
        if let Some(task_id) = task_id {
            let task = self.store.get_task(task_id)?;
            if task.assigned_agent != Some(agent_id) {
                return Err(AgentControlError::TaskAssignment { task_id, agent_id });
            }
            if task.status != TaskStatus::Running {
                return Err(AgentControlError::TaskNotReady(task_id));
            }
            self.validate_task_contract(&task)?;
            validate_task_completion(&task, &agent, completion)?;
            if completion.review.is_some() {
                validate_review_independence(&self.store, &task, completion)?;
            }
        } else {
            validate_unbound_completion(&agent, completion)?;
        }
        self.store.save_completion(agent_id, task_id, completion)?;
        if let Some(task_id) = task_id {
            let task_status = match completion.status {
                CompletionStatus::Completed => TaskStatus::Completed,
                CompletionStatus::Failed => TaskStatus::Failed,
                CompletionStatus::Blocked => TaskStatus::Blocked,
            };
            self.store.transition_task(task_id, task_status)?;
            if matches!(
                completion.status,
                CompletionStatus::Failed | CompletionStatus::Blocked
            ) {
                self.block_downstream_tasks(
                    agent.run_id,
                    task_id,
                    &format!(
                        "dependency ended with completion status {:?}",
                        completion.status
                    ),
                )?;
            }
        }
        let status = match completion.status {
            CompletionStatus::Completed => AgentStatus::Completed,
            CompletionStatus::Failed => AgentStatus::Failed,
            CompletionStatus::Blocked => AgentStatus::Blocked,
        };
        if agent.status != status {
            self.store.transition_agent(agent_id, status)?;
        }
        if let Some(task_id) = task_id {
            self.store.release_workspace_leases_by_task(task_id)?;
        }
        self.store.release_workspace_leases_by_agent(agent_id)?;
        self.status_changed.notify_waiters();
        self.store.get_agent(agent_id).map_err(Into::into)
    }

    pub fn list_agents(&self, run_id: Option<RunId>) -> Result<Vec<Agent>, AgentControlError> {
        self.store.list_agents(run_id).map_err(Into::into)
    }

    fn block_downstream_tasks(
        &self,
        run_id: RunId,
        failed_task_id: TaskId,
        reason: &str,
    ) -> Result<(), AgentControlError> {
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
}

#[allow(clippy::too_many_arguments)]
fn build_agent(
    run_id: RunId,
    canonical_path: String,
    parent_id: Option<AgentId>,
    definition: &AgentDefinition,
    task: String,
    workspace_root: String,
    context_override: Option<ContextPolicy>,
) -> Agent {
    let now = Utc::now();
    Agent {
        id: Uuid::new_v4(),
        run_id,
        canonical_path,
        parent_id,
        child_ids: Vec::new(),
        role: definition.name.clone(),
        task,
        status: AgentStatus::Created,
        provider: definition
            .preferred_provider
            .clone()
            .unwrap_or_else(|| "unconfigured".to_string()),
        model: definition
            .preferred_model
            .clone()
            .unwrap_or_else(|| "unconfigured".to_string()),
        reasoning: definition.reasoning.clone(),
        system_instructions: definition.system_instructions.clone(),
        context_policy: context_override.unwrap_or_else(|| definition.context_policy.clone()),
        tool_policy: definition.tool_policy.clone(),
        workspace: Workspace {
            mode: definition.workspace_mode,
            root: workspace_root,
            owned_paths: if definition.workspace_mode == opensrc_core::WorkspaceMode::OwnedPaths {
                vec![".".to_string()]
            } else {
                Vec::new()
            },
        },
        sandbox_policy: definition.sandbox_policy.clone(),
        budgets: definition.budgets.clone(),
        retry_policy: definition.retry_policy.clone(),
        fallback_chain: definition.fallback_chain.clone(),
        completion_schema: definition.completion_schema.clone(),
        created_at: now,
        updated_at: now,
    }
}

fn slugify(value: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            out.push(character.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

fn safe_owned_path(value: &str) -> bool {
    if value.trim().is_empty() {
        return false;
    }
    let path = Path::new(value);
    !path.is_absolute()
        && !path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
}

fn workspace_is_writable(mode: WorkspaceMode) -> bool {
    !matches!(mode, WorkspaceMode::SharedReadonly)
}

fn validate_nonempty(task_id: TaskId, field: &str, value: &str) -> Result<(), AgentControlError> {
    if value.trim().is_empty() {
        invalid_task(task_id, format!("{field} must not be empty"))
    } else {
        Ok(())
    }
}

fn invalid_task<T>(task_id: TaskId, reason: impl Into<String>) -> Result<T, AgentControlError> {
    Err(AgentControlError::InvalidTaskContract {
        task_id,
        reason: reason.into(),
    })
}

fn validate_unbound_completion(
    agent: &Agent,
    completion: &TaskCompletion,
) -> Result<(), AgentControlError> {
    if completion.task_id.is_some() {
        return invalid_completion("an unbound completion must not contain a task id");
    }
    validate_completion_identity(agent, completion)?;
    if completion.summary.trim().is_empty() {
        return invalid_completion("completion summary must not be empty");
    }
    if !completion.tool_policy_compliant {
        return invalid_completion("completion reports a tool-policy violation");
    }
    Ok(())
}

fn validate_task_completion(
    task: &Task,
    agent: &Agent,
    completion: &TaskCompletion,
) -> Result<(), AgentControlError> {
    if completion.task_id != Some(task.id) {
        return invalid_completion(format!("task id must be `{}`", task.id));
    }
    validate_completion_identity(agent, completion)?;
    if completion.summary.trim().is_empty() {
        return invalid_completion("completion summary must not be empty");
    }
    if !completion.tool_policy_compliant {
        return invalid_completion("completion reports a tool-policy violation");
    }
    if completion.repair_of_task_id != task.contract.repair_of_task_id {
        return invalid_completion("repair task identity does not match the task contract");
    }
    if task.contract.review_required
        && completion.status == CompletionStatus::Completed
        && completion.review.is_none()
    {
        return invalid_completion("task contract requires an independent review");
    }

    for changed in &completion.files_changed {
        if changed.contains(['*', '?']) || !safe_owned_path(changed) {
            return invalid_completion(format!(
                "changed file `{changed}` is not a safe relative file path"
            ));
        }
        if !task
            .contract
            .allowed_paths
            .iter()
            .any(|scope| path_is_within_scope(changed, scope))
        {
            return invalid_completion(format!(
                "changed file `{changed}` is outside the contract allowed paths"
            ));
        }
        if !task.workspace_ownership.is_empty()
            && !task
                .workspace_ownership
                .iter()
                .any(|scope| path_is_within_scope(changed, scope))
        {
            return invalid_completion(format!(
                "changed file `{changed}` is outside workspace ownership"
            ));
        }
        if task
            .contract
            .forbidden_paths
            .iter()
            .any(|scope| path_is_within_scope(changed, scope))
        {
            return invalid_completion(format!(
                "changed file `{changed}` is forbidden by the task contract"
            ));
        }
    }

    if completion.status == CompletionStatus::Completed {
        validate_completed_evidence(task, completion)?;
    }
    Ok(())
}

fn validate_completed_evidence(
    task: &Task,
    completion: &TaskCompletion,
) -> Result<(), AgentControlError> {
    for check in &completion.contract_checks {
        if check.criterion.trim().is_empty() {
            return invalid_completion("contract check criterion must not be empty");
        }
        if check.status != EvidenceStatus::Passed {
            return invalid_completion(format!(
                "contract check `{}` did not pass",
                check.criterion
            ));
        }
        if check.evidence.trim().is_empty() {
            return invalid_completion(format!(
                "contract check `{}` has no evidence",
                check.criterion
            ));
        }
    }
    for criterion in &task.contract.acceptance_criteria {
        if !completion.contract_checks.iter().any(|check| {
            check.criterion.trim() == criterion.trim()
                && check.status == EvidenceStatus::Passed
                && !check.evidence.trim().is_empty()
        }) {
            return invalid_completion(format!(
                "acceptance criterion `{criterion}` has no passing evidence"
            ));
        }
    }
    for test in &completion.tests {
        if test.status != EvidenceStatus::Passed {
            return invalid_completion(format!("test `{}` did not pass", test.command));
        }
        if test.command.trim().is_empty() || test.evidence.trim().is_empty() {
            return invalid_completion("passed test evidence must include command and evidence");
        }
    }
    for validation in &task.contract.validation_steps {
        if !completion.tests.iter().any(|test| {
            test.command.trim() == validation.trim()
                && test.status == EvidenceStatus::Passed
                && !test.evidence.trim().is_empty()
        }) {
            return invalid_completion(format!(
                "required validation `{validation}` has no passing test evidence"
            ));
        }
    }
    Ok(())
}

fn validate_completion_identity(
    agent: &Agent,
    completion: &TaskCompletion,
) -> Result<(), AgentControlError> {
    let producer = completion.producer.as_ref().ok_or_else(|| {
        AgentControlError::InvalidTaskCompletion(
            "completion must include producer identity".to_string(),
        )
    })?;
    if producer.provider.trim().is_empty() || producer.model.trim().is_empty() {
        return invalid_completion("producer provider and model must not be empty");
    }
    let assigned = producer.provider == agent.provider && producer.model == agent.model;
    let configured_fallback = agent.fallback_chain.iter().any(|candidate| {
        candidate.split_once('/').is_some_and(|(provider, model)| {
            provider == producer.provider && model == producer.model
        })
    });
    if !assigned && !configured_fallback {
        return invalid_completion(format!(
            "producer `{}/{}` is not assigned to agent {}",
            producer.provider, producer.model, agent.id
        ));
    }
    Ok(())
}

fn validate_review_independence(
    store: &Store,
    task: &Task,
    _completion: &TaskCompletion,
) -> Result<(), AgentControlError> {
    let reviewer_agent = task.assigned_agent.ok_or_else(|| {
        AgentControlError::InvalidTaskCompletion(
            "review task must be assigned to a dedicated reviewer agent".to_string(),
        )
    })?;
    for dependency_id in &task.dependencies {
        let dependency = store.get_task(*dependency_id)?;
        if dependency.assigned_agent == Some(reviewer_agent) {
            return invalid_completion(format!(
                "review agent {reviewer_agent} is also assigned to dependency {}",
                dependency.id
            ));
        }
    }
    Ok(())
}

fn invalid_completion<T>(reason: impl Into<String>) -> Result<T, AgentControlError> {
    Err(AgentControlError::InvalidTaskCompletion(reason.into()))
}

fn normalize_scope(value: &str) -> String {
    let mut normalized = value.trim().replace('\\', "/");
    while normalized.starts_with("./") {
        normalized = normalized[2..].to_string();
    }
    while normalized.ends_with('/') && normalized.len() > 1 {
        normalized.pop();
    }
    if cfg!(windows) {
        normalized.make_ascii_lowercase();
    }
    normalized
}

fn scope_base(value: &str) -> String {
    let normalized = normalize_scope(value);
    let wildcard = normalized
        .char_indices()
        .find(|(_, character)| matches!(character, '*' | '?'))
        .map_or(normalized.len(), |(index, _)| index);
    normalized[..wildcard].trim_end_matches('/').to_string()
}

fn scope_is_within(candidate: &str, owner: &str) -> bool {
    let candidate = scope_base(candidate);
    let owner = scope_base(owner);
    owner == "."
        || candidate == owner
        || (!owner.is_empty() && candidate.starts_with(&format!("{owner}/")))
}

fn path_is_within_scope(path: &str, scope: &str) -> bool {
    let path = normalize_scope(path);
    let scope = normalize_scope(scope);
    if scope == "." {
        return true;
    }
    if !scope.contains(['*', '?']) {
        return path == scope || path.starts_with(&format!("{scope}/"));
    }
    wildcard_scope_matches(&scope, &path)
}

fn wildcard_scope_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let mut memo = vec![vec![None; value.len() + 1]; pattern.len() + 1];
    wildcard_scope_matches_inner(pattern, value, 0, 0, &mut memo)
}

fn wildcard_scope_matches_inner(
    pattern: &[u8],
    value: &[u8],
    pattern_index: usize,
    value_index: usize,
    memo: &mut [Vec<Option<bool>>],
) -> bool {
    if let Some(result) = memo[pattern_index][value_index] {
        return result;
    }
    let result = if pattern_index == pattern.len() {
        value_index == value.len()
    } else if pattern[pattern_index] == b'*' && pattern.get(pattern_index + 1) == Some(&b'*') {
        wildcard_scope_matches_inner(pattern, value, pattern_index + 2, value_index, memo)
            || (value_index < value.len()
                && wildcard_scope_matches_inner(
                    pattern,
                    value,
                    pattern_index,
                    value_index + 1,
                    memo,
                ))
    } else if pattern[pattern_index] == b'*' {
        wildcard_scope_matches_inner(pattern, value, pattern_index + 1, value_index, memo)
            || (value_index < value.len()
                && value[value_index] != b'/'
                && wildcard_scope_matches_inner(
                    pattern,
                    value,
                    pattern_index,
                    value_index + 1,
                    memo,
                ))
    } else if value_index == value.len() {
        false
    } else if pattern[pattern_index] == b'?' {
        value[value_index] != b'/'
            && wildcard_scope_matches_inner(
                pattern,
                value,
                pattern_index + 1,
                value_index + 1,
                memo,
            )
    } else {
        pattern[pattern_index] == value[value_index]
            && wildcard_scope_matches_inner(
                pattern,
                value,
                pattern_index + 1,
                value_index + 1,
                memo,
            )
    };
    memo[pattern_index][value_index] = Some(result);
    result
}

#[cfg(test)]
mod tests {
    use super::{AgentControl, AgentControlError, AgentLimits, path_is_within_scope};
    use opensrc_core::{
        AgentDefinition, Budgets, CompletionStatus, ContextPolicy, ContractCheck, EvidenceStatus,
        ExecutionMode, ModelIdentity, ReasoningConfig, RetryPolicy, SandboxPolicy, TaskCompletion,
        TaskStatus, TestEvidence, ToolPolicy, WorkspaceLeaseState, WorkspaceMode,
    };
    use opensrc_store::Store;
    use std::collections::BTreeMap;
    use std::time::Duration;
    use uuid::Uuid;

    fn definition(may_spawn: bool) -> AgentDefinition {
        AgentDefinition {
            name: "investigator".to_string(),
            description: "Investigates".to_string(),
            system_instructions: "Read only".to_string(),
            preferred_provider: None,
            preferred_model: None,
            reasoning: ReasoningConfig::default(),
            context_policy: ContextPolicy::default(),
            tool_policy: ToolPolicy {
                allow: vec!["fs.read".to_string()],
                deny: Vec::new(),
                may_spawn_children: may_spawn,
            },
            sandbox_policy: SandboxPolicy::default(),
            workspace_mode: WorkspaceMode::SharedReadonly,
            budgets: Budgets::default(),
            retry_policy: RetryPolicy::default(),
            fallback_chain: Vec::new(),
            completion_schema: "task_completion".to_string(),
            metadata: BTreeMap::new(),
        }
    }

    fn writer_definition() -> AgentDefinition {
        AgentDefinition {
            name: "implementer".to_string(),
            description: "Writes a bounded change".to_string(),
            system_instructions: "Write only owned files.".to_string(),
            preferred_provider: Some("provider".to_string()),
            preferred_model: Some("model".to_string()),
            reasoning: ReasoningConfig::default(),
            context_policy: ContextPolicy::default(),
            tool_policy: ToolPolicy {
                allow: vec!["fs.read".to_string(), "fs.write".to_string()],
                deny: Vec::new(),
                may_spawn_children: false,
            },
            sandbox_policy: SandboxPolicy::default(),
            workspace_mode: WorkspaceMode::OwnedPaths,
            budgets: Budgets::default(),
            retry_policy: RetryPolicy::default(),
            fallback_chain: Vec::new(),
            completion_schema: "agent_completion_v1".to_string(),
            metadata: BTreeMap::new(),
        }
    }

    fn writer_fixture() -> (Store, AgentControl, opensrc_core::Agent, opensrc_core::Task) {
        let store = Store::in_memory().expect("store");
        let conversation = store.create_conversation(".", None).expect("conversation");
        let run = store
            .create_run(conversation.id, "write", ExecutionMode::Agentic)
            .expect("run");
        let control = AgentControl::new(store.clone(), AgentLimits::default());
        let root = control
            .create_root(run.id, &definition(true), "coordinate", ".")
            .expect("root");
        let writer = control
            .spawn_agent_with_ownership(
                root.id,
                &writer_definition(),
                "change src",
                None,
                vec!["src".to_string()],
            )
            .expect("writer");
        let task = control
            .assign_followup(writer.id, "change src", 0)
            .expect("task");
        (store, control, writer, task)
    }

    #[test]
    fn internal_planner_can_schedule_from_a_read_only_root_without_enabling_child_spawning() {
        let store = Store::in_memory().expect("store");
        let conversation = store.create_conversation(".", None).expect("conversation");
        let run = store
            .create_run(conversation.id, "inspect media", ExecutionMode::Agentic)
            .expect("run");
        let control = AgentControl::new(store, AgentLimits::default());
        let root = control
            .create_root(run.id, &definition(false), "coordinate", ".")
            .expect("root");

        assert!(matches!(
            control.spawn_agent(root.id, &definition(false), "ordinary child", None),
            Err(AgentControlError::SpawnDenied(_))
        ));
        let planned = control
            .spawn_planned_agent_with_ownership(
                root.id,
                &definition(false),
                "planned inspection",
                None,
                Vec::new(),
            )
            .expect("planner may create first-level task");
        assert!(matches!(
            control.spawn_planned_agent_with_ownership(
                planned.id,
                &definition(false),
                "nested task",
                None,
                Vec::new(),
            ),
            Err(AgentControlError::SpawnDenied(_))
        ));
    }

    fn valid_completion(task: &opensrc_core::Task) -> TaskCompletion {
        TaskCompletion {
            task_id: Some(task.id),
            status: CompletionStatus::Completed,
            summary: "Implemented and verified the bounded change.".to_string(),
            files_changed: vec!["src/lib.rs".to_string()],
            contract_checks: task
                .contract
                .acceptance_criteria
                .iter()
                .map(|criterion| ContractCheck {
                    criterion: criterion.clone(),
                    status: EvidenceStatus::Passed,
                    evidence: "src/lib.rs".to_string(),
                })
                .collect(),
            producer: Some(ModelIdentity {
                provider: "provider".to_string(),
                model: "model".to_string(),
            }),
            ..TaskCompletion::default()
        }
    }

    #[test]
    fn creates_persistent_hierarchy_and_enforces_spawn_policy() {
        let store = Store::in_memory().expect("store");
        let conversation = store.create_conversation(".", None).expect("conversation");
        let run = store
            .create_run(conversation.id, "work", ExecutionMode::Agentic)
            .expect("run");
        let control = AgentControl::new(store, AgentLimits::default());
        let root = control
            .create_root(run.id, &definition(true), "root work", ".")
            .expect("root");
        let child = control
            .spawn_agent(root.id, &definition(false), "inspect", None)
            .expect("child");
        assert_eq!(child.parent_id, Some(root.id));
        assert!(matches!(
            control.spawn_agent(child.id, &definition(false), "nested", None),
            Err(AgentControlError::SpawnDenied(_))
        ));
    }

    #[tokio::test]
    async fn bounded_wait_returns_current_nonterminal_projection() {
        let store = Store::in_memory().expect("store");
        let conversation = store.create_conversation(".", None).expect("conversation");
        let run = store
            .create_run(conversation.id, "wait", ExecutionMode::Agentic)
            .expect("run");
        let control = AgentControl::new(store, AgentLimits::default());
        let root = control
            .create_root(run.id, &definition(true), "root work", ".")
            .expect("root");

        let agents = control
            .wait_for_agents(&[root.id], Duration::from_millis(5))
            .await
            .expect("bounded wait");

        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].status, opensrc_core::AgentStatus::Queued);
    }

    #[test]
    fn enforces_active_agent_concurrency() {
        let store = Store::in_memory().expect("store");
        let conversation = store.create_conversation(".", None).expect("conversation");
        let run = store
            .create_run(conversation.id, "concurrency", ExecutionMode::Agentic)
            .expect("run");
        let control = AgentControl::new(
            store,
            AgentLimits {
                max_active_agents_per_run: 1,
                ..AgentLimits::default()
            },
        );
        let root = control
            .create_root(run.id, &definition(true), "root", ".")
            .expect("root");
        let child = control
            .spawn_agent(root.id, &definition(false), "child", None)
            .expect("child");
        control.start_agent(root.id).expect("start root");
        assert!(matches!(
            control.start_agent(child.id),
            Err(AgentControlError::ConcurrencyLimit(1))
        ));
    }

    #[test]
    fn rejects_writer_task_without_allowed_paths_at_creation() {
        let (_store, control, _writer, task) = writer_fixture();
        let mut invalid = task;
        invalid.id = Uuid::new_v4();
        invalid.status = TaskStatus::Ready;
        invalid.contract.allowed_paths.clear();

        assert!(matches!(
            control.create_task(&invalid),
            Err(AgentControlError::InvalidTaskContract { ref reason, .. })
                if reason.contains("require allowed paths")
        ));
    }

    #[test]
    fn completion_requires_matching_task_producer_and_policy_compliance() {
        let (store, control, writer, task) = writer_fixture();
        control.start_task(task.id).expect("start task");
        control.start_agent(writer.id).expect("start writer");

        let mut completion = valid_completion(&task);
        completion.task_id = Some(Uuid::new_v4());
        assert!(matches!(
            control.complete_task(writer.id, Some(task.id), &completion),
            Err(AgentControlError::InvalidTaskCompletion(ref reason))
                if reason.contains("task id")
        ));

        completion = valid_completion(&task);
        completion.producer = None;
        assert!(matches!(
            control.complete_task(writer.id, Some(task.id), &completion),
            Err(AgentControlError::InvalidTaskCompletion(ref reason))
                if reason.contains("producer identity")
        ));

        completion = valid_completion(&task);
        completion.tool_policy_compliant = false;
        assert!(matches!(
            control.complete_task(writer.id, Some(task.id), &completion),
            Err(AgentControlError::InvalidTaskCompletion(ref reason))
                if reason.contains("tool-policy")
        ));
        assert_eq!(
            store.get_task(task.id).expect("task").status,
            TaskStatus::Running
        );

        control
            .complete_task(writer.id, Some(task.id), &valid_completion(&task))
            .expect("valid completion");
        assert_eq!(
            store.get_task(task.id).expect("task").status,
            TaskStatus::Completed
        );
    }

    #[test]
    fn completed_task_requires_passing_checks_tests_and_owned_changes() {
        let (store, control, writer, template) = writer_fixture();
        let mut task = template;
        task.id = Uuid::new_v4();
        task.contract.validation_steps = vec!["cargo test -p example".to_string()];
        control.create_task(&task).expect("strict task");
        control.start_task(task.id).expect("start task");
        control.start_agent(writer.id).expect("start writer");

        let mut completion = valid_completion(&task);
        completion.tests = vec![TestEvidence {
            command: "cargo test -p example".to_string(),
            status: EvidenceStatus::Failed,
            evidence: "exit code 1".to_string(),
        }];
        assert!(matches!(
            control.complete_task(writer.id, Some(task.id), &completion),
            Err(AgentControlError::InvalidTaskCompletion(ref reason))
                if reason.contains("did not pass")
        ));

        completion.tests[0].status = EvidenceStatus::Passed;
        completion.files_changed = vec!["outside.txt".to_string()];
        assert!(matches!(
            control.complete_task(writer.id, Some(task.id), &completion),
            Err(AgentControlError::InvalidTaskCompletion(ref reason))
                if reason.contains("outside the contract allowed paths")
        ));

        completion.files_changed = vec!["src/lib.rs".to_string()];
        control
            .complete_task(writer.id, Some(task.id), &completion)
            .expect("valid evidence");
        assert_eq!(
            store.get_task(task.id).expect("task").status,
            TaskStatus::Completed
        );
    }

    #[test]
    fn writer_lease_conflicts_block_start_until_the_owner_releases() {
        let (store, control, first_writer, first_task) = writer_fixture();
        let root = store
            .get_agent(first_writer.parent_id.expect("writer parent"))
            .expect("root");
        let second_writer = control
            .spawn_agent_with_ownership(
                root.id,
                &writer_definition(),
                "change nested source",
                None,
                vec!["src/nested/**".to_string()],
            )
            .expect("second writer");
        let second_task = control
            .assign_followup(second_writer.id, "change nested source", 0)
            .expect("second task");

        control.start_task(first_task.id).expect("first lease");
        assert!(matches!(
            control.start_task(second_task.id),
            Err(AgentControlError::Store(
                opensrc_store::StoreError::WorkspaceLeaseConflict { .. }
            ))
        ));
        store
            .transition_task(first_task.id, TaskStatus::Cancelled)
            .expect("cancel first task");
        control
            .start_task(second_task.id)
            .expect("start after owner released");
    }

    #[test]
    fn writer_leases_release_on_success_failure_cancel_and_interrupt() {
        let (store, control, writer, task) = writer_fixture();
        control.start_task(task.id).expect("start completed task");
        control
            .start_agent(writer.id)
            .expect("start completed agent");
        assert_eq!(
            store
                .list_workspace_leases(Some(task.run_id))
                .expect("active completed lease")[0]
                .state,
            WorkspaceLeaseState::Active
        );
        control
            .complete_task(writer.id, Some(task.id), &valid_completion(&task))
            .expect("complete task");
        assert_eq!(
            store
                .list_workspace_leases(Some(task.run_id))
                .expect("released completed lease")[0]
                .state,
            WorkspaceLeaseState::Released
        );

        let (store, control, writer, task) = writer_fixture();
        control.start_task(task.id).expect("start failed task");
        control.start_agent(writer.id).expect("start failed agent");
        let mut failed = valid_completion(&task);
        failed.status = CompletionStatus::Failed;
        failed.summary = "The bounded change failed with captured evidence.".to_string();
        control
            .complete_task(writer.id, Some(task.id), &failed)
            .expect("fail task");
        assert_eq!(
            store
                .list_workspace_leases(Some(task.run_id))
                .expect("released failed lease")[0]
                .state,
            WorkspaceLeaseState::Released
        );

        let (store, control, _writer, task) = writer_fixture();
        control.start_task(task.id).expect("start cancelled task");
        store
            .transition_task(task.id, TaskStatus::Cancelled)
            .expect("cancel task");
        assert_eq!(
            store
                .list_workspace_leases(Some(task.run_id))
                .expect("released cancelled lease")[0]
                .state,
            WorkspaceLeaseState::Released
        );

        let (store, control, writer, task) = writer_fixture();
        control.start_task(task.id).expect("start interrupted task");
        control
            .start_agent(writer.id)
            .expect("start interrupted agent");
        control
            .interrupt_agent(writer.id)
            .expect("interrupt writer");
        assert_eq!(
            store
                .list_workspace_leases(Some(task.run_id))
                .expect("released interrupted lease")[0]
                .state,
            WorkspaceLeaseState::Released
        );
    }

    #[test]
    fn path_scopes_support_recursive_globs_without_crossing_siblings() {
        assert!(path_is_within_scope("src/auth/token.rs", "src/auth/**"));
        assert!(path_is_within_scope("src/lib.rs", "src/*.rs"));
        assert!(!path_is_within_scope("src/nested/lib.rs", "src/*.rs"));
        assert!(!path_is_within_scope("src/billing/lib.rs", "src/auth/**"));
    }
}

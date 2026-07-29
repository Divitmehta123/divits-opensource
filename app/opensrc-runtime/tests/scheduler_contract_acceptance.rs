use chrono::Utc;
use opensrc_core::{
    Agent, AgentDefinition, Budgets, CompletionStatus, ContextPolicy, ContractCheck,
    EvidenceStatus, ExecutionMode, ModelIdentity, ReasoningConfig, RetryPolicy, ReviewContract,
    ReviewVerdict, SandboxPolicy, Task, TaskCompletion, TaskContract, TaskStatus, ToolPolicy,
    WorkspaceLeaseState, WorkspaceMode,
};
use opensrc_runtime::{AgentControl, AgentControlError, AgentLimits};
use opensrc_store::{Store, StoreError};
use std::collections::BTreeMap;
use uuid::Uuid;

struct Harness {
    store: Store,
    control: AgentControl,
    root: Agent,
}

fn definition(
    name: &str,
    provider: &str,
    model: &str,
    workspace_mode: WorkspaceMode,
    may_spawn_children: bool,
) -> AgentDefinition {
    let writable = workspace_mode != WorkspaceMode::SharedReadonly;
    AgentDefinition {
        name: name.to_string(),
        description: format!("{name} acceptance fixture"),
        system_instructions: "Honor the task contract and report evidence.".to_string(),
        preferred_provider: Some(provider.to_string()),
        preferred_model: Some(model.to_string()),
        reasoning: ReasoningConfig::default(),
        context_policy: ContextPolicy::default(),
        tool_policy: ToolPolicy {
            allow: if writable {
                vec!["fs.read".to_string(), "fs.write".to_string()]
            } else {
                vec!["fs.read".to_string()]
            },
            deny: Vec::new(),
            may_spawn_children,
        },
        sandbox_policy: SandboxPolicy::default(),
        workspace_mode,
        budgets: Budgets::default(),
        retry_policy: RetryPolicy::default(),
        fallback_chain: Vec::new(),
        completion_schema: "agent_completion_v1".to_string(),
        metadata: BTreeMap::new(),
    }
}

fn harness() -> Harness {
    let store = Store::in_memory().expect("store");
    let conversation = store.create_conversation(".", None).expect("conversation");
    let run = store
        .create_run(
            conversation.id,
            "scheduler acceptance",
            ExecutionMode::Agentic,
        )
        .expect("run");
    let control = AgentControl::new(store.clone(), AgentLimits::default());
    let root = control
        .create_root(
            run.id,
            &definition(
                "coordinator",
                "deepseek",
                "deepseek-v4-pro",
                WorkspaceMode::SharedReadonly,
                true,
            ),
            "coordinate",
            ".",
        )
        .expect("root");
    Harness {
        store,
        control,
        root,
    }
}

fn spawn_writer(harness: &Harness, name: &str, owned_paths: &[&str]) -> Agent {
    harness
        .control
        .spawn_agent_with_ownership(
            harness.root.id,
            &definition(
                name,
                "moonshot",
                "kimi-k2.7-code",
                WorkspaceMode::OwnedPaths,
                false,
            ),
            format!("{name} task"),
            None,
            owned_paths.iter().map(ToString::to_string).collect(),
        )
        .expect("writer")
}

fn spawn_reader(harness: &Harness, name: &str, provider: &str, model: &str) -> Agent {
    harness
        .control
        .spawn_agent(
            harness.root.id,
            &definition(name, provider, model, WorkspaceMode::SharedReadonly, false),
            format!("{name} task"),
            None,
        )
        .expect("reader")
}

fn task_for_agent(agent: &Agent, objective: &str, dependencies: Vec<Uuid>) -> Task {
    let now = Utc::now();
    let allowed_paths = if agent.workspace.mode == WorkspaceMode::SharedReadonly {
        Vec::new()
    } else {
        agent.workspace.owned_paths.clone()
    };
    Task {
        id: Uuid::new_v4(),
        run_id: agent.run_id,
        description: objective.to_string(),
        dependencies,
        assigned_agent: Some(agent.id),
        status: TaskStatus::Ready,
        priority: 0,
        expected_output: agent.completion_schema.clone(),
        contract: TaskContract {
            objective: objective.to_string(),
            acceptance_criteria: vec!["The objective is satisfied.".to_string()],
            deliverables: vec!["Structured completion evidence.".to_string()],
            allowed_paths,
            tools: agent.tool_policy.clone(),
            budgets: agent.budgets.clone(),
            completion_schema: agent.completion_schema.clone(),
            max_retries: 2,
            ..TaskContract::default()
        },
        workspace_ownership: agent.workspace.owned_paths.clone(),
        allowed_tools: agent.tool_policy.allow.clone(),
        retry_policy: agent.retry_policy.clone(),
        created_at: now,
        updated_at: now,
    }
}

fn completion_for(
    task: &Task,
    agent: &Agent,
    status: CompletionStatus,
    files_changed: Vec<String>,
) -> TaskCompletion {
    TaskCompletion {
        task_id: Some(task.id),
        status,
        summary: "The bounded task finished with recorded evidence.".to_string(),
        files_changed,
        contract_checks: task
            .contract
            .acceptance_criteria
            .iter()
            .map(|criterion| ContractCheck {
                criterion: criterion.clone(),
                status: EvidenceStatus::Passed,
                evidence: "acceptance fixture evidence".to_string(),
            })
            .collect(),
        producer: Some(ModelIdentity {
            provider: agent.provider.clone(),
            model: agent.model.clone(),
        }),
        ..TaskCompletion::default()
    }
}

#[test]
fn writer_scope_and_completion_evidence_are_hard_contracts() {
    let harness = harness();
    let writer = spawn_writer(&harness, "implementer", &["src"]);
    let task = task_for_agent(&writer, "Edit the source tree.", Vec::new());

    let mut missing_scope = task.clone();
    missing_scope.id = Uuid::new_v4();
    missing_scope.contract.allowed_paths.clear();
    assert!(matches!(
        harness.control.create_task(&missing_scope),
        Err(AgentControlError::InvalidTaskContract { ref reason, .. })
            if reason.contains("require allowed paths")
    ));

    harness.control.create_task(&task).expect("task");
    harness.control.start_task(task.id).expect("start task");
    harness
        .control
        .start_agent(writer.id)
        .expect("start writer");

    let mut evidence_free = completion_for(
        &task,
        &writer,
        CompletionStatus::Completed,
        vec!["src/lib.rs".to_string()],
    );
    evidence_free.contract_checks.clear();
    assert!(matches!(
        harness
            .control
            .complete_task(writer.id, Some(task.id), &evidence_free),
        Err(AgentControlError::InvalidTaskCompletion(ref reason))
            if reason.contains("no passing evidence")
    ));

    let outside_ownership = completion_for(
        &task,
        &writer,
        CompletionStatus::Completed,
        vec!["docs/escape.md".to_string()],
    );
    assert!(matches!(
        harness
            .control
            .complete_task(writer.id, Some(task.id), &outside_ownership),
        Err(AgentControlError::InvalidTaskCompletion(ref reason))
            if reason.contains("outside the contract allowed paths")
    ));
}

#[test]
fn conflicting_writer_leases_serialize_but_independent_writers_overlap() {
    let harness = harness();
    let auth = spawn_writer(&harness, "auth-writer", &["src/auth/**"]);
    let token = spawn_writer(&harness, "token-writer", &["src/auth/token/**"]);
    let billing = spawn_writer(&harness, "billing-writer", &["src/billing/**"]);
    let auth_task = task_for_agent(&auth, "Edit auth.", Vec::new());
    let token_task = task_for_agent(&token, "Edit tokens.", Vec::new());
    let billing_task = task_for_agent(&billing, "Edit billing.", Vec::new());
    for task in [&auth_task, &token_task, &billing_task] {
        harness.control.create_task(task).expect("task");
    }

    harness
        .control
        .start_task(auth_task.id)
        .expect("auth lease");
    assert!(matches!(
        harness.control.start_task(token_task.id),
        Err(AgentControlError::Store(
            StoreError::WorkspaceLeaseConflict { .. }
        ))
    ));
    harness
        .control
        .start_task(billing_task.id)
        .expect("independent lease");

    let active = harness
        .store
        .list_workspace_leases(Some(harness.root.run_id))
        .expect("leases")
        .into_iter()
        .filter(|lease| lease.state == WorkspaceLeaseState::Active)
        .count();
    assert_eq!(active, 2);
}

#[test]
fn failed_dependencies_block_every_downstream_task() {
    let harness = harness();
    let first = spawn_reader(&harness, "investigator", "zai", "glm-4.5");
    let second = spawn_reader(&harness, "implementer", "moonshot", "kimi-k2.7-code");
    let third = spawn_reader(&harness, "reviewer", "deepseek", "deepseek-v4-pro");
    let first_task = task_for_agent(&first, "Investigate.", Vec::new());
    let second_task = task_for_agent(&second, "Implement.", vec![first_task.id]);
    let third_task = task_for_agent(&third, "Review.", vec![second_task.id]);
    for task in [&first_task, &second_task, &third_task] {
        harness.control.create_task(task).expect("task");
    }
    harness
        .control
        .start_task(first_task.id)
        .expect("start first");
    harness
        .control
        .start_agent(first.id)
        .expect("start first agent");

    harness
        .control
        .complete_task(
            first.id,
            Some(first_task.id),
            &completion_for(&first_task, &first, CompletionStatus::Failed, Vec::new()),
        )
        .expect("record failure");

    assert_eq!(
        harness.store.get_task(first_task.id).expect("first").status,
        TaskStatus::Failed
    );
    assert_eq!(
        harness
            .store
            .get_task(second_task.id)
            .expect("second")
            .status,
        TaskStatus::Blocked
    );
    assert_eq!(
        harness.store.get_task(third_task.id).expect("third").status,
        TaskStatus::Blocked
    );
}

#[test]
fn a_dedicated_reviewer_agent_can_reuse_the_locked_single_model() {
    let harness = harness();
    let implementer = spawn_reader(&harness, "implementer", "moonshot", "kimi-k2.7-code");
    let reviewer = spawn_reader(&harness, "reviewer", "moonshot", "kimi-k2.7-code");
    let implementation = task_for_agent(&implementer, "Implement a change.", Vec::new());
    let mut review = task_for_agent(&reviewer, "Review the change.", vec![implementation.id]);
    review.contract.review_required = true;
    for task in [&implementation, &review] {
        harness.control.create_task(task).expect("task");
    }
    harness
        .control
        .start_task(implementation.id)
        .expect("start implementation");
    harness
        .control
        .start_agent(implementer.id)
        .expect("start implementer");
    harness
        .control
        .complete_task(
            implementer.id,
            Some(implementation.id),
            &completion_for(
                &implementation,
                &implementer,
                CompletionStatus::Completed,
                Vec::new(),
            ),
        )
        .expect("implementation");

    harness.control.start_task(review.id).expect("start review");
    harness
        .control
        .start_agent(reviewer.id)
        .expect("start reviewer");
    let mut independent_review =
        completion_for(&review, &reviewer, CompletionStatus::Completed, Vec::new());
    independent_review.review = Some(ReviewContract {
        verdict: ReviewVerdict::Approve,
        summary: "Approved in a separate reviewer agent context.".to_string(),
        findings: Vec::new(),
        test_gaps: Vec::new(),
        architecture_violations: Vec::new(),
        security_findings: Vec::new(),
    });

    harness
        .control
        .complete_task(reviewer.id, Some(review.id), &independent_review)
        .expect("dedicated reviewer may use the session's locked model");
    assert_ne!(implementer.id, reviewer.id);
    assert_eq!(
        harness.store.get_task(review.id).expect("review").status,
        TaskStatus::Completed
    );
}

#[test]
fn writer_leases_release_on_failure_and_interruption() {
    let harness = harness();
    let failed_writer = spawn_writer(&harness, "failed-writer", &["src/failed/**"]);
    let interrupted_writer = spawn_writer(&harness, "interrupted-writer", &["src/interrupted/**"]);
    let failed_task = task_for_agent(&failed_writer, "Fail safely.", Vec::new());
    let interrupted_task = task_for_agent(&interrupted_writer, "Interrupt safely.", Vec::new());
    for task in [&failed_task, &interrupted_task] {
        harness.control.create_task(task).expect("task");
        harness.control.start_task(task.id).expect("start task");
    }
    harness
        .control
        .start_agent(failed_writer.id)
        .expect("start failed writer");
    harness
        .control
        .start_agent(interrupted_writer.id)
        .expect("start interrupted writer");

    harness
        .control
        .complete_task(
            failed_writer.id,
            Some(failed_task.id),
            &completion_for(
                &failed_task,
                &failed_writer,
                CompletionStatus::Failed,
                Vec::new(),
            ),
        )
        .expect("fail writer");
    harness
        .control
        .interrupt_agent(interrupted_writer.id)
        .expect("interrupt writer");

    let leases = harness
        .store
        .list_workspace_leases(Some(harness.root.run_id))
        .expect("leases");
    assert_eq!(leases.len(), 2);
    assert!(
        leases
            .iter()
            .all(|lease| lease.state == WorkspaceLeaseState::Released)
    );
}

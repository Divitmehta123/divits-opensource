use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use uuid::Uuid;

pub type ConversationId = Uuid;
pub type MessageId = Uuid;
pub type ApprovalId = Uuid;
pub type FileChangeId = Uuid;
pub type CheckpointId = Uuid;
pub type RunId = Uuid;
pub type AgentId = Uuid;
pub type TaskId = Uuid;
pub type WorkspaceLeaseId = Uuid;
pub type RoutingBenchmarkId = Uuid;
pub type EventId = i64;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    Direct,
    Focused,
    Agentic,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageContent {
    Text {
        text: String,
    },
    FileReference {
        path: String,
        mime_type: Option<String>,
    },
    ReasoningSummary {
        text: String,
    },
    ToolCall {
        provider_call_id: String,
        canonical_call_id: String,
        name: String,
        arguments: Value,
    },
    ToolResult {
        provider_call_id: String,
        canonical_call_id: String,
        name: String,
        result: Value,
        timing_ms: Option<u64>,
        approval_state: Option<String>,
    },
    ToolError {
        provider_call_id: String,
        canonical_call_id: String,
        name: String,
        error: String,
        timing_ms: Option<u64>,
        approval_state: Option<String>,
    },
    ApprovalRequest {
        approval_id: String,
        summary: String,
        details: Value,
    },
    ApprovalResult {
        approval_id: String,
        decision: String,
        reason: Option<String>,
    },
    ContextSummary {
        text: String,
    },
}

impl MessageContent {
    #[must_use]
    pub fn text(value: impl Into<String>) -> Self {
        Self::Text { text: value.into() }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Created,
    Running,
    Waiting,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Created,
    Queued,
    Running,
    Waiting,
    Blocked,
    Completed,
    Failed,
    Interrupted,
    Unloaded,
    Restoring,
}

impl AgentStatus {
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Interrupted)
    }

    #[must_use]
    pub fn can_transition_to(self, next: Self) -> bool {
        use AgentStatus::{
            Blocked, Completed, Created, Failed, Interrupted, Queued, Restoring, Running, Unloaded,
            Waiting,
        };
        matches!(
            (self, next),
            (Created, Queued | Interrupted)
                | (Queued, Running | Interrupted | Failed)
                | (
                    Running,
                    Waiting | Blocked | Completed | Failed | Interrupted
                )
                | (Waiting, Running | Blocked | Failed | Interrupted)
                | (Blocked, Queued | Running | Failed | Interrupted)
                | (Completed | Failed | Interrupted, Unloaded)
                | (Unloaded, Restoring)
                | (Restoring, Queued | Failed)
        )
    }
}

impl RunStatus {
    #[must_use]
    pub fn can_transition_to(self, next: Self) -> bool {
        use RunStatus::{Cancelled, Completed, Created, Failed, Running, Waiting};
        matches!(
            (self, next),
            (Created, Running | Cancelled)
                | (Running, Waiting | Completed | Failed | Cancelled)
                | (Waiting, Running | Failed | Cancelled)
        )
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Created,
    Ready,
    Running,
    Waiting,
    Blocked,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    #[must_use]
    pub fn can_transition_to(self, next: Self) -> bool {
        use TaskStatus::{Blocked, Cancelled, Completed, Created, Failed, Ready, Running, Waiting};
        matches!(
            (self, next),
            (Created, Ready | Blocked | Cancelled)
                | (Ready, Running | Blocked | Cancelled)
                | (Running, Waiting | Blocked | Completed | Failed | Cancelled)
                | (Waiting, Running | Blocked | Failed | Cancelled)
                | (Blocked, Ready | Running | Failed | Cancelled)
        )
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContextInheritance {
    None,
    SelectedItems,
    LastNTurns,
    FullHistory,
    SummaryOnly,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMode {
    SharedReadonly,
    SharedWrite,
    OwnedPaths,
    GitWorktree,
    TemporaryCopy,
    ContainerIsolated,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReasoningConfig {
    pub level: Option<String>,
    pub temperature: Option<f32>,
}

impl Default for ReasoningConfig {
    fn default() -> Self {
        Self {
            level: None,
            temperature: Some(0.2),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextPolicy {
    pub inheritance: ContextInheritance,
    pub last_n_turns: Option<u32>,
    pub selected_items: Vec<String>,
    pub max_tokens: Option<u64>,
}

impl Default for ContextPolicy {
    fn default() -> Self {
        Self {
            inheritance: ContextInheritance::SummaryOnly,
            last_n_turns: None,
            selected_items: Vec::new(),
            max_tokens: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolPolicy {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub may_spawn_children: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Workspace {
    pub mode: WorkspaceMode,
    pub root: String,
    pub owned_paths: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceLeaseMode {
    Read,
    Write,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceLeaseState {
    Active,
    Released,
    Recovered,
}

impl WorkspaceLeaseState {
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Active)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceLeaseRequest {
    pub run_id: RunId,
    pub agent_id: AgentId,
    pub task_id: Option<TaskId>,
    pub mode: WorkspaceLeaseMode,
    pub root: String,
    pub owned_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceLease {
    pub id: WorkspaceLeaseId,
    pub run_id: RunId,
    pub agent_id: AgentId,
    pub task_id: Option<TaskId>,
    pub mode: WorkspaceLeaseMode,
    pub root: String,
    pub owned_paths: Vec<String>,
    pub state: WorkspaceLeaseState,
    pub created_at: DateTime<Utc>,
    pub released_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SandboxPolicy {
    pub read_paths: Vec<String>,
    pub write_paths: Vec<String>,
    pub network_allow: Vec<String>,
    pub process_allow: Vec<String>,
    pub protected_environment: Vec<String>,
    pub command_allow: Vec<String>,
    pub command_deny: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Budgets {
    pub token_limit: Option<u64>,
    pub cost_limit_microusd: Option<u64>,
    pub time_limit_ms: Option<u64>,
    pub turn_limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub initial_backoff_ms: u64,
    pub max_backoff_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 2,
            initial_backoff_ms: 1_000,
            max_backoff_ms: 10_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Conversation {
    pub id: ConversationId,
    pub project_root: String,
    pub title: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub model_pack: Option<String>,
    #[serde(default)]
    pub reasoning_level: Option<String>,
    #[serde(default)]
    pub preferred_mode: Option<ExecutionMode>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub parent_conversation_id: Option<ConversationId>,
    #[serde(default)]
    pub forked_from_message_id: Option<MessageId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub id: MessageId,
    pub conversation_id: ConversationId,
    pub run_id: Option<RunId>,
    pub sequence: i64,
    pub role: MessageRole,
    pub content: Vec<MessageContent>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub continuation_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Allowed,
    Denied,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    AllowOnce,
    AllowRun,
    AllowProject,
    AlwaysAllowPattern,
    AlwaysAllowAll,
    DenyOnce,
    AlwaysDenyPattern,
}

impl ApprovalDecision {
    #[must_use]
    pub fn allows(self) -> bool {
        matches!(
            self,
            Self::AllowOnce
                | Self::AllowRun
                | Self::AllowProject
                | Self::AlwaysAllowPattern
                | Self::AlwaysAllowAll
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Approval {
    pub id: ApprovalId,
    pub run_id: RunId,
    pub agent_id: Option<AgentId>,
    pub tool_call_id: Option<Uuid>,
    pub tool_name: String,
    pub arguments: Value,
    pub reasons: Vec<String>,
    pub status: ApprovalStatus,
    pub decision: Option<ApprovalDecision>,
    pub edited_arguments: Option<Value>,
    pub decision_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub decided_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PermissionScope {
    Run,
    Project,
    Global,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PermissionEffect {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PermissionRule {
    pub id: Uuid,
    pub scope: PermissionScope,
    pub effect: PermissionEffect,
    pub run_id: Option<RunId>,
    pub project_root: Option<String>,
    pub tool_name: String,
    pub arguments_pattern: Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeState {
    Applied,
    Undone,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileChange {
    pub id: FileChangeId,
    pub run_id: RunId,
    pub agent_id: AgentId,
    pub task_id: Option<TaskId>,
    pub workspace_path: String,
    pub relative_path: String,
    pub preimage_hash: Option<String>,
    pub postimage_hash: Option<String>,
    pub patch: Option<String>,
    pub state: FileChangeState,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Checkpoint {
    pub id: CheckpointId,
    pub run_id: RunId,
    pub agent_id: Option<AgentId>,
    pub task_id: Option<TaskId>,
    pub label: String,
    pub captured_change_ids: Vec<FileChangeId>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Run {
    pub id: RunId,
    pub conversation_id: ConversationId,
    pub request: String,
    pub mode: ExecutionMode,
    pub status: RunStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Agent {
    pub id: AgentId,
    pub run_id: RunId,
    pub canonical_path: String,
    pub parent_id: Option<AgentId>,
    pub child_ids: Vec<AgentId>,
    pub role: String,
    pub task: String,
    pub status: AgentStatus,
    pub provider: String,
    pub model: String,
    pub reasoning: ReasoningConfig,
    pub system_instructions: String,
    pub context_policy: ContextPolicy,
    pub tool_policy: ToolPolicy,
    pub workspace: Workspace,
    pub sandbox_policy: SandboxPolicy,
    pub budgets: Budgets,
    pub retry_policy: RetryPolicy,
    pub fallback_chain: Vec<String>,
    pub completion_schema: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Task {
    pub id: TaskId,
    pub run_id: RunId,
    pub description: String,
    pub dependencies: Vec<TaskId>,
    pub assigned_agent: Option<AgentId>,
    pub status: TaskStatus,
    pub priority: i32,
    pub expected_output: String,
    #[serde(default)]
    pub contract: TaskContract,
    pub workspace_ownership: Vec<String>,
    pub allowed_tools: Vec<String>,
    pub retry_policy: RetryPolicy,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskInputs {
    #[serde(default)]
    pub repository_summary: Option<String>,
    #[serde(default)]
    pub relevant_files: Vec<String>,
    #[serde(default)]
    pub parent_findings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskContract {
    pub objective: String,
    #[serde(default)]
    pub inputs: TaskInputs,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub deliverables: Vec<String>,
    #[serde(default)]
    pub validation_steps: Vec<String>,
    #[serde(default)]
    pub forbidden_actions: Vec<String>,
    #[serde(default)]
    pub handoff_notes: Vec<String>,
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    #[serde(default)]
    pub forbidden_paths: Vec<String>,
    #[serde(default)]
    pub tools: ToolPolicy,
    #[serde(default)]
    pub budgets: Budgets,
    #[serde(default = "default_completion_schema")]
    pub completion_schema: String,
    #[serde(default = "default_task_retries")]
    pub max_retries: u32,
    #[serde(default)]
    pub review_required: bool,
    #[serde(default)]
    pub repair_of_task_id: Option<TaskId>,
}

impl Default for TaskContract {
    fn default() -> Self {
        Self {
            objective: String::new(),
            inputs: TaskInputs::default(),
            acceptance_criteria: Vec::new(),
            deliverables: Vec::new(),
            validation_steps: Vec::new(),
            forbidden_actions: Vec::new(),
            handoff_notes: Vec::new(),
            allowed_paths: Vec::new(),
            forbidden_paths: Vec::new(),
            tools: ToolPolicy::default(),
            budgets: Budgets::default(),
            completion_schema: default_completion_schema(),
            max_retries: default_task_retries(),
            review_required: false,
            repair_of_task_id: None,
        }
    }
}

fn default_completion_schema() -> String {
    "agent_completion_v1".to_string()
}

const fn default_task_retries() -> u32 {
    2
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompletionStatus {
    Completed,
    Failed,
    Blocked,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStatus {
    Passed,
    Failed,
    Skipped,
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TestEvidence {
    pub command: String,
    pub status: EvidenceStatus,
    #[serde(default)]
    pub evidence: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContractCheck {
    pub criterion: String,
    pub status: EvidenceStatus,
    pub evidence: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelIdentity {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    Approve,
    ChangesRequired,
    Blocked,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSeverity {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewFinding {
    pub severity: ReviewSeverity,
    pub category: String,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub line: Option<u32>,
    pub evidence: String,
    pub required_action: String,
    #[serde(default)]
    pub blocking: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewContract {
    pub verdict: ReviewVerdict,
    pub summary: String,
    #[serde(default)]
    pub findings: Vec<ReviewFinding>,
    #[serde(default)]
    pub test_gaps: Vec<String>,
    #[serde(default)]
    pub architecture_violations: Vec<String>,
    #[serde(default)]
    pub security_findings: Vec<String>,
}

impl ReviewContract {
    #[must_use]
    pub fn has_blocking_findings(&self) -> bool {
        self.findings.iter().any(|finding| {
            finding.blocking
                && matches!(
                    finding.severity,
                    ReviewSeverity::High | ReviewSeverity::Critical
                )
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskCompletion {
    #[serde(default)]
    pub task_id: Option<TaskId>,
    pub status: CompletionStatus,
    pub summary: String,
    pub findings: Vec<String>,
    pub files_read: Vec<String>,
    pub files_changed: Vec<String>,
    pub commands_run: Vec<String>,
    pub tests_run: Vec<String>,
    #[serde(default)]
    pub tests: Vec<TestEvidence>,
    #[serde(default)]
    pub contract_checks: Vec<ContractCheck>,
    #[serde(default = "default_true")]
    pub tool_policy_compliant: bool,
    #[serde(default)]
    pub producer: Option<ModelIdentity>,
    #[serde(default)]
    pub review: Option<ReviewContract>,
    #[serde(default)]
    pub repair_of_task_id: Option<TaskId>,
    pub risks: Vec<String>,
    pub unresolved: Vec<String>,
    pub recommended_next_actions: Vec<String>,
}

impl Default for TaskCompletion {
    fn default() -> Self {
        Self {
            task_id: None,
            status: CompletionStatus::Completed,
            summary: String::new(),
            findings: Vec::new(),
            files_read: Vec::new(),
            files_changed: Vec::new(),
            commands_run: Vec::new(),
            tests_run: Vec::new(),
            tests: Vec::new(),
            contract_checks: Vec::new(),
            tool_policy_compliant: true,
            producer: None,
            review: None,
            repair_of_task_id: None,
            risks: Vec::new(),
            unresolved: Vec::new(),
            recommended_next_actions: Vec::new(),
        }
    }
}

const fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Event {
    pub id: EventId,
    pub conversation_id: ConversationId,
    pub run_id: Option<RunId>,
    pub agent_id: Option<AgentId>,
    pub task_id: Option<TaskId>,
    pub kind: String,
    pub payload: Value,
    pub idempotency_key: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsageLedger {
    pub base_instruction_tokens: u64,
    pub user_tokens: u64,
    pub repository_context_tokens: u64,
    pub tool_schema_tokens: u64,
    pub tool_output_tokens: u64,
    pub compaction_tokens: u64,
    pub cached_tokens: u64,
    pub subagent_inheritance_tokens: u64,
    pub output_tokens: u64,
}

impl UsageLedger {
    pub fn merge(&mut self, value: &Self) {
        self.base_instruction_tokens = self
            .base_instruction_tokens
            .saturating_add(value.base_instruction_tokens);
        self.user_tokens = self.user_tokens.saturating_add(value.user_tokens);
        self.repository_context_tokens = self
            .repository_context_tokens
            .saturating_add(value.repository_context_tokens);
        self.tool_schema_tokens = self
            .tool_schema_tokens
            .saturating_add(value.tool_schema_tokens);
        self.tool_output_tokens = self
            .tool_output_tokens
            .saturating_add(value.tool_output_tokens);
        self.compaction_tokens = self
            .compaction_tokens
            .saturating_add(value.compaction_tokens);
        self.cached_tokens = self.cached_tokens.saturating_add(value.cached_tokens);
        self.subagent_inheritance_tokens = self
            .subagent_inheritance_tokens
            .saturating_add(value.subagent_inheritance_tokens);
        self.output_tokens = self.output_tokens.saturating_add(value.output_tokens);
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TimingLedger {
    pub startup_ms: u64,
    pub context_building_ms: u64,
    pub provider_request_ms: u64,
    pub time_to_first_token_ms: u64,
    pub generation_ms: u64,
    pub tool_execution_ms: u64,
    pub sandbox_startup_ms: u64,
    pub agent_waiting_ms: u64,
    pub persistence_ms: u64,
    pub total_ms: u64,
}

impl TimingLedger {
    pub fn merge(&mut self, value: &Self) {
        self.startup_ms = self.startup_ms.saturating_add(value.startup_ms);
        self.context_building_ms = self
            .context_building_ms
            .saturating_add(value.context_building_ms);
        self.provider_request_ms = self
            .provider_request_ms
            .saturating_add(value.provider_request_ms);
        self.time_to_first_token_ms = self
            .time_to_first_token_ms
            .saturating_add(value.time_to_first_token_ms);
        self.generation_ms = self.generation_ms.saturating_add(value.generation_ms);
        self.tool_execution_ms = self
            .tool_execution_ms
            .saturating_add(value.tool_execution_ms);
        self.sandbox_startup_ms = self
            .sandbox_startup_ms
            .saturating_add(value.sandbox_startup_ms);
        self.agent_waiting_ms = self.agent_waiting_ms.saturating_add(value.agent_waiting_ms);
        self.persistence_ms = self.persistence_ms.saturating_add(value.persistence_ms);
        self.total_ms = self.total_ms.saturating_add(value.total_ms);
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PerformanceSnapshot {
    pub usage: UsageLedger,
    pub timing: TimingLedger,
    pub cost_microusd: u64,
    pub model_calls: u64,
    pub tool_calls: u64,
    pub failed_tools: u64,
    pub agents: u64,
    pub inter_agent_messages: u64,
}

/// A score or rate expressed in basis points, where `10_000` is 100%.
pub type BenchmarkBasisPoints = u16;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoutingBenchmarkMetrics {
    #[serde(default)]
    pub architecture_quality_bps: Option<BenchmarkBasisPoints>,
    #[serde(default)]
    pub repository_investigation_accuracy_bps: Option<BenchmarkBasisPoints>,
    #[serde(default)]
    pub patch_success_bps: Option<BenchmarkBasisPoints>,
    #[serde(default)]
    pub test_pass_rate_bps: Option<BenchmarkBasisPoints>,
    #[serde(default)]
    pub tool_call_correctness_bps: Option<BenchmarkBasisPoints>,
    #[serde(default)]
    pub frontend_implementation_quality_bps: Option<BenchmarkBasisPoints>,
    #[serde(default)]
    pub accessibility_finding_quality_bps: Option<BenchmarkBasisPoints>,
    #[serde(default)]
    pub review_precision_bps: Option<BenchmarkBasisPoints>,
    #[serde(default)]
    pub security_review_precision_bps: Option<BenchmarkBasisPoints>,
    pub latency_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_hits: u64,
    pub cost_microusd: u64,
    #[serde(default)]
    pub retry_rate_bps: Option<BenchmarkBasisPoints>,
}

impl RoutingBenchmarkMetrics {
    pub const MAX_BASIS_POINTS: BenchmarkBasisPoints = 10_000;

    #[must_use]
    pub fn scores_are_valid(&self) -> bool {
        [
            self.architecture_quality_bps,
            self.repository_investigation_accuracy_bps,
            self.patch_success_bps,
            self.test_pass_rate_bps,
            self.tool_call_correctness_bps,
            self.frontend_implementation_quality_bps,
            self.accessibility_finding_quality_bps,
            self.review_precision_bps,
            self.security_review_precision_bps,
            self.retry_rate_bps,
        ]
        .into_iter()
        .flatten()
        .all(|score| score <= Self::MAX_BASIS_POINTS)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoutingBenchmarkResult {
    pub id: RoutingBenchmarkId,
    pub policy_version: String,
    pub role: String,
    pub provider: String,
    pub model: String,
    pub scenario_id: String,
    pub metrics: RoutingBenchmarkMetrics,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoutingBenchmarkQuery {
    #[serde(default)]
    pub policy_version: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub scenario_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoutingBenchmarkAggregate {
    pub policy_version: String,
    pub role: String,
    pub provider: String,
    pub model: String,
    pub samples: u64,
    pub mean_metrics: RoutingBenchmarkMetrics,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunExecutionResult {
    pub run_id: RunId,
    pub mode: ExecutionMode,
    pub output: String,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub continuation_id: Option<String>,
    pub usage: UsageLedger,
    pub timing: TimingLedger,
    pub model_calls: u32,
    pub tool_calls: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentDefinition {
    pub name: String,
    pub description: String,
    pub system_instructions: String,
    pub preferred_provider: Option<String>,
    pub preferred_model: Option<String>,
    pub reasoning: ReasoningConfig,
    pub context_policy: ContextPolicy,
    pub tool_policy: ToolPolicy,
    pub sandbox_policy: SandboxPolicy,
    pub workspace_mode: WorkspaceMode,
    pub budgets: Budgets,
    pub retry_policy: RetryPolicy,
    pub fallback_chain: Vec<String>,
    pub completion_schema: String,
    pub metadata: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::{AgentStatus, RoutingBenchmarkMetrics};

    #[test]
    fn agent_transitions_are_explicit() {
        assert!(AgentStatus::Created.can_transition_to(AgentStatus::Queued));
        assert!(AgentStatus::Running.can_transition_to(AgentStatus::Completed));
        assert!(!AgentStatus::Created.can_transition_to(AgentStatus::Completed));
        assert!(!AgentStatus::Completed.can_transition_to(AgentStatus::Running));
    }

    #[test]
    fn routing_benchmark_scores_are_bounded_basis_points() {
        let mut metrics = RoutingBenchmarkMetrics {
            architecture_quality_bps: Some(RoutingBenchmarkMetrics::MAX_BASIS_POINTS),
            ..RoutingBenchmarkMetrics::default()
        };
        assert!(metrics.scores_are_valid());

        metrics.retry_rate_bps = Some(RoutingBenchmarkMetrics::MAX_BASIS_POINTS + 1);
        assert!(!metrics.scores_are_valid());
    }
}

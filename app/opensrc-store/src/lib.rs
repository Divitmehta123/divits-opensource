#![allow(clippy::missing_errors_doc)]

mod migrations;

use chrono::Utc;
use opensrc_core::{
    Agent, AgentId, AgentStatus, Approval, ApprovalDecision, ApprovalId, ApprovalStatus,
    Checkpoint, CheckpointId, Conversation, ConversationId, Event, ExecutionMode, FileChange,
    FileChangeId, FileChangeState, Message, MessageContent, MessageRole, PerformanceSnapshot,
    PermissionEffect, PermissionRule, PermissionScope, RoutingBenchmarkAggregate,
    RoutingBenchmarkId, RoutingBenchmarkMetrics, RoutingBenchmarkQuery, RoutingBenchmarkResult,
    Run, RunId, RunStatus, Task, TaskCompletion, TaskId, TimingLedger, UsageLedger, WorkspaceLease,
    WorkspaceLeaseId, WorkspaceLeaseMode, WorkspaceLeaseRequest, WorkspaceLeaseState,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{entity} `{id}` was not found")]
    NotFound { entity: &'static str, id: String },
    #[error("invalid agent transition from {from:?} to {to:?}")]
    InvalidAgentTransition { from: AgentStatus, to: AgentStatus },
    #[error("invalid run transition from {from:?} to {to:?}")]
    InvalidRunTransition { from: RunStatus, to: RunStatus },
    #[error("invalid task transition from {from:?} to {to:?}")]
    InvalidTaskTransition {
        from: opensrc_core::TaskStatus,
        to: opensrc_core::TaskStatus,
    },
    #[error("database lock was poisoned")]
    Poisoned,
    #[error("approval `{0}` is no longer pending")]
    ApprovalNotPending(ApprovalId),
    #[error("file change `{id}` cannot transition from {from:?} to {to:?}")]
    InvalidFileChangeState {
        id: FileChangeId,
        from: FileChangeState,
        to: FileChangeState,
    },
    #[error("invalid routing benchmark: {0}")]
    InvalidRoutingBenchmark(String),
    #[error("invalid workspace lease: {0}")]
    InvalidWorkspaceLease(String),
    #[error(
        "workspace lease conflicts with active writer lease {conflicting_lease_id} held by agent {conflicting_agent_id} for `{overlapping_scope}`"
    )]
    WorkspaceLeaseConflict {
        conflicting_lease_id: WorkspaceLeaseId,
        conflicting_agent_id: AgentId,
        overlapping_scope: String,
    },
}

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Clone)]
pub struct Store {
    connection: Arc<Mutex<Connection>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolCallClaim {
    Execute { id: Uuid },
    Replay { id: Uuid, output: Value },
    InFlight { id: Uuid },
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        let store = Self::from_connection(connection)?;
        store.recover_workspace_leases()?;
        Ok(store)
    }

    pub fn in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(connection: Connection) -> Result<Self> {
        connection.execute_batch(migrations::SCHEMA_V1)?;
        ensure_column(&connection, "approvals", "data_json", "TEXT")?;
        ensure_column(
            &connection,
            "file_changes",
            "state",
            "TEXT NOT NULL DEFAULT 'applied'",
        )?;
        ensure_column(
            &connection,
            "workspace_leases",
            "task_id",
            "TEXT REFERENCES tasks(id)",
        )?;
        connection.execute(
            "CREATE INDEX IF NOT EXISTS idx_workspace_leases_task
             ON workspace_leases(task_id, state)",
            [],
        )?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.connection.lock().map_err(|_| StoreError::Poisoned)
    }

    pub fn create_conversation(
        &self,
        project_root: impl Into<String>,
        title: Option<String>,
    ) -> Result<Conversation> {
        let now = Utc::now();
        let conversation = Conversation {
            id: Uuid::new_v4(),
            project_root: project_root.into(),
            title,
            provider: None,
            model: None,
            model_pack: None,
            reasoning_level: None,
            preferred_mode: None,
            agent: None,
            archived: false,
            parent_conversation_id: None,
            forked_from_message_id: None,
            created_at: now,
            updated_at: now,
        };
        let data = serde_json::to_string(&conversation)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO conversations(id, project_root, title, data_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                conversation.id.to_string(),
                conversation.project_root,
                conversation.title,
                data,
                now.to_rfc3339(),
                now.to_rfc3339()
            ],
        )?;
        append_event_tx(
            &transaction,
            conversation.id,
            None,
            None,
            None,
            "conversation.created",
            &serde_json::to_value(&conversation)?,
            None,
        )?;
        transaction.commit()?;
        Ok(conversation)
    }

    pub fn get_conversation(&self, id: ConversationId) -> Result<Conversation> {
        let connection = self.lock()?;
        let value = connection
            .query_row(
                "SELECT data_json FROM conversations WHERE id = ?1",
                [id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound {
                entity: "conversation",
                id: id.to_string(),
            })?;
        Ok(serde_json::from_str(&value)?)
    }

    pub fn list_conversations(&self, project_root: Option<&str>) -> Result<Vec<Conversation>> {
        let connection = self.lock()?;
        let mut values = Vec::new();
        if let Some(project_root) = project_root {
            let mut statement = connection.prepare(
                "SELECT data_json FROM conversations
                 WHERE project_root = ?1 ORDER BY updated_at DESC",
            )?;
            let rows = statement.query_map([project_root], |row| row.get::<_, String>(0))?;
            for row in rows {
                let conversation: Conversation = serde_json::from_str(&row?)?;
                if !conversation.archived {
                    values.push(conversation);
                }
            }
        } else {
            let mut statement = connection
                .prepare("SELECT data_json FROM conversations ORDER BY updated_at DESC")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            for row in rows {
                let conversation: Conversation = serde_json::from_str(&row?)?;
                if !conversation.archived {
                    values.push(conversation);
                }
            }
        }
        Ok(values)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_conversation_selection(
        &self,
        id: ConversationId,
        provider: Option<String>,
        model: Option<String>,
        model_pack: Option<String>,
        reasoning_level: Option<String>,
        preferred_mode: Option<ExecutionMode>,
        agent: Option<String>,
    ) -> Result<Conversation> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let value = transaction
            .query_row(
                "SELECT data_json FROM conversations WHERE id = ?1",
                [id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound {
                entity: "conversation",
                id: id.to_string(),
            })?;
        let mut conversation: Conversation = serde_json::from_str(&value)?;
        conversation.provider = provider;
        conversation.model = model;
        conversation.model_pack = model_pack;
        conversation.reasoning_level = reasoning_level;
        conversation.preferred_mode = preferred_mode;
        conversation.agent = agent;
        conversation.updated_at = Utc::now();
        transaction.execute(
            "UPDATE conversations SET data_json = ?2, updated_at = ?3 WHERE id = ?1",
            params![
                id.to_string(),
                serde_json::to_string(&conversation)?,
                conversation.updated_at.to_rfc3339()
            ],
        )?;
        append_event_tx(
            &transaction,
            id,
            None,
            None,
            None,
            "conversation.selection_changed",
            &json!({
                "provider": conversation.provider,
                "model": conversation.model,
                "model_pack": conversation.model_pack,
                "reasoning_level": conversation.reasoning_level,
                "mode": conversation.preferred_mode,
                "agent": conversation.agent
            }),
            None,
        )?;
        if let Some(provider) = conversation.provider.as_deref() {
            prune_provider_conversations_tx(&transaction, provider, 5)?;
        }
        transaction.commit()?;
        Ok(conversation)
    }

    pub fn rename_conversation(
        &self,
        id: ConversationId,
        title: Option<String>,
    ) -> Result<Conversation> {
        self.update_conversation_metadata(
            id,
            |conversation| {
                conversation.title = title;
            },
            "conversation.renamed",
        )
    }

    pub fn archive_conversation(&self, id: ConversationId) -> Result<Conversation> {
        self.update_conversation_metadata(
            id,
            |conversation| {
                conversation.archived = true;
            },
            "conversation.archived",
        )
    }

    pub fn delete_conversation(&self, id: ConversationId) -> Result<()> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        // Verify the id first so a stale picker selection receives the same
        // not-found result as every other conversation operation.
        transaction
            .query_row(
                "SELECT 1 FROM conversations WHERE id = ?1",
                [id.to_string()],
                |_| Ok(()),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound {
                entity: "conversation",
                id: id.to_string(),
            })?;
        delete_conversation_tx(&transaction, id)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn fork_conversation(
        &self,
        id: ConversationId,
        through_message: Option<opensrc_core::MessageId>,
    ) -> Result<Conversation> {
        let source = self.get_conversation(id)?;
        let source_messages = self.list_messages(id)?;
        let cutoff = through_message
            .and_then(|message_id| {
                source_messages
                    .iter()
                    .find(|message| message.id == message_id)
                    .map(|message| message.sequence)
            })
            .unwrap_or(i64::MAX);
        let fork = self.create_conversation(
            source.project_root,
            source.title.map(|title| format!("{title} (fork)")),
        )?;
        let fork = self.update_conversation_metadata(
            fork.id,
            |conversation| {
                conversation.provider.clone_from(&source.provider);
                conversation.model.clone_from(&source.model);
                conversation
                    .reasoning_level
                    .clone_from(&source.reasoning_level);
                conversation.preferred_mode = source.preferred_mode;
                conversation.agent.clone_from(&source.agent);
                conversation.parent_conversation_id = Some(id);
                conversation.forked_from_message_id = through_message;
            },
            "conversation.forked",
        )?;
        for message in source_messages
            .into_iter()
            .filter(|message| message.sequence <= cutoff)
        {
            self.append_message(
                fork.id,
                None,
                message.role,
                message.content,
                message.provider.as_deref(),
                message.model.as_deref(),
                message.continuation_id.as_deref(),
            )?;
        }
        Ok(fork)
    }

    fn update_conversation_metadata(
        &self,
        id: ConversationId,
        update: impl FnOnce(&mut Conversation),
        event_kind: &str,
    ) -> Result<Conversation> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let value = transaction
            .query_row(
                "SELECT data_json FROM conversations WHERE id = ?1",
                [id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound {
                entity: "conversation",
                id: id.to_string(),
            })?;
        let mut conversation: Conversation = serde_json::from_str(&value)?;
        update(&mut conversation);
        conversation.updated_at = Utc::now();
        transaction.execute(
            "UPDATE conversations
             SET title = ?2, data_json = ?3, updated_at = ?4 WHERE id = ?1",
            params![
                id.to_string(),
                conversation.title,
                serde_json::to_string(&conversation)?,
                conversation.updated_at.to_rfc3339()
            ],
        )?;
        append_event_tx(
            &transaction,
            id,
            None,
            None,
            None,
            event_kind,
            &serde_json::to_value(&conversation)?,
            None,
        )?;
        transaction.commit()?;
        Ok(conversation)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn append_message(
        &self,
        conversation_id: ConversationId,
        run_id: Option<RunId>,
        role: MessageRole,
        content: Vec<MessageContent>,
        provider: Option<&str>,
        model: Option<&str>,
        continuation_id: Option<&str>,
    ) -> Result<Message> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let message = append_message_tx(
            &transaction,
            conversation_id,
            run_id,
            role,
            content,
            provider,
            model,
            continuation_id,
        )?;
        transaction.commit()?;
        Ok(message)
    }

    pub fn list_messages(&self, conversation_id: ConversationId) -> Result<Vec<Message>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT data_json FROM messages
             WHERE conversation_id = ?1 ORDER BY sequence",
        )?;
        let rows =
            statement.query_map([conversation_id.to_string()], |row| row.get::<_, String>(0))?;
        let mut messages = Vec::new();
        for row in rows {
            messages.push(serde_json::from_str(&row?)?);
        }
        Ok(messages)
    }

    pub fn create_run(
        &self,
        conversation_id: ConversationId,
        request: impl Into<String>,
        mode: ExecutionMode,
    ) -> Result<Run> {
        let request = request.into();
        self.create_run_with_content(
            conversation_id,
            request.clone(),
            mode,
            vec![MessageContent::text(request)],
        )
    }

    pub fn create_run_with_content(
        &self,
        conversation_id: ConversationId,
        request: impl Into<String>,
        mode: ExecutionMode,
        user_content: Vec<MessageContent>,
    ) -> Result<Run> {
        let now = Utc::now();
        let run = Run {
            id: Uuid::new_v4(),
            conversation_id,
            request: request.into(),
            mode,
            status: RunStatus::Created,
            created_at: now,
            updated_at: now,
        };
        let data = serde_json::to_string(&run)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO runs(id, conversation_id, mode, status, request, data_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                run.id.to_string(),
                conversation_id.to_string(),
                enum_json(&mode)?,
                enum_json(&run.status)?,
                run.request,
                data,
                now.to_rfc3339(),
                now.to_rfc3339()
            ],
        )?;
        append_event_tx(
            &transaction,
            conversation_id,
            Some(run.id),
            None,
            None,
            "run.created",
            &serde_json::to_value(&run)?,
            None,
        )?;
        append_message_tx(
            &transaction,
            conversation_id,
            Some(run.id),
            MessageRole::User,
            user_content,
            None,
            None,
            None,
        )?;
        transaction.commit()?;
        Ok(run)
    }

    pub fn get_run(&self, id: RunId) -> Result<Run> {
        let connection = self.lock()?;
        let json = connection
            .query_row(
                "SELECT data_json FROM runs WHERE id = ?1",
                [id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound {
                entity: "run",
                id: id.to_string(),
            })?;
        Ok(serde_json::from_str(&json)?)
    }

    pub fn transition_run(&self, id: RunId, next: RunStatus) -> Result<Run> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let json = transaction
            .query_row(
                "SELECT data_json FROM runs WHERE id = ?1",
                [id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound {
                entity: "run",
                id: id.to_string(),
            })?;
        let mut run: Run = serde_json::from_str(&json)?;
        if !run.status.can_transition_to(next) {
            return Err(StoreError::InvalidRunTransition {
                from: run.status,
                to: next,
            });
        }
        let previous = run.status;
        run.status = next;
        run.updated_at = Utc::now();
        transaction.execute(
            "UPDATE runs SET status = ?2, data_json = ?3, updated_at = ?4 WHERE id = ?1",
            params![
                id.to_string(),
                enum_json(&next)?,
                serde_json::to_string(&run)?,
                run.updated_at.to_rfc3339()
            ],
        )?;
        append_event_tx(
            &transaction,
            run.conversation_id,
            Some(id),
            None,
            None,
            "run.status_changed",
            &serde_json::json!({"from": previous, "to": next}),
            None,
        )?;
        if matches!(
            next,
            RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled
        ) {
            release_workspace_leases_tx(
                &transaction,
                LeaseReleaseFilter::Run(run.id),
                WorkspaceLeaseState::Released,
            )?;
        }
        transaction.commit()?;
        Ok(run)
    }

    pub fn create_agent(&self, agent: &Agent) -> Result<()> {
        let data = serde_json::to_string(agent)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO agents(id, run_id, parent_id, canonical_path, role, status, provider, model,
             data_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                agent.id.to_string(),
                agent.run_id.to_string(),
                agent.parent_id.map(|id| id.to_string()),
                agent.canonical_path,
                agent.role,
                enum_json(&agent.status)?,
                agent.provider,
                agent.model,
                data,
                agent.created_at.to_rfc3339(),
                agent.updated_at.to_rfc3339()
            ],
        )?;
        let conversation_id = conversation_for_run(&transaction, agent.run_id)?;
        append_event_tx(
            &transaction,
            conversation_id,
            Some(agent.run_id),
            Some(agent.id),
            None,
            "agent.created",
            &serde_json::to_value(agent)?,
            None,
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn get_agent(&self, id: AgentId) -> Result<Agent> {
        let connection = self.lock()?;
        let mut agent = connection
            .query_row(
                "SELECT data_json FROM agents WHERE id = ?1",
                [id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound {
                entity: "agent",
                id: id.to_string(),
            })
            .and_then(|json| Ok(serde_json::from_str::<Agent>(&json)?))?;
        agent.child_ids = child_ids(&connection, id)?;
        Ok(agent)
    }

    pub fn list_agents(&self, run_id: Option<RunId>) -> Result<Vec<Agent>> {
        let connection = self.lock()?;
        let mut agents = Vec::new();
        if let Some(run_id) = run_id {
            let mut statement = connection.prepare(
                "SELECT data_json FROM agents WHERE run_id = ?1 ORDER BY canonical_path",
            )?;
            let rows = statement.query_map([run_id.to_string()], |row| row.get::<_, String>(0))?;
            for row in rows {
                let json = row?;
                let mut agent: Agent = serde_json::from_str(&json)?;
                agent.child_ids = child_ids(&connection, agent.id)?;
                agents.push(agent);
            }
        } else {
            let mut statement = connection
                .prepare("SELECT data_json FROM agents ORDER BY created_at, canonical_path")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            for row in rows {
                let json = row?;
                let mut agent: Agent = serde_json::from_str(&json)?;
                agent.child_ids = child_ids(&connection, agent.id)?;
                agents.push(agent);
            }
        }
        Ok(agents)
    }

    pub fn transition_agent(&self, id: AgentId, next: AgentStatus) -> Result<Agent> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let json = transaction
            .query_row(
                "SELECT data_json FROM agents WHERE id = ?1",
                [id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound {
                entity: "agent",
                id: id.to_string(),
            })?;
        let mut agent: Agent = serde_json::from_str(&json)?;
        if !agent.status.can_transition_to(next) {
            return Err(StoreError::InvalidAgentTransition {
                from: agent.status,
                to: next,
            });
        }
        let previous = agent.status;
        agent.status = next;
        agent.updated_at = Utc::now();
        transaction.execute(
            "UPDATE agents SET status = ?2, data_json = ?3, updated_at = ?4 WHERE id = ?1",
            params![
                id.to_string(),
                enum_json(&next)?,
                serde_json::to_string(&agent)?,
                agent.updated_at.to_rfc3339()
            ],
        )?;
        let conversation_id = conversation_for_run(&transaction, agent.run_id)?;
        append_event_tx(
            &transaction,
            conversation_id,
            Some(agent.run_id),
            Some(agent.id),
            None,
            "agent.status_changed",
            &serde_json::json!({"from": previous, "to": next}),
            None,
        )?;
        if matches!(
            next,
            AgentStatus::Blocked
                | AgentStatus::Completed
                | AgentStatus::Failed
                | AgentStatus::Interrupted
        ) {
            release_workspace_leases_tx(
                &transaction,
                LeaseReleaseFilter::Agent(agent.id),
                WorkspaceLeaseState::Released,
            )?;
        }
        transaction.commit()?;
        Ok(agent)
    }

    pub fn set_agent_route(
        &self,
        id: AgentId,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Agent> {
        let provider = provider.into();
        let model = model.into();
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let json = transaction
            .query_row(
                "SELECT data_json FROM agents WHERE id = ?1",
                [id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound {
                entity: "agent",
                id: id.to_string(),
            })?;
        let mut agent: Agent = serde_json::from_str(&json)?;
        let previous_provider = std::mem::replace(&mut agent.provider, provider);
        let previous_model = std::mem::replace(&mut agent.model, model);
        agent.updated_at = Utc::now();
        transaction.execute(
            "UPDATE agents SET provider = ?2, model = ?3, data_json = ?4, updated_at = ?5
             WHERE id = ?1",
            params![
                id.to_string(),
                agent.provider,
                agent.model,
                serde_json::to_string(&agent)?,
                agent.updated_at.to_rfc3339()
            ],
        )?;
        let conversation_id = conversation_for_run(&transaction, agent.run_id)?;
        append_event_tx(
            &transaction,
            conversation_id,
            Some(agent.run_id),
            Some(agent.id),
            None,
            "agent.route_changed",
            &json!({
                "from_provider": previous_provider,
                "from_model": previous_model,
                "to_provider": agent.provider,
                "to_model": agent.model
            }),
            None,
        )?;
        transaction.commit()?;
        Ok(agent)
    }

    pub fn create_task(&self, task: &Task) -> Result<()> {
        let data = serde_json::to_string(task)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO tasks(id, run_id, assigned_agent, status, priority, data_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                task.id.to_string(),
                task.run_id.to_string(),
                task.assigned_agent.map(|id| id.to_string()),
                enum_json(&task.status)?,
                task.priority,
                data,
                task.created_at.to_rfc3339(),
                task.updated_at.to_rfc3339()
            ],
        )?;
        for dependency in &task.dependencies {
            transaction.execute(
                "INSERT INTO task_dependencies(task_id, depends_on) VALUES (?1, ?2)",
                params![task.id.to_string(), dependency.to_string()],
            )?;
        }
        let conversation_id = conversation_for_run(&transaction, task.run_id)?;
        append_event_tx(
            &transaction,
            conversation_id,
            Some(task.run_id),
            task.assigned_agent,
            Some(task.id),
            "task.created",
            &serde_json::to_value(task)?,
            None,
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn list_tasks(&self, run_id: Option<RunId>) -> Result<Vec<Task>> {
        let connection = self.lock()?;
        let mut tasks = Vec::new();
        if let Some(run_id) = run_id {
            let mut statement = connection.prepare(
                "SELECT data_json FROM tasks WHERE run_id = ?1 ORDER BY priority DESC, created_at",
            )?;
            let rows = statement.query_map([run_id.to_string()], |row| row.get::<_, String>(0))?;
            for row in rows {
                let json = row?;
                tasks.push(serde_json::from_str(&json)?);
            }
        } else {
            let mut statement = connection
                .prepare("SELECT data_json FROM tasks ORDER BY created_at, priority DESC")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            for row in rows {
                let json = row?;
                tasks.push(serde_json::from_str(&json)?);
            }
        }
        Ok(tasks)
    }

    pub fn get_task(&self, id: TaskId) -> Result<Task> {
        let connection = self.lock()?;
        let json = connection
            .query_row(
                "SELECT data_json FROM tasks WHERE id = ?1",
                [id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound {
                entity: "task",
                id: id.to_string(),
            })?;
        Ok(serde_json::from_str(&json)?)
    }

    pub fn transition_task(&self, id: TaskId, next: opensrc_core::TaskStatus) -> Result<Task> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let json = transaction
            .query_row(
                "SELECT data_json FROM tasks WHERE id = ?1",
                [id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound {
                entity: "task",
                id: id.to_string(),
            })?;
        let mut task: Task = serde_json::from_str(&json)?;
        if !task.status.can_transition_to(next) {
            return Err(StoreError::InvalidTaskTransition {
                from: task.status,
                to: next,
            });
        }
        let previous = task.status;
        task.status = next;
        task.updated_at = Utc::now();
        transaction.execute(
            "UPDATE tasks SET status = ?2, data_json = ?3, updated_at = ?4 WHERE id = ?1",
            params![
                id.to_string(),
                enum_json(&next)?,
                serde_json::to_string(&task)?,
                task.updated_at.to_rfc3339()
            ],
        )?;
        let conversation_id = conversation_for_run(&transaction, task.run_id)?;
        append_event_tx(
            &transaction,
            conversation_id,
            Some(task.run_id),
            task.assigned_agent,
            Some(task.id),
            "task.status_changed",
            &json!({"from": previous, "to": next}),
            None,
        )?;
        if matches!(
            next,
            opensrc_core::TaskStatus::Blocked
                | opensrc_core::TaskStatus::Completed
                | opensrc_core::TaskStatus::Failed
                | opensrc_core::TaskStatus::Cancelled
        ) {
            release_workspace_leases_tx(
                &transaction,
                LeaseReleaseFilter::Task(task.id),
                WorkspaceLeaseState::Released,
            )?;
        }
        transaction.commit()?;
        Ok(task)
    }

    pub fn reassign_task(&self, id: TaskId, agent_id: AgentId) -> Result<Task> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let json = transaction
            .query_row(
                "SELECT data_json FROM tasks WHERE id = ?1",
                [id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound {
                entity: "task",
                id: id.to_string(),
            })?;
        let mut task: Task = serde_json::from_str(&json)?;
        let agent_run = transaction
            .query_row(
                "SELECT run_id FROM agents WHERE id = ?1",
                [agent_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound {
                entity: "agent",
                id: agent_id.to_string(),
            })?;
        if agent_run != task.run_id.to_string() {
            return Err(StoreError::NotFound {
                entity: "agent_in_task_run",
                id: agent_id.to_string(),
            });
        }
        let previous = task.assigned_agent;
        task.assigned_agent = Some(agent_id);
        task.updated_at = Utc::now();
        transaction.execute(
            "UPDATE tasks SET assigned_agent = ?2, data_json = ?3, updated_at = ?4 WHERE id = ?1",
            params![
                id.to_string(),
                agent_id.to_string(),
                serde_json::to_string(&task)?,
                task.updated_at.to_rfc3339()
            ],
        )?;
        let conversation_id = conversation_for_run(&transaction, task.run_id)?;
        append_event_tx(
            &transaction,
            conversation_id,
            Some(task.run_id),
            Some(agent_id),
            Some(task.id),
            "task.reassigned",
            &json!({"from": previous, "to": agent_id}),
            None,
        )?;
        transaction.commit()?;
        Ok(task)
    }

    pub fn save_completion(
        &self,
        agent_id: AgentId,
        task_id: Option<TaskId>,
        completion: &TaskCompletion,
    ) -> Result<()> {
        let agent = self.get_agent(agent_id)?;
        let connection = self.lock()?;
        connection.execute(
            "INSERT INTO completion_objects(id, run_id, agent_id, task_id, completion_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(agent_id, task_id) DO UPDATE SET
               completion_json = excluded.completion_json,
               created_at = excluded.created_at",
            params![
                Uuid::new_v4().to_string(),
                agent.run_id.to_string(),
                agent_id.to_string(),
                task_id.map(|id| id.to_string()),
                serde_json::to_string(completion)?,
                Utc::now().to_rfc3339()
            ],
        )?;
        drop(connection);
        self.append_event(
            agent.run_id,
            Some(agent_id),
            task_id,
            "agent.completed_contract",
            &serde_json::to_value(completion)?,
            None,
        )?;
        Ok(())
    }

    pub fn get_agent_completion(&self, agent_id: AgentId) -> Result<Option<TaskCompletion>> {
        let connection = self.lock()?;
        let value = connection
            .query_row(
                "SELECT completion_json FROM completion_objects
                 WHERE agent_id = ?1
                 ORDER BY created_at DESC LIMIT 1",
                [agent_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        value
            .map(|value| serde_json::from_str(&value).map_err(StoreError::from))
            .transpose()
    }

    pub fn acquire_workspace_lease(
        &self,
        request: &WorkspaceLeaseRequest,
    ) -> Result<WorkspaceLease> {
        let request = normalize_lease_request(request)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_lease_references(&transaction, &request)?;

        let active = list_workspace_leases_tx(&transaction)?
            .into_iter()
            .filter(|lease| lease.state.is_active())
            .collect::<Vec<_>>();
        if let Some(existing) = active.iter().find(|lease| {
            lease.run_id == request.run_id
                && lease.agent_id == request.agent_id
                && lease.task_id == request.task_id
                && lease.mode == request.mode
                && lease.root == request.root
                && lease.owned_paths == request.owned_paths
        }) {
            let existing = existing.clone();
            transaction.commit()?;
            return Ok(existing);
        }

        if request.mode == WorkspaceLeaseMode::Write {
            for existing in active
                .iter()
                .filter(|lease| lease.mode == WorkspaceLeaseMode::Write)
            {
                if let Some(overlapping_scope) = lease_overlap(
                    &request.root,
                    &request.owned_paths,
                    &existing.root,
                    &existing.owned_paths,
                ) {
                    return Err(StoreError::WorkspaceLeaseConflict {
                        conflicting_lease_id: existing.id,
                        conflicting_agent_id: existing.agent_id,
                        overlapping_scope,
                    });
                }
            }
        }

        let lease = WorkspaceLease {
            id: Uuid::new_v4(),
            run_id: request.run_id,
            agent_id: request.agent_id,
            task_id: request.task_id,
            mode: request.mode,
            root: request.root,
            owned_paths: request.owned_paths,
            state: WorkspaceLeaseState::Active,
            created_at: Utc::now(),
            released_at: None,
        };
        transaction.execute(
            "INSERT INTO workspace_leases(
                id, run_id, agent_id, task_id, mode, root, owned_paths_json, state,
                created_at, released_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL)",
            params![
                lease.id.to_string(),
                lease.run_id.to_string(),
                lease.agent_id.to_string(),
                lease.task_id.map(|id| id.to_string()),
                enum_json(&lease.mode)?,
                lease.root,
                serde_json::to_string(&lease.owned_paths)?,
                enum_json(&lease.state)?,
                lease.created_at.to_rfc3339(),
            ],
        )?;
        append_event_tx(
            &transaction,
            conversation_for_run(&transaction, lease.run_id)?,
            Some(lease.run_id),
            Some(lease.agent_id),
            lease.task_id,
            "workspace.lease_acquired",
            &serde_json::to_value(&lease)?,
            Some(&format!("workspace-lease-acquired:{}", lease.id)),
        )?;
        transaction.commit()?;
        Ok(lease)
    }

    pub fn list_workspace_leases(&self, run_id: Option<RunId>) -> Result<Vec<WorkspaceLease>> {
        let connection = self.lock()?;
        let leases = list_workspace_leases_tx(&connection)?;
        Ok(leases
            .into_iter()
            .filter(|lease| run_id.is_none_or(|run_id| lease.run_id == run_id))
            .collect())
    }

    pub fn release_workspace_lease(&self, lease_id: WorkspaceLeaseId) -> Result<usize> {
        self.release_workspace_leases_matching(
            LeaseReleaseFilter::Lease(lease_id),
            WorkspaceLeaseState::Released,
        )
    }

    pub fn release_workspace_leases_by_task(&self, task_id: TaskId) -> Result<usize> {
        self.release_workspace_leases_matching(
            LeaseReleaseFilter::Task(task_id),
            WorkspaceLeaseState::Released,
        )
    }

    pub fn release_workspace_leases_by_agent(&self, agent_id: AgentId) -> Result<usize> {
        self.release_workspace_leases_matching(
            LeaseReleaseFilter::Agent(agent_id),
            WorkspaceLeaseState::Released,
        )
    }

    pub fn release_workspace_leases_by_run(&self, run_id: RunId) -> Result<usize> {
        self.release_workspace_leases_matching(
            LeaseReleaseFilter::Run(run_id),
            WorkspaceLeaseState::Released,
        )
    }

    /// Marks every lease left active by a previous process as recovered.
    ///
    /// `Store::open` calls this once after migrations. Runtime code may call it
    /// explicitly when taking over an abandoned database connection.
    pub fn recover_workspace_leases(&self) -> Result<usize> {
        self.release_workspace_leases_matching(
            LeaseReleaseFilter::All,
            WorkspaceLeaseState::Recovered,
        )
    }

    fn release_workspace_leases_matching(
        &self,
        filter: LeaseReleaseFilter,
        next_state: WorkspaceLeaseState,
    ) -> Result<usize> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let released = release_workspace_leases_tx(&transaction, filter, next_state)?;
        transaction.commit()?;
        Ok(released)
    }

    pub fn begin_model_call(
        &self,
        run_id: RunId,
        agent_id: Option<AgentId>,
        provider: &str,
        model: &str,
        request: &Value,
    ) -> Result<Uuid> {
        let id = Uuid::new_v4();
        let connection = self.lock()?;
        connection.execute(
            "INSERT INTO model_calls(
                id, run_id, agent_id, provider, model, request_json, state, started_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'running', ?7)",
            params![
                id.to_string(),
                run_id.to_string(),
                agent_id.map(|value| value.to_string()),
                provider,
                model,
                serde_json::to_string(request)?,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(id)
    }

    pub fn finish_model_call(&self, id: Uuid, state: &str, response: &Value) -> Result<()> {
        let connection = self.lock()?;
        connection.execute(
            "UPDATE model_calls SET state = ?2, response_json = ?3, finished_at = ?4
             WHERE id = ?1",
            params![
                id.to_string(),
                state,
                serde_json::to_string(response)?,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn claim_tool_call(
        &self,
        run_id: RunId,
        agent_id: AgentId,
        task_id: Option<TaskId>,
        tool_name: &str,
        input: &Value,
        idempotency_key: &str,
        destructive: bool,
    ) -> Result<ToolCallClaim> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let existing = transaction
            .query_row(
                "SELECT id, state, output_json FROM tool_calls WHERE idempotency_key = ?1",
                [idempotency_key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        if let Some((id, state, output)) = existing {
            let id = parse_uuid("tool_call", &id)?;
            if matches!(state.as_str(), "completed" | "failed" | "denied") {
                let output = output
                    .as_deref()
                    .map(serde_json::from_str)
                    .transpose()?
                    .unwrap_or(Value::Null);
                return Ok(ToolCallClaim::Replay { id, output });
            }
            return Ok(ToolCallClaim::InFlight { id });
        }
        let id = Uuid::new_v4();
        transaction.execute(
            "INSERT INTO tool_calls(
                id, run_id, agent_id, task_id, tool_name, state, input_json,
                idempotency_key, destructive, started_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6, ?7, ?8, ?9)",
            params![
                id.to_string(),
                run_id.to_string(),
                agent_id.to_string(),
                task_id.map(|value| value.to_string()),
                tool_name,
                serde_json::to_string(input)?,
                idempotency_key,
                destructive,
                Utc::now().to_rfc3339()
            ],
        )?;
        transaction.commit()?;
        Ok(ToolCallClaim::Execute { id })
    }

    pub fn finish_tool_call(&self, id: Uuid, state: &str, output: &Value) -> Result<()> {
        let connection = self.lock()?;
        connection.execute(
            "UPDATE tool_calls SET state = ?2, output_json = ?3, finished_at = ?4 WHERE id = ?1",
            params![
                id.to_string(),
                state,
                serde_json::to_string(output)?,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn create_approval(
        &self,
        run_id: RunId,
        agent_id: Option<AgentId>,
        tool_call_id: Option<Uuid>,
        tool_name: impl Into<String>,
        arguments: Value,
        reasons: Vec<String>,
    ) -> Result<Approval> {
        let now = Utc::now();
        let approval = Approval {
            id: Uuid::new_v4(),
            run_id,
            agent_id,
            tool_call_id,
            tool_name: tool_name.into(),
            arguments,
            reasons,
            status: ApprovalStatus::Pending,
            decision: None,
            edited_arguments: None,
            decision_reason: None,
            created_at: now,
            decided_at: None,
        };
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO approvals(
                id, run_id, agent_id, tool_call_id, decision, reason, data_json, created_at
             ) VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, ?7)",
            params![
                approval.id.to_string(),
                run_id.to_string(),
                agent_id.map(|id| id.to_string()),
                tool_call_id.map(|id| id.to_string()),
                approval.reasons.join("; "),
                serde_json::to_string(&approval)?,
                now.to_rfc3339()
            ],
        )?;
        append_event_tx(
            &transaction,
            conversation_for_run(&transaction, run_id)?,
            Some(run_id),
            agent_id,
            None,
            "approval.created",
            &serde_json::to_value(&approval)?,
            Some(&format!("approval:{}", approval.id)),
        )?;
        transaction.commit()?;
        Ok(approval)
    }

    pub fn get_approval(&self, id: ApprovalId) -> Result<Approval> {
        let connection = self.lock()?;
        let value = connection
            .query_row(
                "SELECT data_json FROM approvals WHERE id = ?1",
                [id.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
            .ok_or_else(|| StoreError::NotFound {
                entity: "approval",
                id: id.to_string(),
            })?;
        Ok(serde_json::from_str(&value)?)
    }

    pub fn list_approvals(&self, pending_only: bool) -> Result<Vec<Approval>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT data_json FROM approvals
             WHERE data_json IS NOT NULL AND (?1 = 0 OR decision = 'pending')
             ORDER BY created_at",
        )?;
        let rows = statement.query_map([i64::from(pending_only)], |row| row.get::<_, String>(0))?;
        let mut approvals = Vec::new();
        for row in rows {
            approvals.push(serde_json::from_str(&row?)?);
        }
        Ok(approvals)
    }

    #[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
    pub fn decide_approval(
        &self,
        id: ApprovalId,
        decision: ApprovalDecision,
        edited_arguments: Option<Value>,
        reason: Option<String>,
    ) -> Result<Approval> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let value = transaction
            .query_row(
                "SELECT data_json FROM approvals WHERE id = ?1",
                [id.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
            .ok_or_else(|| StoreError::NotFound {
                entity: "approval",
                id: id.to_string(),
            })?;
        let mut approval: Approval = serde_json::from_str(&value)?;
        if approval.status != ApprovalStatus::Pending {
            return Err(StoreError::ApprovalNotPending(id));
        }
        let decided_at = Utc::now();
        approval.status = if decision.allows() {
            ApprovalStatus::Allowed
        } else {
            ApprovalStatus::Denied
        };
        approval.decision = Some(decision);
        approval.edited_arguments.clone_from(&edited_arguments);
        approval.decision_reason.clone_from(&reason);
        approval.decided_at = Some(decided_at);
        transaction.execute(
            "UPDATE approvals
             SET decision = ?2, reason = ?3, data_json = ?4, decided_at = ?5
             WHERE id = ?1",
            params![
                id.to_string(),
                enum_json(&decision)?,
                reason,
                serde_json::to_string(&approval)?,
                decided_at.to_rfc3339()
            ],
        )?;
        let persistent_rule = match decision {
            ApprovalDecision::AllowRun => Some((
                PermissionScope::Run,
                PermissionEffect::Allow,
                Some(approval.run_id),
                None,
            )),
            ApprovalDecision::AllowProject => Some((
                PermissionScope::Project,
                PermissionEffect::Allow,
                None,
                Some(project_for_run(&transaction, approval.run_id)?),
            )),
            ApprovalDecision::AlwaysAllowPattern | ApprovalDecision::AlwaysAllowAll => {
                Some((PermissionScope::Global, PermissionEffect::Allow, None, None))
            }
            ApprovalDecision::AlwaysDenyPattern => {
                Some((PermissionScope::Global, PermissionEffect::Deny, None, None))
            }
            ApprovalDecision::AllowOnce | ApprovalDecision::DenyOnce => None,
        };
        if let Some((scope, effect, rule_run_id, project_root)) = persistent_rule {
            if decision == ApprovalDecision::AlwaysAllowAll {
                transaction.execute("DELETE FROM permission_rules", [])?;
            }
            let rule = PermissionRule {
                id: Uuid::new_v4(),
                scope,
                effect,
                run_id: rule_run_id,
                project_root,
                tool_name: if decision == ApprovalDecision::AlwaysAllowAll {
                    "*".to_string()
                } else {
                    approval.tool_name.clone()
                },
                arguments_pattern: if decision == ApprovalDecision::AlwaysAllowAll {
                    Value::Null
                } else {
                    edited_arguments.unwrap_or_else(|| approval.arguments.clone())
                },
                created_at: decided_at,
            };
            transaction.execute(
                "INSERT INTO permission_rules(
                    id, scope, effect, run_id, project_root, tool_name, arguments_json,
                    data_json, created_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    rule.id.to_string(),
                    enum_json(&rule.scope)?,
                    enum_json(&rule.effect)?,
                    rule.run_id.map(|value| value.to_string()),
                    rule.project_root,
                    rule.tool_name,
                    serde_json::to_string(&rule.arguments_pattern)?,
                    serde_json::to_string(&rule)?,
                    rule.created_at.to_rfc3339()
                ],
            )?;
        }
        append_event_tx(
            &transaction,
            conversation_for_run(&transaction, approval.run_id)?,
            Some(approval.run_id),
            approval.agent_id,
            None,
            "approval.decided",
            &serde_json::to_value(&approval)?,
            Some(&format!("approval-decision:{}", approval.id)),
        )?;
        transaction.commit()?;
        Ok(approval)
    }

    pub fn list_permission_rules(&self) -> Result<Vec<PermissionRule>> {
        let connection = self.lock()?;
        let mut statement =
            connection.prepare("SELECT data_json FROM permission_rules ORDER BY created_at")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut rules = Vec::new();
        for row in rows {
            rules.push(serde_json::from_str(&row?)?);
        }
        Ok(rules)
    }

    pub fn delete_permission_rule(&self, id: Uuid) -> Result<()> {
        let connection = self.lock()?;
        let changed = connection.execute(
            "DELETE FROM permission_rules WHERE id = ?1",
            [id.to_string()],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound {
                entity: "permission rule",
                id: id.to_string(),
            });
        }
        Ok(())
    }

    pub fn permission_effect(
        &self,
        run_id: RunId,
        tool_name: &str,
        arguments: &Value,
    ) -> Result<Option<PermissionEffect>> {
        let connection = self.lock()?;
        let project_root = project_for_run(&connection, run_id)?;
        let mut statement = connection.prepare(
            "SELECT data_json FROM permission_rules
             WHERE tool_name = ?1 OR tool_name = '*'
             ORDER BY created_at DESC",
        )?;
        let rows = statement.query_map([tool_name], |row| row.get::<_, String>(0))?;
        let mut allowed = false;
        for row in rows {
            let rule: PermissionRule = serde_json::from_str(&row?)?;
            let applies = match rule.scope {
                PermissionScope::Run => rule.run_id == Some(run_id),
                PermissionScope::Project => rule.project_root.as_deref() == Some(&project_root),
                PermissionScope::Global => true,
            };
            if !applies {
                continue;
            }
            let arguments_match = rule.tool_name == "*" || rule.arguments_pattern == *arguments;
            if !arguments_match {
                continue;
            }
            if rule.effect == PermissionEffect::Deny {
                return Ok(Some(PermissionEffect::Deny));
            }
            allowed = true;
        }
        Ok(allowed.then_some(PermissionEffect::Allow))
    }

    pub fn record_usage(
        &self,
        run_id: RunId,
        agent_id: Option<AgentId>,
        provider: &str,
        model: &str,
        usage: &UsageLedger,
        cost_microusd: u64,
    ) -> Result<()> {
        let connection = self.lock()?;
        connection.execute(
            "INSERT INTO token_usage(
                run_id, agent_id, provider, model, usage_json, cost_microusd, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                run_id.to_string(),
                agent_id.map(|value| value.to_string()),
                provider,
                model,
                serde_json::to_string(usage)?,
                i64::try_from(cost_microusd).unwrap_or(i64::MAX),
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn record_timing(
        &self,
        run_id: RunId,
        agent_id: Option<AgentId>,
        timing: &TimingLedger,
    ) -> Result<()> {
        let connection = self.lock()?;
        connection.execute(
            "INSERT INTO timings(run_id, agent_id, timing_json, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                run_id.to_string(),
                agent_id.map(|value| value.to_string()),
                serde_json::to_string(timing)?,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn performance_snapshot(&self, run_id: Option<RunId>) -> Result<PerformanceSnapshot> {
        let connection = self.lock()?;
        let run_filter = run_id.map(|value| value.to_string());
        let mut snapshot = PerformanceSnapshot::default();

        let mut usage_statement = connection.prepare(
            "SELECT usage_json, cost_microusd FROM token_usage
             WHERE (?1 IS NULL OR run_id = ?1)",
        )?;
        let usage_rows = usage_statement.query_map([run_filter.as_deref()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in usage_rows {
            let (json, cost) = row?;
            snapshot.usage.merge(&serde_json::from_str(&json)?);
            snapshot.cost_microusd = snapshot
                .cost_microusd
                .saturating_add(u64::try_from(cost).unwrap_or(0));
        }

        let mut timing_statement = connection
            .prepare("SELECT timing_json FROM timings WHERE (?1 IS NULL OR run_id = ?1)")?;
        let timing_rows =
            timing_statement.query_map([run_filter.as_deref()], |row| row.get::<_, String>(0))?;
        for row in timing_rows {
            snapshot.timing.merge(&serde_json::from_str(&row?)?);
        }

        snapshot.model_calls = count_filtered(
            &connection,
            "SELECT COUNT(*) FROM model_calls WHERE (?1 IS NULL OR run_id = ?1)",
            run_filter.as_deref(),
        )?;
        snapshot.tool_calls = count_filtered(
            &connection,
            "SELECT COUNT(*) FROM tool_calls WHERE (?1 IS NULL OR run_id = ?1)",
            run_filter.as_deref(),
        )?;
        snapshot.failed_tools = count_filtered(
            &connection,
            "SELECT COUNT(*) FROM tool_calls
             WHERE state = 'failed' AND (?1 IS NULL OR run_id = ?1)",
            run_filter.as_deref(),
        )?;
        snapshot.agents = count_filtered(
            &connection,
            "SELECT COUNT(*) FROM agents WHERE (?1 IS NULL OR run_id = ?1)",
            run_filter.as_deref(),
        )?;
        snapshot.inter_agent_messages = count_filtered(
            &connection,
            "SELECT COUNT(*) FROM events
             WHERE kind = 'agent.message_received' AND (?1 IS NULL OR run_id = ?1)",
            run_filter.as_deref(),
        )?;
        Ok(snapshot)
    }

    pub fn record_routing_benchmark(&self, result: &RoutingBenchmarkResult) -> Result<()> {
        self.upsert_routing_benchmark(result)
    }

    pub fn upsert_routing_benchmark(&self, result: &RoutingBenchmarkResult) -> Result<()> {
        validate_routing_benchmark(result)?;
        let connection = self.lock()?;
        connection.execute(
            "INSERT INTO routing_benchmarks(
                id, policy_version, role, provider, model, scenario_id, result_json,
                created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(id) DO UPDATE SET
                policy_version = excluded.policy_version,
                role = excluded.role,
                provider = excluded.provider,
                model = excluded.model,
                scenario_id = excluded.scenario_id,
                result_json = excluded.result_json,
                created_at = excluded.created_at,
                updated_at = excluded.updated_at",
            params![
                result.id.to_string(),
                result.policy_version,
                result.role,
                result.provider,
                result.model,
                result.scenario_id,
                serde_json::to_string(result)?,
                result.created_at.to_rfc3339(),
                result.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn get_routing_benchmark(&self, id: RoutingBenchmarkId) -> Result<RoutingBenchmarkResult> {
        let connection = self.lock()?;
        let document = connection
            .query_row(
                "SELECT result_json FROM routing_benchmarks WHERE id = ?1",
                [id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound {
                entity: "routing_benchmark",
                id: id.to_string(),
            })?;
        Ok(serde_json::from_str(&document)?)
    }

    pub fn list_routing_benchmarks(
        &self,
        query: &RoutingBenchmarkQuery,
    ) -> Result<Vec<RoutingBenchmarkResult>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT result_json
             FROM routing_benchmarks
             WHERE (?1 IS NULL OR policy_version = ?1)
               AND (?2 IS NULL OR role = ?2)
               AND (?3 IS NULL OR provider = ?3)
               AND (?4 IS NULL OR model = ?4)
               AND (?5 IS NULL OR scenario_id = ?5)
             ORDER BY created_at DESC, id DESC",
        )?;
        let rows = statement.query_map(
            params![
                query.policy_version.as_deref(),
                query.role.as_deref(),
                query.provider.as_deref(),
                query.model.as_deref(),
                query.scenario_id.as_deref(),
            ],
            |row| row.get::<_, String>(0),
        )?;
        rows.map(|row| {
            let document = row?;
            serde_json::from_str(&document).map_err(StoreError::from)
        })
        .collect()
    }

    pub fn delete_routing_benchmark(&self, id: RoutingBenchmarkId) -> Result<bool> {
        let connection = self.lock()?;
        Ok(connection.execute(
            "DELETE FROM routing_benchmarks WHERE id = ?1",
            [id.to_string()],
        )? > 0)
    }

    pub fn aggregate_routing_benchmarks(
        &self,
        query: &RoutingBenchmarkQuery,
    ) -> Result<Vec<RoutingBenchmarkAggregate>> {
        let mut groups =
            BTreeMap::<(String, String, String, String), Vec<RoutingBenchmarkMetrics>>::new();
        for result in self.list_routing_benchmarks(query)? {
            groups
                .entry((
                    result.policy_version,
                    result.role,
                    result.provider,
                    result.model,
                ))
                .or_default()
                .push(result.metrics);
        }
        Ok(groups
            .into_iter()
            .map(
                |((policy_version, role, provider, model), metrics)| RoutingBenchmarkAggregate {
                    policy_version,
                    role,
                    provider,
                    model,
                    samples: u64::try_from(metrics.len()).unwrap_or(u64::MAX),
                    mean_metrics: mean_routing_metrics(&metrics),
                },
            )
            .collect())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_checkpoint(
        &self,
        run_id: RunId,
        agent_id: Option<AgentId>,
        task_id: Option<TaskId>,
        label: impl Into<String>,
    ) -> Result<Checkpoint> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let captured_change_ids = {
            let mut statement = transaction
                .prepare("SELECT id FROM file_changes WHERE run_id = ?1 ORDER BY created_at, id")?;
            let rows = statement.query_map([run_id.to_string()], |row| row.get::<_, String>(0))?;
            rows.map(|row| {
                row.and_then(|value| {
                    Uuid::parse_str(&value).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })
                })
            })
            .collect::<std::result::Result<Vec<_>, _>>()?
        };
        let checkpoint = Checkpoint {
            id: Uuid::new_v4(),
            run_id,
            agent_id,
            task_id,
            label: label.into(),
            captured_change_ids,
            created_at: Utc::now(),
        };
        transaction.execute(
            "INSERT INTO checkpoints(id, run_id, agent_id, task_id, state_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                checkpoint.id.to_string(),
                run_id.to_string(),
                agent_id.map(|value| value.to_string()),
                task_id.map(|value| value.to_string()),
                serde_json::to_string(&checkpoint)?,
                checkpoint.created_at.to_rfc3339()
            ],
        )?;
        append_event_tx(
            &transaction,
            conversation_for_run(&transaction, run_id)?,
            Some(run_id),
            agent_id,
            task_id,
            "checkpoint.created",
            &serde_json::to_value(&checkpoint)?,
            Some(&format!("checkpoint:{}", checkpoint.id)),
        )?;
        transaction.commit()?;
        Ok(checkpoint)
    }

    pub fn ensure_automatic_checkpoint(
        &self,
        run_id: RunId,
        agent_id: Option<AgentId>,
        task_id: Option<TaskId>,
    ) -> Result<Checkpoint> {
        if let Some(checkpoint) = self.list_checkpoints(Some(run_id))?.into_iter().next() {
            return Ok(checkpoint);
        }
        self.create_checkpoint(run_id, agent_id, task_id, "Before first mutation")
    }

    pub fn get_checkpoint(&self, id: CheckpointId) -> Result<Checkpoint> {
        let connection = self.lock()?;
        let value = connection
            .query_row(
                "SELECT state_json FROM checkpoints WHERE id = ?1",
                [id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound {
                entity: "checkpoint",
                id: id.to_string(),
            })?;
        Ok(serde_json::from_str(&value)?)
    }

    pub fn list_checkpoints(&self, run_id: Option<RunId>) -> Result<Vec<Checkpoint>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT state_json FROM checkpoints
             WHERE (?1 IS NULL OR run_id = ?1)
             ORDER BY created_at DESC",
        )?;
        let run_id = run_id.map(|value| value.to_string());
        let rows = statement.query_map([run_id.as_deref()], |row| row.get::<_, String>(0))?;
        rows.map(|row| {
            row.map_err(StoreError::from)
                .and_then(|value| serde_json::from_str(&value).map_err(StoreError::from))
        })
        .collect()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_file_change(
        &self,
        run_id: RunId,
        agent_id: AgentId,
        task_id: Option<TaskId>,
        workspace_path: &str,
        relative_path: &str,
        preimage_hash: Option<&str>,
        postimage_hash: Option<&str>,
        patch: Option<&str>,
    ) -> Result<FileChange> {
        let now = Utc::now();
        let change = FileChange {
            id: Uuid::new_v4(),
            run_id,
            agent_id,
            task_id,
            workspace_path: workspace_path.to_string(),
            relative_path: relative_path.to_string(),
            preimage_hash: preimage_hash.map(str::to_string),
            postimage_hash: postimage_hash.map(str::to_string),
            patch: patch.map(str::to_string),
            state: FileChangeState::Applied,
            created_at: now,
            updated_at: now,
        };
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO file_changes(
                id, run_id, agent_id, task_id, workspace_path, relative_path,
                preimage_hash, postimage_hash, patch, state, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'applied', ?10)",
            params![
                change.id.to_string(),
                run_id.to_string(),
                agent_id.to_string(),
                task_id.map(|value| value.to_string()),
                workspace_path,
                relative_path,
                preimage_hash,
                postimage_hash,
                patch,
                now.to_rfc3339()
            ],
        )?;
        append_event_tx(
            &transaction,
            conversation_for_run(&transaction, run_id)?,
            Some(run_id),
            Some(agent_id),
            task_id,
            "change.recorded",
            &serde_json::to_value(&change)?,
            Some(&format!("change:{}", change.id)),
        )?;
        transaction.commit()?;
        Ok(change)
    }

    pub fn get_file_change(&self, id: FileChangeId) -> Result<FileChange> {
        let connection = self.lock()?;
        let raw = connection
            .query_row(
                "SELECT id, run_id, agent_id, task_id, workspace_path, relative_path,
                        preimage_hash, postimage_hash, patch, state, created_at
                 FROM file_changes WHERE id = ?1",
                [id.to_string()],
                raw_file_change,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound {
                entity: "file_change",
                id: id.to_string(),
            })?;
        file_change_from_raw(raw)
    }

    pub fn list_file_changes(&self, run_id: Option<RunId>) -> Result<Vec<FileChange>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT id, run_id, agent_id, task_id, workspace_path, relative_path,
                    preimage_hash, postimage_hash, patch, state, created_at
             FROM file_changes WHERE (?1 IS NULL OR run_id = ?1)
             ORDER BY created_at DESC",
        )?;
        let run_id = run_id.map(|id| id.to_string());
        let rows = statement.query_map([run_id.as_deref()], raw_file_change)?;
        let mut changes = Vec::new();
        for row in rows {
            changes.push(file_change_from_raw(row?)?);
        }
        Ok(changes)
    }

    pub fn transition_file_change(
        &self,
        id: FileChangeId,
        expected: FileChangeState,
        next: FileChangeState,
    ) -> Result<FileChange> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let raw = transaction
            .query_row(
                "SELECT id, run_id, agent_id, task_id, workspace_path, relative_path,
                        preimage_hash, postimage_hash, patch, state, created_at
                 FROM file_changes WHERE id = ?1",
                [id.to_string()],
                raw_file_change,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound {
                entity: "file_change",
                id: id.to_string(),
            })?;
        let mut change = file_change_from_raw(raw)?;
        if change.state != expected {
            return Err(StoreError::InvalidFileChangeState {
                id,
                from: change.state,
                to: next,
            });
        }
        change.state = next;
        change.updated_at = Utc::now();
        transaction.execute(
            "UPDATE file_changes SET state = ?2 WHERE id = ?1",
            params![id.to_string(), enum_json(&next)?],
        )?;
        append_event_tx(
            &transaction,
            conversation_for_run(&transaction, change.run_id)?,
            Some(change.run_id),
            Some(change.agent_id),
            change.task_id,
            "change.state_changed",
            &serde_json::to_value(&change)?,
            Some(&format!("change-state:{}:{next:?}", change.id)),
        )?;
        transaction.commit()?;
        Ok(change)
    }

    pub fn append_event(
        &self,
        run_id: RunId,
        agent_id: Option<AgentId>,
        task_id: Option<TaskId>,
        kind: &str,
        payload: &Value,
        idempotency_key: Option<&str>,
    ) -> Result<i64> {
        let connection = self.lock()?;
        let conversation_id = conversation_for_run(&connection, run_id)?;
        append_event_tx(
            &connection,
            conversation_id,
            Some(run_id),
            agent_id,
            task_id,
            kind,
            payload,
            idempotency_key,
        )
    }

    pub fn events_after(&self, after: i64, limit: usize) -> Result<Vec<Event>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT id, conversation_id, run_id, agent_id, task_id, kind, payload_json,
                    idempotency_key, created_at
             FROM events WHERE id > ?1 ORDER BY id LIMIT ?2",
        )?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = statement.query_map(params![after, limit], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, String>(8)?,
            ))
        })?;
        let mut events = Vec::new();
        for row in rows {
            let (id, conversation, run, agent, task, kind, payload, key, created) = row?;
            events.push(Event {
                id,
                conversation_id: parse_uuid("conversation", &conversation)?,
                run_id: parse_optional_uuid("run", run.as_deref())?,
                agent_id: parse_optional_uuid("agent", agent.as_deref())?,
                task_id: parse_optional_uuid("task", task.as_deref())?,
                kind,
                payload: serde_json::from_str(&payload)?,
                idempotency_key: key,
                created_at: chrono::DateTime::parse_from_rfc3339(&created)
                    .map_err(|error| {
                        StoreError::Serialization(serde_json::Error::io(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            error,
                        )))
                    })?
                    .with_timezone(&Utc),
            });
        }
        Ok(events)
    }

    pub fn agent_messages_after(
        &self,
        run_id: RunId,
        agent_id: AgentId,
        after: i64,
        limit: usize,
    ) -> Result<Vec<Event>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT id, conversation_id, run_id, agent_id, task_id, kind, payload_json,
                    idempotency_key, created_at
             FROM events
             WHERE id > ?1
               AND run_id = ?2
               AND agent_id = ?3
               AND kind = 'agent.message_received'
             ORDER BY id
             LIMIT ?4",
        )?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = statement.query_map(
            params![after, run_id.to_string(), agent_id.to_string(), limit],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, String>(8)?,
                ))
            },
        )?;
        let mut events = Vec::new();
        for row in rows {
            let (id, conversation, run, agent, task, kind, payload, key, created) = row?;
            events.push(Event {
                id,
                conversation_id: parse_uuid("conversation", &conversation)?,
                run_id: parse_optional_uuid("run", run.as_deref())?,
                agent_id: parse_optional_uuid("agent", agent.as_deref())?,
                task_id: parse_optional_uuid("task", task.as_deref())?,
                kind,
                payload: serde_json::from_str(&payload)?,
                idempotency_key: key,
                created_at: chrono::DateTime::parse_from_rfc3339(&created)
                    .map_err(|error| {
                        StoreError::Serialization(serde_json::Error::io(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            error,
                        )))
                    })?
                    .with_timezone(&Utc),
            });
        }
        Ok(events)
    }

    pub fn latest_event_id(&self) -> Result<i64> {
        let connection = self.lock()?;
        connection
            .query_row("SELECT COALESCE(MAX(id), 0) FROM events", [], |row| {
                row.get(0)
            })
            .map_err(Into::into)
    }
}

#[derive(Debug, Clone, Copy)]
enum LeaseReleaseFilter {
    Lease(WorkspaceLeaseId),
    Task(TaskId),
    Agent(AgentId),
    Run(RunId),
    All,
}

fn release_workspace_leases_tx(
    transaction: &Transaction<'_>,
    filter: LeaseReleaseFilter,
    next_state: WorkspaceLeaseState,
) -> Result<usize> {
    let leases = list_workspace_leases_tx(transaction)?
        .into_iter()
        .filter(|lease| {
            lease.state.is_active()
                && match filter {
                    LeaseReleaseFilter::Lease(id) => lease.id == id,
                    LeaseReleaseFilter::Task(id) => lease.task_id == Some(id),
                    LeaseReleaseFilter::Agent(id) => lease.agent_id == id,
                    LeaseReleaseFilter::Run(id) => lease.run_id == id,
                    LeaseReleaseFilter::All => true,
                }
        })
        .collect::<Vec<_>>();
    let released_at = Utc::now();
    for lease in &leases {
        transaction.execute(
            "UPDATE workspace_leases
             SET state = ?2, released_at = ?3
             WHERE id = ?1 AND state = 'active'",
            params![
                lease.id.to_string(),
                enum_json(&next_state)?,
                released_at.to_rfc3339()
            ],
        )?;
        append_event_tx(
            transaction,
            conversation_for_run(transaction, lease.run_id)?,
            Some(lease.run_id),
            Some(lease.agent_id),
            lease.task_id,
            if next_state == WorkspaceLeaseState::Recovered {
                "workspace.lease_recovered"
            } else {
                "workspace.lease_released"
            },
            &json!({
                "lease_id": lease.id,
                "state": next_state,
                "released_at": released_at,
            }),
            Some(&format!(
                "workspace-lease-{}:{}",
                enum_json(&next_state)?,
                lease.id
            )),
        )?;
    }
    Ok(leases.len())
}

type RawWorkspaceLease = (
    String,
    String,
    String,
    Option<String>,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
);

fn list_workspace_leases_tx(connection: &Connection) -> Result<Vec<WorkspaceLease>> {
    let mut statement = connection.prepare(
        "SELECT id, run_id, agent_id, task_id, mode, root, owned_paths_json, state,
                created_at, released_at
         FROM workspace_leases
         ORDER BY created_at, id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, String>(8)?,
            row.get::<_, Option<String>>(9)?,
        ))
    })?;
    let mut leases = Vec::new();
    for row in rows {
        leases.push(workspace_lease_from_raw(row?)?);
    }
    Ok(leases)
}

fn workspace_lease_from_raw(raw: RawWorkspaceLease) -> Result<WorkspaceLease> {
    let (id, run_id, agent_id, task_id, mode, root, owned_paths, state, created_at, released_at) =
        raw;
    Ok(WorkspaceLease {
        id: parse_uuid("workspace_lease", &id)?,
        run_id: parse_uuid("run", &run_id)?,
        agent_id: parse_uuid("agent", &agent_id)?,
        task_id: parse_optional_uuid("task", task_id.as_deref())?,
        mode: parse_workspace_lease_mode(&mode)?,
        root,
        owned_paths: serde_json::from_str(&owned_paths)?,
        state: parse_workspace_lease_state(&state)?,
        created_at: parse_timestamp(&created_at)?,
        released_at: released_at.as_deref().map(parse_timestamp).transpose()?,
    })
}

fn parse_workspace_lease_mode(value: &str) -> Result<WorkspaceLeaseMode> {
    match value {
        "read" => Ok(WorkspaceLeaseMode::Read),
        "write" => Ok(WorkspaceLeaseMode::Write),
        _ => invalid_lease(format!("unknown mode `{value}`")),
    }
}

fn parse_workspace_lease_state(value: &str) -> Result<WorkspaceLeaseState> {
    match value {
        "active" => Ok(WorkspaceLeaseState::Active),
        "released" => Ok(WorkspaceLeaseState::Released),
        "recovered" => Ok(WorkspaceLeaseState::Recovered),
        _ => invalid_lease(format!("unknown state `{value}`")),
    }
}

fn parse_timestamp(value: &str) -> Result<chrono::DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| {
            StoreError::Serialization(serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                error,
            )))
        })
}

fn normalize_lease_request(request: &WorkspaceLeaseRequest) -> Result<WorkspaceLeaseRequest> {
    let root = normalize_lease_root(&request.root)?;
    if request.owned_paths.is_empty() {
        return invalid_lease("owned paths must not be empty");
    }
    let mut owned_paths = request
        .owned_paths
        .iter()
        .map(|path| normalize_owned_scope(path, root_is_windows(&root)))
        .collect::<Result<Vec<_>>>()?;
    owned_paths.sort();
    owned_paths.dedup();
    Ok(WorkspaceLeaseRequest {
        run_id: request.run_id,
        agent_id: request.agent_id,
        task_id: request.task_id,
        mode: request.mode,
        root,
        owned_paths,
    })
}

fn normalize_lease_root(value: &str) -> Result<String> {
    let mut value = value.trim().replace('\\', "/");
    if value.is_empty() {
        return invalid_lease("workspace root must not be empty");
    }
    if let Some(stripped) = value.strip_prefix("//?/") {
        value = stripped.to_string();
    }
    let unc = value.starts_with("//");
    let windows = cfg!(windows)
        || unc
        || value
            .as_bytes()
            .get(1)
            .is_some_and(|character| *character == b':');
    let absolute = value.starts_with('/') && !unc;
    let mut segments = Vec::new();
    for segment in value.split('/').filter(|segment| !segment.is_empty()) {
        match segment {
            "." => {}
            ".." => {
                if segments.pop().is_none() {
                    return invalid_lease("workspace root escapes its lexical base");
                }
            }
            _ => segments.push(segment.to_string()),
        }
    }
    let mut normalized = if unc {
        format!("unc:/{}", segments.join("/"))
    } else if absolute {
        format!("/{}", segments.join("/"))
    } else if segments.is_empty() {
        ".".to_string()
    } else {
        segments.join("/")
    };
    if windows {
        normalized.make_ascii_lowercase();
    }
    Ok(normalized)
}

fn normalize_owned_scope(value: &str, windows: bool) -> Result<String> {
    let value = value.trim().replace('\\', "/");
    if value.is_empty() {
        return invalid_lease("owned path must not be empty");
    }
    if value.starts_with('/')
        || value.starts_with("//")
        || value
            .as_bytes()
            .get(1)
            .is_some_and(|character| *character == b':')
    {
        return invalid_lease(format!("owned path `{value}` must be relative"));
    }
    let mut segments = Vec::new();
    for segment in value.split('/').filter(|segment| !segment.is_empty()) {
        match segment {
            "." => {}
            ".." => {
                return invalid_lease(format!("owned path `{value}` escapes the workspace root"));
            }
            _ if segment.contains(':') => {
                return invalid_lease(format!("owned path `{value}` contains a path prefix"));
            }
            _ => segments.push(segment.to_string()),
        }
    }
    let mut normalized = if segments.is_empty() {
        ".".to_string()
    } else {
        segments.join("/")
    };
    if windows {
        normalized.make_ascii_lowercase();
    }
    Ok(normalized)
}

fn root_is_windows(root: &str) -> bool {
    cfg!(windows)
        || root.starts_with("unc:/")
        || root
            .as_bytes()
            .get(1)
            .is_some_and(|character| *character == b':')
}

fn validate_lease_references(
    transaction: &Transaction<'_>,
    request: &WorkspaceLeaseRequest,
) -> Result<()> {
    let agent_run = transaction
        .query_row(
            "SELECT run_id FROM agents WHERE id = ?1",
            [request.agent_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::NotFound {
            entity: "agent",
            id: request.agent_id.to_string(),
        })?;
    if agent_run != request.run_id.to_string() {
        return invalid_lease("agent does not belong to the requested run");
    }
    if let Some(task_id) = request.task_id {
        let task_owner = transaction
            .query_row(
                "SELECT run_id, assigned_agent FROM tasks WHERE id = ?1",
                [task_id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound {
                entity: "task",
                id: task_id.to_string(),
            })?;
        if task_owner.0 != request.run_id.to_string() {
            return invalid_lease("task does not belong to the requested run");
        }
        if task_owner.1.as_deref() != Some(&request.agent_id.to_string()) {
            return invalid_lease("task is not assigned to the requested agent");
        }
    }
    Ok(())
}

fn invalid_lease<T>(reason: impl Into<String>) -> Result<T> {
    Err(StoreError::InvalidWorkspaceLease(reason.into()))
}

fn lease_overlap(
    left_root: &str,
    left_paths: &[String],
    right_root: &str,
    right_paths: &[String],
) -> Option<String> {
    for left in left_paths {
        let left = rooted_lease_scope(left_root, left);
        for right in right_paths {
            let right = rooted_lease_scope(right_root, right);
            if lease_scope_patterns_overlap(&left, &right) {
                return Some(format!("{left} <-> {right}"));
            }
        }
    }
    None
}

fn rooted_lease_scope(root: &str, owned_path: &str) -> String {
    match (root, owned_path) {
        (".", owned) => owned.to_string(),
        (root, ".") => root.to_string(),
        (root, owned) => format!("{root}/{owned}"),
    }
}

fn lease_scope_patterns_overlap(left: &str, right: &str) -> bool {
    let mut left = left
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let mut right = right
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if !left.iter().any(|segment| segment.contains(['*', '?'])) {
        left.push("**");
    }
    if !right.iter().any(|segment| segment.contains(['*', '?'])) {
        right.push("**");
    }
    let mut memo = vec![vec![None; right.len() + 1]; left.len() + 1];
    lease_segments_overlap(&left, &right, 0, 0, &mut memo)
}

fn lease_segments_overlap(
    left: &[&str],
    right: &[&str],
    left_index: usize,
    right_index: usize,
    memo: &mut [Vec<Option<bool>>],
) -> bool {
    if let Some(value) = memo[left_index][right_index] {
        return value;
    }
    let value = if left_index == left.len() {
        right[right_index..].iter().all(|segment| *segment == "**")
    } else if right_index == right.len() {
        left[left_index..].iter().all(|segment| *segment == "**")
    } else if left[left_index] == "**" || right[right_index] == "**" {
        lease_segments_overlap(left, right, left_index + 1, right_index, memo)
            || lease_segments_overlap(left, right, left_index, right_index + 1, memo)
    } else {
        segment_globs_overlap(left[left_index], right[right_index])
            && lease_segments_overlap(left, right, left_index + 1, right_index + 1, memo)
    };
    memo[left_index][right_index] = Some(value);
    value
}

fn segment_globs_overlap(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let mut memo = vec![vec![None; right.len() + 1]; left.len() + 1];
    segment_globs_overlap_inner(left, right, 0, 0, &mut memo)
}

fn segment_globs_overlap_inner(
    left: &[u8],
    right: &[u8],
    left_index: usize,
    right_index: usize,
    memo: &mut [Vec<Option<bool>>],
) -> bool {
    if let Some(value) = memo[left_index][right_index] {
        return value;
    }
    let value = if left_index == left.len() {
        right[right_index..]
            .iter()
            .all(|character| *character == b'*')
    } else if right_index == right.len() {
        left[left_index..]
            .iter()
            .all(|character| *character == b'*')
    } else if left[left_index] == b'*' || right[right_index] == b'*' {
        segment_globs_overlap_inner(left, right, left_index + 1, right_index, memo)
            || segment_globs_overlap_inner(left, right, left_index, right_index + 1, memo)
    } else {
        (left[left_index] == b'?'
            || right[right_index] == b'?'
            || left[left_index] == right[right_index])
            && segment_globs_overlap_inner(left, right, left_index + 1, right_index + 1, memo)
    };
    memo[left_index][right_index] = Some(value);
    value
}

fn enum_json<T: serde::Serialize>(value: &T) -> Result<String> {
    Ok(serde_json::to_string(value)?.trim_matches('"').to_string())
}

#[allow(clippy::too_many_arguments)]
fn append_message_tx(
    connection: &Connection,
    conversation_id: ConversationId,
    run_id: Option<RunId>,
    role: MessageRole,
    content: Vec<MessageContent>,
    provider: Option<&str>,
    model: Option<&str>,
    continuation_id: Option<&str>,
) -> Result<Message> {
    let sequence = connection.query_row(
        "SELECT COALESCE(MAX(sequence), 0) + 1 FROM messages WHERE conversation_id = ?1",
        [conversation_id.to_string()],
        |row| row.get::<_, i64>(0),
    )?;
    let message = Message {
        id: Uuid::new_v4(),
        conversation_id,
        run_id,
        sequence,
        role,
        content,
        provider: provider.map(str::to_string),
        model: model.map(str::to_string),
        continuation_id: continuation_id.map(str::to_string),
        created_at: Utc::now(),
    };
    connection.execute(
        "INSERT INTO messages(
            id, conversation_id, run_id, sequence, role, data_json, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            message.id.to_string(),
            conversation_id.to_string(),
            run_id.map(|id| id.to_string()),
            message.sequence,
            enum_json(&message.role)?,
            serde_json::to_string(&message)?,
            message.created_at.to_rfc3339()
        ],
    )?;
    touch_conversation(connection, conversation_id, message.created_at)?;
    append_event_tx(
        connection,
        conversation_id,
        run_id,
        None,
        None,
        "message.created",
        &serde_json::to_value(&message)?,
        Some(&format!("message:{}", message.id)),
    )?;
    Ok(message)
}

fn touch_conversation(
    connection: &Connection,
    conversation_id: ConversationId,
    updated_at: chrono::DateTime<Utc>,
) -> Result<()> {
    let value = connection
        .query_row(
            "SELECT data_json FROM conversations WHERE id = ?1",
            [conversation_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::NotFound {
            entity: "conversation",
            id: conversation_id.to_string(),
        })?;
    let mut conversation: Conversation = serde_json::from_str(&value)?;
    conversation.updated_at = updated_at;
    connection.execute(
        "UPDATE conversations SET data_json = ?2, updated_at = ?3 WHERE id = ?1",
        params![
            conversation_id.to_string(),
            serde_json::to_string(&conversation)?,
            updated_at.to_rfc3339()
        ],
    )?;
    Ok(())
}

fn conversation_for_run(connection: &Connection, run_id: RunId) -> Result<ConversationId> {
    let value = connection
        .query_row(
            "SELECT conversation_id FROM runs WHERE id = ?1",
            [run_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::NotFound {
            entity: "run",
            id: run_id.to_string(),
        })?;
    parse_uuid("conversation", &value)
}

fn project_for_run(connection: &Connection, run_id: RunId) -> Result<String> {
    connection
        .query_row(
            "SELECT conversations.project_root
             FROM runs
             JOIN conversations ON conversations.id = runs.conversation_id
             WHERE runs.id = ?1",
            [run_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::NotFound {
            entity: "run",
            id: run_id.to_string(),
        })
}

fn child_ids(connection: &Connection, parent: AgentId) -> Result<Vec<AgentId>> {
    let mut statement =
        connection.prepare("SELECT id FROM agents WHERE parent_id = ?1 ORDER BY canonical_path")?;
    let rows = statement.query_map([parent.to_string()], |row| row.get::<_, String>(0))?;
    let mut ids = Vec::new();
    for row in rows {
        ids.push(parse_uuid("agent", &row?)?);
    }
    Ok(ids)
}

#[allow(clippy::too_many_arguments)]
fn append_event_tx(
    connection: &Connection,
    conversation_id: ConversationId,
    run_id: Option<RunId>,
    agent_id: Option<AgentId>,
    task_id: Option<TaskId>,
    kind: &str,
    payload: &Value,
    idempotency_key: Option<&str>,
) -> Result<i64> {
    let inserted = connection.execute(
        "INSERT INTO events(conversation_id, run_id, agent_id, task_id, kind, payload_json,
         idempotency_key, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(idempotency_key) DO NOTHING",
        params![
            conversation_id.to_string(),
            run_id.map(|id| id.to_string()),
            agent_id.map(|id| id.to_string()),
            task_id.map(|id| id.to_string()),
            kind,
            serde_json::to_string(payload)?,
            idempotency_key,
            Utc::now().to_rfc3339()
        ],
    )?;
    if inserted == 0
        && let Some(key) = idempotency_key
    {
        return connection
            .query_row(
                "SELECT id FROM events WHERE idempotency_key = ?1",
                [key],
                |row| row.get(0),
            )
            .map_err(Into::into);
    }
    Ok(connection.last_insert_rowid())
}

fn parse_uuid(entity: &'static str, value: &str) -> Result<Uuid> {
    Uuid::parse_str(value).map_err(|_| StoreError::NotFound {
        entity,
        id: value.to_string(),
    })
}

fn parse_optional_uuid(entity: &'static str, value: Option<&str>) -> Result<Option<Uuid>> {
    value.map(|value| parse_uuid(entity, value)).transpose()
}

fn count_filtered(connection: &Connection, sql: &str, run_id: Option<&str>) -> Result<u64> {
    let count = connection.query_row(sql, [run_id], |row| row.get::<_, i64>(0))?;
    Ok(u64::try_from(count).unwrap_or(0))
}

type RawFileChange = (
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    String,
);

fn raw_file_change(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawFileChange> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
    ))
}

fn file_change_from_raw(raw: RawFileChange) -> Result<FileChange> {
    let (
        id,
        run_id,
        agent_id,
        task_id,
        workspace_path,
        relative_path,
        preimage_hash,
        postimage_hash,
        patch,
        state,
        created_at,
    ) = raw;
    let id = parse_uuid("file_change", &id)?;
    let agent_id = agent_id
        .as_deref()
        .map(|value| parse_uuid("agent", value))
        .transpose()?
        .ok_or_else(|| StoreError::NotFound {
            entity: "agent for file change",
            id: id.to_string(),
        })?;
    let state = match state.as_str() {
        "applied" => FileChangeState::Applied,
        "undone" => FileChangeState::Undone,
        _ => {
            return Err(StoreError::Serialization(serde_json::Error::io(
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid file change state `{state}`"),
                ),
            )));
        }
    };
    let created_at = chrono::DateTime::parse_from_rfc3339(&created_at)
        .map_err(|error| {
            StoreError::Serialization(serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                error,
            )))
        })?
        .with_timezone(&Utc);
    Ok(FileChange {
        id,
        run_id: parse_uuid("run", &run_id)?,
        agent_id,
        task_id: parse_optional_uuid("task", task_id.as_deref())?,
        workspace_path,
        relative_path,
        preimage_hash,
        postimage_hash,
        patch,
        state,
        created_at,
        updated_at: created_at,
    })
}

fn validate_routing_benchmark(result: &RoutingBenchmarkResult) -> Result<()> {
    for (label, value) in [
        ("policy_version", result.policy_version.as_str()),
        ("role", result.role.as_str()),
        ("provider", result.provider.as_str()),
        ("model", result.model.as_str()),
        ("scenario_id", result.scenario_id.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(StoreError::InvalidRoutingBenchmark(format!(
                "{label} cannot be empty"
            )));
        }
    }
    if !result.metrics.scores_are_valid() {
        return Err(StoreError::InvalidRoutingBenchmark(
            "quality scores and rates must be at most 10_000 basis points".to_string(),
        ));
    }
    if result.updated_at < result.created_at {
        return Err(StoreError::InvalidRoutingBenchmark(
            "updated_at cannot be earlier than created_at".to_string(),
        ));
    }
    Ok(())
}

fn mean_routing_metrics(metrics: &[RoutingBenchmarkMetrics]) -> RoutingBenchmarkMetrics {
    RoutingBenchmarkMetrics {
        architecture_quality_bps: mean_optional_score(metrics, |value| {
            value.architecture_quality_bps
        }),
        repository_investigation_accuracy_bps: mean_optional_score(metrics, |value| {
            value.repository_investigation_accuracy_bps
        }),
        patch_success_bps: mean_optional_score(metrics, |value| value.patch_success_bps),
        test_pass_rate_bps: mean_optional_score(metrics, |value| value.test_pass_rate_bps),
        tool_call_correctness_bps: mean_optional_score(metrics, |value| {
            value.tool_call_correctness_bps
        }),
        frontend_implementation_quality_bps: mean_optional_score(metrics, |value| {
            value.frontend_implementation_quality_bps
        }),
        accessibility_finding_quality_bps: mean_optional_score(metrics, |value| {
            value.accessibility_finding_quality_bps
        }),
        review_precision_bps: mean_optional_score(metrics, |value| value.review_precision_bps),
        security_review_precision_bps: mean_optional_score(metrics, |value| {
            value.security_review_precision_bps
        }),
        latency_ms: mean_u64(metrics, |value| value.latency_ms),
        input_tokens: mean_u64(metrics, |value| value.input_tokens),
        output_tokens: mean_u64(metrics, |value| value.output_tokens),
        cache_hits: mean_u64(metrics, |value| value.cache_hits),
        cost_microusd: mean_u64(metrics, |value| value.cost_microusd),
        retry_rate_bps: mean_optional_score(metrics, |value| value.retry_rate_bps),
    }
}

fn mean_optional_score(
    metrics: &[RoutingBenchmarkMetrics],
    value: fn(&RoutingBenchmarkMetrics) -> Option<u16>,
) -> Option<u16> {
    let (sum, count) = metrics
        .iter()
        .filter_map(value)
        .fold((0_u64, 0_u64), |(sum, count), value| {
            (sum.saturating_add(u64::from(value)), count + 1)
        });
    (count > 0).then(|| u16::try_from(sum / count).unwrap_or(u16::MAX))
}

fn mean_u64(
    metrics: &[RoutingBenchmarkMetrics],
    value: fn(&RoutingBenchmarkMetrics) -> u64,
) -> u64 {
    if metrics.is_empty() {
        return 0;
    }
    let sum = metrics
        .iter()
        .map(|metric| u128::from(value(metric)))
        .sum::<u128>();
    let count = u128::try_from(metrics.len()).unwrap_or(u128::MAX);
    u64::try_from(sum / count).unwrap_or(u64::MAX)
}

fn ensure_column(
    connection: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<()> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row? == column {
            return Ok(());
        }
    }
    connection.execute(
        &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
        [],
    )?;
    Ok(())
}

fn prune_provider_conversations_tx(
    transaction: &Transaction<'_>,
    provider: &str,
    limit: usize,
) -> Result<()> {
    let candidates = {
        let mut statement =
            transaction.prepare("SELECT data_json FROM conversations ORDER BY updated_at DESC")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut candidates = Vec::new();
        for row in rows {
            let conversation: Conversation = serde_json::from_str(&row?)?;
            if !conversation.archived && conversation.provider.as_deref() == Some(provider) {
                candidates.push(conversation.id);
            }
        }
        candidates
    };
    for conversation_id in candidates.into_iter().skip(limit) {
        delete_conversation_tx(transaction, conversation_id)?;
    }
    Ok(())
}

fn delete_conversation_tx(
    transaction: &Transaction<'_>,
    conversation_id: ConversationId,
) -> Result<()> {
    let id = conversation_id.to_string();
    let run_query = "SELECT id FROM runs WHERE conversation_id = ?1";
    for table in [
        "approvals",
        "tool_calls",
        "model_calls",
        "token_usage",
        "timings",
        "file_changes",
        "errors",
        "checkpoints",
        "completion_objects",
        "workspace_leases",
        "permission_rules",
    ] {
        transaction.execute(
            &format!("DELETE FROM {table} WHERE run_id IN ({run_query})"),
            [&id],
        )?;
    }
    transaction.execute("DELETE FROM messages WHERE conversation_id = ?1", [&id])?;
    transaction.execute("DELETE FROM events WHERE conversation_id = ?1", [&id])?;
    transaction.execute(
        "DELETE FROM tasks WHERE run_id IN (SELECT id FROM runs WHERE conversation_id = ?1)",
        [&id],
    )?;
    transaction.execute(
        "DELETE FROM agents WHERE run_id IN (SELECT id FROM runs WHERE conversation_id = ?1)",
        [&id],
    )?;
    transaction.execute("DELETE FROM runs WHERE conversation_id = ?1", [&id])?;
    transaction.execute("DELETE FROM conversations WHERE id = ?1", [&id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Store, StoreError};
    use opensrc_core::{
        Agent, AgentStatus, ApprovalDecision, Budgets, ContextPolicy, ExecutionMode,
        MessageContent, MessageRole, PermissionEffect, ReasoningConfig, RetryPolicy,
        RoutingBenchmarkMetrics, RoutingBenchmarkQuery, RoutingBenchmarkResult, SandboxPolicy,
        Task, TaskContract, TaskStatus, ToolPolicy, Workspace, WorkspaceLeaseMode,
        WorkspaceLeaseRequest, WorkspaceLeaseState, WorkspaceMode,
    };
    use std::collections::BTreeMap;
    use uuid::Uuid;

    fn test_agent(run_id: Uuid) -> Agent {
        test_agent_at(run_id, "/root")
    }

    fn test_agent_at(run_id: Uuid, canonical_path: &str) -> Agent {
        let now = chrono::Utc::now();
        Agent {
            id: Uuid::new_v4(),
            run_id,
            canonical_path: canonical_path.to_string(),
            parent_id: None,
            child_ids: Vec::new(),
            role: "generalist".to_string(),
            task: "test".to_string(),
            status: AgentStatus::Created,
            provider: "mock".to_string(),
            model: "mock".to_string(),
            reasoning: ReasoningConfig::default(),
            system_instructions: String::new(),
            context_policy: ContextPolicy::default(),
            tool_policy: ToolPolicy::default(),
            workspace: Workspace {
                mode: WorkspaceMode::SharedReadonly,
                root: ".".to_string(),
                owned_paths: Vec::new(),
            },
            sandbox_policy: SandboxPolicy::default(),
            budgets: Budgets::default(),
            retry_policy: RetryPolicy::default(),
            fallback_chain: Vec::new(),
            completion_schema: "task_completion".to_string(),
            created_at: now,
            updated_at: now,
        }
    }

    fn test_task(run_id: Uuid, agent_id: Uuid, owned_path: &str) -> Task {
        let now = chrono::Utc::now();
        Task {
            id: Uuid::new_v4(),
            run_id,
            description: "bounded write".to_string(),
            dependencies: Vec::new(),
            assigned_agent: Some(agent_id),
            status: TaskStatus::Ready,
            priority: 0,
            expected_output: "agent_completion_v1".to_string(),
            contract: TaskContract::default(),
            workspace_ownership: vec![owned_path.to_string()],
            allowed_tools: vec!["fs.write".to_string()],
            retry_policy: RetryPolicy::default(),
            created_at: now,
            updated_at: now,
        }
    }

    fn lease_fixture(store: &Store) -> (Uuid, Agent, Task, Agent, Task) {
        let conversation = store.create_conversation(".", None).expect("conversation");
        let run = store
            .create_run(conversation.id, "parallel writes", ExecutionMode::Agentic)
            .expect("run");
        let first_agent = test_agent_at(run.id, "/root/writer-1");
        let second_agent = test_agent_at(run.id, "/root/writer-2");
        store.create_agent(&first_agent).expect("first agent");
        store.create_agent(&second_agent).expect("second agent");
        let first_task = test_task(run.id, first_agent.id, "src/auth/**");
        let second_task = test_task(run.id, second_agent.id, "src/billing/**");
        store.create_task(&first_task).expect("first task");
        store.create_task(&second_task).expect("second task");
        (run.id, first_agent, first_task, second_agent, second_task)
    }

    fn test_routing_benchmark(
        role: &str,
        provider: &str,
        model: &str,
        scenario_id: &str,
        metrics: RoutingBenchmarkMetrics,
    ) -> RoutingBenchmarkResult {
        let now = chrono::Utc::now();
        RoutingBenchmarkResult {
            id: Uuid::new_v4(),
            policy_version: "routing-v1".to_string(),
            role: role.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            scenario_id: scenario_id.to_string(),
            metrics,
            metadata: BTreeMap::from([("suite".to_string(), "project-opensource".to_string())]),
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn persists_conversation_run_agent_and_events() {
        let store = Store::in_memory().expect("store");
        let conversation = store
            .create_conversation(".", Some("Test".to_string()))
            .expect("conversation");
        let run = store
            .create_run(conversation.id, "inspect", ExecutionMode::Agentic)
            .expect("run");
        let agent = test_agent(run.id);
        store.create_agent(&agent).expect("agent");
        store
            .transition_agent(agent.id, AgentStatus::Queued)
            .expect("queue");
        let loaded = store.get_agent(agent.id).expect("load");
        assert_eq!(loaded.status, AgentStatus::Queued);
        assert!(store.events_after(0, 100).expect("events").len() >= 4);
    }

    #[test]
    fn rejects_invalid_agent_transition() {
        let store = Store::in_memory().expect("store");
        let conversation = store.create_conversation(".", None).expect("conversation");
        let run = store
            .create_run(conversation.id, "inspect", ExecutionMode::Agentic)
            .expect("run");
        let agent = test_agent(run.id);
        store.create_agent(&agent).expect("agent");
        assert!(
            store
                .transition_agent(agent.id, AgentStatus::Completed)
                .is_err()
        );
    }

    #[test]
    fn writer_leases_reject_canonical_windows_and_glob_overlaps() {
        let store = Store::in_memory().expect("store");
        let (run_id, first_agent, first_task, second_agent, second_task) = lease_fixture(&store);
        let first = store
            .acquire_workspace_lease(&WorkspaceLeaseRequest {
                run_id,
                agent_id: first_agent.id,
                task_id: Some(first_task.id),
                mode: WorkspaceLeaseMode::Write,
                root: r"C:\Repo".to_string(),
                owned_paths: vec![r"src\auth\**".to_string()],
            })
            .expect("first lease");
        let replay = store
            .acquire_workspace_lease(&WorkspaceLeaseRequest {
                run_id,
                agent_id: first_agent.id,
                task_id: Some(first_task.id),
                mode: WorkspaceLeaseMode::Write,
                root: "c:/repo".to_string(),
                owned_paths: vec!["src/auth/**".to_string()],
            })
            .expect("idempotent acquisition");
        assert_eq!(replay.id, first.id);

        let conflict = store.acquire_workspace_lease(&WorkspaceLeaseRequest {
            run_id,
            agent_id: second_agent.id,
            task_id: Some(second_task.id),
            mode: WorkspaceLeaseMode::Write,
            root: "C:/REPO/src".to_string(),
            owned_paths: vec!["auth/*.rs".to_string()],
        });
        assert!(matches!(
            conflict,
            Err(super::StoreError::WorkspaceLeaseConflict {
                conflicting_lease_id,
                ..
            }) if conflicting_lease_id == first.id
        ));
        assert_eq!(
            store
                .list_workspace_leases(Some(run_id))
                .expect("leases")
                .len(),
            1
        );
    }

    #[test]
    fn independent_writer_scopes_can_run_and_release_filters_are_idempotent() {
        let store = Store::in_memory().expect("store");
        let (run_id, first_agent, first_task, second_agent, second_task) = lease_fixture(&store);
        let first = store
            .acquire_workspace_lease(&WorkspaceLeaseRequest {
                run_id,
                agent_id: first_agent.id,
                task_id: Some(first_task.id),
                mode: WorkspaceLeaseMode::Write,
                root: "C:/repo".to_string(),
                owned_paths: vec!["src/auth/**".to_string()],
            })
            .expect("first lease");
        store
            .acquire_workspace_lease(&WorkspaceLeaseRequest {
                run_id,
                agent_id: second_agent.id,
                task_id: Some(second_task.id),
                mode: WorkspaceLeaseMode::Write,
                root: "c:/repo".to_string(),
                owned_paths: vec!["src/billing/**".to_string()],
            })
            .expect("independent lease");

        assert_eq!(
            store
                .release_workspace_leases_by_task(first_task.id)
                .expect("release by task"),
            1
        );
        assert_eq!(
            store
                .release_workspace_leases_by_task(first_task.id)
                .expect("repeat release"),
            0
        );
        assert_eq!(
            store
                .release_workspace_leases_by_agent(second_agent.id)
                .expect("release by agent"),
            1
        );
        let states = store
            .list_workspace_leases(Some(run_id))
            .expect("released leases");
        assert!(states.iter().all(|lease| {
            lease.state == WorkspaceLeaseState::Released && lease.released_at.is_some()
        }));

        let third = store
            .acquire_workspace_lease(&WorkspaceLeaseRequest {
                run_id,
                agent_id: first_agent.id,
                task_id: Some(first_task.id),
                mode: WorkspaceLeaseMode::Write,
                root: "c:/repo".to_string(),
                owned_paths: vec!["src".to_string()],
            })
            .expect("lease after release");
        assert_ne!(third.id, first.id);
        assert_eq!(
            store
                .release_workspace_leases_by_run(run_id)
                .expect("release by run"),
            1
        );
    }

    #[test]
    fn reopening_recovers_abandoned_active_leases() {
        let root = std::env::temp_dir().join(format!("opensrc-lease-reopen-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("create temporary directory");
        let database_path = root.join("state.sqlite3");
        let run_id;
        let agent_id;
        let task_id;
        {
            let store = Store::open(&database_path).expect("initial store");
            let fixture = lease_fixture(&store);
            run_id = fixture.0;
            agent_id = fixture.1.id;
            task_id = fixture.2.id;
            store
                .acquire_workspace_lease(&WorkspaceLeaseRequest {
                    run_id,
                    agent_id,
                    task_id: Some(task_id),
                    mode: WorkspaceLeaseMode::Write,
                    root: "C:/repo".to_string(),
                    owned_paths: vec!["src/**".to_string()],
                })
                .expect("active lease");
        }

        {
            let store = Store::open(&database_path).expect("reopened store");
            let leases = store
                .list_workspace_leases(Some(run_id))
                .expect("recovered leases");
            assert_eq!(leases.len(), 1);
            assert_eq!(leases[0].state, WorkspaceLeaseState::Recovered);
            assert!(leases[0].released_at.is_some());
            store
                .acquire_workspace_lease(&WorkspaceLeaseRequest {
                    run_id,
                    agent_id,
                    task_id: Some(task_id),
                    mode: WorkspaceLeaseMode::Write,
                    root: "c:/repo".to_string(),
                    owned_paths: vec!["src/**".to_string()],
                })
                .expect("lease after recovery");
        }

        std::fs::remove_dir_all(&root).expect("remove temporary directory");
    }

    #[test]
    fn event_idempotency_returns_original_cursor_without_duplicate() {
        let store = Store::in_memory().expect("store");
        let conversation = store.create_conversation(".", None).expect("conversation");
        let run = store
            .create_run(conversation.id, "inspect", ExecutionMode::Focused)
            .expect("run");

        let first = store
            .append_event(
                run.id,
                None,
                None,
                "test.idempotent",
                &serde_json::json!({"attempt": 1}),
                Some("stable-key"),
            )
            .expect("first event");
        let second = store
            .append_event(
                run.id,
                None,
                None,
                "test.idempotent",
                &serde_json::json!({"attempt": 2}),
                Some("stable-key"),
            )
            .expect("duplicate event");

        assert_eq!(first, second);
        assert_eq!(
            store
                .events_after(0, 100)
                .expect("events")
                .iter()
                .filter(|event| event.kind == "test.idempotent")
                .count(),
            1
        );
    }

    #[test]
    fn persists_and_applies_scoped_permission_rules() {
        let store = Store::in_memory().expect("store");
        let conversation = store
            .create_conversation("C:/workspace", None)
            .expect("conversation");
        let run = store
            .create_run(conversation.id, "edit", ExecutionMode::Focused)
            .expect("run");
        let arguments = serde_json::json!({"path": "src/lib.rs", "content": "updated"});
        let approval = store
            .create_approval(
                run.id,
                None,
                None,
                "fs.write",
                arguments.clone(),
                vec!["file mutation requires approval".to_string()],
            )
            .expect("approval");
        store
            .decide_approval(
                approval.id,
                ApprovalDecision::AllowProject,
                None,
                Some("trusted project edit".to_string()),
            )
            .expect("decision");
        assert_eq!(
            store
                .permission_effect(run.id, "fs.write", &arguments)
                .expect("permission lookup"),
            Some(PermissionEffect::Allow)
        );
        assert_eq!(
            store
                .permission_effect(
                    run.id,
                    "fs.write",
                    &serde_json::json!({"path": "different"})
                )
                .expect("different arguments"),
            None
        );
        let rules = store.list_permission_rules().expect("rules");
        assert_eq!(rules.len(), 1);
        store
            .delete_permission_rule(rules[0].id)
            .expect("delete rule");
        assert!(
            store
                .list_permission_rules()
                .expect("rules after delete")
                .is_empty()
        );
    }

    #[test]
    fn always_allow_all_applies_to_every_tool_and_argument() {
        let store = Store::in_memory().expect("store");
        let conversation = store
            .create_conversation("C:/workspace", None)
            .expect("conversation");
        let run = store
            .create_run(
                conversation.id,
                "trusted automation",
                ExecutionMode::Focused,
            )
            .expect("run");
        let approval = store
            .create_approval(
                run.id,
                None,
                None,
                "shell.run",
                serde_json::json!({"program": "cargo", "args": ["test"]}),
                vec!["process execution requires approval".to_string()],
            )
            .expect("approval");
        store
            .decide_approval(
                approval.id,
                ApprovalDecision::AlwaysAllowAll,
                None,
                Some("trusted local agent".to_string()),
            )
            .expect("decision");

        for (tool, arguments) in [
            ("fs.write", serde_json::json!({"path": "src/lib.rs"})),
            (
                "shell.run",
                serde_json::json!({"program": "cargo", "args": ["clippy"]}),
            ),
            (
                "mcp.invoke",
                serde_json::json!({"server": "browser", "tool": "open"}),
            ),
        ] {
            assert_eq!(
                store
                    .permission_effect(run.id, tool, &arguments)
                    .expect("permission lookup"),
                Some(PermissionEffect::Allow)
            );
        }
        let rule = store
            .list_permission_rules()
            .expect("rules")
            .into_iter()
            .next()
            .expect("wildcard rule");
        assert_eq!(rule.tool_name, "*");
        assert_eq!(rule.arguments_pattern, serde_json::Value::Null);
    }

    #[test]
    fn keeps_only_five_newest_sessions_for_each_provider() {
        let store = Store::in_memory().expect("store");
        let mut oldest_alpha = None;
        for index in 0..7 {
            let conversation = store
                .create_conversation(".", Some(format!("alpha-{index}")))
                .expect("conversation");
            oldest_alpha.get_or_insert(conversation.id);
            store
                .update_conversation_selection(
                    conversation.id,
                    Some("alpha".to_string()),
                    Some("model-a".to_string()),
                    None,
                    None,
                    None,
                    None,
                )
                .expect("alpha selection");
            if index == 0 {
                let run = store
                    .create_run(conversation.id, "old request", ExecutionMode::Focused)
                    .expect("old run");
                store
                    .append_message(
                        conversation.id,
                        Some(run.id),
                        opensrc_core::MessageRole::User,
                        vec![opensrc_core::MessageContent::Text {
                            text: "old message".to_string(),
                        }],
                        Some("alpha"),
                        Some("model-a"),
                        None,
                    )
                    .expect("old message");
            }
        }
        for index in 0..6 {
            let conversation = store
                .create_conversation(".", Some(format!("beta-{index}")))
                .expect("conversation");
            store
                .update_conversation_selection(
                    conversation.id,
                    Some("beta".to_string()),
                    Some("model-b".to_string()),
                    None,
                    None,
                    None,
                    None,
                )
                .expect("beta selection");
        }
        let sessions = store.list_conversations(None).expect("sessions");
        assert_eq!(
            sessions
                .iter()
                .filter(|session| session.provider.as_deref() == Some("alpha"))
                .count(),
            5
        );
        assert_eq!(
            sessions
                .iter()
                .filter(|session| session.provider.as_deref() == Some("beta"))
                .count(),
            5
        );
        assert!(matches!(
            store.get_conversation(oldest_alpha.expect("oldest alpha")),
            Err(super::StoreError::NotFound { .. })
        ));
    }

    #[test]
    fn permanently_deletes_a_conversation_and_its_messages() {
        let store = Store::in_memory().expect("store");
        let conversation = store
            .create_conversation(".", Some("temporary session".to_string()))
            .expect("conversation");
        store
            .append_message(
                conversation.id,
                None,
                MessageRole::User,
                vec![MessageContent::text("remove me")],
                None,
                None,
                None,
            )
            .expect("message");

        store
            .delete_conversation(conversation.id)
            .expect("delete conversation");

        assert!(matches!(
            store.get_conversation(conversation.id),
            Err(StoreError::NotFound { .. })
        ));
        assert!(store.list_conversations(None).expect("sessions").is_empty());
    }

    #[test]
    fn routing_benchmarks_persist_across_reopen_and_support_crud() {
        let root = std::env::temp_dir().join(format!(
            "opensrc-routing-benchmark-reopen-{}",
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("create temporary directory");
        let database_path = root.join("state.sqlite3");
        let mut benchmark = test_routing_benchmark(
            "architect",
            "openrouter",
            "deepseek-v4-pro",
            "architecture-001",
            RoutingBenchmarkMetrics {
                architecture_quality_bps: Some(9_200),
                repository_investigation_accuracy_bps: Some(8_800),
                latency_ms: 450,
                input_tokens: 1_200,
                output_tokens: 480,
                cache_hits: 3,
                cost_microusd: 1_250,
                retry_rate_bps: Some(250),
                ..RoutingBenchmarkMetrics::default()
            },
        );
        let benchmark_id = benchmark.id;

        {
            let store = Store::open(&database_path).expect("initial store");
            store
                .record_routing_benchmark(&benchmark)
                .expect("record benchmark");
            assert_eq!(
                store
                    .get_routing_benchmark(benchmark_id)
                    .expect("load benchmark"),
                benchmark
            );
        }

        {
            let store = Store::open(&database_path).expect("reopened store");
            let persisted = store
                .list_routing_benchmarks(&RoutingBenchmarkQuery {
                    provider: Some("openrouter".to_string()),
                    ..RoutingBenchmarkQuery::default()
                })
                .expect("list persisted benchmarks");
            assert_eq!(persisted, vec![benchmark.clone()]);

            benchmark.metrics.architecture_quality_bps = Some(9_500);
            benchmark.updated_at = chrono::Utc::now();
            store
                .upsert_routing_benchmark(&benchmark)
                .expect("update benchmark");
            assert_eq!(
                store
                    .get_routing_benchmark(benchmark_id)
                    .expect("load updated benchmark")
                    .metrics
                    .architecture_quality_bps,
                Some(9_500)
            );

            assert!(
                store
                    .delete_routing_benchmark(benchmark_id)
                    .expect("delete benchmark")
            );
            assert!(
                !store
                    .delete_routing_benchmark(benchmark_id)
                    .expect("delete missing benchmark")
            );
            assert!(matches!(
                store.get_routing_benchmark(benchmark_id),
                Err(super::StoreError::NotFound { .. })
            ));
        }

        std::fs::remove_dir_all(&root).expect("remove temporary directory");
    }

    #[test]
    fn routing_benchmark_aggregates_are_filtered_per_route() {
        let store = Store::in_memory().expect("store");
        let first_architect = test_routing_benchmark(
            "architect",
            "openrouter",
            "deepseek-v4-pro",
            "architecture-001",
            RoutingBenchmarkMetrics {
                architecture_quality_bps: Some(9_000),
                repository_investigation_accuracy_bps: Some(8_000),
                latency_ms: 100,
                input_tokens: 1_000,
                output_tokens: 300,
                cache_hits: 2,
                cost_microusd: 900,
                retry_rate_bps: Some(500),
                ..RoutingBenchmarkMetrics::default()
            },
        );
        let second_architect = test_routing_benchmark(
            "architect",
            "openrouter",
            "deepseek-v4-pro",
            "architecture-002",
            RoutingBenchmarkMetrics {
                architecture_quality_bps: Some(7_000),
                latency_ms: 300,
                input_tokens: 3_000,
                output_tokens: 700,
                cache_hits: 4,
                cost_microusd: 1_100,
                retry_rate_bps: Some(1_500),
                ..RoutingBenchmarkMetrics::default()
            },
        );
        let frontend = test_routing_benchmark(
            "frontend-specialist",
            "zen",
            "glm-4.5",
            "frontend-001",
            RoutingBenchmarkMetrics {
                frontend_implementation_quality_bps: Some(9_400),
                latency_ms: 50,
                input_tokens: 500,
                output_tokens: 200,
                cache_hits: 1,
                cost_microusd: 400,
                ..RoutingBenchmarkMetrics::default()
            },
        );
        for benchmark in [&first_architect, &second_architect, &frontend] {
            store
                .record_routing_benchmark(benchmark)
                .expect("record benchmark");
        }

        let aggregates = store
            .aggregate_routing_benchmarks(&RoutingBenchmarkQuery {
                role: Some("architect".to_string()),
                ..RoutingBenchmarkQuery::default()
            })
            .expect("aggregate architect route");
        assert_eq!(aggregates.len(), 1);
        let aggregate = &aggregates[0];
        assert_eq!(aggregate.policy_version, "routing-v1");
        assert_eq!(aggregate.role, "architect");
        assert_eq!(aggregate.provider, "openrouter");
        assert_eq!(aggregate.model, "deepseek-v4-pro");
        assert_eq!(aggregate.samples, 2);
        assert_eq!(aggregate.mean_metrics.architecture_quality_bps, Some(8_000));
        assert_eq!(
            aggregate.mean_metrics.repository_investigation_accuracy_bps,
            Some(8_000)
        );
        assert_eq!(aggregate.mean_metrics.latency_ms, 200);
        assert_eq!(aggregate.mean_metrics.input_tokens, 2_000);
        assert_eq!(aggregate.mean_metrics.output_tokens, 500);
        assert_eq!(aggregate.mean_metrics.cache_hits, 3);
        assert_eq!(aggregate.mean_metrics.cost_microusd, 1_000);
        assert_eq!(aggregate.mean_metrics.retry_rate_bps, Some(1_000));
    }

    #[test]
    fn rejects_invalid_routing_benchmark_scores() {
        let store = Store::in_memory().expect("store");
        let benchmark = test_routing_benchmark(
            "reviewer",
            "zen",
            "review-model",
            "review-001",
            RoutingBenchmarkMetrics {
                review_precision_bps: Some(10_001),
                ..RoutingBenchmarkMetrics::default()
            },
        );

        assert!(matches!(
            store.record_routing_benchmark(&benchmark),
            Err(super::StoreError::InvalidRoutingBenchmark(_))
        ));
    }
}

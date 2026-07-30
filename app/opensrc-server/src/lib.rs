use axum::extract::Request;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use opensrc_core::{
    Agent, AgentDefinition, Approval, ApprovalDecision, CanonicalMessage, CanonicalModelRequest,
    CommandDescriptor, ContextPolicy, Event as DomainEvent, ExecutionMode, FileChange, Message,
    MessageContent, MessageRole, PermissionRule, ProviderCapabilities, ProviderError,
    RoutingBenchmarkAggregate, RoutingBenchmarkQuery, RoutingBenchmarkResult, Run,
    RunExecutionResult, RunId, Task, TaskCompletion, builtin_commands,
};
use opensrc_providers::{
    GeminiAdapter, GeminiConfig, OpenAiCompatibleAdapter, OpenAiCompatibleConfig,
    OpenAiCompatibleFamily, ProviderConfigError, ProviderEntry, ProviderFile, ProviderProtocol,
    is_anonymous_local_compatible, read_provider_file, store_provider_credential,
};
use opensrc_runtime::{
    AgentControlError, ChangeError, DefinitionError, ExecutionError, McpError, McpServer,
    ModeClassifier, ModelPack, ModelPackError, ModelPackStage, RequiredCapabilities, RolePolicy,
    RolePolicyDescriptor, RouterError, RoutingPolicyError, RoutingPolicySet, Runtime, SkillError,
    ToolExecutionError, apply_role_policy, built_in_agent_definitions, discover_agent_definitions,
    discover_custom_commands, model_is_chat_capable, resolve_agent_definition, selected_file_paths,
};
use opensrc_store::StoreError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::convert::Infallible;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;

#[derive(Clone)]
pub struct ServerState {
    pub runtime: Runtime,
    pub provider_config_path: Option<PathBuf>,
}

#[allow(clippy::too_many_lines)]
pub fn router(state: ServerState) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/openapi.json", get(openapi))
        .route(
            "/v1/conversations",
            get(list_conversations).post(create_conversation),
        )
        .route("/v1/conversations/import", post(import_conversation))
        .route(
            "/v1/conversations/{id}",
            get(get_conversation).delete(delete_conversation),
        )
        .route("/v1/conversations/{id}/rename", post(rename_conversation))
        .route("/v1/conversations/{id}/archive", post(archive_conversation))
        .route("/v1/conversations/{id}/fork", post(fork_conversation))
        .route("/v1/conversations/{id}/export", get(export_conversation))
        .route("/v1/conversations/{id}/compact", post(compact_conversation))
        .route(
            "/v1/conversations/{id}/messages",
            get(list_conversation_messages),
        )
        .route(
            "/v1/conversations/{id}/selection",
            post(update_conversation_selection),
        )
        .route("/v1/chat", post(chat))
        .route("/v1/runs", post(create_run))
        .route("/v1/runs/{id}", get(get_run))
        .route("/v1/runs/{id}/execute", post(execute_run))
        .route("/v1/runs/{id}/cancel", post(cancel_run))
        .route("/v1/providers", get(list_providers))
        .route("/v1/providers/connect", post(connect_provider))
        .route("/v1/providers/{id}", delete(disconnect_provider))
        .route("/v1/models", get(list_models))
        .route(
            "/v1/routing-policies",
            get(list_routing_policies).post(replace_routing_policies),
        )
        .route("/v1/routing-policies/{role}", post(upsert_role_policy))
        .route(
            "/v1/model-packs",
            get(list_model_packs).post(upsert_model_pack),
        )
        .route("/v1/model-packs/{id}", delete(remove_model_pack))
        .route("/v1/approvals", get(list_approvals))
        .route("/v1/approvals/{id}/decision", post(decide_approval))
        .route("/v1/permissions", get(list_permissions))
        .route("/v1/permissions/{id}", delete(delete_permission))
        .route("/v1/changes", get(list_changes))
        .route("/v1/changes/{id}/undo", post(undo_change))
        .route("/v1/changes/{id}/redo", post(redo_change))
        .route(
            "/v1/checkpoints",
            get(list_checkpoints).post(create_checkpoint),
        )
        .route("/v1/checkpoints/{id}/restore", post(restore_checkpoint))
        .route("/v1/tools", get(list_tools))
        .route(
            "/v1/workspace/roots",
            get(list_workspace_roots)
                .post(add_workspace_root)
                .delete(remove_workspace_root),
        )
        .route("/v1/commands", get(list_commands))
        .route("/v1/commands/custom", get(list_custom_commands))
        .route("/v1/skills", get(list_skills))
        .route("/v1/skills/{name}/activate", post(activate_skill))
        .route("/v1/mcp", get(list_mcp_servers).post(upsert_mcp_server))
        .route("/v1/mcp/{name}", delete(remove_mcp_server))
        .route("/v1/mcp/{name}/enable", post(enable_mcp_server))
        .route("/v1/mcp/{name}/disable", post(disable_mcp_server))
        .route("/v1/mcp/{name}/debug", post(debug_mcp_server))
        .route("/v1/metrics", get(get_metrics))
        .route(
            "/v1/routing-benchmarks",
            get(list_routing_benchmarks).post(record_routing_benchmark),
        )
        .route(
            "/v1/routing-benchmarks/aggregate",
            get(aggregate_routing_benchmarks),
        )
        .route(
            "/v1/routing-benchmarks/{role}/promote",
            post(promote_routing_benchmark),
        )
        .route("/v1/agents", get(list_agents))
        .route("/v1/agent-definitions", get(list_agent_definitions))
        .route("/v1/agents/wait", post(wait_for_agents))
        .route("/v1/agents/root", post(create_root_agent))
        .route("/v1/agents/spawn", post(spawn_agent))
        .route("/v1/agents/{id}", get(get_agent_status))
        .route("/v1/agents/{id}/messages", post(send_message))
        .route("/v1/agents/{id}/followups", post(assign_followup))
        .route("/v1/agents/{id}/start", post(start_agent))
        .route("/v1/agents/{id}/wait", post(wait_agent))
        .route("/v1/agents/{id}/interrupt", post(interrupt_agent))
        .route("/v1/agents/{id}/resume", post(resume_agent))
        .route("/v1/agents/{id}/complete", post(complete_task))
        .route("/v1/tasks", get(list_tasks))
        .route("/v1/tasks/ready", get(list_ready_tasks))
        .route("/v1/tasks/{id}", get(get_task))
        .route("/v1/tasks/{id}/start", post(start_task))
        .route("/v1/tasks/{id}/reassign", post(reassign_task))
        .route("/v1/events", get(list_events))
        .route("/v1/events/cursor", get(event_cursor))
        .route("/v1/events/stream", get(stream_events))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

pub async fn serve(state: ServerState, address: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(address).await?;
    tracing::info!(%address, "Project OpenSource app server listening");
    let mut app = router(state);
    if !address.ip().is_loopback() {
        let token = std::env::var("OPENSOURCE_SERVER_TOKEN").map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "OPENSOURCE_SERVER_TOKEN is required for non-loopback binding",
            )
        })?;
        if token.len() < 24 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "OPENSOURCE_SERVER_TOKEN must be at least 24 characters",
            ));
        }
        app = app.layer(middleware::from_fn_with_state(
            Arc::<str>::from(token),
            require_bearer,
        ));
    }
    if let Ok(origins) = std::env::var("OPENSOURCE_CORS_ORIGIN") {
        let origins = origins
            .split(',')
            .map(str::trim)
            .filter(|origin| !origin.is_empty())
            .map(|origin| {
                origin.parse().map_err(|error| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("invalid OPENSOURCE_CORS_ORIGIN `{origin}`: {error}"),
                    )
                })
            })
            .collect::<std::io::Result<Vec<axum::http::HeaderValue>>>()?;
        if !origins.is_empty() {
            app = app.layer(
                CorsLayer::new()
                    .allow_origin(AllowOrigin::list(origins))
                    .allow_methods([
                        axum::http::Method::GET,
                        axum::http::Method::POST,
                        axum::http::Method::DELETE,
                    ])
                    .allow_headers([
                        axum::http::header::CONTENT_TYPE,
                        axum::http::header::AUTHORIZATION,
                    ]),
            );
        }
    }
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
}

async fn require_bearer(State(token): State<Arc<str>>, request: Request, next: Next) -> Response {
    let expected = format!("Bearer {token}");
    if request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == expected)
    {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": {
                    "code": "unauthorized",
                    "message": "a valid bearer token is required"
                }
            })),
        )
            .into_response()
    }
}

#[derive(Debug, Error)]
enum ApiError {
    #[error("{0}")]
    Store(#[from] StoreError),
    #[error("{0}")]
    Agent(#[from] AgentControlError),
    #[error("{0}")]
    Execution(#[from] ExecutionError),
    #[error("{0}")]
    Skill(#[from] SkillError),
    #[error("{0}")]
    Mcp(#[from] McpError),
    #[error("{0}")]
    Definition(#[from] DefinitionError),
    #[error("{0}")]
    ModelPack(#[from] ModelPackError),
    #[error("{0}")]
    RoutingPolicy(#[from] RoutingPolicyError),
    #[error("{0}")]
    ProviderConfig(#[from] ProviderConfigError),
    #[error("{0}")]
    Provider(#[from] ProviderError),
    #[error("{0}")]
    Router(#[from] RouterError),
    #[error("{0}")]
    Change(#[from] ChangeError),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    BadRequest(String),
    #[error("invalid identifier: {0}")]
    InvalidId(String),
}

impl ApiError {
    fn public_message(&self) -> String {
        match self {
            Self::Provider(error) | Self::Execution(ExecutionError::Provider(error)) => {
                public_provider_error(error).to_string()
            }
            Self::Router(RouterError::ModelDiscovery { provider, .. })
            | Self::Execution(ExecutionError::Router(RouterError::ModelDiscovery {
                provider,
                ..
            })) => {
                format!("model discovery failed for provider `{provider}`")
            }
            Self::ProviderConfig(ProviderConfigError::CredentialStore { provider, .. }) => {
                format!("credential storage failed for provider `{provider}`")
            }
            Self::ProviderConfig(ProviderConfigError::InvalidBaseUrl { provider, .. }) => {
                format!("provider `{provider}` has an invalid base URL")
            }
            _ => self.to_string(),
        }
    }
}

fn public_provider_error(error: &ProviderError) -> &'static str {
    match error {
        ProviderError::Authentication(_) => "provider authentication failed",
        ProviderError::Transient(_) | ProviderError::RateLimited { .. } => {
            "provider is temporarily unavailable"
        }
        ProviderError::Rejected(_) => "provider rejected the request",
        ProviderError::InvalidResponse(_) => "provider returned an invalid response",
    }
}

impl IntoResponse for ApiError {
    #[allow(clippy::too_many_lines)]
    fn into_response(self) -> Response {
        let status = match &self {
            Self::Store(StoreError::NotFound { .. })
            | Self::Skill(SkillError::Unknown(_))
            | Self::Mcp(McpError::UnknownServer(_))
            | Self::ModelPack(ModelPackError::Unknown(_))
            | Self::RoutingPolicy(RoutingPolicyError::UnknownRole(_)) => StatusCode::NOT_FOUND,
            Self::Store(
                StoreError::InvalidAgentTransition { .. }
                | StoreError::InvalidRunTransition { .. }
                | StoreError::InvalidTaskTransition { .. }
                | StoreError::ApprovalNotPending(_)
                | StoreError::InvalidFileChangeState { .. },
            )
            | Self::Agent(
                AgentControlError::DepthLimit(_)
                | AgentControlError::ChildLimit(_)
                | AgentControlError::RunLimit(_)
                | AgentControlError::ConcurrencyLimit(_)
                | AgentControlError::WriterConcurrencyLimit(_)
                | AgentControlError::DeepReasoningConcurrencyLimit(_)
                | AgentControlError::SpawnDenied(_)
                | AgentControlError::InvalidRoot
                | AgentControlError::TaskAssignment { .. }
                | AgentControlError::TaskNotReady(_)
                | AgentControlError::DifferentRun { .. }
                | AgentControlError::MessageTargetTerminal(_)
                | AgentControlError::InvalidOwnedPath(_),
            )
            | Self::Execution(
                ExecutionError::InvalidRunState { .. }
                | ExecutionError::MissingRootAgent(_)
                | ExecutionError::ToolCallInFlight(_)
                | ExecutionError::TurnLimit(_)
                | ExecutionError::Cancelled(_)
                | ExecutionError::Tool(
                    ToolExecutionError::Denied { .. } | ToolExecutionError::ApprovalRequired { .. },
                ),
            )
            | Self::Change(
                ChangeError::ConcurrentModification { .. }
                | ChangeError::NoPatch(_)
                | ChangeError::Store(StoreError::InvalidFileChangeState { .. }),
            ) => StatusCode::CONFLICT,
            Self::Store(StoreError::InvalidRoutingBenchmark(_))
            | Self::InvalidId(_)
            | Self::Mcp(McpError::InvalidName(_) | McpError::Disabled(_))
            | Self::BadRequest(_)
            | Self::ProviderConfig(
                ProviderConfigError::MissingCredential { .. }
                | ProviderConfigError::InvalidBaseUrl { .. }
                | ProviderConfigError::DuplicateId(_)
                | ProviderConfigError::Invalid { .. },
            )
            | Self::Execution(ExecutionError::Router(
                RouterError::UnknownProvider(_) | RouterError::MissingCapability { .. },
            ))
            | Self::Router(
                RouterError::UnknownProvider(_) | RouterError::MissingCapability { .. },
            )
            | Self::ModelPack(ModelPackError::Invalid(_))
            | Self::RoutingPolicy(
                RoutingPolicyError::Invalid(_) | RoutingPolicyError::ModelUnavailable { .. },
            )
            | Self::Change(ChangeError::UnsafePath(_)) => StatusCode::BAD_REQUEST,
            Self::Router(RouterError::ModelDiscovery { .. })
            | Self::Execution(
                ExecutionError::Router(RouterError::ModelDiscovery { .. })
                | ExecutionError::Provider(_),
            )
            | Self::Provider(_)
            | Self::Mcp(
                McpError::Http(_) | McpError::Protocol(_) | McpError::Timeout(_) | McpError::Io(_),
            ) => StatusCode::BAD_GATEWAY,
            Self::Store(_)
            | Self::Agent(_)
            | Self::Execution(_)
            | Self::Skill(_)
            | Self::Mcp(_)
            | Self::Definition(_)
            | Self::ModelPack(ModelPackError::Io { .. } | ModelPackError::InvalidFile { .. })
            | Self::RoutingPolicy(
                RoutingPolicyError::Io { .. } | RoutingPolicyError::InvalidFile { .. },
            )
            | Self::ProviderConfig(
                ProviderConfigError::Io { .. } | ProviderConfigError::CredentialStore { .. },
            )
            | Self::Io(_)
            | Self::Change(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let code = match status {
            StatusCode::BAD_REQUEST => "bad_request",
            StatusCode::NOT_FOUND => "not_found",
            StatusCode::CONFLICT => "conflict",
            StatusCode::BAD_GATEWAY => "provider_error",
            _ => "internal_error",
        };
        let message = self.public_message();
        (
            status,
            Json(json!({
                "error": {
                    "code": code,
                    "message": message
                }
            })),
        )
            .into_response()
    }
}

async fn health(State(state): State<ServerState>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "protocol": "v1",
        "version": env!("CARGO_PKG_VERSION"),
        "runtime": "canonical",
        "sandbox": {"mode": "policy_only", "protection": "limited"},
        "providers": state.runtime.providers.provider_ids()
    }))
}

async fn openapi() -> Json<Value> {
    let get = |summary: &str| {
        json!({
            "get": {
                "summary": summary,
                "responses": {"200": {"description": "Success"}}
            }
        })
    };
    let get_post = |get_summary: &str, post_summary: &str| {
        json!({
            "get": {
                "summary": get_summary,
                "responses": {"200": {"description": "Success"}}
            },
            "post": {
                "summary": post_summary,
                "responses": {"200": {"description": "Recorded"}}
            }
        })
    };
    Json(json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Project OpenSource API",
            "version": env!("CARGO_PKG_VERSION")
        },
        "servers": [{"url": "http://127.0.0.1:4545"}],
        "paths": {
            "/v1/health": get("Health and version"),
            "/v1/conversations": get("List conversations"),
            "/v1/conversations/{id}/messages": get("List durable messages"),
            "/v1/providers": get("List connected providers"),
            "/v1/models": get("List or refresh provider models"),
            "/v1/model-packs": get("List or refresh three-model packs"),
            "/v1/approvals": get("List approvals"),
            "/v1/permissions": get("List persistent permission rules"),
            "/v1/changes": get("List tracked file changes"),
            "/v1/tools": get("List dynamic tool descriptors"),
            "/v1/skills": get("List available skills"),
            "/v1/mcp": get("List configured MCP servers"),
            "/v1/commands": get("List connected product commands"),
            "/v1/agents": get("List executable agents"),
            "/v1/tasks": get("List persistent tasks"),
            "/v1/metrics": get("Read usage and timing metrics"),
            "/v1/routing-benchmarks": get_post(
                "List persisted routing benchmark results",
                "Record a routing benchmark result"
            ),
            "/v1/routing-benchmarks/aggregate": get("Aggregate routing quality, latency, and cost by role and model"),
            "/v1/routing-benchmarks/{role}/promote": {
                "post": {
                    "summary": "Promote the measured route for a role into routing policy",
                    "responses": {"200": {"description": "Updated role policy"}}
                }
            },
            "/v1/events": get("Read event pages"),
            "/v1/events/stream": get("Stream server-sent events")
        }
    }))
}

#[derive(Debug, Deserialize)]
struct CreateConversationRequest {
    project_root: String,
    title: Option<String>,
}

async fn create_conversation(
    State(state): State<ServerState>,
    Json(request): Json<CreateConversationRequest>,
) -> Result<Json<opensrc_core::Conversation>, ApiError> {
    Ok(Json(state.runtime.store.create_conversation(
        request.project_root,
        request.title,
    )?))
}

#[derive(Debug, Deserialize)]
struct ImportConversationRequest {
    project_root: String,
    document: Value,
}

async fn import_conversation(
    State(state): State<ServerState>,
    Json(request): Json<ImportConversationRequest>,
) -> Result<Json<opensrc_core::Conversation>, ApiError> {
    let source = request
        .document
        .get("conversation")
        .cloned()
        .ok_or_else(|| ApiError::BadRequest("import is missing `conversation`".to_string()))?;
    let source: opensrc_core::Conversation = serde_json::from_value(source)
        .map_err(|error| ApiError::BadRequest(format!("invalid conversation import: {error}")))?;
    let messages = request
        .document
        .get("messages")
        .cloned()
        .ok_or_else(|| ApiError::BadRequest("import is missing `messages`".to_string()))?;
    let messages: Vec<Message> = serde_json::from_value(messages)
        .map_err(|error| ApiError::BadRequest(format!("invalid message import: {error}")))?;
    let conversation = state
        .runtime
        .store
        .create_conversation(request.project_root, source.title)?;
    state.runtime.store.update_conversation_selection(
        conversation.id,
        source.provider,
        source.model,
        source.model_pack,
        source.reasoning_level,
        source.preferred_mode,
        source.agent,
    )?;
    for message in messages {
        state.runtime.store.append_message(
            conversation.id,
            None,
            message.role,
            message.content,
            message.provider.as_deref(),
            message.model.as_deref(),
            message.continuation_id.as_deref(),
        )?;
    }
    Ok(Json(state.runtime.store.get_conversation(conversation.id)?))
}

#[derive(Debug, Deserialize)]
struct ConversationQuery {
    project_root: Option<String>,
}

async fn list_conversations(
    State(state): State<ServerState>,
    Query(query): Query<ConversationQuery>,
) -> Result<Json<Vec<opensrc_core::Conversation>>, ApiError> {
    Ok(Json(
        state
            .runtime
            .store
            .list_conversations(query.project_root.as_deref())?,
    ))
}

async fn get_conversation(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> Result<Json<opensrc_core::Conversation>, ApiError> {
    Ok(Json(state.runtime.store.get_conversation(parse_id(&id)?)?))
}

async fn delete_conversation(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.runtime.store.delete_conversation(parse_id(&id)?)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct RenameConversationRequest {
    title: Option<String>,
}

async fn rename_conversation(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    Json(request): Json<RenameConversationRequest>,
) -> Result<Json<opensrc_core::Conversation>, ApiError> {
    Ok(Json(
        state
            .runtime
            .store
            .rename_conversation(parse_id(&id)?, request.title)?,
    ))
}

async fn archive_conversation(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> Result<Json<opensrc_core::Conversation>, ApiError> {
    Ok(Json(
        state.runtime.store.archive_conversation(parse_id(&id)?)?,
    ))
}

#[derive(Debug, Deserialize)]
struct ForkConversationRequest {
    through_message_id: Option<String>,
}

async fn fork_conversation(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    Json(request): Json<ForkConversationRequest>,
) -> Result<Json<opensrc_core::Conversation>, ApiError> {
    Ok(Json(
        state.runtime.store.fork_conversation(
            parse_id(&id)?,
            request
                .through_message_id
                .as_deref()
                .map(parse_id)
                .transpose()?,
        )?,
    ))
}

async fn export_conversation(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let id = parse_id(&id)?;
    let conversation = state.runtime.store.get_conversation(id)?;
    let messages = state.runtime.store.list_messages(id)?;
    let mut markdown = format!(
        "# {}\n\nProject: `{}`\n\n",
        conversation
            .title
            .as_deref()
            .unwrap_or("OpenSource session"),
        conversation.project_root
    );
    for message in &messages {
        let _ = write!(markdown, "## {:?}\n\n", message.role);
        for block in &message.content {
            match block {
                MessageContent::Text { text }
                | MessageContent::ReasoningSummary { text }
                | MessageContent::ContextSummary { text } => {
                    markdown.push_str(text);
                    markdown.push_str("\n\n");
                }
                other => {
                    markdown.push_str("```json\n");
                    markdown.push_str(
                        &serde_json::to_string_pretty(other)
                            .unwrap_or_else(|_| "{\"type\":\"unavailable\"}".to_string()),
                    );
                    markdown.push_str("\n```\n\n");
                }
            }
        }
    }
    Ok(Json(json!({
        "conversation": conversation,
        "messages": messages,
        "json": {
            "format": "opensource.session.v1",
            "conversation": conversation,
            "messages": messages
        },
        "markdown": markdown
    })))
}

async fn compact_conversation(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> Result<Json<Message>, ApiError> {
    let id = parse_id(&id)?;
    let messages = state.runtime.store.list_messages(id)?;
    if messages.is_empty() {
        return Err(ApiError::BadRequest(
            "cannot compact an empty conversation".to_string(),
        ));
    }
    let mut summary = format!(
        "Conversation summary through {} messages. Preserve these facts for future turns:\n",
        messages.len()
    );
    for message in &messages {
        let role = match message.role {
            MessageRole::System => "system",
            MessageRole::Developer => "developer",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        };
        for block in &message.content {
            let detail = match block {
                MessageContent::Text { text }
                | MessageContent::ReasoningSummary { text }
                | MessageContent::ContextSummary { text } => text.clone(),
                MessageContent::FileReference { path, .. } => format!("referenced @{path}"),
                MessageContent::ToolCall { name, .. } => format!("called tool {name}"),
                MessageContent::ToolResult { name, result, .. } => {
                    format!("tool {name} returned {result}")
                }
                MessageContent::ToolError { name, error, .. } => {
                    format!("tool {name} failed: {error}")
                }
                MessageContent::ApprovalRequest { summary, .. } => summary.clone(),
                MessageContent::ApprovalResult { decision, .. } => {
                    format!("approval decision: {decision}")
                }
            };
            let remaining = 16_000_usize.saturating_sub(summary.len());
            if remaining == 0 {
                break;
            }
            let _ = writeln!(
                summary,
                "- {role}: {}",
                detail
                    .chars()
                    .take(remaining.min(1_000))
                    .collect::<String>()
            );
        }
    }
    let compacted = state.runtime.store.append_message(
        id,
        None,
        MessageRole::Developer,
        vec![MessageContent::ContextSummary { text: summary }],
        None,
        None,
        None,
    )?;
    Ok(Json(compacted))
}

async fn list_conversation_messages(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<Message>>, ApiError> {
    Ok(Json(state.runtime.store.list_messages(parse_id(&id)?)?))
}

#[derive(Debug, Deserialize)]
struct ConversationSelectionRequest {
    provider: Option<String>,
    model: Option<String>,
    model_pack: Option<String>,
    reasoning_level: Option<String>,
    mode: Option<ExecutionMode>,
    agent: Option<String>,
}

async fn update_conversation_selection(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    Json(request): Json<ConversationSelectionRequest>,
) -> Result<Json<opensrc_core::Conversation>, ApiError> {
    Ok(Json(state.runtime.store.update_conversation_selection(
        parse_id(&id)?,
        request.provider,
        request.model,
        request.model_pack,
        request.reasoning_level,
        request.mode,
        request.agent,
    )?))
}

#[derive(Debug, Deserialize)]
struct ChatRequest {
    conversation_id: Option<String>,
    project_root: String,
    message: String,
    provider: Option<String>,
    model: Option<String>,
    model_pack: Option<String>,
    reasoning_level: Option<String>,
    mode: Option<ExecutionMode>,
    #[serde(default)]
    auto: bool,
    agent: Option<String>,
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default)]
    allowed_tools: Vec<String>,
    #[serde(default)]
    attachments: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ChatResponse {
    conversation: opensrc_core::Conversation,
    run: Run,
    result: RunExecutionResult,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkspaceRoots {
    #[serde(default)]
    roots: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct WorkspaceRootQuery {
    project_root: PathBuf,
    path: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct WorkspaceRootRequest {
    project_root: PathBuf,
    path: PathBuf,
}

fn workspace_roots_path(project_root: &std::path::Path) -> PathBuf {
    project_root
        .join(".opensource")
        .join("workspace-roots.json")
}

fn read_workspace_roots(project_root: &std::path::Path) -> Result<Vec<String>, ApiError> {
    let project_root = std::fs::canonicalize(project_root)?;
    let path = workspace_roots_path(&project_root);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let value: WorkspaceRoots = serde_json::from_slice(&std::fs::read(path)?)
        .map_err(|error| ApiError::BadRequest(format!("invalid workspace roots file: {error}")))?;
    let mut roots = value
        .roots
        .into_iter()
        .filter_map(|value| std::fs::canonicalize(value).ok())
        .filter(|value| value != &project_root && value.is_dir())
        .map(|value| value.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    roots.sort();
    roots.dedup();
    Ok(roots)
}

fn write_workspace_roots(project_root: &std::path::Path, roots: &[String]) -> Result<(), ApiError> {
    let directory = project_root.join(".opensource");
    std::fs::create_dir_all(&directory)?;
    let bytes = serde_json::to_vec_pretty(&WorkspaceRoots {
        roots: roots.to_vec(),
    })
    .map_err(|error| ApiError::BadRequest(error.to_string()))?;
    std::fs::write(directory.join("workspace-roots.json"), bytes)?;
    Ok(())
}

fn attachment_mime(path: &std::path::Path) -> Option<String> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let mime = match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        _ => return None,
    };
    Some(mime.to_string())
}

fn resolve_attachments(
    project_root: &std::path::Path,
    request: &str,
    explicit: &[String],
) -> Result<Vec<MessageContent>, ApiError> {
    let selected = selected_file_paths(request, 50);
    if selected.is_empty() && explicit.is_empty() {
        return Ok(Vec::new());
    }
    let project_root = std::fs::canonicalize(project_root)?;
    let mut roots = vec![project_root.clone()];
    roots.extend(
        read_workspace_roots(&project_root)?
            .into_iter()
            .map(PathBuf::from),
    );
    let mut attachments = Vec::new();
    let mut seen = BTreeSet::new();
    for requested in explicit.iter().take(50) {
        let path = std::fs::canonicalize(requested).map_err(|error| {
            ApiError::BadRequest(format!("cannot attach `{requested}`: {error}"))
        })?;
        if !path.is_file() {
            return Err(ApiError::BadRequest(format!(
                "attachment `{}` is not a file",
                path.display()
            )));
        }
        if seen.insert(path.clone()) {
            attachments.push(MessageContent::FileReference {
                mime_type: attachment_mime(&path),
                path: path.to_string_lossy().into_owned(),
            });
        }
    }
    for requested in selected {
        let candidate = PathBuf::from(&requested);
        let candidate = if candidate.is_absolute() {
            candidate
        } else {
            project_root.join(&candidate)
        };
        let Ok(path) = std::fs::canonicalize(&candidate) else {
            continue;
        };
        if !roots.iter().any(|root| path.starts_with(root)) {
            return Err(ApiError::BadRequest(format!(
                "`{}` is outside the available directories; run /add-dir \"{}\" first",
                path.display(),
                path.parent().unwrap_or(&path).display()
            )));
        }
        if seen.insert(path.clone()) {
            attachments.push(MessageContent::FileReference {
                mime_type: attachment_mime(&path),
                path: path.to_string_lossy().into_owned(),
            });
        }
    }
    Ok(attachments)
}

async fn list_workspace_roots(
    Query(query): Query<WorkspaceRootQuery>,
) -> Result<Json<WorkspaceRoots>, ApiError> {
    let project_root = std::fs::canonicalize(query.project_root)?;
    Ok(Json(WorkspaceRoots {
        roots: read_workspace_roots(&project_root)?,
    }))
}

async fn add_workspace_root(
    Json(request): Json<WorkspaceRootRequest>,
) -> Result<Json<WorkspaceRoots>, ApiError> {
    let project_root = std::fs::canonicalize(request.project_root)?;
    let path = std::fs::canonicalize(request.path)?;
    if !path.is_dir() {
        return Err(ApiError::BadRequest(
            "workspace root must be an existing directory".to_string(),
        ));
    }
    let mut roots = read_workspace_roots(&project_root)?;
    if path != project_root {
        roots.push(path.to_string_lossy().into_owned());
        roots.sort();
        roots.dedup();
        write_workspace_roots(&project_root, &roots)?;
    }
    Ok(Json(WorkspaceRoots { roots }))
}

async fn remove_workspace_root(
    Query(query): Query<WorkspaceRootQuery>,
) -> Result<Json<WorkspaceRoots>, ApiError> {
    let project_root = std::fs::canonicalize(query.project_root)?;
    let path = query
        .path
        .ok_or_else(|| ApiError::BadRequest("path query parameter is required".to_string()))?;
    let path = std::fs::canonicalize(path)?;
    let mut roots = read_workspace_roots(&project_root)?;
    roots.retain(|root| std::path::Path::new(root) != path);
    write_workspace_roots(&project_root, &roots)?;
    Ok(Json(WorkspaceRoots { roots }))
}

#[allow(clippy::too_many_lines)]
async fn chat(
    State(state): State<ServerState>,
    Json(request): Json<ChatRequest>,
) -> Result<Json<ChatResponse>, ApiError> {
    if request.message.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "chat message cannot be empty".to_string(),
        ));
    }
    let conversation = if let Some(id) = request.conversation_id.as_deref() {
        state.runtime.store.get_conversation(parse_id(id)?)?
    } else {
        state.runtime.store.create_conversation(
            request.project_root.clone(),
            Some(request.message.chars().take(80).collect()),
        )?
    };
    if conversation.project_root != request.project_root {
        return Err(ApiError::BadRequest(
            "conversation belongs to a different project".to_string(),
        ));
    }
    let inherited_request = if is_continuation_request(&request.message) {
        state
            .runtime
            .store
            .list_messages(conversation.id)?
            .into_iter()
            .rev()
            .filter(|message| message.role == MessageRole::User)
            .find_map(|message| {
                let text = message
                    .content
                    .into_iter()
                    .filter_map(|content| match content {
                        MessageContent::Text { text } => Some(text),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                (!text.trim().is_empty() && !is_continuation_request(&text)).then_some(text)
            })
    } else {
        None
    };
    let routing_request = inherited_request.as_deref().unwrap_or(&request.message);

    let provider = request
        .provider
        .or_else(|| conversation.provider.clone())
        .or_else(|| state.runtime.providers.provider_ids().into_iter().next())
        .ok_or_else(|| ApiError::BadRequest("no provider is connected".to_string()))?;
    let model = request
        .model
        .or_else(|| conversation.model.clone())
        .or_else(|| state.runtime.providers.default_model(&provider))
        .ok_or_else(|| ApiError::BadRequest("no model is selected".to_string()))?;
    let automatic = ModeClassifier::classify(routing_request);
    let (mode, mode_reasons, preferred_mode) = if request.auto {
        (automatic.mode, automatic.reasons, None)
    } else if let Some(mode) = request.mode.or(conversation.preferred_mode) {
        (mode, vec!["selected by the user"], Some(mode))
    } else {
        (automatic.mode, automatic.reasons, None)
    };
    let selected_agent = request
        .agent
        .clone()
        .or_else(|| explicit_agent_name(&request.message))
        .unwrap_or_else(|| automatic_agent_name(routing_request, mode).to_string());
    let reasoning_level = request
        .reasoning_level
        .as_deref()
        .map(str::trim)
        .filter(|level| !level.is_empty())
        .map(str::to_ascii_lowercase);
    let model_pack = request
        .model_pack
        .or_else(|| conversation.model_pack.clone())
        .filter(|value| !value.trim().is_empty());
    if let Some(id) = model_pack.as_deref() {
        let pack = state
            .runtime
            .model_packs
            .get(id, &state.runtime.providers)?;
        let missing = pack
            .members
            .iter()
            .filter(|member| {
                !state
                    .runtime
                    .providers
                    .provider_ids()
                    .contains(&member.provider)
            })
            .map(|member| member.provider.clone())
            .collect::<BTreeSet<_>>();
        if !missing.is_empty() {
            return Err(ApiError::BadRequest(format!(
                "model pack `{id}` needs connected provider(s): {}",
                missing.into_iter().collect::<Vec<_>>().join(", ")
            )));
        }
        let missing_models = pack
            .members
            .iter()
            .filter(|member| {
                let known = state.runtime.providers.known_models(&member.provider);
                !known.is_empty() && !known.contains(&member.model)
            })
            .map(|member| format!("{}/{}", member.provider, member.model))
            .collect::<BTreeSet<_>>();
        if !missing_models.is_empty() {
            return Err(ApiError::BadRequest(format!(
                "model pack `{id}` references unavailable model(s): {}",
                missing_models.into_iter().collect::<Vec<_>>().join(", ")
            )));
        }
    }
    let selected = state.runtime.store.update_conversation_selection(
        conversation.id,
        Some(provider.clone()),
        Some(model.clone()),
        model_pack.clone(),
        reasoning_level.clone(),
        preferred_mode,
        Some(selected_agent.clone()),
    )?;
    let mut skill_names = request.skills;
    skill_names.extend(explicit_skill_names(&request.message));
    skill_names.extend(state.runtime.skills.matching_triggers(&request.message));
    skill_names.sort();
    skill_names.dedup();
    let mut activated_skills = Vec::new();
    for name in skill_names {
        let skill = state.runtime.skills.activate(&name)?;
        activated_skills.push(skill.metadata.name.clone());
        state.runtime.store.append_message(
            selected.id,
            None,
            MessageRole::Developer,
            vec![MessageContent::text(format!(
                "Activated skill `{}` from `{}`.\nAvailable resources: {}\n\n{}",
                skill.metadata.name,
                skill.source_path,
                if skill.resources.is_empty() {
                    "none".to_string()
                } else {
                    skill.resources.join(", ")
                },
                skill.instructions
            ))],
            None,
            None,
            None,
        )?;
    }
    let attachments = resolve_attachments(
        std::path::Path::new(&request.project_root),
        &request.message,
        &request.attachments,
    )?;
    let requires_vision = attachments.iter().any(|attachment| {
        matches!(
            attachment,
            MessageContent::FileReference {
                mime_type: Some(mime_type),
                ..
            } if mime_type.starts_with("image/")
        )
    });
    state.runtime.providers.resolve_model(
        &provider,
        &model,
        &RequiredCapabilities {
            multimodal: requires_vision,
            ..RequiredCapabilities::default()
        },
    )?;
    let attachment_paths = attachments
        .iter()
        .filter_map(|content| match content {
            MessageContent::FileReference { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut user_content = vec![MessageContent::text(request.message.clone())];
    user_content.extend(attachments);
    let run = state.runtime.store.create_run_with_content(
        selected.id,
        request.message.clone(),
        mode,
        user_content,
    )?;
    state.runtime.store.append_event(
        run.id,
        None,
        None,
        "mode.selected",
        &json!({"mode": mode, "reasons": mode_reasons}),
        Some(&format!("mode:{}", run.id)),
    )?;
    for skill in activated_skills {
        state.runtime.store.append_event(
            run.id,
            None,
            None,
            "skill.activated",
            &json!({"name": skill}),
            Some(&format!("skill:{}:{skill}", run.id)),
        )?;
    }
    if let Some(pack) = model_pack.as_deref() {
        state.runtime.store.append_event(
            run.id,
            None,
            None,
            "model_pack.selected",
            &json!({"id": pack}),
            Some(&format!("model-pack:{}", run.id)),
        )?;
    }
    state.runtime.store.append_event(
        run.id,
        None,
        None,
        "routing.selection_locked",
        &if let Some(pack) = model_pack.as_deref() {
            json!({
                "strategy": "model_pack",
                "pack": pack
            })
        } else {
            json!({
                "strategy": "single_model",
                "provider": provider,
                "model": model
            })
        },
        Some(&format!("routing-selection:{}", run.id)),
    )?;
    if mode != ExecutionMode::Direct {
        let mut definition = resolve_agent_definition(&request.project_root, &selected_agent)?;
        let role_policy = state.runtime.routing_policies.role(&selected_agent);
        if let Some(policy) = role_policy.as_ref() {
            apply_role_policy(&mut definition, policy, None, &[]);
        }
        if request_needs_mutation(routing_request) {
            ensure_mutation_capabilities(&mut definition);
        }
        if role_policy.as_ref().is_some_and(|policy| {
            policy.execution == opensrc_runtime::RoleExecutionKind::Deterministic
        }) {
            definition.preferred_provider = Some("runtime".to_string());
            definition.preferred_model = Some("deterministic".to_string());
            definition.fallback_chain.clear();
        } else if model_pack.is_none() {
            definition.preferred_provider = Some(provider.clone());
            definition.preferred_model = Some(model.clone());
            definition.fallback_chain.clear();
        }
        if let Some(pack_id) = model_pack.as_deref() {
            let pack = state
                .runtime
                .model_packs
                .get(pack_id, &state.runtime.providers)?;
            let routing_stage = match mode {
                ExecutionMode::Agentic => ModelPackStage::Plan,
                ExecutionMode::Focused => ModelPackStage::Execute,
                ExecutionMode::Direct => ModelPackStage::Synthesize,
            };
            if let Some(member) = pack.select(routing_stage, &selected_agent) {
                definition.preferred_provider = Some(member.provider.clone());
                definition.preferred_model = Some(member.model.clone());
                if member.reasoning_level.is_some() {
                    definition
                        .reasoning
                        .level
                        .clone_from(&member.reasoning_level);
                }
                definition.fallback_chain = pack.fallback_chain(&member);
            }
        }
        if reasoning_level.is_some() {
            definition.reasoning.level.clone_from(&reasoning_level);
        }
        let workspace_roots = read_workspace_roots(std::path::Path::new(&request.project_root))?;
        let project_root = std::path::Path::new(&request.project_root)
            .canonicalize()
            .unwrap_or_else(|_| std::path::PathBuf::from(&request.project_root))
            .to_string_lossy()
            .into_owned();
        definition
            .sandbox_policy
            .read_paths
            .push(project_root.clone());
        definition.sandbox_policy.write_paths.push(project_root);
        definition
            .sandbox_policy
            .read_paths
            .extend(workspace_roots.iter().cloned());
        definition
            .sandbox_policy
            .write_paths
            .extend(workspace_roots);
        definition
            .sandbox_policy
            .read_paths
            .extend(attachment_paths.iter().cloned());
        definition
            .sandbox_policy
            .write_paths
            .extend(attachment_paths);
        definition.system_instructions.push_str(
            "\n\nFilesystem access: Tools accept absolute local paths, including paths on \
             other drives. When the user naturally asks to inspect, manage, edit, or analyze \
             a directory outside the current project, call the appropriate filesystem tool \
             with that absolute path immediately. Do not ask for a special command or ask in \
             prose first: the tool call itself opens the access approval prompt, and execution \
             resumes automatically if the user allows it. Attached files and the accompanying \
             user text are one request: inspect the attachments and carry out the stated task. \
             Do not ask what to do with an attachment when the user's text already says what to do.",
        );
        if !request.allowed_tools.is_empty() {
            let visible = state
                .runtime
                .tools
                .registry()
                .visible_for(&definition.tool_policy);
            let requested = request
                .allowed_tools
                .iter()
                .filter(|pattern| {
                    visible.iter().any(|tool| {
                        tool.name == **pattern
                            || pattern.strip_suffix(".*").is_some_and(|prefix| {
                                tool.name
                                    .strip_prefix(prefix)
                                    .is_some_and(|suffix| suffix.starts_with('.'))
                            })
                    })
                })
                .cloned()
                .collect::<Vec<_>>();
            if requested.is_empty() {
                return Err(ApiError::BadRequest(
                    "custom command allowed_tools did not match this agent's tool policy"
                        .to_string(),
                ));
            }
            definition.tool_policy.allow = requested;
        }
        state.runtime.agents.create_root(
            run.id,
            &definition,
            request.message,
            request.project_root,
        )?;
    }
    let result = state
        .runtime
        .execution
        .execute_run_with_policy(run.id, &provider, &model, model_pack.as_deref(), false)
        .await?;
    let run = state.runtime.store.get_run(run.id)?;
    Ok(Json(ChatResponse {
        conversation: state.runtime.store.get_conversation(selected.id)?,
        run,
        result,
    }))
}

fn explicit_skill_names(message: &str) -> Vec<String> {
    message
        .split_whitespace()
        .filter_map(|word| {
            word.strip_prefix('$').and_then(|name| {
                let normalized = name.trim_matches(|character: char| {
                    !character.is_ascii_alphanumeric() && character != '-' && character != '_'
                });
                (!normalized.is_empty()).then(|| normalized.to_string())
            })
        })
        .collect()
}

fn explicit_agent_name(message: &str) -> Option<String> {
    message.split_whitespace().find_map(|word| {
        word.strip_prefix("@agent:")
            .map(|name| {
                name.trim_matches(|character: char| {
                    !character.is_ascii_alphanumeric() && !matches!(character, '-' | '_')
                })
            })
            .filter(|name| !name.is_empty())
            .map(str::to_string)
    })
}

#[allow(clippy::too_many_lines)]
fn automatic_agent_name(message: &str, mode: ExecutionMode) -> &'static str {
    if mode == ExecutionMode::Direct {
        return "generalist";
    }
    let message = message.to_ascii_lowercase();
    let contains_any = |needles: &[&str]| needles.iter().any(|needle| message.contains(needle));
    let mutation = request_needs_mutation(&message);
    if mutation
        && (contains_any(&[
            "html",
            "css",
            "javascript",
            "web page",
            "webpage",
            "website",
            "frontend",
            "interface",
            "calculator",
        ]) || message.split_whitespace().any(|word| {
            word.trim_matches(|character: char| !character.is_ascii_alphanumeric()) == "ui"
        }))
    {
        "frontend-specialist"
    } else if contains_any(&[
        "wait for tests",
        "wait for the tests",
        "wait for process",
        "wait for agents",
        "keep waiting",
        "poll until",
    ]) {
        "awaiter"
    } else if contains_any(&["security", "vulnerability", "secret", "sandbox", "threat"]) {
        "security-reviewer"
    } else if contains_any(&[
        "accessibility",
        "screen reader",
        "keyboard navigation",
        "focus order",
        "contrast",
        "a11y",
    ]) {
        "accessibility-specialist"
    } else if contains_any(&[
        "performance",
        "benchmark",
        "latency",
        "throughput",
        "memory leak",
        "optimize",
        "profil",
    ]) {
        "performance-specialist"
    } else if contains_any(&[
        "dependency",
        "dependencies",
        "package update",
        "lockfile",
        "cargo.toml",
        "supply chain",
    ]) {
        "dependency-specialist"
    } else if contains_any(&[
        "release",
        "ship",
        "packaging",
        "installer",
        "distribution",
        "release build",
    ]) {
        "release-specialist"
    } else if contains_any(&[
        "test",
        "failing",
        "failure",
        "bug",
        "debug",
        "crash",
        "error",
        "regression",
    ]) {
        "test-debugging-specialist"
    } else if !mutation
        && contains_any(&[
            "video",
            "audio",
            "recording",
            "screenshot",
            "image",
            "codec",
            "transcript",
        ])
    {
        "media-specialist"
    } else if contains_any(&[
        "browser",
        "web page",
        "webpage",
        "playwright",
        "chromium",
        "responsive",
    ]) {
        "browser-validation-specialist"
    } else if contains_any(&[
        "tui",
        "terminal ui",
        "frontend",
        "interface",
        "layout",
        "cursor",
        "color",
        "popup",
        "ratatui",
    ]) {
        "frontend-specialist"
    } else if contains_any(&[
        "database",
        "sqlite",
        "schema migration",
        "database migration",
        "query",
        "index",
    ]) {
        "database-specialist"
    } else if contains_any(&[
        "provider",
        "protocol",
        "integration",
        "streaming",
        "mcp",
        "api",
    ]) {
        "integration-specialist"
    } else if contains_any(&["backend", "server", "service"]) {
        "backend-specialist"
    } else if contains_any(&["refactor", "restructure", "extract", "rename across"]) {
        "refactoring-specialist"
    } else if contains_any(&[
        "map repository",
        "repository map",
        "codebase structure",
        "project structure",
        "entry points",
    ]) {
        "repository-mapper"
    } else if contains_any(&["architecture", "design", "migration", "boundaries", "plan"]) {
        "architect"
    } else if contains_any(&["review", "audit", "inspect changes"]) {
        "code-reviewer"
    } else if contains_any(&["documentation", "readme", "docs", "guide"]) {
        "documentation-specialist"
    } else if contains_any(&["investigate", "trace", "find why", "explain code"]) {
        "investigator"
    } else if mutation {
        "implementer"
    } else {
        "generalist"
    }
}

fn request_needs_mutation(message: &str) -> bool {
    let mut message = message.to_ascii_lowercase();
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
        message = message.replace(negated, "");
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
    .any(|marker| message.contains(marker))
}

fn is_continuation_request(message: &str) -> bool {
    let message = message.trim().to_ascii_lowercase();
    message.len() <= 240
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
        .any(|marker| message.contains(marker))
}

fn ensure_mutation_capabilities(definition: &mut AgentDefinition) {
    for capability in [
        "fs.*",
        "search.*",
        "patch.apply",
        "shell.run",
        "shell.test",
        "process.*",
        "git.diff",
        "git.status",
        "git.log",
        "git.show",
        "git.branch",
        "skill.activate",
        "mcp.*",
    ] {
        if !definition
            .tool_policy
            .allow
            .iter()
            .any(|current| current == capability)
        {
            definition.tool_policy.allow.push(capability.to_string());
        }
    }
    definition.workspace_mode = opensrc_core::WorkspaceMode::OwnedPaths;
}

#[derive(Debug, Deserialize)]
struct CreateRunRequest {
    conversation_id: String,
    request: String,
    mode: Option<ExecutionMode>,
}

async fn create_run(
    State(state): State<ServerState>,
    Json(request): Json<CreateRunRequest>,
) -> Result<Json<Run>, ApiError> {
    let conversation_id = parse_id(&request.conversation_id)?;
    let mode = request
        .mode
        .unwrap_or_else(|| ModeClassifier::classify(&request.request).mode);
    Ok(Json(state.runtime.store.create_run(
        conversation_id,
        request.request,
        mode,
    )?))
}

async fn get_run(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> Result<Json<Run>, ApiError> {
    Ok(Json(state.runtime.store.get_run(parse_id(&id)?)?))
}

#[derive(Debug, Deserialize)]
struct ExecuteRunRequest {
    provider: String,
    model: String,
}

async fn execute_run(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    Json(request): Json<ExecuteRunRequest>,
) -> Result<Json<opensrc_core::RunExecutionResult>, ApiError> {
    Ok(Json(
        state
            .runtime
            .execution
            .execute_run(parse_id(&id)?, &request.provider, &request.model)
            .await?,
    ))
}

async fn cancel_run(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> Result<Json<Run>, ApiError> {
    Ok(Json(state.runtime.execution.cancel_run(parse_id(&id)?)?))
}

async fn list_providers(State(state): State<ServerState>) -> Json<Value> {
    Json(json!({"providers": state.runtime.providers.descriptors()}))
}

#[derive(Debug, Deserialize)]
struct ModelQuery {
    provider: Option<String>,
    #[serde(default)]
    refresh: bool,
}

fn model_descriptor(
    state: &ServerState,
    provider: &str,
    model: &str,
    source: &'static str,
) -> Value {
    let chat = model_is_chat_capable(model);
    let capabilities = state
        .runtime
        .providers
        .capabilities_for_model(provider, model)
        .unwrap_or_default();
    json!({
        "provider": provider,
        "id": model,
        "source": source,
        "capabilities": {
            "chat": chat,
            "tools": capabilities.supports_tool_calls,
            "multimodal": capabilities.supports_multimodal_input
        }
    })
}

async fn list_models(
    State(state): State<ServerState>,
    Query(query): Query<ModelQuery>,
) -> Result<Json<Value>, ApiError> {
    let descriptors = state.runtime.providers.descriptors();
    let selected = descriptors.into_iter().filter(|descriptor| {
        query
            .provider
            .as_deref()
            .is_none_or(|provider| provider == descriptor.id)
    });
    let mut models = Vec::new();
    let mut discovery_errors = Vec::new();
    for descriptor in selected {
        let discovered = if query.refresh {
            if let Ok(models) = state.runtime.providers.list_models(&descriptor.id).await {
                if !models.is_empty() {
                    persist_discovered_models(&state, &descriptor.id, &models)?;
                }
                models
            } else {
                // Model discovery is optional. A provider can still serve the configured
                // default model when its catalog endpoint is unavailable or rate limited.
                discovery_errors.push(json!({
                    "provider": descriptor.id,
                    "message": "Could not refresh this provider's model catalog; saved and default models remain available."
                }));
                state.runtime.providers.known_models(&descriptor.id)
            }
        } else {
            state.runtime.providers.known_models(&descriptor.id)
        };
        if discovered.is_empty() {
            if let Some(model) = descriptor.default_model {
                models.push(model_descriptor(&state, &descriptor.id, &model, "default"));
            }
        } else {
            models.extend(
                discovered
                    .into_iter()
                    .map(|model| model_descriptor(&state, &descriptor.id, &model, "discovered")),
            );
        }
    }
    Ok(Json(json!({
        "models": models,
        "discovery_errors": discovery_errors
    })))
}

#[derive(Debug, Deserialize)]
struct ModelPackQuery {
    #[serde(default)]
    refresh: bool,
}

async fn list_model_packs(
    State(state): State<ServerState>,
    Query(query): Query<ModelPackQuery>,
) -> Result<Json<Value>, ApiError> {
    let mut discovery_errors = Vec::new();
    if query.refresh {
        for provider in state.runtime.providers.provider_ids() {
            match state.runtime.providers.list_models(&provider).await {
                Ok(models) => {
                    persist_discovered_models(&state, &provider, &models)?;
                }
                Err(error) => discovery_errors.push(format!("{provider}: {error}")),
            }
        }
    }
    Ok(Json(json!({
        "packs": state.runtime.model_packs.list(&state.runtime.providers),
        "discovery_errors": discovery_errors
    })))
}

async fn list_routing_policies(State(state): State<ServerState>) -> Result<Json<Value>, ApiError> {
    let snapshot = state.runtime.routing_policies.snapshot();
    let roles: Vec<RolePolicyDescriptor> = state
        .runtime
        .routing_policies
        .descriptors(&state.runtime.providers);
    Ok(Json(json!({
        "version": snapshot.version,
        "limits": snapshot.limits,
        "models": snapshot.models,
        "roles": roles
    })))
}

async fn replace_routing_policies(
    State(state): State<ServerState>,
    Json(policy): Json<RoutingPolicySet>,
) -> Result<Json<Value>, ApiError> {
    state.runtime.routing_policies.replace(policy)?;
    Ok(Json(json!({
        "status": "updated",
        "policy": state.runtime.routing_policies.snapshot()
    })))
}

async fn upsert_role_policy(
    State(state): State<ServerState>,
    Path(role): Path<String>,
    Json(mut policy): Json<RolePolicy>,
) -> Result<Json<RolePolicy>, ApiError> {
    if !policy.role.is_empty() && policy.role != role {
        return Err(ApiError::BadRequest(format!(
            "role path `{role}` does not match payload role `{}`",
            policy.role
        )));
    }
    policy.role = role;
    Ok(Json(state.runtime.routing_policies.upsert_role(policy)?))
}

async fn upsert_model_pack(
    State(state): State<ServerState>,
    Json(pack): Json<ModelPack>,
) -> Result<Json<ModelPack>, ApiError> {
    Ok(Json(state.runtime.model_packs.upsert(pack)?))
}

async fn remove_model_pack(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    if state.runtime.model_packs.remove(&id)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::BadRequest(format!(
            "custom model pack `{id}` is not configured"
        )))
    }
}

async fn disconnect_provider(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let removed_runtime = state.runtime.providers.unregister(&id);
    let mut removed_config = false;
    if let Some(path) = state.provider_config_path.as_ref()
        && path.is_file()
    {
        let mut document = read_provider_file(path)?;
        let before = document.providers.len();
        document.providers.retain(|provider| provider.id != id);
        removed_config = document.providers.len() != before;
        if removed_config {
            write_provider_file(path, &document)?;
        }
    }
    if !removed_runtime && !removed_config {
        return Err(ApiError::BadRequest(format!(
            "provider `{id}` is not configured"
        )));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct ApprovalQuery {
    pending: Option<bool>,
}

async fn list_approvals(
    State(state): State<ServerState>,
    Query(query): Query<ApprovalQuery>,
) -> Result<Json<Vec<Approval>>, ApiError> {
    Ok(Json(
        state
            .runtime
            .store
            .list_approvals(query.pending.unwrap_or(false))?,
    ))
}

#[derive(Debug, Deserialize)]
struct ApprovalDecisionRequest {
    decision: ApprovalDecision,
    edited_arguments: Option<Value>,
    reason: Option<String>,
}

async fn decide_approval(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    Json(request): Json<ApprovalDecisionRequest>,
) -> Result<Json<Approval>, ApiError> {
    Ok(Json(state.runtime.store.decide_approval(
        parse_id(&id)?,
        request.decision,
        request.edited_arguments,
        request.reason,
    )?))
}

async fn list_permissions(
    State(state): State<ServerState>,
) -> Result<Json<Vec<PermissionRule>>, ApiError> {
    Ok(Json(state.runtime.store.list_permission_rules()?))
}

async fn list_commands() -> Json<Vec<CommandDescriptor>> {
    Json(builtin_commands())
}

#[derive(Debug, Deserialize)]
struct CustomCommandQuery {
    project_root: PathBuf,
}

async fn list_custom_commands(
    Query(query): Query<CustomCommandQuery>,
) -> Result<Json<Value>, ApiError> {
    let project_root = std::fs::canonicalize(&query.project_root).map_err(|error| {
        ApiError::BadRequest(format!(
            "project root `{}` is not accessible: {error}",
            query.project_root.display()
        ))
    })?;
    let mut roots = Vec::new();
    if let Some(root) = user_command_root() {
        roots.push(root);
    }
    roots.push(project_root.join(".opensource").join("commands"));
    let builtins = builtin_commands()
        .into_iter()
        .flat_map(|command| std::iter::once(command.name).chain(command.aliases.iter().copied()))
        .collect::<std::collections::BTreeSet<_>>();
    let commands = discover_custom_commands(&roots)
        .into_iter()
        .filter(|command| !builtins.contains(command.name.as_str()))
        .collect::<Vec<_>>();
    Ok(Json(json!({"commands": commands})))
}

fn user_command_root() -> Option<PathBuf> {
    std::env::var_os("OPENSOURCE_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("APPDATA")
                .or_else(|| std::env::var_os("XDG_CONFIG_HOME"))
                .map(PathBuf::from)
        })
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(|value| PathBuf::from(value).join(".config"))
        })
        .map(|root| root.join("opensource").join("commands"))
}

async fn delete_permission(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.runtime.store.delete_permission_rule(parse_id(&id)?)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct ChangeQuery {
    run_id: Option<String>,
}

async fn list_changes(
    State(state): State<ServerState>,
    Query(query): Query<ChangeQuery>,
) -> Result<Json<Vec<FileChange>>, ApiError> {
    Ok(Json(state.runtime.store.list_file_changes(
        query.run_id.as_deref().map(parse_id).transpose()?,
    )?))
}

async fn undo_change(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> Result<Json<FileChange>, ApiError> {
    Ok(Json(state.runtime.changes.undo(parse_id(&id)?)?))
}

async fn redo_change(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> Result<Json<FileChange>, ApiError> {
    Ok(Json(state.runtime.changes.redo(parse_id(&id)?)?))
}

#[derive(Debug, Deserialize)]
struct CheckpointQuery {
    run_id: Option<String>,
}

async fn list_checkpoints(
    State(state): State<ServerState>,
    Query(query): Query<CheckpointQuery>,
) -> Result<Json<Vec<opensrc_core::Checkpoint>>, ApiError> {
    Ok(Json(state.runtime.store.list_checkpoints(
        query.run_id.as_deref().map(parse_id).transpose()?,
    )?))
}

#[derive(Debug, Deserialize)]
struct CreateCheckpointRequest {
    run_id: String,
    agent_id: Option<String>,
    task_id: Option<String>,
    label: Option<String>,
}

async fn create_checkpoint(
    State(state): State<ServerState>,
    Json(request): Json<CreateCheckpointRequest>,
) -> Result<Json<opensrc_core::Checkpoint>, ApiError> {
    Ok(Json(
        state.runtime.store.create_checkpoint(
            parse_id(&request.run_id)?,
            request.agent_id.as_deref().map(parse_id).transpose()?,
            request.task_id.as_deref().map(parse_id).transpose()?,
            request
                .label
                .unwrap_or_else(|| "Manual checkpoint".to_string()),
        )?,
    ))
}

async fn restore_checkpoint(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> Result<Json<opensrc_runtime::CheckpointRestore>, ApiError> {
    Ok(Json(
        state.runtime.changes.restore_checkpoint(parse_id(&id)?)?,
    ))
}

#[derive(Deserialize)]
struct ConnectProviderRequest {
    id: String,
    protocol: ProviderProtocol,
    family: Option<OpenAiCompatibleFamily>,
    base_url: String,
    api_key: Option<String>,
    api_key_env: Option<String>,
    default_model: String,
    #[serde(default)]
    test_connection: bool,
}

impl std::fmt::Debug for ConnectProviderRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConnectProviderRequest")
            .field("id", &self.id)
            .field("protocol", &self.protocol)
            .field("family", &self.family)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field("api_key_env", &self.api_key_env)
            .field("default_model", &self.default_model)
            .field("test_connection", &self.test_connection)
            .finish()
    }
}

async fn connect_provider(
    State(state): State<ServerState>,
    Json(request): Json<ConnectProviderRequest>,
) -> Result<Json<Value>, ApiError> {
    let is_local_compatible = is_anonymous_local_compatible(&request.protocol, &request.base_url);
    if request.id.trim().is_empty() || request.base_url.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "provider and base URL are required".to_string(),
        ));
    }
    let api_key =
        if is_local_compatible && request.api_key.is_none() && request.api_key_env.is_none() {
            String::new()
        } else if let Some(value) = request.api_key.as_deref() {
            value.to_string()
        } else if let Some(variable) = request.api_key_env.as_deref() {
            std::env::var(variable).map_err(|_| {
                ApiError::BadRequest(format!(
                    "environment variable `{variable}` is not available to the server"
                ))
            })?
        } else {
            return Err(ApiError::BadRequest(
                "an API key or environment-variable reference is required".to_string(),
            ));
        };
    let mut capabilities = standard_provider_capabilities(&request.protocol);
    if request.family == Some(OpenAiCompatibleFamily::OpenRouter) {
        capabilities.supports_structured_output = false;
    }
    let adapter: Arc<dyn opensrc_core::ProviderAdapter> = match &request.protocol {
        ProviderProtocol::OpenaiCompatible => {
            Arc::new(OpenAiCompatibleAdapter::new(OpenAiCompatibleConfig::new(
                request.id.clone(),
                request.family.unwrap_or(OpenAiCompatibleFamily::Custom),
                request.base_url.clone(),
                api_key.clone(),
                capabilities.clone(),
            )))
        }
        ProviderProtocol::Gemini => Arc::new(GeminiAdapter::new(GeminiConfig::new(
            request.id.clone(),
            request.base_url.clone(),
            api_key.clone(),
            capabilities.clone(),
        ))),
    };
    if request.test_connection && !request.default_model.trim().is_empty() {
        adapter
            .execute(CanonicalModelRequest {
                model: request.default_model.clone(),
                system: "Connection test. Reply with OK.".to_string(),
                messages: vec![CanonicalMessage::text(MessageRole::User, "OK")],
                tools: Vec::new(),
                structured_output_schema: None,
                reasoning_level: None,
                temperature: Some(0.0),
                max_output_tokens: Some(8),
                cache_hints: BTreeMap::default(),
            })
            .await?;
    }
    if request.default_model.trim().is_empty() {
        state.runtime.providers.register(adapter);
    } else {
        state
            .runtime
            .providers
            .register_with_model(adapter, request.default_model.clone());
    }
    let (persisted, credential_kind) =
        persist_connected_provider(&state, &request, &api_key, &capabilities)?;
    Ok(Json(json!({
        "provider": request.id,
        "model": request.default_model,
        "connected": true,
        "persisted": persisted,
        "credential": credential_kind
    })))
}

fn persist_connected_provider(
    state: &ServerState,
    request: &ConnectProviderRequest,
    api_key: &str,
    capabilities: &ProviderCapabilities,
) -> Result<(bool, &'static str), ApiError> {
    let is_local_compatible = is_anonymous_local_compatible(&request.protocol, &request.base_url);
    if is_local_compatible && request.api_key.is_none() && request.api_key_env.is_none() {
        let persisted = persist_provider(
            state,
            ProviderEntry {
                id: request.id.clone(),
                protocol: request.protocol.clone(),
                base_url: request.base_url.clone(),
                api_key_env: String::new(),
                default_model: (!request.default_model.trim().is_empty())
                    .then(|| request.default_model.clone()),
                models: if request.default_model.trim().is_empty() {
                    Vec::new()
                } else {
                    vec![request.default_model.clone()]
                },
                family: request.family,
                capabilities: Some(capabilities.clone()),
            },
        )?;
        return Ok((persisted, if persisted { "local" } else { "memory_only" }));
    }
    if let Some(variable) = request.api_key_env.as_ref() {
        let persisted = persist_provider(
            state,
            ProviderEntry {
                id: request.id.clone(),
                protocol: request.protocol.clone(),
                base_url: request.base_url.clone(),
                api_key_env: variable.clone(),
                default_model: (!request.default_model.trim().is_empty())
                    .then(|| request.default_model.clone()),
                models: if request.default_model.trim().is_empty() {
                    Vec::new()
                } else {
                    vec![request.default_model.clone()]
                },
                family: request.family,
                capabilities: Some(capabilities.clone()),
            },
        )?;
        return Ok((
            persisted,
            if persisted {
                "environment"
            } else {
                "memory_only"
            },
        ));
    }
    if request.api_key.is_some() {
        let reference = store_provider_credential(&request.id, api_key)?;
        let persisted = persist_provider(
            state,
            ProviderEntry {
                id: request.id.clone(),
                protocol: request.protocol.clone(),
                base_url: request.base_url.clone(),
                api_key_env: reference,
                default_model: (!request.default_model.trim().is_empty())
                    .then(|| request.default_model.clone()),
                models: if request.default_model.trim().is_empty() {
                    Vec::new()
                } else {
                    vec![request.default_model.clone()]
                },
                family: request.family,
                capabilities: Some(capabilities.clone()),
            },
        )?;
        return Ok((persisted, if persisted { "keyring" } else { "memory_only" }));
    }
    Ok((false, "memory_only"))
}

fn standard_provider_capabilities(protocol: &ProviderProtocol) -> ProviderCapabilities {
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

fn persist_provider(state: &ServerState, entry: ProviderEntry) -> Result<bool, ApiError> {
    let Some(path) = state.provider_config_path.as_ref() else {
        return Ok(false);
    };
    let mut document = if path.is_file() {
        read_provider_file(path)?
    } else {
        ProviderFile {
            providers: Vec::new(),
        }
    };
    if let Some(existing) = document
        .providers
        .iter_mut()
        .find(|provider| provider.id == entry.id)
    {
        *existing = entry;
    } else {
        document.providers.push(entry);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_provider_file(path, &document)?;
    Ok(true)
}

fn persist_discovered_models(
    state: &ServerState,
    provider: &str,
    models: &[String],
) -> Result<(), ApiError> {
    let Some(path) = state.provider_config_path.as_ref() else {
        return Ok(());
    };
    if !path.is_file() {
        return Ok(());
    }
    let mut document = read_provider_file(path)?;
    let Some(entry) = document
        .providers
        .iter_mut()
        .find(|entry| entry.id == provider)
    else {
        return Ok(());
    };
    let mut models = models.to_vec();
    if let Some(default) = entry.default_model.clone() {
        models.push(default);
    }
    models.sort();
    models.dedup();
    if entry.models != models {
        entry.models = models;
        write_provider_file(path, &document)?;
    }
    Ok(())
}

fn write_provider_file(path: &std::path::Path, document: &ProviderFile) -> Result<(), ApiError> {
    let bytes = serde_json::to_vec_pretty(document).map_err(|error| {
        ApiError::BadRequest(format!(
            "failed to serialize provider configuration: {error}"
        ))
    })?;
    let temporary = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    std::fs::write(&temporary, bytes)?;
    if path.exists() {
        #[cfg(windows)]
        {
            std::fs::remove_file(path)?;
        }
    }
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(ApiError::Io(error));
    }
    Ok(())
}

async fn list_tools(State(state): State<ServerState>) -> Json<Value> {
    Json(json!({"tools": state.runtime.tools.registry().metadata()}))
}

async fn list_skills(State(state): State<ServerState>) -> Json<Value> {
    Json(json!({"skills": state.runtime.skills.metadata()}))
}

async fn activate_skill(
    State(state): State<ServerState>,
    Path(name): Path<String>,
) -> Result<Json<opensrc_runtime::ActivatedSkill>, ApiError> {
    Ok(Json(state.runtime.skills.activate(&name)?))
}

async fn list_mcp_servers(State(state): State<ServerState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({"servers": state.runtime.mcp.list()?})))
}

async fn upsert_mcp_server(
    State(state): State<ServerState>,
    Json(server): Json<McpServer>,
) -> Result<Json<McpServer>, ApiError> {
    Ok(Json(state.runtime.mcp.upsert(server)?))
}

async fn remove_mcp_server(
    State(state): State<ServerState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.runtime.mcp.remove(&name)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn enable_mcp_server(
    State(state): State<ServerState>,
    Path(name): Path<String>,
) -> Result<Json<McpServer>, ApiError> {
    Ok(Json(state.runtime.mcp.set_enabled(&name, true)?))
}

async fn disable_mcp_server(
    State(state): State<ServerState>,
    Path(name): Path<String>,
) -> Result<Json<McpServer>, ApiError> {
    Ok(Json(state.runtime.mcp.set_enabled(&name, false)?))
}

async fn debug_mcp_server(
    State(state): State<ServerState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({
        "server": name,
        "discovery": state.runtime.mcp.list_tools(&name).await?
    })))
}

async fn get_metrics(
    State(state): State<ServerState>,
    Query(query): Query<RunQuery>,
) -> Result<Json<opensrc_core::PerformanceSnapshot>, ApiError> {
    let run_id = query.run_id.as_deref().map(parse_id).transpose()?;
    Ok(Json(state.runtime.store.performance_snapshot(run_id)?))
}

async fn record_routing_benchmark(
    State(state): State<ServerState>,
    Json(result): Json<RoutingBenchmarkResult>,
) -> Result<Json<RoutingBenchmarkResult>, ApiError> {
    state.runtime.store.record_routing_benchmark(&result)?;
    Ok(Json(result))
}

async fn list_routing_benchmarks(
    State(state): State<ServerState>,
    Query(query): Query<RoutingBenchmarkQuery>,
) -> Result<Json<Vec<RoutingBenchmarkResult>>, ApiError> {
    Ok(Json(state.runtime.store.list_routing_benchmarks(&query)?))
}

async fn aggregate_routing_benchmarks(
    State(state): State<ServerState>,
    Query(query): Query<RoutingBenchmarkQuery>,
) -> Result<Json<Vec<RoutingBenchmarkAggregate>>, ApiError> {
    Ok(Json(
        state.runtime.store.aggregate_routing_benchmarks(&query)?,
    ))
}

#[derive(Debug, Default, Deserialize)]
struct PromoteRoutingBenchmarkRequest {
    minimum_samples: Option<u64>,
    policy_version: Option<String>,
}

async fn promote_routing_benchmark(
    State(state): State<ServerState>,
    Path(role): Path<String>,
    Json(request): Json<PromoteRoutingBenchmarkRequest>,
) -> Result<Json<RolePolicy>, ApiError> {
    let policy_version = request
        .policy_version
        .unwrap_or_else(|| state.runtime.routing_policies.snapshot().version);
    let aggregates = state
        .runtime
        .store
        .aggregate_routing_benchmarks(&RoutingBenchmarkQuery {
            policy_version: Some(policy_version),
            role: Some(role.clone()),
            ..RoutingBenchmarkQuery::default()
        })?;
    let minimum_samples = request.minimum_samples.unwrap_or(3).max(1);
    let updated = state
        .runtime
        .routing_policies
        .apply_benchmark_preference(&role, &aggregates, minimum_samples)?
        .ok_or_else(|| {
            ApiError::BadRequest(format!(
                "no eligible benchmark route for role `{role}` with at least {minimum_samples} samples"
            ))
        })?;
    Ok(Json(updated))
}

#[derive(Debug, Deserialize)]
struct RunQuery {
    run_id: Option<String>,
}

async fn list_agents(
    State(state): State<ServerState>,
    Query(query): Query<RunQuery>,
) -> Result<Json<Vec<Agent>>, ApiError> {
    let run_id = query.run_id.as_deref().map(parse_id).transpose()?;
    Ok(Json(state.runtime.agents.list_agents(run_id)?))
}

#[derive(Debug, Deserialize)]
struct AgentDefinitionQuery {
    project_root: Option<PathBuf>,
}

async fn list_agent_definitions(
    Query(query): Query<AgentDefinitionQuery>,
) -> Result<Json<Vec<AgentDefinition>>, ApiError> {
    Ok(Json(if let Some(project_root) = query.project_root {
        discover_agent_definitions(project_root)?
    } else {
        built_in_agent_definitions()?
    }))
}

async fn get_agent_status(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> Result<Json<Agent>, ApiError> {
    Ok(Json(state.runtime.agents.get_agent_status(parse_id(&id)?)?))
}

#[derive(Debug, Deserialize)]
struct WaitAgentsRequest {
    agent_ids: Vec<String>,
    timeout_ms: Option<u64>,
}

async fn wait_for_agents(
    State(state): State<ServerState>,
    Json(request): Json<WaitAgentsRequest>,
) -> Result<Json<Vec<Agent>>, ApiError> {
    let ids = request
        .agent_ids
        .iter()
        .map(|id| parse_id(id))
        .collect::<Result<Vec<_>, _>>()?;
    let timeout =
        std::time::Duration::from_millis(request.timeout_ms.unwrap_or(30_000).min(300_000));
    Ok(Json(
        state.runtime.agents.wait_for_agents(&ids, timeout).await?,
    ))
}

#[derive(Debug, Deserialize)]
struct RootAgentRequest {
    run_id: String,
    definition: AgentDefinition,
    task: String,
    workspace_root: String,
}

async fn create_root_agent(
    State(state): State<ServerState>,
    Json(request): Json<RootAgentRequest>,
) -> Result<Json<Agent>, ApiError> {
    Ok(Json(state.runtime.agents.create_root(
        parse_id(&request.run_id)?,
        &request.definition,
        request.task,
        request.workspace_root,
    )?))
}

#[derive(Debug, Deserialize)]
struct SpawnAgentRequest {
    parent_id: String,
    definition: AgentDefinition,
    task: String,
    context_policy: Option<ContextPolicy>,
    #[serde(default)]
    owned_paths: Vec<String>,
}

async fn spawn_agent(
    State(state): State<ServerState>,
    Json(request): Json<SpawnAgentRequest>,
) -> Result<Json<Agent>, ApiError> {
    Ok(Json(state.runtime.agents.spawn_agent_with_ownership(
        parse_id(&request.parent_id)?,
        &request.definition,
        request.task,
        request.context_policy,
        request.owned_paths,
    )?))
}

#[derive(Debug, Deserialize)]
struct MessageRequest {
    message: String,
}

async fn send_message(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    Json(request): Json<MessageRequest>,
) -> Result<StatusCode, ApiError> {
    state
        .runtime
        .agents
        .send_message(parse_id(&id)?, request.message)?;
    Ok(StatusCode::ACCEPTED)
}

#[derive(Debug, Deserialize)]
struct FollowupRequest {
    description: String,
    #[serde(default)]
    priority: i32,
}

async fn assign_followup(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    Json(request): Json<FollowupRequest>,
) -> Result<Json<Task>, ApiError> {
    Ok(Json(state.runtime.agents.assign_followup(
        parse_id(&id)?,
        request.description,
        request.priority,
    )?))
}

async fn start_agent(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> Result<Json<Agent>, ApiError> {
    Ok(Json(state.runtime.agents.start_agent(parse_id(&id)?)?))
}

async fn wait_agent(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> Result<Json<Agent>, ApiError> {
    Ok(Json(state.runtime.agents.wait_agent(parse_id(&id)?)?))
}

async fn interrupt_agent(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.runtime.agents.interrupt_agent(parse_id(&id)?)?;
    Ok(StatusCode::ACCEPTED)
}

async fn resume_agent(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> Result<Json<Agent>, ApiError> {
    Ok(Json(state.runtime.agents.resume_agent(parse_id(&id)?)?))
}

#[derive(Debug, Deserialize)]
struct CompleteRequest {
    task_id: Option<String>,
    completion: TaskCompletion,
}

async fn complete_task(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    Json(request): Json<CompleteRequest>,
) -> Result<Json<Agent>, ApiError> {
    let task_id = request.task_id.as_deref().map(parse_id).transpose()?;
    Ok(Json(state.runtime.agents.complete_task(
        parse_id(&id)?,
        task_id,
        &request.completion,
    )?))
}

async fn list_tasks(
    State(state): State<ServerState>,
    Query(query): Query<RunQuery>,
) -> Result<Json<Vec<Task>>, ApiError> {
    let run_id: Option<RunId> = query.run_id.as_deref().map(parse_id).transpose()?;
    Ok(Json(state.runtime.store.list_tasks(run_id)?))
}

async fn get_task(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> Result<Json<Task>, ApiError> {
    Ok(Json(state.runtime.store.get_task(parse_id(&id)?)?))
}

async fn list_ready_tasks(
    State(state): State<ServerState>,
    Query(query): Query<RunQuery>,
) -> Result<Json<Vec<String>>, ApiError> {
    let run_id = query
        .run_id
        .as_deref()
        .map(parse_id)
        .transpose()?
        .ok_or_else(|| ApiError::InvalidId("run_id query parameter is required".to_string()))?;
    let tasks = state.runtime.store.list_tasks(Some(run_id))?;
    opensrc_runtime::validate_task_graph(&tasks)
        .map_err(|error| ApiError::InvalidId(error.to_string()))?;
    Ok(Json(
        opensrc_runtime::ready_tasks(&tasks)
            .into_iter()
            .map(|id| id.to_string())
            .collect(),
    ))
}

async fn start_task(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> Result<Json<Task>, ApiError> {
    Ok(Json(state.runtime.agents.start_task(parse_id(&id)?)?))
}

#[derive(Debug, Deserialize)]
struct ReassignTaskRequest {
    agent_id: String,
}

async fn reassign_task(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    Json(request): Json<ReassignTaskRequest>,
) -> Result<Json<Task>, ApiError> {
    Ok(Json(state.runtime.agents.reassign_task(
        parse_id(&id)?,
        parse_id(&request.agent_id)?,
    )?))
}

#[derive(Debug, Deserialize)]
struct EventQuery {
    #[serde(default)]
    after: i64,
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
struct EventPage {
    events: Vec<DomainEvent>,
    next_after: i64,
}

async fn list_events(
    State(state): State<ServerState>,
    Query(query): Query<EventQuery>,
) -> Result<Json<EventPage>, ApiError> {
    let events = state
        .runtime
        .store
        .events_after(query.after, query.limit.unwrap_or(200).min(1_000))?;
    let next_after = events.last().map_or(query.after, |event| event.id);
    Ok(Json(EventPage { events, next_after }))
}

async fn event_cursor(State(state): State<ServerState>) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        json!({"after": state.runtime.store.latest_event_id()?}),
    ))
}

async fn stream_events(
    State(state): State<ServerState>,
    Query(query): Query<EventQuery>,
) -> Sse<impl futures::Stream<Item = Result<SseEvent, Infallible>>> {
    let stream = futures::stream::unfold(
        (
            state.runtime.store,
            query.after,
            VecDeque::<DomainEvent>::new(),
        ),
        |(store, mut cursor, mut pending)| async move {
            loop {
                if let Some(event) = pending.pop_front() {
                    cursor = event.id;
                    let data = serde_json::to_string(&event)
                        .unwrap_or_else(|error| json!({"error": error.to_string()}).to_string());
                    let item = SseEvent::default()
                        .id(event.id.to_string())
                        .event(event.kind)
                        .data(data);
                    return Some((Ok(item), (store, cursor, pending)));
                }
                match store.events_after(cursor, 200) {
                    Ok(events) => pending.extend(events),
                    Err(error) => {
                        let item = SseEvent::default()
                            .event("stream.error")
                            .data(json!({"error": error.to_string()}).to_string());
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        return Some((Ok(item), (store, cursor, pending)));
                    }
                }
                if pending.is_empty() {
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
            }
        },
    );
    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn parse_id(value: &str) -> Result<uuid::Uuid, ApiError> {
    uuid::Uuid::parse_str(value).map_err(|_| ApiError::InvalidId(value.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{
        ApiError, ConnectProviderRequest, ProviderConfigError, ProviderProtocol, ServerState,
        automatic_agent_name, request_needs_mutation, router,
    };
    use async_trait::async_trait;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use axum::response::IntoResponse;
    use opensrc_core::{
        CanonicalModelRequest, MessageContent, MessageRole, ModelEvent, ProviderAdapter,
        ProviderCapabilities, ProviderError,
    };
    use opensrc_runtime::{AgentLimits, ProviderRouter, Runtime, ToolExecutor};
    use opensrc_store::Store;
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;
    use uuid::Uuid;

    const SENTINEL_SECRET: &str = "sk-opensource-sentinel-never-log";

    #[test]
    fn provider_connect_debug_output_redacts_the_api_key() {
        let request = ConnectProviderRequest {
            id: "sentinel".to_string(),
            protocol: ProviderProtocol::OpenaiCompatible,
            family: None,
            base_url: "https://provider.example/v1".to_string(),
            api_key: Some(SENTINEL_SECRET.to_string()),
            api_key_env: None,
            default_model: "sentinel-model".to_string(),
            test_connection: false,
        };

        let debug = format!("{request:?}");
        assert!(!debug.contains(SENTINEL_SECRET));
        assert!(debug.contains("[REDACTED]"));
    }

    #[tokio::test]
    async fn provider_error_responses_never_echo_sentinel_secrets() {
        let response = ApiError::Provider(ProviderError::Authentication(format!(
            "Bearer {SENTINEL_SECRET}"
        )))
        .into_response();
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("provider error body");
        let body = String::from_utf8(body.to_vec()).expect("UTF-8 error body");

        assert!(!body.contains(SENTINEL_SECRET));
        assert!(body.contains("provider authentication failed"));

        let config_error = ApiError::ProviderConfig(ProviderConfigError::InvalidBaseUrl {
            provider: "sentinel".to_string(),
            base_url: format!("https://user:{SENTINEL_SECRET}@provider.example"),
        });
        assert!(!config_error.public_message().contains(SENTINEL_SECRET));
    }

    struct RecordingProvider {
        requests: Arc<Mutex<Vec<CanonicalModelRequest>>>,
    }

    #[async_trait]
    impl ProviderAdapter for RecordingProvider {
        fn id(&self) -> &'static str {
            "fixture"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                supports_streaming: true,
                supports_multimodal_input: true,
                ..ProviderCapabilities::default()
            }
        }

        async fn execute(
            &self,
            request: CanonicalModelRequest,
        ) -> Result<Vec<ModelEvent>, ProviderError> {
            let turn = {
                let mut requests = self.requests.lock().expect("request capture");
                requests.push(request);
                requests.len()
            };
            Ok(vec![
                ModelEvent::TextDelta {
                    text: format!("fixture turn {turn}"),
                },
                ModelEvent::Usage {
                    input_tokens: turn as u64,
                    output_tokens: 3,
                    cached_tokens: 0,
                },
                ModelEvent::Completed {
                    response_id: Some(format!("fixture-{turn}")),
                },
            ])
        }
    }

    struct FailingDiscoveryProvider;

    #[async_trait]
    impl ProviderAdapter for FailingDiscoveryProvider {
        fn id(&self) -> &'static str {
            "fixture-discovery"
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }

        async fn execute(
            &self,
            _request: CanonicalModelRequest,
        ) -> Result<Vec<ModelEvent>, ProviderError> {
            Err(ProviderError::Rejected(
                "not used by model discovery".to_string(),
            ))
        }

        async fn list_models(&self) -> Result<Vec<String>, ProviderError> {
            Err(ProviderError::Transient(
                "catalog endpoint unavailable".to_string(),
            ))
        }
    }

    #[tokio::test]
    async fn model_refresh_keeps_default_models_when_a_provider_catalog_fails() {
        let store = Store::in_memory().expect("store");
        let providers = ProviderRouter::default();
        providers.register_with_model(Arc::new(FailingDiscoveryProvider), "configured-model");
        let app = router(ServerState {
            runtime: Runtime::with_services(
                store,
                AgentLimits::default(),
                providers,
                ToolExecutor::default(),
            ),
            provider_config_path: None,
        });

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/models?refresh=true")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("body"),
        )
        .expect("JSON response");
        assert_eq!(body["models"][0]["id"], "configured-model");
        assert_eq!(body["discovery_errors"][0]["provider"], "fixture-discovery");
    }

    #[test]
    fn builds_versioned_router() {
        let store = Store::in_memory().expect("store");
        let state = ServerState {
            runtime: Runtime::new(store, AgentLimits::default()),
            provider_config_path: None,
        };
        let _router = router(state);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn routing_benchmark_endpoints_record_filter_and_aggregate_results() {
        let store = Store::in_memory().expect("store");
        let app = router(ServerState {
            runtime: Runtime::new(store, AgentLimits::default()),
            provider_config_path: None,
        });
        let benchmark_id = Uuid::new_v4();
        let recorded = send_json(
            app.clone(),
            "/v1/routing-benchmarks",
            serde_json::json!({
                "id": benchmark_id,
                "policy_version": "1",
                "role": "architect",
                "provider": "deepseek",
                "model": "deepseek-v4-pro",
                "scenario_id": "architecture-001",
                "metrics": {
                    "architecture_quality_bps": 9200,
                    "latency_ms": 1250,
                    "input_tokens": 800,
                    "output_tokens": 240,
                    "cache_hits": 100,
                    "cost_microusd": 2350
                },
                "created_at": "2026-07-29T00:00:00Z",
                "updated_at": "2026-07-29T00:00:00Z"
            }),
        )
        .await;
        assert_eq!(recorded["id"], benchmark_id.to_string());
        send_json(
            app.clone(),
            "/v1/routing-benchmarks",
            serde_json::json!({
                "id": Uuid::new_v4(),
                "policy_version": "1",
                "role": "architect",
                "provider": "kimi",
                "model": "kimi-k2.7-code",
                "scenario_id": "architecture-002",
                "metrics": {
                    "architecture_quality_bps": 9800,
                    "latency_ms": 1250,
                    "input_tokens": 800,
                    "output_tokens": 240,
                    "cache_hits": 100,
                    "cost_microusd": 2350
                },
                "created_at": "2026-07-29T00:01:00Z",
                "updated_at": "2026-07-29T00:01:00Z"
            }),
        )
        .await;

        let list_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/routing-benchmarks?role=architect&provider=deepseek")
                    .body(Body::empty())
                    .expect("list request"),
            )
            .await
            .expect("list response");
        assert_eq!(list_response.status(), StatusCode::OK);
        let list: serde_json::Value = serde_json::from_slice(
            &to_bytes(list_response.into_body(), usize::MAX)
                .await
                .expect("list body"),
        )
        .expect("list JSON");
        assert_eq!(list.as_array().map(Vec::len), Some(1));

        let aggregate_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/routing-benchmarks/aggregate?role=architect")
                    .body(Body::empty())
                    .expect("aggregate request"),
            )
            .await
            .expect("aggregate response");
        assert_eq!(aggregate_response.status(), StatusCode::OK);
        let aggregates: serde_json::Value = serde_json::from_slice(
            &to_bytes(aggregate_response.into_body(), usize::MAX)
                .await
                .expect("aggregate body"),
        )
        .expect("aggregate JSON");
        assert_eq!(aggregates.as_array().map(Vec::len), Some(2));
        assert_eq!(aggregates[0]["samples"], 1);
        assert_eq!(
            aggregates[0]["mean_metrics"]["architecture_quality_bps"],
            9200
        );
        assert_eq!(aggregates[0]["mean_metrics"]["latency_ms"], 1250);
        assert_eq!(aggregates[0]["mean_metrics"]["cost_microusd"], 2350);

        let promoted = send_json(
            app,
            "/v1/routing-benchmarks/architect/promote",
            serde_json::json!({
                "minimum_samples": 1,
                "policy_version": "1"
            }),
        )
        .await;
        assert_eq!(promoted["primary_model"], "kimi-code");
        assert_eq!(promoted["fallback_models"][0], "deepseek-pro");
    }

    #[tokio::test]
    async fn routing_benchmark_endpoint_rejects_out_of_range_scores() {
        let store = Store::in_memory().expect("store");
        let app = router(ServerState {
            runtime: Runtime::new(store, AgentLimits::default()),
            provider_config_path: None,
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/routing-benchmarks")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "id": Uuid::new_v4(),
                            "policy_version": "1",
                            "role": "architect",
                            "provider": "deepseek",
                            "model": "deepseek-v4-pro",
                            "scenario_id": "invalid-score",
                            "metrics": {
                                "architecture_quality_bps": 10001,
                                "latency_ms": 1,
                                "input_tokens": 1,
                                "output_tokens": 1,
                                "cache_hits": 0,
                                "cost_microusd": 1
                            },
                            "created_at": "2026-07-29T00:00:00Z",
                            "updated_at": "2026-07-29T00:00:00Z"
                        })
                        .to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn automatically_routes_specialized_coding_work() {
        assert_eq!(
            automatic_agent_name(
                "Fix the Ratatui cursor and improve the terminal UI",
                opensrc_core::ExecutionMode::Focused
            ),
            "frontend-specialist"
        );
        assert_eq!(
            automatic_agent_name(
                "Implement a new provider protocol and API",
                opensrc_core::ExecutionMode::Focused
            ),
            "integration-specialist"
        );
        assert_eq!(
            automatic_agent_name(
                "Analyze this screen recording and inspect its video codecs",
                opensrc_core::ExecutionMode::Focused
            ),
            "media-specialist"
        );
        assert_eq!(
            automatic_agent_name(
                "Benchmark rendering latency and find the performance bottleneck",
                opensrc_core::ExecutionMode::Focused
            ),
            "performance-specialist"
        );
        assert_eq!(
            automatic_agent_name(
                "Design the migration boundaries",
                opensrc_core::ExecutionMode::Agentic
            ),
            "architect"
        );
        assert_eq!(
            automatic_agent_name("What is Rust?", opensrc_core::ExecutionMode::Direct),
            "generalist"
        );
    }

    #[test]
    fn routes_media_backed_implementation_to_a_writer() {
        assert_eq!(
            automatic_agent_name(
                "Analyze the calculator in this image and replicate it in HTML, CSS, and JavaScript",
                opensrc_core::ExecutionMode::Focused,
            ),
            "frontend-specialist"
        );
        assert_eq!(
            automatic_agent_name(
                "Use this recording to build the requested local project",
                opensrc_core::ExecutionMode::Focused,
            ),
            "implementer"
        );
        assert_eq!(
            automatic_agent_name(
                "Analyze this recording and report what happens",
                opensrc_core::ExecutionMode::Focused,
            ),
            "media-specialist"
        );
    }

    #[test]
    fn negated_write_language_does_not_request_mutation() {
        assert!(!request_needs_mutation(
            "Inspect the files with fs.read_many. Do not write or modify anything."
        ));
        assert!(!request_needs_mutation(
            "Perform a read-only review with no changes."
        ));
        assert!(request_needs_mutation("Write the fixed files."));
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn chat_stream_history_survives_a_database_reopen() {
        let database = std::env::temp_dir().join(format!("opensrc-chat-{}.db", Uuid::new_v4()));
        let project = std::env::temp_dir()
            .join(format!("opensrc-project-{}", Uuid::new_v4()))
            .to_string_lossy()
            .into_owned();
        std::fs::create_dir_all(&project).expect("project");
        let attachment =
            std::env::temp_dir().join(format!("opensrc-direct-attachment-{}.png", Uuid::new_v4()));
        std::fs::write(&attachment, b"image").expect("attachment");
        let store = Store::open(&database).expect("store");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let providers = ProviderRouter::default();
        providers.register_with_model(
            Arc::new(RecordingProvider {
                requests: requests.clone(),
            }),
            "fixture-model",
        );
        let state = ServerState {
            runtime: Runtime::with_services(
                store.clone(),
                AgentLimits::default(),
                providers,
                ToolExecutor::default(),
            ),
            provider_config_path: None,
        };
        let app = router(state);

        let first = send_json(
            app.clone(),
            "/v1/chat",
            serde_json::json!({
                "project_root": project,
                "message": "first prompt",
                "provider": "fixture",
                "model": "fixture-model",
                "mode": "direct",
                "attachments": [attachment]
            }),
        )
        .await;
        assert_eq!(first["result"]["output"], "fixture turn 1");
        let conversation_id = first["conversation"]["id"]
            .as_str()
            .expect("conversation id")
            .to_string();

        let second = send_json(
            app,
            "/v1/chat",
            serde_json::json!({
                "conversation_id": conversation_id,
                "project_root": project,
                "message": "second prompt",
                "mode": "direct"
            }),
        )
        .await;
        assert_eq!(second["result"]["output"], "fixture turn 2");

        let captured = requests.lock().expect("captured requests");
        assert_eq!(captured.len(), 2);
        assert!(matches!(
            captured[0].messages[0].content.get(1),
            Some(MessageContent::FileReference { path, mime_type })
                if std::path::Path::new(path)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("png"))
                    && mime_type.as_deref() == Some("image/png")
        ));
        assert_eq!(captured[1].messages.len(), 3);
        assert_eq!(captured[1].messages[0].role, MessageRole::User);
        assert_eq!(captured[1].messages[1].role, MessageRole::Assistant);
        assert_eq!(captured[1].messages[2].role, MessageRole::User);
        assert_eq!(
            captured[1].messages[1].content,
            vec![MessageContent::text("fixture turn 1")]
        );
        drop(captured);

        let reopened = Store::open(&database).expect("reopened store");
        let restored = reopened
            .list_messages(Uuid::parse_str(&conversation_id).expect("id"))
            .expect("restored messages");
        assert_eq!(restored.len(), 4);
        assert_eq!(
            restored
                .iter()
                .map(|message| message.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        assert_eq!(
            restored[3].content,
            vec![MessageContent::text("fixture turn 2")]
        );
        let conversation = reopened
            .get_conversation(Uuid::parse_str(&conversation_id).expect("id"))
            .expect("restored conversation");
        assert_eq!(conversation.provider.as_deref(), Some("fixture"));
        assert_eq!(conversation.model.as_deref(), Some("fixture-model"));

        drop(conversation);
        drop(restored);
        drop(reopened);
        drop(store);
        std::fs::remove_file(database).expect("cleanup database");
        std::fs::remove_file(attachment).expect("cleanup attachment");
        std::fs::remove_dir_all(project).expect("cleanup project");
    }

    #[tokio::test]
    async fn compacts_exports_and_imports_a_conversation() {
        let store = Store::in_memory().expect("store");
        let conversation = store
            .create_conversation("C:/source", Some("Portable".to_string()))
            .expect("conversation");
        store
            .append_message(
                conversation.id,
                None,
                MessageRole::User,
                vec![MessageContent::text("remember the architecture")],
                None,
                None,
                None,
            )
            .expect("user message");
        store
            .append_message(
                conversation.id,
                None,
                MessageRole::Assistant,
                vec![MessageContent::text("architecture remembered")],
                None,
                None,
                None,
            )
            .expect("assistant message");
        let app = router(ServerState {
            runtime: Runtime::new(store.clone(), AgentLimits::default()),
            provider_config_path: None,
        });
        let compacted = send_json(
            app.clone(),
            &format!("/v1/conversations/{}/compact", conversation.id),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(compacted["content"][0]["type"], "context_summary");

        let export_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/conversations/{}/export", conversation.id))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(export_response.status(), StatusCode::OK);
        let bytes = to_bytes(export_response.into_body(), usize::MAX)
            .await
            .expect("body");
        let exported: serde_json::Value = serde_json::from_slice(&bytes).expect("export response");
        assert_eq!(exported["json"]["format"], "opensource.session.v1");
        assert_eq!(
            exported["json"]["messages"].as_array().map(Vec::len),
            Some(3)
        );

        let imported = send_json(
            app,
            "/v1/conversations/import",
            serde_json::json!({
                "project_root": "C:/destination",
                "document": exported["json"]
            }),
        )
        .await;
        assert_eq!(imported["project_root"], "C:/destination");
        let imported_id =
            Uuid::parse_str(imported["id"].as_str().expect("imported id")).expect("uuid");
        assert_eq!(
            store
                .list_messages(imported_id)
                .expect("imported messages")
                .len(),
            3
        );
    }

    async fn send_json(
        app: axum::Router,
        uri: &str,
        value: serde_json::Value,
    ) -> serde_json::Value {
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(value.to_string()))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json response")
    }
}

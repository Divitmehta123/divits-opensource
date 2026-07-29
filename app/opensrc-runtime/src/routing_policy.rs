use crate::ProviderRouter;
use opensrc_core::{
    AgentDefinition, ContextInheritance, ContextPolicy, RetryPolicy, RoutingBenchmarkAggregate,
    RoutingBenchmarkMetrics, ToolPolicy, WorkspaceMode,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use thiserror::Error;
use uuid::Uuid;

pub const ROUTING_POLICY_VERSION: &str = "opensource-multi-llm-v1";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoleExecutionKind {
    Llm,
    Deterministic,
    Hybrid,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingMode {
    Disabled,
    Enabled,
    Always,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolProfile {
    RuntimeOnly,
    ReadOnly,
    BrowserReadOnly,
    BoundedWriter,
    TestWriter,
    IntegrationWriter,
    ReleaseGate,
}

impl ToolProfile {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::RuntimeOnly => "runtime only",
            Self::ReadOnly => "read only",
            Self::BrowserReadOnly => "browser read only",
            Self::BoundedWriter => "bounded writer",
            Self::TestWriter => "test writer",
            Self::IntegrationWriter => "integration writer",
            Self::ReleaseGate => "release gate",
        }
    }

    #[must_use]
    pub const fn writes(self) -> bool {
        matches!(
            self,
            Self::BoundedWriter | Self::TestWriter | Self::IntegrationWriter
        )
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CostClass {
    Zero,
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LatencyClass {
    Instant,
    Fast,
    Moderate,
    Slow,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelTarget {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelAlias {
    pub id: String,
    pub display_name: String,
    pub context_window: u64,
    #[serde(default)]
    pub multimodal: bool,
    #[serde(default)]
    pub always_thinking: bool,
    pub targets: Vec<ModelTarget>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeRoutingLimits {
    pub max_active_agents: usize,
    pub max_active_writers: usize,
    pub max_agent_depth: usize,
    pub max_deep_reasoning_agents: usize,
    pub max_retries_per_task: u32,
}

impl Default for RuntimeRoutingLimits {
    fn default() -> Self {
        Self {
            max_active_agents: 4,
            max_active_writers: 2,
            max_agent_depth: 2,
            max_deep_reasoning_agents: 1,
            max_retries_per_task: 2,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RolePolicy {
    pub role: String,
    pub execution: RoleExecutionKind,
    pub primary_model: Option<String>,
    #[serde(default)]
    pub fallback_models: Vec<String>,
    pub thinking: ThinkingMode,
    pub reasoning_effort: Option<String>,
    pub context_policy: ContextPolicy,
    pub tool_profile: ToolProfile,
    #[serde(default)]
    pub writable_paths: Vec<String>,
    pub cost_class: CostClass,
    pub latency_class: LatencyClass,
    #[serde(default)]
    pub independent_reviewer: Option<String>,
    #[serde(default)]
    pub deterministic_services: Vec<String>,
    #[serde(default)]
    pub maximum_retries: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoutingPolicySet {
    pub version: String,
    pub limits: RuntimeRoutingLimits,
    pub models: BTreeMap<String, ModelAlias>,
    pub roles: BTreeMap<String, RolePolicy>,
}

impl Default for RoutingPolicySet {
    fn default() -> Self {
        built_in_policy_set()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedModelAssignment {
    pub alias: String,
    pub display_name: String,
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RolePolicyDescriptor {
    #[serde(flatten)]
    pub policy: RolePolicy,
    pub primary: Option<ResolvedModelAssignment>,
    pub fallbacks: Vec<ResolvedModelAssignment>,
    #[serde(default)]
    pub missing_models: Vec<String>,
}

pub fn apply_role_policy(
    definition: &mut AgentDefinition,
    policy: &RolePolicy,
    assignment: Option<&ResolvedModelAssignment>,
    fallbacks: &[ResolvedModelAssignment],
) {
    if let Some(assignment) = assignment {
        definition.preferred_provider = Some(assignment.provider.clone());
        definition.preferred_model = Some(assignment.model.clone());
    } else if policy.execution == RoleExecutionKind::Deterministic {
        definition.preferred_provider = Some("runtime".to_string());
        definition.preferred_model = Some(format!("deterministic:{}", policy.role));
    }
    definition
        .reasoning
        .level
        .clone_from(&policy.reasoning_effort);
    definition.context_policy = policy.context_policy.clone();
    definition.tool_policy = tool_policy_for_profile(policy.tool_profile);
    definition.workspace_mode = if policy.tool_profile.writes() {
        WorkspaceMode::OwnedPaths
    } else {
        WorkspaceMode::SharedReadonly
    };
    definition.budgets.turn_limit = Some(20);
    definition.budgets.token_limit = Some(30_000);
    definition.budgets.time_limit_ms = Some(1_200_000);
    definition.retry_policy = RetryPolicy {
        max_attempts: u32::from(policy.execution != RoleExecutionKind::Deterministic) + 1,
        initial_backoff_ms: 750,
        max_backoff_ms: 8_000,
    };
    definition.fallback_chain = fallbacks
        .iter()
        .map(|fallback| format!("{}/{}", fallback.provider, fallback.model))
        .collect();
    definition.completion_schema =
        if matches!(policy.role.as_str(), "code-reviewer" | "security-reviewer") {
            "review_completion_v1".to_string()
        } else {
            "agent_completion_v1".to_string()
        };
    definition.metadata.insert(
        "routing_policy".to_string(),
        ROUTING_POLICY_VERSION.to_string(),
    );
    definition.metadata.insert(
        "execution_kind".to_string(),
        format!("{:?}", policy.execution).to_ascii_lowercase(),
    );
    definition.metadata.insert(
        "thinking_mode".to_string(),
        format!("{:?}", policy.thinking).to_ascii_lowercase(),
    );
    definition.metadata.insert(
        "tool_profile".to_string(),
        policy.tool_profile.label().to_string(),
    );
    definition.metadata.insert(
        "cost_class".to_string(),
        format!("{:?}", policy.cost_class).to_ascii_lowercase(),
    );
    definition.metadata.insert(
        "latency_class".to_string(),
        format!("{:?}", policy.latency_class).to_ascii_lowercase(),
    );
}

fn tool_policy_for_profile(profile: ToolProfile) -> ToolPolicy {
    let values: &[&str] = match profile {
        ToolProfile::RuntimeOnly => &["agents.status", "agents.wait", "process.poll"],
        ToolProfile::ReadOnly => &[
            "fs.read",
            "fs.read_many",
            "fs.list",
            "fs.stat",
            "fs.glob",
            "fs.view_image",
            "search.*",
            "git.diff",
            "git.status",
            "git.log",
            "git.show",
            "git.branch",
            "shell.test",
            "skill.activate",
            "mcp.list_tools",
        ],
        ToolProfile::BrowserReadOnly => &[
            "fs.read",
            "fs.read_many",
            "fs.list",
            "fs.stat",
            "fs.glob",
            "fs.view_image",
            "search.*",
            "shell.test",
            "skill.activate",
            "mcp.list_tools",
            "mcp.invoke",
        ],
        ToolProfile::BoundedWriter => &[
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
            "git.worktree",
            "skill.*",
            "mcp.*",
        ],
        ToolProfile::TestWriter => &[
            "fs.*",
            "search.*",
            "patch.apply",
            "shell.run",
            "shell.test",
            "process.*",
            "git.diff",
            "git.status",
            "skill.activate",
            "mcp.list_tools",
        ],
        ToolProfile::IntegrationWriter => &[
            "fs.*",
            "search.*",
            "patch.apply",
            "shell.run",
            "shell.test",
            "process.*",
            "git.*",
            "skill.*",
            "mcp.*",
        ],
        ToolProfile::ReleaseGate => &["git.status", "git.diff", "shell.test", "process.poll"],
    };
    ToolPolicy {
        allow: values.iter().map(|value| (*value).to_string()).collect(),
        deny: vec![
            "deploy.*".to_string(),
            "publish.*".to_string(),
            "secret.*".to_string(),
        ],
        may_spawn_children: matches!(
            profile,
            ToolProfile::IntegrationWriter | ToolProfile::BoundedWriter
        ),
    }
}

#[derive(Debug, Error)]
pub enum RoutingPolicyError {
    #[error("failed to read or write routing policy at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid routing policy file {path}: {source}")]
    InvalidFile {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("invalid routing policy: {0}")]
    Invalid(String),
    #[error("unknown routing role `{0}`")]
    UnknownRole(String),
    #[error("model alias `{alias}` is unavailable")]
    ModelUnavailable { alias: String },
}

#[derive(Clone, Default)]
pub struct RoutingPolicyRegistry {
    path: Option<Arc<PathBuf>>,
    policy: Arc<RwLock<RoutingPolicySet>>,
}

impl RoutingPolicyRegistry {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RoutingPolicyError> {
        let path = path.as_ref().to_path_buf();
        let policy = if path.is_file() {
            let bytes = std::fs::read(&path).map_err(|source| RoutingPolicyError::Io {
                path: path.clone(),
                source,
            })?;
            serde_json::from_slice(&bytes).map_err(|source| RoutingPolicyError::InvalidFile {
                path: path.clone(),
                source,
            })?
        } else {
            RoutingPolicySet::default()
        };
        validate_policy_set(&policy)?;
        Ok(Self {
            path: Some(Arc::new(path)),
            policy: Arc::new(RwLock::new(policy)),
        })
    }

    #[must_use]
    pub fn snapshot(&self) -> RoutingPolicySet {
        self.policy
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    #[must_use]
    pub fn limits(&self) -> RuntimeRoutingLimits {
        self.snapshot().limits
    }

    #[must_use]
    pub fn role(&self, role: &str) -> Option<RolePolicy> {
        self.policy
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .roles
            .get(&normalize_role(role))
            .cloned()
    }

    #[must_use]
    pub fn descriptors(&self, providers: &ProviderRouter) -> Vec<RolePolicyDescriptor> {
        let policy = self.snapshot();
        policy
            .roles
            .into_values()
            .map(|role| {
                let mut missing_models = Vec::new();
                let primary = role.primary_model.as_deref().and_then(|alias| {
                    let resolved = resolve_alias_from_set(&policy.models, alias, providers);
                    if resolved.is_none() {
                        missing_models.push(alias.to_string());
                    }
                    resolved
                });
                let fallbacks = role
                    .fallback_models
                    .iter()
                    .filter_map(|alias| {
                        let resolved = resolve_alias_from_set(&policy.models, alias, providers);
                        if resolved.is_none() {
                            missing_models.push(alias.clone());
                        }
                        resolved
                    })
                    .collect();
                RolePolicyDescriptor {
                    policy: role,
                    primary,
                    fallbacks,
                    missing_models,
                }
            })
            .collect()
    }

    pub fn resolve_for_role(
        &self,
        role: &str,
        request: &str,
        providers: &ProviderRouter,
    ) -> Result<Option<ResolvedModelAssignment>, RoutingPolicyError> {
        let policy = self
            .role(role)
            .ok_or_else(|| RoutingPolicyError::UnknownRole(role.to_string()))?;
        if policy.execution == RoleExecutionKind::Deterministic {
            return Ok(None);
        }
        let alias = dynamic_primary_alias(&policy, request);
        alias
            .map(|alias| {
                self.resolve_alias(&alias, providers)
                    .ok_or(RoutingPolicyError::ModelUnavailable { alias })
            })
            .transpose()
    }

    #[must_use]
    pub fn resolve_alias(
        &self,
        alias: &str,
        providers: &ProviderRouter,
    ) -> Option<ResolvedModelAssignment> {
        let policy = self
            .policy
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        resolve_alias_from_set(&policy.models, alias, providers)
    }

    #[must_use]
    pub fn fallback_assignments(
        &self,
        role: &str,
        providers: &ProviderRouter,
    ) -> Vec<ResolvedModelAssignment> {
        self.role(role).map_or_else(Vec::new, |policy| {
            policy
                .fallback_models
                .iter()
                .filter_map(|alias| self.resolve_alias(alias, providers))
                .collect()
        })
    }

    pub fn upsert_role(&self, policy: RolePolicy) -> Result<RolePolicy, RoutingPolicyError> {
        validate_role_policy(&policy, &self.snapshot().models)?;
        let role = normalize_role(&policy.role);
        let mut policy = policy;
        policy.role.clone_from(&role);
        self.policy
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .roles
            .insert(role, policy.clone());
        self.persist()?;
        Ok(policy)
    }

    pub fn apply_benchmark_preference(
        &self,
        role: &str,
        aggregates: &[RoutingBenchmarkAggregate],
        minimum_samples: u64,
    ) -> Result<Option<RolePolicy>, RoutingPolicyError> {
        let normalized_role = normalize_role(role);
        let snapshot = self.snapshot();
        let Some(current) = snapshot.roles.get(&normalized_role).cloned() else {
            return Err(RoutingPolicyError::UnknownRole(role.to_string()));
        };
        if current.execution == RoleExecutionKind::Deterministic {
            return Ok(None);
        }
        let aliases_by_target = snapshot
            .models
            .iter()
            .flat_map(|(alias, profile)| {
                profile.targets.iter().map(move |target| {
                    (
                        (target.provider.as_str(), target.model.as_str()),
                        alias.as_str(),
                    )
                })
            })
            .collect::<BTreeMap<_, _>>();
        let winner = aggregates
            .iter()
            .filter(|aggregate| {
                normalize_role(&aggregate.role) == normalized_role
                    && aggregate.samples >= minimum_samples.max(1)
            })
            .filter_map(|aggregate| {
                aliases_by_target
                    .get(&(aggregate.provider.as_str(), aggregate.model.as_str()))
                    .map(|alias| {
                        (
                            benchmark_route_score(&normalized_role, &aggregate.mean_metrics),
                            (*alias).to_string(),
                        )
                    })
            })
            .max_by(|left, right| left.0.cmp(&right.0).then_with(|| right.1.cmp(&left.1)));
        let Some((_, winner)) = winner else {
            return Ok(None);
        };
        if current.primary_model.as_deref() == Some(&winner) {
            return Ok(Some(current));
        }
        let mut updated = current;
        if let Some(previous) = updated.primary_model.replace(winner.clone()) {
            updated.fallback_models.retain(|alias| alias != &winner);
            if previous != winner {
                updated.fallback_models.retain(|alias| alias != &previous);
                updated.fallback_models.insert(0, previous);
            }
        }
        self.upsert_role(updated.clone())?;
        Ok(Some(updated))
    }

    pub fn replace(&self, policy: RoutingPolicySet) -> Result<(), RoutingPolicyError> {
        validate_policy_set(&policy)?;
        *self
            .policy
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = policy;
        self.persist()
    }

    fn persist(&self) -> Result<(), RoutingPolicyError> {
        let Some(path) = self.path.as_deref() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| RoutingPolicyError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let bytes = serde_json::to_vec_pretty(&self.snapshot()).map_err(|source| {
            RoutingPolicyError::InvalidFile {
                path: path.clone(),
                source,
            }
        })?;
        let temporary = path.with_extension(format!("tmp-{}", Uuid::new_v4()));
        std::fs::write(&temporary, bytes).map_err(|source| RoutingPolicyError::Io {
            path: temporary.clone(),
            source,
        })?;
        if path.exists() {
            std::fs::remove_file(path).map_err(|source| RoutingPolicyError::Io {
                path: path.clone(),
                source,
            })?;
        }
        std::fs::rename(&temporary, path).map_err(|source| RoutingPolicyError::Io {
            path: path.clone(),
            source,
        })
    }
}

fn resolve_alias_from_set(
    models: &BTreeMap<String, ModelAlias>,
    alias: &str,
    providers: &ProviderRouter,
) -> Option<ResolvedModelAssignment> {
    let definition = models.get(alias)?;
    let registered = providers
        .provider_ids()
        .into_iter()
        .collect::<BTreeSet<_>>();
    for target in &definition.targets {
        if !registered.contains(&target.provider) {
            continue;
        }
        let known = providers.known_models(&target.provider);
        if known.is_empty()
            || known
                .iter()
                .any(|model| model.eq_ignore_ascii_case(&target.model))
        {
            return Some(ResolvedModelAssignment {
                alias: definition.id.clone(),
                display_name: definition.display_name.clone(),
                provider: target.provider.clone(),
                model: target.model.clone(),
            });
        }
    }
    None
}

fn dynamic_primary_alias(policy: &RolePolicy, request: &str) -> Option<String> {
    let lower = request.to_ascii_lowercase();
    match policy.role.as_str() {
        "generalist" => {
            if contains_any(
                &lower,
                &[
                    "implement",
                    "edit",
                    "fix",
                    "refactor",
                    "debug",
                    "write code",
                    "multi-file",
                ],
            ) {
                Some("kimi-code".to_string())
            } else if contains_any(
                &lower,
                &[
                    "architecture",
                    "security",
                    "investigate",
                    "root cause",
                    "migration",
                    "whole repository",
                ],
            ) {
                Some("deepseek-pro".to_string())
            } else {
                Some("glm-agent".to_string())
            }
        }
        "browser-validation-specialist" => {
            if contains_any(
                &lower,
                &[
                    "screenshot",
                    "screen recording",
                    "reference image",
                    "visual",
                    "rendered page",
                ],
            ) {
                Some("kimi-code".to_string())
            } else {
                Some("glm-agent".to_string())
            }
        }
        "database-specialist" => {
            if contains_any(
                &lower,
                &[
                    "implement",
                    "apply migration",
                    "write migration",
                    "backfill code",
                ],
            ) {
                Some("kimi-code".to_string())
            } else {
                Some("deepseek-pro".to_string())
            }
        }
        "performance-specialist" => {
            if contains_any(&lower, &["implement fix", "apply fix", "optimize code"]) {
                Some("kimi-code".to_string())
            } else {
                Some("deepseek-pro".to_string())
            }
        }
        _ => policy.primary_model.clone(),
    }
}

fn benchmark_route_score(role: &str, metrics: &RoutingBenchmarkMetrics) -> i128 {
    let role_quality = match role {
        "architect" => metrics.architecture_quality_bps,
        "investigator" | "repository-mapper" => metrics.repository_investigation_accuracy_bps,
        "frontend-specialist" => metrics.frontend_implementation_quality_bps,
        "accessibility-specialist" => metrics.accessibility_finding_quality_bps,
        "code-reviewer" => metrics.review_precision_bps,
        "security-reviewer" => metrics.security_review_precision_bps,
        "test-debugging-specialist" => metrics.test_pass_rate_bps,
        "implementer"
        | "backend-specialist"
        | "refactoring-specialist"
        | "integration-specialist"
        | "database-specialist"
        | "media-specialist" => metrics.patch_success_bps,
        _ => metrics.tool_call_correctness_bps,
    }
    .or(metrics.tool_call_correctness_bps)
    .or(metrics.test_pass_rate_bps)
    .or(metrics.patch_success_bps)
    .unwrap_or_default();
    let retry = metrics.retry_rate_bps.unwrap_or_default();
    i128::from(role_quality) * 1_000_000
        - i128::from(retry) * 10_000
        - i128::from(metrics.latency_ms) * 100
        - i128::from(metrics.cost_microusd)
}

fn contains_any(value: &str, markers: &[&str]) -> bool {
    markers.iter().any(|marker| value.contains(marker))
}

fn validate_policy_set(policy: &RoutingPolicySet) -> Result<(), RoutingPolicyError> {
    if policy.version.trim().is_empty() {
        return Err(RoutingPolicyError::Invalid(
            "routing policy version is required".to_string(),
        ));
    }
    if policy.limits.max_active_agents == 0
        || policy.limits.max_active_writers == 0
        || policy.limits.max_agent_depth == 0
        || policy.limits.max_deep_reasoning_agents == 0
    {
        return Err(RoutingPolicyError::Invalid(
            "routing concurrency limits must be greater than zero".to_string(),
        ));
    }
    if policy.limits.max_active_writers > policy.limits.max_active_agents {
        return Err(RoutingPolicyError::Invalid(
            "max_active_writers cannot exceed max_active_agents".to_string(),
        ));
    }
    for (id, model) in &policy.models {
        if id != &model.id || model.targets.is_empty() {
            return Err(RoutingPolicyError::Invalid(format!(
                "model alias `{id}` must have a matching id and at least one target"
            )));
        }
    }
    for (role, definition) in &policy.roles {
        if role != &normalize_role(&definition.role) {
            return Err(RoutingPolicyError::Invalid(format!(
                "role map key `{role}` does not match `{}`",
                definition.role
            )));
        }
        validate_role_policy(definition, &policy.models)?;
    }
    Ok(())
}

fn validate_role_policy(
    policy: &RolePolicy,
    models: &BTreeMap<String, ModelAlias>,
) -> Result<(), RoutingPolicyError> {
    if policy.role.trim().is_empty() {
        return Err(RoutingPolicyError::Invalid(
            "role name is required".to_string(),
        ));
    }
    if policy.execution == RoleExecutionKind::Llm && policy.primary_model.is_none() {
        return Err(RoutingPolicyError::Invalid(format!(
            "LLM role `{}` needs a primary model",
            policy.role
        )));
    }
    for alias in policy
        .primary_model
        .iter()
        .chain(policy.fallback_models.iter())
    {
        if !models.contains_key(alias) {
            return Err(RoutingPolicyError::Invalid(format!(
                "role `{}` references unknown model alias `{alias}`",
                policy.role
            )));
        }
    }
    if policy.tool_profile.writes() && policy.writable_paths.is_empty() {
        return Err(RoutingPolicyError::Invalid(format!(
            "writing role `{}` needs a writable path policy",
            policy.role
        )));
    }
    Ok(())
}

fn normalize_role(role: &str) -> String {
    match role.trim().to_ascii_lowercase().as_str() {
        "plan" => "architect".to_string(),
        "build" => "implementer".to_string(),
        "frontend" => "frontend-specialist".to_string(),
        "backend" => "backend-specialist".to_string(),
        "test" | "debug" => "test-debugging-specialist".to_string(),
        "review" => "code-reviewer".to_string(),
        "security" => "security-reviewer".to_string(),
        "browser" => "browser-validation-specialist".to_string(),
        "docs" => "documentation-specialist".to_string(),
        value => value.to_string(),
    }
}

#[allow(clippy::too_many_lines)]
fn built_in_policy_set() -> RoutingPolicySet {
    let models = [
        (
            "deepseek-pro",
            ModelAlias {
                id: "deepseek-pro".to_string(),
                display_name: "DeepSeek V4 Pro".to_string(),
                context_window: 1_048_576,
                multimodal: false,
                always_thinking: false,
                targets: model_targets("deepseek", &["deepseek-v4-pro"], &["openrouter"]),
            },
        ),
        (
            "kimi-code",
            ModelAlias {
                id: "kimi-code".to_string(),
                display_name: "Kimi K2.7 Code".to_string(),
                context_window: 262_144,
                multimodal: true,
                always_thinking: true,
                targets: model_targets(
                    "kimi",
                    &["kimi-k2.7-code", "kimi-for-coding"],
                    &["openrouter"],
                ),
            },
        ),
        (
            "glm-agent",
            ModelAlias {
                id: "glm-agent".to_string(),
                display_name: "GLM current".to_string(),
                context_window: 131_072,
                multimodal: false,
                always_thinking: false,
                targets: model_targets(
                    "zai",
                    &["glm-5.2", "glm-5.1", "glm-5", "glm-4.5"],
                    &["openrouter"],
                ),
            },
        ),
    ]
    .into_iter()
    .map(|(id, model)| (id.to_string(), model))
    .collect();

    let mut roles = BTreeMap::new();
    let mut add = |policy: RolePolicy| {
        roles.insert(policy.role.clone(), policy);
    };
    add(deterministic_role(
        "auto",
        ToolProfile::RuntimeOnly,
        &["classification", "task scheduling", "dependency readiness"],
    ));
    add(llm_role(
        "accessibility-specialist",
        "glm-agent",
        &["kimi-code"],
        ThinkingMode::Enabled,
        Some("high"),
        selected_artifacts_context(),
        ToolProfile::BrowserReadOnly,
        CostClass::Low,
        LatencyClass::Fast,
        None,
    ));
    add(llm_role(
        "architect",
        "deepseek-pro",
        &["glm-agent"],
        ThinkingMode::Enabled,
        Some("max"),
        selected_artifacts_context(),
        ToolProfile::ReadOnly,
        CostClass::High,
        LatencyClass::Slow,
        None,
    ));
    add(deterministic_role(
        "awaiter",
        ToolProfile::RuntimeOnly,
        &["process waiting", "provider polling", "dependency wakeups"],
    ));
    add(writer_role(
        "backend-specialist",
        "kimi-code",
        &["deepseek-pro"],
        ToolProfile::BoundedWriter,
        Some("deepseek-pro"),
    ));
    add(llm_role(
        "browser-validation-specialist",
        "glm-agent",
        &["kimi-code"],
        ThinkingMode::Enabled,
        Some("high"),
        selected_artifacts_context(),
        ToolProfile::BrowserReadOnly,
        CostClass::Medium,
        LatencyClass::Moderate,
        None,
    ));
    add(llm_role(
        "code-reviewer",
        "deepseek-pro",
        &["glm-agent"],
        ThinkingMode::Enabled,
        Some("high"),
        selected_artifacts_context(),
        ToolProfile::ReadOnly,
        CostClass::High,
        LatencyClass::Slow,
        None,
    ));
    add(llm_role(
        "database-specialist",
        "deepseek-pro",
        &["kimi-code"],
        ThinkingMode::Enabled,
        Some("high"),
        selected_artifacts_context(),
        ToolProfile::BoundedWriter,
        CostClass::High,
        LatencyClass::Slow,
        Some("deepseek-pro"),
    ));
    add(llm_role(
        "dependency-specialist",
        "deepseek-pro",
        &["kimi-code"],
        ThinkingMode::Enabled,
        Some("high"),
        selected_artifacts_context(),
        ToolProfile::ReadOnly,
        CostClass::High,
        LatencyClass::Slow,
        None,
    ));
    add(llm_role(
        "documentation-specialist",
        "glm-agent",
        &["kimi-code"],
        ThinkingMode::Disabled,
        None,
        selected_artifacts_context(),
        ToolProfile::BoundedWriter,
        CostClass::Low,
        LatencyClass::Fast,
        None,
    ));
    add(writer_role(
        "frontend-specialist",
        "glm-agent",
        &["kimi-code", "deepseek-pro"],
        ToolProfile::BoundedWriter,
        Some("deepseek-pro"),
    ));
    add(llm_role(
        "generalist",
        "glm-agent",
        &["kimi-code", "deepseek-pro"],
        ThinkingMode::Enabled,
        Some("high"),
        summary_context(),
        ToolProfile::BoundedWriter,
        CostClass::Medium,
        LatencyClass::Moderate,
        Some("deepseek-pro"),
    ));
    add(writer_role(
        "implementer",
        "kimi-code",
        &["deepseek-pro"],
        ToolProfile::BoundedWriter,
        Some("deepseek-pro"),
    ));
    add(writer_role(
        "integration-specialist",
        "kimi-code",
        &["deepseek-pro"],
        ToolProfile::IntegrationWriter,
        Some("deepseek-pro"),
    ));
    add(llm_role(
        "investigator",
        "deepseek-pro",
        &["glm-agent"],
        ThinkingMode::Enabled,
        Some("high"),
        selected_artifacts_context(),
        ToolProfile::ReadOnly,
        CostClass::High,
        LatencyClass::Slow,
        None,
    ));
    add(llm_role(
        "media-specialist",
        "kimi-code",
        &["deepseek-pro"],
        ThinkingMode::Always,
        Some("high"),
        selected_artifacts_context(),
        ToolProfile::ReadOnly,
        CostClass::Medium,
        LatencyClass::Moderate,
        None,
    ));
    add(llm_role(
        "performance-specialist",
        "deepseek-pro",
        &["kimi-code"],
        ThinkingMode::Enabled,
        Some("high"),
        selected_artifacts_context(),
        ToolProfile::BoundedWriter,
        CostClass::High,
        LatencyClass::Slow,
        Some("deepseek-pro"),
    ));
    add(writer_role(
        "refactoring-specialist",
        "kimi-code",
        &["deepseek-pro"],
        ToolProfile::BoundedWriter,
        Some("deepseek-pro"),
    ));
    add(hybrid_role(
        "release-specialist",
        Some("glm-agent"),
        ThinkingMode::Disabled,
        ToolProfile::ReleaseGate,
        &[
            "clean-tree check",
            "tests",
            "formatting",
            "linting",
            "artifact build",
        ],
    ));
    add(hybrid_role(
        "repository-mapper",
        Some("deepseek-pro"),
        ThinkingMode::Enabled,
        ToolProfile::RuntimeOnly,
        &[
            "file indexing",
            "language detection",
            "symbol extraction",
            "dependency graph",
        ],
    ));
    add(llm_role(
        "security-reviewer",
        "deepseek-pro",
        &[],
        ThinkingMode::Enabled,
        Some("max"),
        selected_artifacts_context(),
        ToolProfile::ReadOnly,
        CostClass::High,
        LatencyClass::Slow,
        None,
    ));
    add(writer_role(
        "test-debugging-specialist",
        "kimi-code",
        &["deepseek-pro"],
        ToolProfile::TestWriter,
        Some("deepseek-pro"),
    ));

    RoutingPolicySet {
        version: ROUTING_POLICY_VERSION.to_string(),
        limits: RuntimeRoutingLimits::default(),
        models,
        roles,
    }
}

fn model_targets(direct_provider: &str, model_ids: &[&str], gateways: &[&str]) -> Vec<ModelTarget> {
    let mut targets = Vec::new();
    for model in model_ids {
        targets.push(ModelTarget {
            provider: direct_provider.to_string(),
            model: (*model).to_string(),
        });
    }
    for gateway in gateways {
        for model in model_ids {
            targets.push(ModelTarget {
                provider: (*gateway).to_string(),
                model: (*model).to_string(),
            });
        }
    }
    targets
}

#[allow(clippy::too_many_arguments)]
fn llm_role(
    role: &str,
    primary: &str,
    fallbacks: &[&str],
    thinking: ThinkingMode,
    effort: Option<&str>,
    context_policy: ContextPolicy,
    tool_profile: ToolProfile,
    cost_class: CostClass,
    latency_class: LatencyClass,
    independent_reviewer: Option<&str>,
) -> RolePolicy {
    RolePolicy {
        role: role.to_string(),
        execution: RoleExecutionKind::Llm,
        primary_model: Some(primary.to_string()),
        fallback_models: fallbacks.iter().map(|value| (*value).to_string()).collect(),
        thinking,
        reasoning_effort: effort.map(str::to_string),
        context_policy,
        tool_profile,
        writable_paths: if tool_profile.writes() {
            vec!["<task-owned>".to_string()]
        } else {
            Vec::new()
        },
        cost_class,
        latency_class,
        independent_reviewer: independent_reviewer.map(str::to_string),
        deterministic_services: Vec::new(),
        maximum_retries: 2,
    }
}

fn writer_role(
    role: &str,
    primary: &str,
    fallbacks: &[&str],
    profile: ToolProfile,
    reviewer: Option<&str>,
) -> RolePolicy {
    llm_role(
        role,
        primary,
        fallbacks,
        ThinkingMode::Always,
        Some("high"),
        selected_artifacts_context(),
        profile,
        CostClass::Medium,
        LatencyClass::Moderate,
        reviewer,
    )
}

fn deterministic_role(role: &str, profile: ToolProfile, services: &[&str]) -> RolePolicy {
    RolePolicy {
        role: role.to_string(),
        execution: RoleExecutionKind::Deterministic,
        primary_model: None,
        fallback_models: Vec::new(),
        thinking: ThinkingMode::Disabled,
        reasoning_effort: None,
        context_policy: ContextPolicy {
            inheritance: ContextInheritance::None,
            ..ContextPolicy::default()
        },
        tool_profile: profile,
        writable_paths: Vec::new(),
        cost_class: CostClass::Zero,
        latency_class: LatencyClass::Instant,
        independent_reviewer: None,
        deterministic_services: services.iter().map(|value| (*value).to_string()).collect(),
        maximum_retries: 0,
    }
}

fn hybrid_role(
    role: &str,
    summary_model: Option<&str>,
    thinking: ThinkingMode,
    profile: ToolProfile,
    services: &[&str],
) -> RolePolicy {
    RolePolicy {
        role: role.to_string(),
        execution: RoleExecutionKind::Hybrid,
        primary_model: summary_model.map(str::to_string),
        fallback_models: Vec::new(),
        thinking,
        reasoning_effort: thinking
            .ne(&ThinkingMode::Disabled)
            .then(|| "high".to_string()),
        context_policy: selected_artifacts_context(),
        tool_profile: profile,
        writable_paths: Vec::new(),
        cost_class: CostClass::Low,
        latency_class: LatencyClass::Fast,
        independent_reviewer: None,
        deterministic_services: services.iter().map(|value| (*value).to_string()).collect(),
        maximum_retries: 1,
    }
}

fn selected_artifacts_context() -> ContextPolicy {
    ContextPolicy {
        inheritance: ContextInheritance::SelectedItems,
        last_n_turns: None,
        selected_items: Vec::new(),
        max_tokens: Some(100_000),
    }
}

fn summary_context() -> ContextPolicy {
    ContextPolicy {
        inheritance: ContextInheritance::SummaryOnly,
        last_n_turns: None,
        selected_items: Vec::new(),
        max_tokens: Some(64_000),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        RoleExecutionKind, RoutingPolicyRegistry, ThinkingMode, ToolProfile, built_in_policy_set,
        validate_policy_set,
    };
    use crate::ProviderRouter;
    use async_trait::async_trait;
    use futures::stream;
    use opensrc_core::{
        CanonicalModelRequest, ModelEvent, ModelEventStream, ProviderAdapter, ProviderCapabilities,
        ProviderError, RoutingBenchmarkAggregate, RoutingBenchmarkMetrics,
    };
    use std::sync::Arc;

    #[derive(Clone)]
    struct Provider {
        id: &'static str,
    }

    #[async_trait]
    impl ProviderAdapter for Provider {
        fn id(&self) -> &str {
            self.id
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }

        async fn execute(
            &self,
            _request: CanonicalModelRequest,
        ) -> Result<Vec<ModelEvent>, ProviderError> {
            Ok(Vec::new())
        }

        async fn stream(
            &self,
            _request: CanonicalModelRequest,
        ) -> Result<ModelEventStream, ProviderError> {
            Ok(Box::pin(stream::empty()))
        }
    }

    fn providers() -> ProviderRouter {
        let providers = ProviderRouter::default();
        providers.register_with_models(
            Arc::new(Provider { id: "deepseek" }),
            "deepseek-v4-pro",
            Vec::new(),
        );
        providers.register_with_models(
            Arc::new(Provider { id: "kimi" }),
            "kimi-k2.7-code",
            vec!["kimi-for-coding".to_string()],
        );
        providers.register_with_models(Arc::new(Provider { id: "zai" }), "glm-4.5", Vec::new());
        providers
    }

    #[test]
    fn built_in_policy_is_valid_and_covers_every_role() {
        let policy = built_in_policy_set();
        validate_policy_set(&policy).expect("valid built-in policy");
        assert_eq!(policy.roles.len(), 22);
        assert_eq!(
            policy.roles["awaiter"].execution,
            RoleExecutionKind::Deterministic
        );
        assert_eq!(
            policy.roles["architect"].reasoning_effort.as_deref(),
            Some("max")
        );
        assert_eq!(policy.roles["implementer"].thinking, ThinkingMode::Always);
        assert_eq!(
            policy.roles["code-reviewer"].tool_profile,
            ToolProfile::ReadOnly
        );
    }

    #[test]
    fn real_aliases_resolve_to_their_configured_providers() {
        let providers = providers();
        let registry = RoutingPolicyRegistry::default();
        let architect = registry
            .resolve_for_role("architect", "Design the system", &providers)
            .expect("route")
            .expect("model");
        let implementer = registry
            .resolve_for_role("implementer", "Implement it", &providers)
            .expect("route")
            .expect("model");
        let frontend = registry
            .resolve_for_role("frontend-specialist", "Build a component", &providers)
            .expect("route")
            .expect("model");
        assert_eq!(
            (architect.provider.as_str(), architect.model.as_str()),
            ("deepseek", "deepseek-v4-pro")
        );
        assert_eq!(
            (implementer.provider.as_str(), implementer.model.as_str()),
            ("kimi", "kimi-k2.7-code")
        );
        assert_eq!(
            (frontend.provider.as_str(), frontend.model.as_str()),
            ("zai", "glm-4.5")
        );
    }

    #[test]
    fn wait_is_deterministic_and_visual_browser_work_uses_kimi() {
        let providers = providers();
        let registry = RoutingPolicyRegistry::default();
        assert!(
            registry
                .resolve_for_role("awaiter", "Wait for tests", &providers)
                .expect("route")
                .is_none()
        );
        let visual = registry
            .resolve_for_role(
                "browser-validation-specialist",
                "Inspect this screenshot",
                &providers,
            )
            .expect("route")
            .expect("model");
        assert_eq!(visual.alias, "kimi-code");
    }

    #[test]
    fn measured_benchmark_preference_changes_the_next_role_route() {
        let providers = providers();
        let registry = RoutingPolicyRegistry::default();
        let aggregate = |provider: &str, model: &str, quality| RoutingBenchmarkAggregate {
            policy_version: super::ROUTING_POLICY_VERSION.to_string(),
            role: "architect".to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            samples: 5,
            mean_metrics: RoutingBenchmarkMetrics {
                architecture_quality_bps: Some(quality),
                latency_ms: 100,
                input_tokens: 1_000,
                output_tokens: 250,
                cache_hits: 0,
                cost_microusd: 100,
                ..RoutingBenchmarkMetrics::default()
            },
        };
        registry
            .apply_benchmark_preference(
                "architect",
                &[
                    aggregate("deepseek", "deepseek-v4-pro", 7_500),
                    aggregate("kimi", "kimi-k2.7-code", 9_000),
                ],
                3,
            )
            .expect("benchmark policy update")
            .expect("updated role");

        let routed = registry
            .resolve_for_role("architect", "Design the measured system", &providers)
            .expect("route")
            .expect("model");
        assert_eq!(routed.alias, "kimi-code");
        assert_eq!(
            registry
                .role("architect")
                .expect("architect")
                .fallback_models
                .first()
                .map(String::as_str),
            Some("deepseek-pro")
        );
    }
}

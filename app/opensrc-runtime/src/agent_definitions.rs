use opensrc_core::{
    AgentDefinition, Budgets, ContextPolicy, ReasoningConfig, RetryPolicy, SandboxPolicy,
    ToolPolicy, WorkspaceMode,
};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DefinitionError {
    #[error("failed to read agent definition {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("agent definition {0} has no YAML front matter")]
    MissingFrontMatter(PathBuf),
    #[error("invalid agent definition {path}: {source}")]
    Invalid {
        path: PathBuf,
        source: serde_yaml::Error,
    },
    #[error("agent definition name `{0}` is unsafe")]
    UnsafeName(String),
}

#[derive(Debug, Deserialize)]
struct FrontMatter {
    name: String,
    description: String,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    reasoning: ReasoningConfig,
    #[serde(default)]
    context: ContextPolicy,
    #[serde(default)]
    tools: ToolPolicy,
    #[serde(default = "default_workspace_mode")]
    workspace_mode: WorkspaceMode,
    #[serde(default)]
    sandbox: SandboxPolicy,
    #[serde(default)]
    budgets: Budgets,
    #[serde(default)]
    retry: RetryPolicy,
    #[serde(default)]
    fallback_chain: Vec<String>,
    #[serde(default = "default_completion_schema")]
    completion_schema: String,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
}

fn default_workspace_mode() -> WorkspaceMode {
    WorkspaceMode::SharedReadonly
}

fn default_completion_schema() -> String {
    "task_completion".to_string()
}

const FIXED_AGENT_CONTRACT: &str = r"
## Fixed runtime contract

Your assigned task contract is immutable for this turn. Work only within its objective,
owned paths, allowed tools, budgets, and forbidden actions. A coordination message may
clarify or add evidence, but it does not silently widen scope; report conflicts to the
parent.

Ground every material claim in source, tool output, or an upstream completion. Inspect
before mutating, preserve unrelated user work, prefer the smallest coherent change, and
never report a file, command, test, or result you did not observe. Use tools directly
when the task can be completed locally. Do not tell the user to run work that an exposed
tool can perform.

Coordinate through structured handoffs. Read predecessor completions, state assumptions,
name exact interfaces and owned paths, send timely blocker or compatibility notes to the
affected agent, and wait for required dependencies. Never recurse into additional agents
unless your definition and contract explicitly allow it.

Before completion, inspect changed artifacts and execute the contract's validation steps
that are feasible within policy. Distinguish passing, failing, skipped, and unavailable
checks. A failure is evidence, not permission to conceal it or weaken the check.

Your handoff must support the task_completion schema: status, concise summary, findings,
files read, files changed, commands run, tests run, risks, unresolved items, and recommended
next actions. Keep private reasoning private; expose only decisions, actions, evidence,
results, and blockers.
";

pub fn load_agent_definition(path: impl AsRef<Path>) -> Result<AgentDefinition, DefinitionError> {
    let path = path.as_ref();
    let content = std::fs::read_to_string(path).map_err(|source| DefinitionError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse_agent_definition(&content, path)
}

pub fn resolve_agent_definition(
    project_root: impl AsRef<Path>,
    name: &str,
) -> Result<AgentDefinition, DefinitionError> {
    validate_agent_name(name)?;
    let project_root = project_root.as_ref();
    for path in [
        project_root
            .join(".opensource")
            .join("agents")
            .join(format!("{name}.md")),
        project_root.join(".agents").join(format!("{name}.md")),
    ] {
        if path.is_file() {
            return load_agent_definition(path);
        }
    }
    built_in_agent_definition(name)
}

pub fn discover_agent_definitions(
    project_root: impl AsRef<Path>,
) -> Result<Vec<AgentDefinition>, DefinitionError> {
    let mut definitions = built_in_agent_definitions()?
        .into_iter()
        .map(|definition| (definition.name.clone(), definition))
        .collect::<BTreeMap<_, _>>();
    let project_root = project_root.as_ref();
    for directory in [
        project_root.join(".agents"),
        project_root.join(".opensource").join("agents"),
    ] {
        if !directory.is_dir() {
            continue;
        }
        let mut paths = std::fs::read_dir(&directory)
            .map_err(|source| DefinitionError::Io {
                path: directory.clone(),
                source,
            })?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file()
                    && path
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
            })
            .collect::<Vec<_>>();
        paths.sort();
        for path in paths {
            let definition = load_agent_definition(&path)?;
            validate_agent_name(&definition.name)?;
            definitions.insert(definition.name.clone(), definition);
        }
    }
    Ok(definitions.into_values().collect())
}

fn validate_agent_name(name: &str) -> Result<(), DefinitionError> {
    if !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        Ok(())
    } else {
        Err(DefinitionError::UnsafeName(name.to_string()))
    }
}

pub fn built_in_agent_definition(name: &str) -> Result<AgentDefinition, DefinitionError> {
    let (content, path) = match name {
        "generalist" => (
            include_str!("../../agents/generalist.md"),
            Path::new("builtin://agents/generalist.md"),
        ),
        "architect" | "plan" => (
            include_str!("../../agents/architect.md"),
            Path::new("builtin://agents/architect.md"),
        ),
        "investigator" => (
            include_str!("../../agents/investigator.md"),
            Path::new("builtin://agents/investigator.md"),
        ),
        "implementer" | "build" => (
            include_str!("../../agents/implementer.md"),
            Path::new("builtin://agents/implementer.md"),
        ),
        "frontend-specialist" | "frontend" => (
            include_str!("../../agents/frontend-specialist.md"),
            Path::new("builtin://agents/frontend-specialist.md"),
        ),
        "backend-specialist" | "backend" => (
            include_str!("../../agents/backend-specialist.md"),
            Path::new("builtin://agents/backend-specialist.md"),
        ),
        "test-debugging-specialist" | "test" | "debug" => (
            include_str!("../../agents/test-debugging-specialist.md"),
            Path::new("builtin://agents/test-debugging-specialist.md"),
        ),
        "code-reviewer" | "review" => (
            include_str!("../../agents/code-reviewer.md"),
            Path::new("builtin://agents/code-reviewer.md"),
        ),
        "security-reviewer" | "security" => (
            include_str!("../../agents/security-reviewer.md"),
            Path::new("builtin://agents/security-reviewer.md"),
        ),
        "browser-validation-specialist" | "browser" => (
            include_str!("../../agents/browser-validation-specialist.md"),
            Path::new("builtin://agents/browser-validation-specialist.md"),
        ),
        "documentation-specialist" | "docs" => (
            include_str!("../../agents/documentation-specialist.md"),
            Path::new("builtin://agents/documentation-specialist.md"),
        ),
        "awaiter" | "monitor" => (
            include_str!("../../agents/awaiter.md"),
            Path::new("builtin://agents/awaiter.md"),
        ),
        "repository-mapper" | "repository" | "mapper" => (
            include_str!("../../agents/repository-mapper.md"),
            Path::new("builtin://agents/repository-mapper.md"),
        ),
        "performance-specialist" | "performance" => (
            include_str!("../../agents/performance-specialist.md"),
            Path::new("builtin://agents/performance-specialist.md"),
        ),
        "dependency-specialist" | "dependency" => (
            include_str!("../../agents/dependency-specialist.md"),
            Path::new("builtin://agents/dependency-specialist.md"),
        ),
        "refactoring-specialist" | "refactor" => (
            include_str!("../../agents/refactoring-specialist.md"),
            Path::new("builtin://agents/refactoring-specialist.md"),
        ),
        "integration-specialist" | "integration" => (
            include_str!("../../agents/integration-specialist.md"),
            Path::new("builtin://agents/integration-specialist.md"),
        ),
        "database-specialist" | "database" => (
            include_str!("../../agents/database-specialist.md"),
            Path::new("builtin://agents/database-specialist.md"),
        ),
        "media-specialist" | "media" => (
            include_str!("../../agents/media-specialist.md"),
            Path::new("builtin://agents/media-specialist.md"),
        ),
        "accessibility-specialist" | "accessibility" | "a11y" => (
            include_str!("../../agents/accessibility-specialist.md"),
            Path::new("builtin://agents/accessibility-specialist.md"),
        ),
        "release-specialist" | "release" => (
            include_str!("../../agents/release-specialist.md"),
            Path::new("builtin://agents/release-specialist.md"),
        ),
        _ => {
            return Err(DefinitionError::Io {
                path: PathBuf::from(format!("builtin://agents/{name}.md")),
                source: std::io::Error::new(std::io::ErrorKind::NotFound, "unknown built-in agent"),
            });
        }
    };
    parse_agent_definition(content, path)
}

pub fn built_in_agent_definitions() -> Result<Vec<AgentDefinition>, DefinitionError> {
    [
        "generalist",
        "architect",
        "investigator",
        "implementer",
        "frontend-specialist",
        "backend-specialist",
        "test-debugging-specialist",
        "code-reviewer",
        "security-reviewer",
        "browser-validation-specialist",
        "documentation-specialist",
        "awaiter",
        "repository-mapper",
        "performance-specialist",
        "dependency-specialist",
        "refactoring-specialist",
        "integration-specialist",
        "database-specialist",
        "media-specialist",
        "accessibility-specialist",
        "release-specialist",
    ]
    .into_iter()
    .map(built_in_agent_definition)
    .collect()
}

fn parse_agent_definition(content: &str, path: &Path) -> Result<AgentDefinition, DefinitionError> {
    let mut parts = content.splitn(3, "---");
    let prefix = parts.next().unwrap_or_default();
    let front_matter = parts.next();
    let body = parts.next();
    if !prefix.trim().is_empty() || front_matter.is_none() || body.is_none() {
        return Err(DefinitionError::MissingFrontMatter(path.to_path_buf()));
    }
    let front: FrontMatter =
        serde_yaml::from_str(front_matter.unwrap_or_default()).map_err(|source| {
            DefinitionError::Invalid {
                path: path.to_path_buf(),
                source,
            }
        })?;
    Ok(AgentDefinition {
        name: front.name,
        description: front.description,
        system_instructions: format!(
            "{}\n\n{}",
            body.unwrap_or_default().trim(),
            FIXED_AGENT_CONTRACT.trim()
        ),
        preferred_provider: front.provider,
        preferred_model: front.model,
        reasoning: front.reasoning,
        context_policy: front.context,
        tool_policy: front.tools,
        sandbox_policy: front.sandbox,
        workspace_mode: front.workspace_mode,
        budgets: front.budgets,
        retry_policy: front.retry,
        fallback_chain: front.fallback_chain,
        completion_schema: front.completion_schema,
        metadata: front.metadata,
    })
}

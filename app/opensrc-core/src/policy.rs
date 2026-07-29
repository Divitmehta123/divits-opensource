use crate::{Agent, SandboxPolicy, ToolPolicy, WorkspaceMode};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecision {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct ToolRequest {
    pub tool_name: String,
    pub writes_files: bool,
    pub uses_network: bool,
    pub spawns_process: bool,
    pub target_paths: Vec<String>,
    pub command: Option<String>,
    pub requires_approval: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PolicyEvaluation {
    pub decision: PolicyDecision,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct PolicyEngine;

impl PolicyEngine {
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn evaluate(agent: &Agent, request: &ToolRequest) -> PolicyEvaluation {
        let mut reasons = Vec::new();
        if denied_by_tool_policy(&agent.tool_policy, &request.tool_name) {
            reasons.push(format!(
                "tool `{}` is denied for this agent",
                request.tool_name
            ));
            return PolicyEvaluation {
                decision: PolicyDecision::Deny,
                reasons,
            };
        }
        if !allowed_by_tool_policy(&agent.tool_policy, &request.tool_name) {
            reasons.push(format!(
                "tool `{}` is not in the agent allowlist",
                request.tool_name
            ));
            return PolicyEvaluation {
                decision: PolicyDecision::Deny,
                reasons,
            };
        }
        if request.writes_files && agent.workspace.mode == WorkspaceMode::SharedReadonly {
            reasons.push("workspace is shared-readonly".to_string());
            return PolicyEvaluation {
                decision: PolicyDecision::Deny,
                reasons,
            };
        }
        if request.writes_files
            && agent.workspace.mode == WorkspaceMode::OwnedPaths
            && !request
                .target_paths
                .iter()
                .all(|path| path_is_owned(path, &agent.workspace.owned_paths))
        {
            if request
                .target_paths
                .iter()
                .any(|path| Path::new(path).is_absolute())
            {
                reasons.push("access to an external absolute path requires approval".to_string());
            } else {
                reasons.push("target path is not owned by this agent".to_string());
                return PolicyEvaluation {
                    decision: PolicyDecision::Deny,
                    reasons,
                };
            }
        }
        if request.writes_files {
            reasons.push("file mutation requires approval".to_string());
        }
        if request.requires_approval {
            reasons.push("this capability always requires explicit approval".to_string());
        }
        let sandbox_paths = if request.writes_files {
            &agent.sandbox_policy.write_paths
        } else {
            &agent.sandbox_policy.read_paths
        };
        if !sandbox_paths.is_empty()
            && !request
                .target_paths
                .iter()
                .all(|path| !Path::new(path).is_absolute() || path_is_owned(path, sandbox_paths))
        {
            reasons.push(
                "access to a path outside the current workspace requires approval".to_string(),
            );
        }
        if request.uses_network {
            let target = request.command.as_deref().unwrap_or_default();
            if !agent
                .sandbox_policy
                .network_allow
                .iter()
                .any(|pattern| network_target_matches(pattern, target))
            {
                reasons.push(format!("network access to `{target}` requires approval"));
            }
        }
        if request.spawns_process {
            let command = request.command.as_deref().unwrap_or_default();
            if agent
                .sandbox_policy
                .command_deny
                .iter()
                .any(|pattern| matches_pattern(pattern, command))
            {
                reasons.push(format!("command `{command}` is denied"));
                return PolicyEvaluation {
                    decision: PolicyDecision::Deny,
                    reasons,
                };
            }
            let allowed = agent
                .sandbox_policy
                .command_allow
                .iter()
                .chain(&agent.sandbox_policy.process_allow)
                .any(|pattern| matches_pattern(pattern, command));
            if !allowed {
                reasons.push(format!("process `{command}` requires approval"));
            }
        }
        let decision = if reasons.is_empty() {
            PolicyDecision::Allow
        } else {
            PolicyDecision::Ask
        };
        PolicyEvaluation { decision, reasons }
    }
}

fn path_is_owned(path: &str, owned_paths: &[String]) -> bool {
    let absolute = Path::new(path).is_absolute();
    let normalized = normalize_policy_path(path);
    owned_paths.iter().any(|owned| {
        let owned = normalize_policy_path(owned);
        let owned = owned.trim_end_matches('/');
        owned.is_empty()
            || (owned == "." && !absolute)
            || normalized == owned
            || normalized.starts_with(&format!("{owned}/"))
    })
}

fn normalize_policy_path(path: &str) -> String {
    let mut normalized = path.replace('\\', "/");
    if let Some(extended) = normalized.strip_prefix("//?/") {
        normalized = extended.to_string();
    }
    if let Some(unc) = normalized.strip_prefix("UNC/") {
        normalized = format!("//{unc}");
    }
    if normalized.starts_with("./") {
        normalized = normalized.trim_start_matches("./").to_string();
    }
    if cfg!(windows) {
        normalized.make_ascii_lowercase();
    }
    normalized
}

fn denied_by_tool_policy(policy: &ToolPolicy, name: &str) -> bool {
    policy
        .deny
        .iter()
        .any(|pattern| matches_pattern(pattern, name))
}

fn allowed_by_tool_policy(policy: &ToolPolicy, name: &str) -> bool {
    policy.allow.is_empty()
        || policy
            .allow
            .iter()
            .any(|pattern| matches_pattern(pattern, name))
}

fn matches_pattern(pattern: &str, name: &str) -> bool {
    pattern == "*"
        || pattern == name
        || pattern
            .strip_suffix(".*")
            .is_some_and(|p| name == p || name.starts_with(&format!("{p}.")))
}

fn network_target_matches(pattern: &str, target: &str) -> bool {
    if pattern == "*" || pattern == target {
        return true;
    }
    let host = target
        .split_once("://")
        .map_or(target, |(_, remainder)| remainder)
        .split('/')
        .next()
        .unwrap_or_default()
        .rsplit_once('@')
        .map_or_else(
            || {
                target
                    .split_once("://")
                    .map_or(target, |(_, remainder)| remainder)
                    .split('/')
                    .next()
                    .unwrap_or_default()
            },
            |(_, host)| host,
        )
        .split(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let pattern = pattern.to_ascii_lowercase();
    host == pattern
        || pattern
            .strip_prefix("*.")
            .is_some_and(|suffix| host.ends_with(&format!(".{suffix}")))
        || (pattern.ends_with('/') && target.to_ascii_lowercase().starts_with(&pattern))
}

#[allow(dead_code)]
fn _policy_is_serializable(_: &SandboxPolicy) {}

#[cfg(test)]
mod tests {
    use super::path_is_owned;

    #[test]
    fn extended_windows_root_owns_the_same_normal_path_and_children() {
        let roots = vec![r"\\?\F:\Project OpenSource\app".to_string()];

        assert!(path_is_owned(r"F:\Project OpenSource\app", &roots));
        assert!(path_is_owned(
            r"F:\Project OpenSource\app\opensrc-core",
            &roots
        ));
        assert!(!path_is_owned(r"F:\Project OpenSource\other", &roots));
    }
}

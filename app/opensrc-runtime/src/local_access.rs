use opensrc_core::{Agent, AgentDefinition, SandboxPolicy};
#[cfg(windows)]
use std::path::Path;

/// Host capability profile applied by a trusted local launcher.
///
/// Provider and model behavior remain canonical; only the host decides which
/// local roots and processes can execute without an approval round trip.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalAccessProfile {
    trusted: bool,
    roots: Vec<String>,
}

impl LocalAccessProfile {
    #[must_use]
    pub fn restricted() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn trusted_host() -> Self {
        Self {
            trusted: true,
            roots: host_roots(),
        }
    }

    #[must_use]
    pub fn is_trusted(&self) -> bool {
        self.trusted
    }

    pub fn apply_to_definition(&self, definition: &mut AgentDefinition) {
        if !self.trusted {
            return;
        }
        definition.sandbox_policy.trusted_local = true;
        merge_unique(
            &mut definition.sandbox_policy.read_paths,
            self.roots.iter().cloned(),
        );
        merge_unique(
            &mut definition.sandbox_policy.write_paths,
            self.roots.iter().cloned(),
        );
        merge_unique(
            &mut definition.sandbox_policy.command_allow,
            ["*".to_string()],
        );
        merge_unique(
            &mut definition.sandbox_policy.process_allow,
            ["*".to_string()],
        );
    }
}

/// Preserve the root runtime's trusted-local capability when a bounded child
/// definition is resolved from disk. Workspace ownership remains independently
/// enforced by the child agent's assigned paths.
pub fn inherit_trusted_local_access(parent: &Agent, definition: &mut AgentDefinition) {
    if !parent.sandbox_policy.trusted_local {
        return;
    }
    definition.sandbox_policy.trusted_local = true;
    inherit_policy_paths(&parent.sandbox_policy, &mut definition.sandbox_policy);
}

fn inherit_policy_paths(parent: &SandboxPolicy, child: &mut SandboxPolicy) {
    merge_unique(&mut child.read_paths, parent.read_paths.iter().cloned());
    merge_unique(&mut child.write_paths, parent.write_paths.iter().cloned());
    merge_unique(
        &mut child.command_allow,
        parent.command_allow.iter().cloned(),
    );
    merge_unique(
        &mut child.process_allow,
        parent.process_allow.iter().cloned(),
    );
}

fn merge_unique(target: &mut Vec<String>, values: impl IntoIterator<Item = String>) {
    for value in values {
        if !target.contains(&value) {
            target.push(value);
        }
    }
}

#[cfg(windows)]
fn host_roots() -> Vec<String> {
    ('A'..='Z')
        .map(|drive| format!("{drive}:\\"))
        .filter(|root| Path::new(root).exists())
        .collect()
}

#[cfg(not(windows))]
fn host_roots() -> Vec<String> {
    vec!["/".to_string()]
}

#[cfg(test)]
mod tests {
    use super::LocalAccessProfile;
    use crate::built_in_agent_definition;

    #[test]
    fn trusted_host_profile_enables_local_paths_and_processes() {
        let mut definition = built_in_agent_definition("generalist").expect("generalist");
        let profile = LocalAccessProfile::trusted_host();

        profile.apply_to_definition(&mut definition);

        assert!(profile.is_trusted());
        assert!(definition.sandbox_policy.trusted_local);
        assert!(!definition.sandbox_policy.read_paths.is_empty());
        assert!(!definition.sandbox_policy.write_paths.is_empty());
        assert!(
            definition
                .sandbox_policy
                .command_allow
                .contains(&"*".to_string())
        );
    }
}

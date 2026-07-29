use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CommandId {
    Help,
    New,
    Sessions,
    Resume,
    Fork,
    Rename,
    Delete,
    Export,
    Import,
    Compact,
    Undo,
    Redo,
    Quit,
    Connect,
    Disconnect,
    Providers,
    Models,
    ModelPacks,
    ModelPack,
    Reasoning,
    Mode,
    Agent,
    Agents,
    Tasks,
    Diff,
    Checkpoint,
    Terminal,
    Directories,
    AddDirectory,
    RemoveDirectory,
    Skills,
    Skill,
    Tools,
    Mcp,
    Permissions,
    Settings,
    Stats,
    Logs,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CommandDescriptor {
    pub id: CommandId,
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub summary: &'static str,
    pub usage: &'static str,
    pub category: &'static str,
}

#[must_use]
#[allow(clippy::too_many_lines)]
pub fn builtin_commands() -> Vec<CommandDescriptor> {
    vec![
        command(
            CommandId::Help,
            "/help",
            &[],
            "Show commands and keybindings",
            "/help",
            "ui",
        ),
        command(
            CommandId::New,
            "/new",
            &["/clear"],
            "Start a new conversation",
            "/new",
            "session",
        ),
        command(
            CommandId::Sessions,
            "/sessions",
            &[],
            "Browse saved conversations",
            "/sessions",
            "session",
        ),
        command(
            CommandId::Resume,
            "/resume",
            &[],
            "Resume a saved conversation",
            "/resume",
            "session",
        ),
        command(
            CommandId::Fork,
            "/fork",
            &[],
            "Fork the active conversation",
            "/fork",
            "session",
        ),
        command(
            CommandId::Rename,
            "/rename",
            &[],
            "Rename the active conversation",
            "/rename <title>",
            "session",
        ),
        command(
            CommandId::Delete,
            "/delete",
            &[],
            "Permanently delete the active conversation",
            "/delete",
            "session",
        ),
        command(
            CommandId::Export,
            "/export",
            &[],
            "Export the active conversation",
            "/export",
            "session",
        ),
        command(
            CommandId::Import,
            "/import",
            &[],
            "Import a JSON conversation",
            "/import <file>",
            "session",
        ),
        command(
            CommandId::Compact,
            "/compact",
            &[],
            "Summarize older context for future turns",
            "/compact",
            "session",
        ),
        command(
            CommandId::Undo,
            "/undo",
            &[],
            "Undo the latest safe file change",
            "/undo",
            "changes",
        ),
        command(
            CommandId::Redo,
            "/redo",
            &[],
            "Redo the latest safe file change",
            "/redo",
            "changes",
        ),
        command(
            CommandId::Quit,
            "/quit",
            &[],
            "Exit the application",
            "/quit",
            "ui",
        ),
        command(
            CommandId::Connect,
            "/connect",
            &[],
            "Connect a provider",
            "/connect",
            "provider",
        ),
        command(
            CommandId::Disconnect,
            "/disconnect",
            &[],
            "Remove a configured provider",
            "/disconnect [provider]",
            "provider",
        ),
        command(
            CommandId::Providers,
            "/providers",
            &[],
            "Select a provider",
            "/providers",
            "provider",
        ),
        command(
            CommandId::Models,
            "/models",
            &["/model"],
            "Select a model",
            "/models",
            "provider",
        ),
        command(
            CommandId::ModelPacks,
            "/packs",
            &[],
            "Browse cost-aware multi-model packs",
            "/packs",
            "provider",
        ),
        command(
            CommandId::ModelPack,
            "/pack",
            &[],
            "Select a multi-model pack or return to one model",
            "/pack <name|off>",
            "provider",
        ),
        command(
            CommandId::Reasoning,
            "/reasoning",
            &["/variant"],
            "Set the reasoning level or variant",
            "/reasoning <level>",
            "provider",
        ),
        command(
            CommandId::Mode,
            "/mode",
            &[],
            "Select auto, direct, focused, or agentic execution",
            "/mode <auto|direct|focused|agentic>",
            "agent",
        ),
        command(
            CommandId::Agent,
            "/agent",
            &[],
            "Select an agent role",
            "/agent",
            "agent",
        ),
        command(
            CommandId::Agents,
            "/agents",
            &[],
            "Open running agents",
            "/agents",
            "agent",
        ),
        command(
            CommandId::Tasks,
            "/tasks",
            &[],
            "Open task progress",
            "/tasks",
            "agent",
        ),
        command(
            CommandId::Diff,
            "/diff",
            &["/changes"],
            "Open tracked file changes",
            "/diff",
            "changes",
        ),
        command(
            CommandId::Checkpoint,
            "/checkpoint",
            &[],
            "Capture the current reversible change boundary",
            "/checkpoint [label]",
            "changes",
        ),
        command(
            CommandId::Terminal,
            "/terminal",
            &["/test"],
            "Open process and test output",
            "/terminal",
            "execution",
        ),
        command(
            CommandId::Directories,
            "/dirs",
            &["/directories"],
            "Show directories available to the agent",
            "/dirs",
            "filesystem",
        ),
        command(
            CommandId::AddDirectory,
            "/add-dir",
            &[],
            "Grant persistent access to a local directory",
            "/add-dir <directory>",
            "filesystem",
        ),
        command(
            CommandId::RemoveDirectory,
            "/remove-dir",
            &[],
            "Revoke access to a local directory",
            "/remove-dir <directory>",
            "filesystem",
        ),
        command(
            CommandId::Skills,
            "/skills",
            &[],
            "Browse available skills",
            "/skills",
            "capability",
        ),
        command(
            CommandId::Skill,
            "/skill",
            &[],
            "Activate a skill for the next prompt",
            "/skill <name>",
            "capability",
        ),
        command(
            CommandId::Tools,
            "/tools",
            &[],
            "Browse dynamically exposed tools",
            "/tools",
            "capability",
        ),
        command(
            CommandId::Mcp,
            "/mcp",
            &[],
            "Open configured MCP servers",
            "/mcp",
            "capability",
        ),
        command(
            CommandId::Permissions,
            "/permissions",
            &[],
            "Inspect persistent permission rules",
            "/permissions",
            "safety",
        ),
        command(
            CommandId::Settings,
            "/settings",
            &["/config"],
            "Open models, permissions, directories, and capabilities",
            "/settings",
            "ui",
        ),
        command(
            CommandId::Stats,
            "/stats",
            &["/cost", "/tokens"],
            "Open usage and timing metrics",
            "/stats",
            "diagnostics",
        ),
        command(
            CommandId::Logs,
            "/logs",
            &[],
            "Open the event ledger",
            "/logs",
            "diagnostics",
        ),
    ]
}

#[must_use]
pub fn resolve_command(name: &str) -> Option<CommandDescriptor> {
    builtin_commands()
        .into_iter()
        .find(|command| command.name == name || command.aliases.contains(&name))
}

#[must_use]
pub fn command_names() -> Vec<&'static str> {
    let mut names = Vec::new();
    for command in builtin_commands() {
        names.push(command.name);
        names.extend(command.aliases);
    }
    names
}

const fn command(
    id: CommandId,
    name: &'static str,
    aliases: &'static [&'static str],
    summary: &'static str,
    usage: &'static str,
    category: &'static str,
) -> CommandDescriptor {
    CommandDescriptor {
        id,
        name,
        aliases,
        summary,
        usage,
        category,
    }
}

#[cfg(test)]
mod tests {
    use super::{CommandId, command_names, resolve_command};

    #[test]
    fn resolves_canonical_commands_and_aliases_without_duplicates() {
        assert_eq!(
            resolve_command("/changes").map(|command| command.id),
            Some(CommandId::Diff)
        );
        let mut names = command_names();
        let original = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), original);
    }
}

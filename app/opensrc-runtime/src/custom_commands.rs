use opensrc_core::ExecutionMode;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;
use walkdir::WalkDir;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct CommandFrontMatter {
    name: Option<String>,
    description: String,
    allowed_tools: Vec<String>,
    agent: Option<String>,
    model: Option<String>,
    mode: Option<ExecutionMode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CustomCommand {
    pub name: String,
    pub description: String,
    pub template: String,
    pub allowed_tools: Vec<String>,
    pub preferred_agent: Option<String>,
    pub preferred_model: Option<String>,
    pub preferred_mode: Option<ExecutionMode>,
    pub source: PathBuf,
}

#[derive(Debug, Error)]
pub enum CustomCommandError {
    #[error("failed to read custom command `{path}`: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid front matter in custom command `{path}`: {message}")]
    FrontMatter { path: PathBuf, message: String },
    #[error("custom command name `{0}` is invalid")]
    InvalidName(String),
    #[error("custom command invocation is empty")]
    EmptyInvocation,
    #[error("custom command arguments are invalid: {0}")]
    Arguments(String),
    #[error("custom command `{0}` requires positional argument ${1}")]
    MissingPositional(String, usize),
    #[error("custom command `{0}` requires named argument `--{1}`")]
    MissingNamed(String, String),
}

#[must_use]
pub fn discover_custom_commands(roots: &[PathBuf]) -> Vec<CustomCommand> {
    let mut commands = BTreeMap::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        for entry in WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry.file_type().is_file()
                    && entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
            })
        {
            if let Ok(command) = read_custom_command(root, entry.path()) {
                commands.insert(command.name.clone(), command);
            }
        }
    }
    commands.into_values().collect()
}

pub fn validate_custom_command(path: &Path) -> Result<CustomCommand, CustomCommandError> {
    let root = path.parent().unwrap_or_else(|| Path::new("."));
    read_custom_command(root, path)
}

pub fn expand_custom_command(
    command: &CustomCommand,
    invocation: &str,
) -> Result<String, CustomCommandError> {
    let words = shell_words::split(invocation)
        .map_err(|error| CustomCommandError::Arguments(error.to_string()))?;
    if words.is_empty() {
        return Err(CustomCommandError::EmptyInvocation);
    }
    let mut positional = Vec::new();
    let mut named = BTreeMap::new();
    let mut index = 1;
    while index < words.len() {
        let word = &words[index];
        if let Some(argument) = word.strip_prefix("--") {
            if let Some((name, value)) = argument.split_once('=') {
                named.insert(name.to_string(), value.to_string());
            } else {
                let value = words.get(index + 1).ok_or_else(|| {
                    CustomCommandError::Arguments(format!("`--{argument}` requires a value"))
                })?;
                named.insert(argument.to_string(), value.clone());
                index += 1;
            }
        } else {
            positional.push(word.clone());
        }
        index += 1;
    }

    let mut expanded = command.template.clone();
    expanded = expanded.replace("$ARGUMENTS", &positional.join(" "));
    for position in 1..=32 {
        let placeholder = format!("${position}");
        if expanded.contains(&placeholder) {
            let value = positional.get(position - 1).ok_or_else(|| {
                CustomCommandError::MissingPositional(command.name.clone(), position)
            })?;
            expanded = expanded.replace(&placeholder, value);
        }
    }
    for placeholder in named_placeholders(&expanded) {
        let value = named.get(&placeholder).ok_or_else(|| {
            CustomCommandError::MissingNamed(command.name.clone(), placeholder.clone())
        })?;
        expanded = expanded.replace(&format!("{{{{{placeholder}}}}}"), value);
    }
    Ok(expanded)
}

fn read_custom_command(root: &Path, path: &Path) -> Result<CustomCommand, CustomCommandError> {
    let document = std::fs::read_to_string(path).map_err(|source| CustomCommandError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let (metadata, template) = parse_document(path, &document)?;
    let inferred = infer_name(root, path)?;
    let name = metadata.name.unwrap_or(inferred);
    let name = normalize_name(&name)?;
    Ok(CustomCommand {
        name,
        description: if metadata.description.trim().is_empty() {
            first_summary_line(template)
        } else {
            metadata.description
        },
        template: template.trim().to_string(),
        allowed_tools: metadata.allowed_tools,
        preferred_agent: metadata.agent,
        preferred_model: metadata.model,
        preferred_mode: metadata.mode,
        source: path.to_path_buf(),
    })
}

fn parse_document<'a>(
    path: &Path,
    document: &'a str,
) -> Result<(CommandFrontMatter, &'a str), CustomCommandError> {
    if let Some(rest) = document.strip_prefix("---\n")
        && let Some((front_matter, body)) = rest.split_once("\n---\n")
    {
        let metadata = serde_yaml::from_str(front_matter).map_err(|error| {
            CustomCommandError::FrontMatter {
                path: path.to_path_buf(),
                message: error.to_string(),
            }
        })?;
        return Ok((metadata, body));
    }
    if let Some(rest) = document.strip_prefix("+++\n")
        && let Some((front_matter, body)) = rest.split_once("\n+++\n")
    {
        let metadata =
            toml::from_str(front_matter).map_err(|error| CustomCommandError::FrontMatter {
                path: path.to_path_buf(),
                message: error.to_string(),
            })?;
        return Ok((metadata, body));
    }
    Ok((CommandFrontMatter::default(), document))
}

fn infer_name(root: &Path, path: &Path) -> Result<String, CustomCommandError> {
    let relative = path.strip_prefix(root).unwrap_or(path);
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(CustomCommandError::InvalidName(
            relative.display().to_string(),
        ));
    }
    let mut components = relative
        .with_extension("")
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>();
    components.retain(|component| !component.is_empty());
    Ok(format!("/{}", components.join("/")))
}

fn normalize_name(name: &str) -> Result<String, CustomCommandError> {
    let name = format!("/{}", name.trim().trim_start_matches('/'));
    let valid = name.len() > 1
        && name.split('/').skip(1).all(|component| {
            !component.is_empty()
                && component
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || "-_".contains(character))
        });
    if valid {
        Ok(name)
    } else {
        Err(CustomCommandError::InvalidName(name))
    }
}

fn first_summary_line(template: &str) -> String {
    template
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("Custom prompt command")
        .trim_start_matches('#')
        .trim()
        .chars()
        .take(120)
        .collect()
}

fn named_placeholders(template: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            break;
        };
        let name = &after[..end];
        if !name.is_empty()
            && name
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "-_".contains(character))
            && !names.iter().any(|value| value == name)
        {
            names.push(name.to_string());
        }
        rest = &after[end + 2..];
    }
    names
}

#[cfg(test)]
mod tests {
    use super::{discover_custom_commands, expand_custom_command, validate_custom_command};
    use opensrc_core::ExecutionMode;
    use uuid::Uuid;

    #[test]
    fn discovers_namespaced_yaml_and_toml_commands_and_expands_arguments() {
        let root = std::env::temp_dir().join(format!("opensrc-commands-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("review")).expect("command directory");
        let yaml = root.join("review").join("security.md");
        std::fs::write(
            &yaml,
            "---\ndescription: Review one file\nagent: security-review\nmode: agentic\nallowed_tools: [fs.read, search.text]\n---\nReview $1 for {{focus}}.",
        )
        .expect("yaml command");
        let toml = root.join("explain.md");
        std::fs::write(
            &toml,
            "+++\ndescription = \"Explain input\"\nmodel = \"openai/gpt-test\"\n+++\nExplain $ARGUMENTS.",
        )
        .expect("toml command");

        let commands = discover_custom_commands(std::slice::from_ref(&root));
        assert_eq!(commands.len(), 2);
        let security = commands
            .iter()
            .find(|command| command.name == "/review/security")
            .expect("security");
        assert_eq!(security.preferred_mode, Some(ExecutionMode::Agentic));
        assert_eq!(
            expand_custom_command(
                security,
                "/review/security src/lib.rs --focus \"unsafe boundaries\""
            )
            .expect("expand"),
            "Review src/lib.rs for unsafe boundaries."
        );
        assert!(validate_custom_command(&toml).is_ok());
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}

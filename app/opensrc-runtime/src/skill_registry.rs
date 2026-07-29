use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub triggers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActivatedSkill {
    pub metadata: SkillMetadata,
    pub instructions: String,
    pub source_path: String,
    pub resources: Vec<String>,
}

#[derive(Debug, Clone)]
struct SkillEntry {
    metadata: SkillMetadata,
    source: SkillSource,
}

#[derive(Debug, Clone)]
enum SkillSource {
    File(PathBuf),
    BuiltIn {
        display_path: PathBuf,
        content: &'static str,
    },
}

#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    entries: BTreeMap<String, SkillEntry>,
    roots: Vec<PathBuf>,
}

#[derive(Debug, Error)]
pub enum SkillError {
    #[error("failed to read skill {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("skill {0} has invalid or missing YAML front matter")]
    InvalidFrontMatter(PathBuf),
    #[error("invalid skill metadata in {path}: {source}")]
    InvalidMetadata {
        path: PathBuf,
        source: serde_yaml::Error,
    },
    #[error("skill `{0}` is not registered")]
    Unknown(String),
    #[error("skill name `{0}` is duplicated")]
    Duplicate(String),
}

impl SkillRegistry {
    pub fn discover(directory: impl AsRef<Path>) -> Result<Self, SkillError> {
        let directory = directory.as_ref();
        if !directory.exists() {
            return Ok(Self {
                entries: BTreeMap::new(),
                roots: vec![directory.to_path_buf()],
            });
        }
        let entries = std::fs::read_dir(directory).map_err(|source| SkillError::Io {
            path: directory.to_path_buf(),
            source,
        })?;
        let mut registry = Self {
            entries: BTreeMap::new(),
            roots: vec![directory.to_path_buf()],
        };
        let mut paths = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter_map(|path| {
                if path.is_dir() {
                    let skill = path.join("SKILL.md");
                    skill.is_file().then_some(skill)
                } else if path.extension().is_some_and(|extension| extension == "md") {
                    Some(path)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        paths.sort();
        for path in paths {
            let metadata = read_metadata(&path)?;
            if registry
                .entries
                .insert(
                    metadata.name.clone(),
                    SkillEntry {
                        metadata: metadata.clone(),
                        source: SkillSource::File(path),
                    },
                )
                .is_some()
            {
                return Err(SkillError::Duplicate(metadata.name));
            }
        }
        Ok(registry)
    }

    pub fn discover_with_builtins(directory: impl AsRef<Path>) -> Result<Self, SkillError> {
        Self::discover_many_with_builtins([directory.as_ref()])
    }

    pub fn discover_many_with_builtins<I, P>(directories: I) -> Result<Self, SkillError>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut registry = Self::builtins()?;
        for directory in directories {
            let discovered = Self::discover(directory)?;
            registry.roots.extend(discovered.roots);
            for (name, entry) in discovered.entries {
                registry.entries.insert(name, entry);
            }
        }
        registry.roots.sort();
        registry.roots.dedup();
        Ok(registry)
    }

    pub fn builtins() -> Result<Self, SkillError> {
        let mut registry = Self::default();
        for (filename, content) in [
            (
                "focused-validation.md",
                include_str!("../../skills/focused-validation.md"),
            ),
            (
                "repository-map.md",
                include_str!("../../skills/repository-map.md"),
            ),
            (
                "security-review.md",
                include_str!("../../skills/security-review.md"),
            ),
        ] {
            let path = PathBuf::from(format!("builtin://skills/{filename}"));
            let metadata = metadata_from_content(content, &path)?;
            registry.entries.insert(
                metadata.name.clone(),
                SkillEntry {
                    metadata,
                    source: SkillSource::BuiltIn {
                        display_path: path,
                        content,
                    },
                },
            );
        }
        Ok(registry)
    }

    #[must_use]
    pub fn metadata(&self) -> Vec<SkillMetadata> {
        self.fresh_entries()
            .values()
            .map(|entry| entry.metadata.clone())
            .collect()
    }

    pub fn activate(&self, name: &str) -> Result<ActivatedSkill, SkillError> {
        let entries = self.fresh_entries();
        let entry = entries
            .get(name)
            .ok_or_else(|| SkillError::Unknown(name.to_string()))?;
        let (content, path) = match &entry.source {
            SkillSource::File(path) => (
                std::fs::read_to_string(path).map_err(|source| SkillError::Io {
                    path: path.clone(),
                    source,
                })?,
                path.clone(),
            ),
            SkillSource::BuiltIn {
                display_path,
                content,
            } => ((*content).to_string(), display_path.clone()),
        };
        let (_, instructions) = split_front_matter(&content, &path)?;
        let resources = skill_resources(&path);
        Ok(ActivatedSkill {
            metadata: entry.metadata.clone(),
            instructions: instructions.trim().to_string(),
            source_path: path.to_string_lossy().into_owned(),
            resources,
        })
    }

    #[must_use]
    pub fn matching_triggers(&self, request: &str) -> Vec<String> {
        let request = request.to_ascii_lowercase();
        self.fresh_entries()
            .values()
            .filter(|entry| {
                entry.metadata.triggers.iter().any(|trigger| {
                    let trigger = trigger.trim().to_ascii_lowercase();
                    !trigger.is_empty() && request.contains(&trigger)
                })
            })
            .map(|entry| entry.metadata.name.clone())
            .collect()
    }

    pub fn inspect(path: impl AsRef<Path>) -> Result<SkillMetadata, SkillError> {
        read_metadata(path.as_ref())
    }

    fn fresh_entries(&self) -> BTreeMap<String, SkillEntry> {
        let mut entries = self.entries.clone();
        for root in &self.roots {
            let Ok(discovered) = Self::discover(root) else {
                continue;
            };
            for (name, entry) in discovered.entries {
                entries.insert(name, entry);
            }
        }
        entries
    }
}

fn skill_resources(path: &Path) -> Vec<String> {
    let Some(directory) = path.parent() else {
        return Vec::new();
    };
    let mut resources = walkdir::WalkDir::new(directory)
        .max_depth(3)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file() && entry.path() != path)
        .filter_map(|entry| {
            entry
                .path()
                .strip_prefix(directory)
                .ok()
                .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        })
        .collect::<Vec<_>>();
    resources.sort();
    resources
}

fn read_metadata(path: &Path) -> Result<SkillMetadata, SkillError> {
    let file = std::fs::File::open(path).map_err(|source| SkillError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|source| SkillError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if line.trim() != "---" {
        return Err(SkillError::InvalidFrontMatter(path.to_path_buf()));
    }
    let mut front_matter = String::new();
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|source| SkillError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        if read == 0 {
            return Err(SkillError::InvalidFrontMatter(path.to_path_buf()));
        }
        if line.trim() == "---" {
            break;
        }
        front_matter.push_str(&line);
    }
    serde_yaml::from_str(&front_matter).map_err(|source| SkillError::InvalidMetadata {
        path: path.to_path_buf(),
        source,
    })
}

fn metadata_from_content(content: &str, path: &Path) -> Result<SkillMetadata, SkillError> {
    let (front_matter, _) = split_front_matter(content, path)?;
    serde_yaml::from_str(front_matter).map_err(|source| SkillError::InvalidMetadata {
        path: path.to_path_buf(),
        source,
    })
}

fn split_front_matter<'a>(content: &'a str, path: &Path) -> Result<(&'a str, &'a str), SkillError> {
    let mut parts = content.splitn(3, "---");
    if !parts.next().unwrap_or_default().trim().is_empty() {
        return Err(SkillError::InvalidFrontMatter(path.to_path_buf()));
    }
    let metadata = parts
        .next()
        .ok_or_else(|| SkillError::InvalidFrontMatter(path.to_path_buf()))?;
    let body = parts
        .next()
        .ok_or_else(|| SkillError::InvalidFrontMatter(path.to_path_buf()))?;
    Ok((metadata, body))
}

#[cfg(test)]
mod tests {
    use super::SkillRegistry;
    use uuid::Uuid;

    #[test]
    fn exposes_metadata_then_activates_instructions() {
        let root = std::env::temp_dir().join(format!("opensrc-skills-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("skill directory");
        std::fs::write(
            root.join("review.md"),
            "---\nname: review\ndescription: Review changes.\ntriggers: [review]\n---\nRead the diff.",
        )
        .expect("skill");
        let registry = SkillRegistry::discover(&root).expect("registry");
        assert_eq!(registry.metadata()[0].description, "Review changes.");
        assert_eq!(
            registry
                .activate("review")
                .expect("activation")
                .instructions,
            "Read the diff."
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn discovers_directory_skills_resources_and_trigger_matches() {
        let root = std::env::temp_dir().join(format!("opensrc-dir-skill-{}", Uuid::new_v4()));
        let skill = root.join("rust-review");
        std::fs::create_dir_all(skill.join("references")).expect("skill directories");
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: rust-review\ndescription: Review Rust code.\ntriggers: [review rust]\n---\nInspect ownership and error paths.",
        )
        .expect("skill");
        std::fs::write(skill.join("references").join("checklist.md"), "Checklist")
            .expect("resource");
        let registry = SkillRegistry::discover(&root).expect("registry");
        assert_eq!(
            registry.matching_triggers("Please REVIEW RUST before merging"),
            vec!["rust-review".to_string()]
        );
        let activated = registry.activate("rust-review").expect("activation");
        assert_eq!(
            activated.resources,
            vec!["references/checklist.md".to_string()]
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn discovers_skills_added_after_startup_without_restart() {
        let root = std::env::temp_dir().join(format!("opensrc-live-skill-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("skill root");
        let registry = SkillRegistry::discover(&root).expect("registry");
        assert!(registry.metadata().is_empty());

        let skill = root.join("live-install");
        std::fs::create_dir_all(&skill).expect("skill directory");
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: live-install\ndescription: Installed live.\ntriggers: [live]\n---\nContinue immediately.",
        )
        .expect("skill");

        assert_eq!(registry.metadata()[0].name, "live-install");
        assert_eq!(
            registry
                .activate("live-install")
                .expect("live activation")
                .instructions,
            "Continue immediately."
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}

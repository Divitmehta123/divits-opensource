use diffy::Patch;
use opensrc_core::{Checkpoint, CheckpointId, FileChange, FileChangeId, FileChangeState};
use opensrc_store::{Store, StoreError};
use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ChangeError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("change `{0}` has no reversible text patch")]
    NoPatch(FileChangeId),
    #[error("workspace-relative path is unsafe: {0}")]
    UnsafePath(String),
    #[error("file changed after the recorded edit: expected {expected}, found {actual}")]
    ConcurrentModification { expected: String, actual: String },
    #[error("patch could not be applied: {0}")]
    Patch(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Clone)]
pub struct ChangeManager {
    store: Store,
}

#[derive(Debug, serde::Serialize)]
pub struct CheckpointRestore {
    pub checkpoint: Checkpoint,
    pub undone: Vec<FileChangeId>,
}

impl ChangeManager {
    #[must_use]
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    #[allow(clippy::similar_names)]
    pub fn undo(&self, id: FileChangeId) -> Result<FileChange, ChangeError> {
        let change = self.store.get_file_change(id)?;
        if change.state != FileChangeState::Applied {
            return Err(StoreError::InvalidFileChangeState {
                id,
                from: change.state,
                to: FileChangeState::Undone,
            }
            .into());
        }
        let patch = parse_patch(&change)?;
        let path = safe_change_path(&change)?;
        let current = if let Some(expected) = &change.postimage_hash {
            let value = std::fs::read_to_string(&path)?;
            verify_hash(&value, expected)?;
            value
        } else {
            verify_absent(&path)?;
            String::new()
        };
        let reverted = diffy::apply(&current, &patch.reverse())
            .map_err(|error| ChangeError::Patch(error.to_string()))?;
        if let Some(expected) = &change.preimage_hash {
            verify_hash(&reverted, expected)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, reverted)?;
        } else {
            if !reverted.is_empty() {
                return Err(ChangeError::Patch(
                    "reverse patch for a newly created file was not empty".to_string(),
                ));
            }
            std::fs::remove_file(&path)?;
        }
        self.store
            .transition_file_change(id, FileChangeState::Applied, FileChangeState::Undone)
            .map_err(Into::into)
    }

    #[allow(clippy::similar_names)]
    pub fn redo(&self, id: FileChangeId) -> Result<FileChange, ChangeError> {
        let change = self.store.get_file_change(id)?;
        if change.state != FileChangeState::Undone {
            return Err(StoreError::InvalidFileChangeState {
                id,
                from: change.state,
                to: FileChangeState::Applied,
            }
            .into());
        }
        let patch = parse_patch(&change)?;
        let path = safe_change_path(&change)?;
        let original = if let Some(expected) = &change.preimage_hash {
            let value = std::fs::read_to_string(&path)?;
            verify_hash(&value, expected)?;
            value
        } else {
            if path.exists() {
                return Err(ChangeError::ConcurrentModification {
                    expected: "file absent".to_string(),
                    actual: sha256(&std::fs::read(&path)?),
                });
            }
            String::new()
        };
        let changed = diffy::apply(&original, &patch)
            .map_err(|error| ChangeError::Patch(error.to_string()))?;
        if let Some(expected) = &change.postimage_hash {
            verify_hash(&changed, expected)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, changed)?;
        } else {
            if !changed.is_empty() {
                return Err(ChangeError::Patch(
                    "forward patch for a deleted file was not empty".to_string(),
                ));
            }
            std::fs::remove_file(&path)?;
        }
        self.store
            .transition_file_change(id, FileChangeState::Undone, FileChangeState::Applied)
            .map_err(Into::into)
    }

    pub fn restore_checkpoint(&self, id: CheckpointId) -> Result<CheckpointRestore, ChangeError> {
        let checkpoint = self.store.get_checkpoint(id)?;
        let captured = checkpoint
            .captured_change_ids
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        let pending = self
            .store
            .list_file_changes(Some(checkpoint.run_id))?
            .into_iter()
            .filter(|change| {
                change.state == FileChangeState::Applied && !captured.contains(&change.id)
            })
            .collect::<Vec<_>>();
        let mut undone = Vec::with_capacity(pending.len());
        for change in pending {
            self.undo(change.id)?;
            undone.push(change.id);
        }
        Ok(CheckpointRestore { checkpoint, undone })
    }
}

fn parse_patch(change: &FileChange) -> Result<Patch<'_, str>, ChangeError> {
    let value = change
        .patch
        .as_deref()
        .ok_or(ChangeError::NoPatch(change.id))?;
    Patch::from_str(value).map_err(|error| ChangeError::Patch(error.to_string()))
}

fn safe_change_path(change: &FileChange) -> Result<PathBuf, ChangeError> {
    let relative = Path::new(&change.relative_path);
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(ChangeError::UnsafePath(change.relative_path.clone()));
    }
    let root = std::fs::canonicalize(&change.workspace_path)?;
    let path = root.join(relative);
    if path.exists() {
        let canonical = std::fs::canonicalize(&path)?;
        if !canonical.starts_with(&root) {
            return Err(ChangeError::UnsafePath(change.relative_path.clone()));
        }
        Ok(canonical)
    } else {
        let mut ancestor = path.parent().unwrap_or(&root);
        while !ancestor.exists() {
            ancestor = ancestor
                .parent()
                .ok_or_else(|| ChangeError::UnsafePath(change.relative_path.clone()))?;
        }
        let canonical_ancestor = std::fs::canonicalize(ancestor)?;
        if !canonical_ancestor.starts_with(&root) {
            return Err(ChangeError::UnsafePath(change.relative_path.clone()));
        }
        Ok(path)
    }
}

fn verify_absent(path: &Path) -> Result<(), ChangeError> {
    if path.exists() {
        return Err(ChangeError::ConcurrentModification {
            expected: "file absent".to_string(),
            actual: sha256(&std::fs::read(path)?),
        });
    }
    Ok(())
}

fn verify_hash(value: &str, expected: &str) -> Result<(), ChangeError> {
    let actual = sha256(value.as_bytes());
    if actual == expected {
        Ok(())
    } else {
        Err(ChangeError::ConcurrentModification {
            expected: expected.to_string(),
            actual,
        })
    }
}

fn sha256(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

#[cfg(test)]
mod tests {
    use super::ChangeManager;
    use crate::{AgentControl, AgentLimits};
    use opensrc_core::{
        AgentDefinition, Budgets, ContextPolicy, ExecutionMode, ReasoningConfig, RetryPolicy,
        SandboxPolicy, ToolPolicy, WorkspaceMode,
    };
    use opensrc_store::Store;
    use std::collections::BTreeMap;
    use std::path::Path;
    use uuid::Uuid;

    fn create_run_and_agent(store: &Store, workspace: &Path) -> (Uuid, Uuid) {
        let workspace = workspace.to_string_lossy().into_owned();
        let conversation = store
            .create_conversation(workspace.clone(), None)
            .expect("conversation");
        let run = store
            .create_run(conversation.id, "mutate files", ExecutionMode::Direct)
            .expect("run");
        let definition = AgentDefinition {
            name: "editor".to_string(),
            description: "Edits files".to_string(),
            system_instructions: "Edit carefully.".to_string(),
            preferred_provider: None,
            preferred_model: None,
            reasoning: ReasoningConfig::default(),
            context_policy: ContextPolicy::default(),
            tool_policy: ToolPolicy::default(),
            sandbox_policy: SandboxPolicy::default(),
            workspace_mode: WorkspaceMode::OwnedPaths,
            budgets: Budgets::default(),
            retry_policy: RetryPolicy::default(),
            fallback_chain: Vec::new(),
            completion_schema: "task_completion".to_string(),
            metadata: BTreeMap::new(),
        };
        let agent = AgentControl::new(store.clone(), AgentLimits::default())
            .create_root(run.id, &definition, "mutate", workspace)
            .expect("agent");
        (run.id, agent.id)
    }

    #[test]
    #[allow(clippy::similar_names)]
    fn restores_and_reapplies_a_deleted_file() {
        let workspace = std::env::temp_dir().join(format!("opensrc-delete-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).expect("workspace");
        let path = workspace.join("sample.txt");
        let original = "before\n";
        std::fs::write(&path, original).expect("fixture");
        let store = Store::in_memory().expect("store");
        let (run_id, agent_id) = create_run_and_agent(&store, &workspace);
        let checkpoint = store
            .create_checkpoint(run_id, Some(agent_id), None, "before delete")
            .expect("checkpoint");
        let patch = diffy::create_patch(original, "").to_string();
        std::fs::remove_file(&path).expect("delete");
        let change = store
            .record_file_change(
                run_id,
                agent_id,
                None,
                &workspace.to_string_lossy(),
                "sample.txt",
                Some(&super::sha256(original.as_bytes())),
                None,
                Some(&patch),
            )
            .expect("record");
        let manager = ChangeManager::new(store);
        manager.undo(change.id).expect("undo delete");
        assert_eq!(std::fs::read_to_string(&path).expect("restored"), original);
        manager.redo(change.id).expect("redo delete");
        assert!(!path.exists());
        let restored = manager
            .restore_checkpoint(checkpoint.id)
            .expect("restore checkpoint");
        assert_eq!(restored.undone, vec![change.id]);
        assert_eq!(std::fs::read_to_string(&path).expect("restored"), original);
        std::fs::remove_dir_all(workspace).expect("cleanup");
    }

    #[test]
    fn reverses_and_reapplies_both_sides_of_a_move() {
        let workspace = std::env::temp_dir().join(format!("opensrc-move-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).expect("workspace");
        let source = workspace.join("source.txt");
        let destination = workspace.join("destination.txt");
        let content = "move me\n";
        std::fs::write(&source, content).expect("fixture");
        let store = Store::in_memory().expect("store");
        let (run_id, agent_id) = create_run_and_agent(&store, &workspace);
        std::fs::rename(&source, &destination).expect("move");
        let content_hash = super::sha256(content.as_bytes());
        let destination_change = store
            .record_file_change(
                run_id,
                agent_id,
                None,
                &workspace.to_string_lossy(),
                "destination.txt",
                None,
                Some(&content_hash),
                Some(&diffy::create_patch("", content).to_string()),
            )
            .expect("record destination");
        let source_change = store
            .record_file_change(
                run_id,
                agent_id,
                None,
                &workspace.to_string_lossy(),
                "source.txt",
                Some(&content_hash),
                None,
                Some(&diffy::create_patch(content, "").to_string()),
            )
            .expect("record source");
        let manager = ChangeManager::new(store);
        manager.undo(source_change.id).expect("restore source");
        manager
            .undo(destination_change.id)
            .expect("remove destination");
        assert_eq!(std::fs::read_to_string(&source).expect("source"), content);
        assert!(!destination.exists());
        manager
            .redo(destination_change.id)
            .expect("restore destination");
        manager.redo(source_change.id).expect("remove source");
        assert!(!source.exists());
        assert_eq!(
            std::fs::read_to_string(&destination).expect("destination"),
            content
        );
        std::fs::remove_dir_all(workspace).expect("cleanup");
    }
}

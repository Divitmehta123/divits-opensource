use opensrc_core::{Task, TaskId, TaskStatus};
use std::collections::{HashMap, HashSet};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TaskGraphError {
    #[error("task {task} depends on missing task {dependency}")]
    MissingDependency { task: TaskId, dependency: TaskId },
    #[error("task graph contains a cycle")]
    Cycle,
}

pub fn validate_task_graph(tasks: &[Task]) -> Result<(), TaskGraphError> {
    let ids: HashSet<_> = tasks.iter().map(|task| task.id).collect();
    for task in tasks {
        for dependency in &task.dependencies {
            if !ids.contains(dependency) {
                return Err(TaskGraphError::MissingDependency {
                    task: task.id,
                    dependency: *dependency,
                });
            }
        }
    }

    let dependencies: HashMap<_, _> = tasks
        .iter()
        .map(|task| (task.id, task.dependencies.as_slice()))
        .collect();
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    for task in tasks {
        visit(task.id, &dependencies, &mut visiting, &mut visited)?;
    }
    Ok(())
}

fn visit(
    id: TaskId,
    dependencies: &HashMap<TaskId, &[TaskId]>,
    visiting: &mut HashSet<TaskId>,
    visited: &mut HashSet<TaskId>,
) -> Result<(), TaskGraphError> {
    if visited.contains(&id) {
        return Ok(());
    }
    if !visiting.insert(id) {
        return Err(TaskGraphError::Cycle);
    }
    if let Some(children) = dependencies.get(&id) {
        for dependency in *children {
            visit(*dependency, dependencies, visiting, visited)?;
        }
    }
    visiting.remove(&id);
    visited.insert(id);
    Ok(())
}

#[must_use]
pub fn ready_tasks(tasks: &[Task]) -> Vec<TaskId> {
    let completed: HashSet<_> = tasks
        .iter()
        .filter(|task| task.status == TaskStatus::Completed)
        .map(|task| task.id)
        .collect();
    tasks
        .iter()
        .filter(|task| {
            matches!(task.status, TaskStatus::Created | TaskStatus::Ready)
                && task
                    .dependencies
                    .iter()
                    .all(|dependency| completed.contains(dependency))
        })
        .map(|task| task.id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{TaskGraphError, validate_task_graph};
    use chrono::Utc;
    use opensrc_core::{RetryPolicy, Task, TaskContract, TaskStatus};
    use uuid::Uuid;

    fn task(id: Uuid, dependencies: Vec<Uuid>) -> Task {
        let now = Utc::now();
        Task {
            id,
            run_id: Uuid::new_v4(),
            description: String::new(),
            dependencies,
            assigned_agent: None,
            status: TaskStatus::Created,
            priority: 0,
            expected_output: String::new(),
            contract: TaskContract::default(),
            workspace_ownership: Vec::new(),
            allowed_tools: Vec::new(),
            retry_policy: RetryPolicy::default(),
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn detects_cycles() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let tasks = [task(first, vec![second]), task(second, vec![first])];
        assert_eq!(validate_task_graph(&tasks), Err(TaskGraphError::Cycle));
    }
}

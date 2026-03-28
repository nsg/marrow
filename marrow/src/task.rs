use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Healing,
    Escalated,
}

impl TaskStatus {
    pub fn can_transition_to(&self, next: TaskStatus) -> bool {
        matches!(
            (self, next),
            (TaskStatus::Pending, TaskStatus::Running)
                | (TaskStatus::Running, TaskStatus::Succeeded)
                | (TaskStatus::Running, TaskStatus::Failed)
                | (TaskStatus::Failed, TaskStatus::Healing)
                | (TaskStatus::Healing, TaskStatus::Succeeded)
                | (TaskStatus::Healing, TaskStatus::Escalated)
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: Uuid,
    pub description: String,
    pub status: TaskStatus,
    pub persist: bool,
    pub model_role: String,
    pub context_refs: Vec<String>,
    pub tool_refs: Vec<String>,
    pub output: Option<serde_json::Value>,
    pub error: Option<String>,
}

impl Task {
    pub fn new(description: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            description: description.into(),
            status: TaskStatus::Pending,
            persist: false,
            model_role: String::from("default"),
            context_refs: Vec::new(),
            tool_refs: Vec::new(),
            output: None,
            error: None,
        }
    }

    pub fn transition(&mut self, next: TaskStatus) -> Result<(), InvalidTransition> {
        if self.status.can_transition_to(next) {
            self.status = next;
            Ok(())
        } else {
            Err(InvalidTransition {
                from: self.status,
                to: next,
            })
        }
    }
}

#[derive(Debug)]
pub struct InvalidTransition {
    pub from: TaskStatus,
    pub to: TaskStatus,
}

impl std::fmt::Display for InvalidTransition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid task transition: {:?} -> {:?}",
            self.from, self.to
        )
    }
}

impl std::error::Error for InvalidTransition {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_transitions() {
        assert!(TaskStatus::Pending.can_transition_to(TaskStatus::Running));
        assert!(TaskStatus::Running.can_transition_to(TaskStatus::Succeeded));
        assert!(TaskStatus::Running.can_transition_to(TaskStatus::Failed));
        assert!(TaskStatus::Failed.can_transition_to(TaskStatus::Healing));
        assert!(TaskStatus::Healing.can_transition_to(TaskStatus::Succeeded));
        assert!(TaskStatus::Healing.can_transition_to(TaskStatus::Escalated));
    }

    #[test]
    fn invalid_transitions() {
        assert!(!TaskStatus::Pending.can_transition_to(TaskStatus::Succeeded));
        assert!(!TaskStatus::Pending.can_transition_to(TaskStatus::Failed));
        assert!(!TaskStatus::Running.can_transition_to(TaskStatus::Pending));
        assert!(!TaskStatus::Succeeded.can_transition_to(TaskStatus::Running));
        assert!(!TaskStatus::Failed.can_transition_to(TaskStatus::Succeeded));
        assert!(!TaskStatus::Escalated.can_transition_to(TaskStatus::Healing));
        assert!(!TaskStatus::Pending.can_transition_to(TaskStatus::Pending));
    }

    #[test]
    fn task_transition_updates_status() {
        let mut task = Task::new("test");
        assert_eq!(task.status, TaskStatus::Pending);

        task.transition(TaskStatus::Running).unwrap();
        assert_eq!(task.status, TaskStatus::Running);

        task.transition(TaskStatus::Failed).unwrap();
        assert_eq!(task.status, TaskStatus::Failed);
    }

    #[test]
    fn task_invalid_transition_returns_error() {
        let mut task = Task::new("test");
        let err = task.transition(TaskStatus::Succeeded).unwrap_err();
        assert_eq!(err.from, TaskStatus::Pending);
        assert_eq!(err.to, TaskStatus::Succeeded);
        assert_eq!(task.status, TaskStatus::Pending);
    }

    #[test]
    fn task_new_defaults() {
        let task = Task::new("do something");
        assert_eq!(task.description, "do something");
        assert_eq!(task.status, TaskStatus::Pending);
        assert_eq!(task.model_role, "default");
        assert!(!task.persist);
        assert!(task.output.is_none());
        assert!(task.error.is_none());
    }
}

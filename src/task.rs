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

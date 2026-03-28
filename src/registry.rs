use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::executor::{Context, Executor};
use crate::session::Message;
use crate::task::{InvalidTransition, Task, TaskStatus};

#[derive(Debug, Clone)]
pub struct TaskRegistry {
    tasks: Arc<RwLock<HashMap<Uuid, Task>>>,
}

impl TaskRegistry {
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn create(&self, task: Task) -> Uuid {
        let id = task.id;
        self.tasks.write().await.insert(id, task);
        id
    }

    pub async fn get(&self, id: Uuid) -> Option<Task> {
        self.tasks.read().await.get(&id).cloned()
    }

    pub async fn transition(&self, id: Uuid, status: TaskStatus) -> Result<(), TransitionError> {
        let mut tasks = self.tasks.write().await;
        let task = tasks.get_mut(&id).ok_or(TransitionError::NotFound(id))?;
        task.transition(status).map_err(TransitionError::Invalid)
    }

    pub async fn run(
        &self,
        id: Uuid,
        executor: &impl Executor,
        context: &Context,
        history: Option<&[Message]>,
    ) -> Result<serde_json::Value, RunError> {
        self.transition(id, TaskStatus::Running)
            .await
            .map_err(RunError::Transition)?;

        let task = self
            .get(id)
            .await
            .ok_or(RunError::Transition(TransitionError::NotFound(id)))?;

        match executor.execute(&task, context, history).await {
            Ok(output) => {
                let mut tasks = self.tasks.write().await;
                let t = tasks.get_mut(&id).unwrap();
                t.output = Some(output.clone());
                t.transition(TaskStatus::Succeeded)
                    .map_err(|e| RunError::Transition(TransitionError::Invalid(e)))?;
                Ok(output)
            }
            Err(e) => {
                let mut tasks = self.tasks.write().await;
                let t = tasks.get_mut(&id).unwrap();
                t.error = Some(e.to_string());
                t.transition(TaskStatus::Failed)
                    .map_err(|e| RunError::Transition(TransitionError::Invalid(e)))?;
                Err(RunError::Execution(t.error.clone().unwrap()))
            }
        }
    }

    pub async fn list_by_status(&self, status: TaskStatus) -> Vec<Task> {
        self.tasks
            .read()
            .await
            .values()
            .filter(|t| t.status == status)
            .cloned()
            .collect()
    }
}

impl Default for TaskRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub enum TransitionError {
    NotFound(Uuid),
    Invalid(InvalidTransition),
}

impl std::fmt::Display for TransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransitionError::NotFound(id) => write!(f, "task not found: {id}"),
            TransitionError::Invalid(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for TransitionError {}

#[derive(Debug)]
pub enum RunError {
    Transition(TransitionError),
    Execution(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Transition(e) => write!(f, "{e}"),
            RunError::Execution(e) => write!(f, "execution failed: {e}"),
        }
    }
}

impl std::error::Error for RunError {}

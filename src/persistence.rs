use std::path::PathBuf;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::task::{Task, TaskStatus};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TaskEvent {
    task_id: Uuid,
    status: TaskStatus,
    #[serde(with = "chrono_millis")]
    timestamp_ms: u64,
}

mod chrono_millis {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(val: &u64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(*val)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        u64::deserialize(d)
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

pub struct TaskStore {
    dir: PathBuf,
}

impl TaskStore {
    pub async fn new(dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir).await?;
        fs::create_dir_all(dir.join("tasks")).await?;
        fs::create_dir_all(dir.join("events")).await?;
        Ok(Self { dir })
    }

    fn task_path(&self, id: Uuid) -> PathBuf {
        self.dir.join("tasks").join(format!("{id}.json"))
    }

    fn events_path(&self) -> PathBuf {
        self.dir.join("events").join("log.jsonl")
    }

    pub async fn save_task(&self, task: &Task) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(task)?;
        fs::write(self.task_path(task.id), json).await
    }

    pub async fn load_task(&self, id: Uuid) -> std::io::Result<Task> {
        let data = fs::read_to_string(self.task_path(id)).await?;
        Ok(serde_json::from_str(&data)?)
    }

    pub async fn append_event(&self, task_id: Uuid, status: TaskStatus) -> std::io::Result<()> {
        let event = TaskEvent {
            task_id,
            status,
            timestamp_ms: now_ms(),
        };
        let mut line = serde_json::to_string(&event)?;
        line.push('\n');

        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.events_path())
            .await?;
        file.write_all(line.as_bytes()).await
    }

    pub async fn load_events(&self) -> std::io::Result<Vec<TaskEvent>> {
        let path = self.events_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let data = fs::read_to_string(&path).await?;
        let events = data
            .lines()
            .filter(|l| !l.is_empty())
            .map(serde_json::from_str)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(events)
    }

    pub async fn list_tasks(&self) -> std::io::Result<Vec<Task>> {
        let tasks_dir = self.dir.join("tasks");
        let mut tasks = Vec::new();
        let mut entries = fs::read_dir(&tasks_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            if entry.path().extension().is_some_and(|e| e == "json") {
                let data = fs::read_to_string(entry.path()).await?;
                if let Ok(task) = serde_json::from_str::<Task>(&data) {
                    tasks.push(task);
                }
            }
        }
        Ok(tasks)
    }
}

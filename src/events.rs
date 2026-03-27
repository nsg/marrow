use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    TaskCreated {
        task_id: String,
        description: String,
        role: String,
    },
    ToolSelected {
        task_id: String,
        tools: Vec<String>,
    },
    ToolGenerated {
        name: String,
        description: String,
    },
    ContextAssembled {
        task_id: String,
        providers: Vec<String>,
    },
    TaskExecuted {
        task_id: String,
        status: String,
    },
    JanitorReview {
        tool: String,
        attempt: u32,
        passed: bool,
        issues: Option<String>,
    },
    JanitorRegenerate {
        tool: String,
        attempt: u32,
    },
    JanitorEscalated {
        tool: String,
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize)]
struct LogEntry {
    timestamp_ms: u64,
    #[serde(flatten)]
    event: Event,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[derive(Clone)]
pub struct EventLog {
    file: Arc<Mutex<Option<tokio::fs::File>>>,
    verbose: bool,
}

impl EventLog {
    pub async fn new(path: Option<PathBuf>, verbose: bool) -> std::io::Result<Self> {
        let file = if let Some(p) = path {
            if let Some(parent) = p.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let f = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .await?;
            Some(f)
        } else {
            None
        };

        Ok(Self {
            file: Arc::new(Mutex::new(file)),
            verbose,
        })
    }

    pub async fn emit(&self, event: Event) {
        let entry = LogEntry {
            timestamp_ms: now_ms(),
            event: event.clone(),
        };

        // Write to JSONL file
        if let Ok(mut line) = serde_json::to_string(&entry) {
            line.push('\n');
            let mut guard = self.file.lock().await;
            if let Some(f) = guard.as_mut() {
                let _ = f.write_all(line.as_bytes()).await;
            }
        }

        // Print to stderr based on verbosity
        self.display(&event);
    }

    fn display(&self, event: &Event) {
        match event {
            // Always shown (key milestones)
            Event::ToolSelected { tools, .. } if !tools.is_empty() => {
                eprintln!("[marrow] selected tools: {}", tools.join(", "));
            }
            Event::ToolGenerated { name, .. } => {
                eprintln!("[marrow] generated new tool: {name}");
            }
            Event::JanitorEscalated { tool, reason } => {
                eprintln!("[janitor] ESCALATE '{tool}': {reason}");
            }
            Event::JanitorReview {
                tool, passed: true, ..
            } => {
                eprintln!("[janitor] '{tool}' validated");
            }

            // Verbose only
            Event::TaskCreated {
                task_id,
                description,
                role,
            } if self.verbose => {
                eprintln!("[marrow] task {task_id}: \"{description}\" (role: {role})");
            }
            Event::ToolSelected { task_id, tools } if self.verbose && tools.is_empty() => {
                eprintln!("[marrow] task {task_id}: no existing tools matched");
            }
            Event::ContextAssembled { task_id, providers } if self.verbose => {
                eprintln!(
                    "[marrow] task {task_id}: context from [{}]",
                    providers.join(", ")
                );
            }
            Event::TaskExecuted { task_id, status } if self.verbose => {
                eprintln!("[marrow] task {task_id}: {status}");
            }
            Event::JanitorReview {
                tool,
                attempt,
                passed: false,
                issues,
            } if self.verbose => {
                let issues_str = issues.as_deref().unwrap_or("unknown");
                eprintln!("[janitor] '{tool}' failed review (attempt {attempt}): {issues_str}");
            }
            Event::JanitorRegenerate { tool, attempt } if self.verbose => {
                eprintln!("[janitor] regenerating '{tool}' (attempt {attempt})");
            }
            _ => {}
        }
    }
}

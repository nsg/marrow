use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;

use reqwest::Client;

use crate::agent;
use crate::agent::{IncomingRx, ProgressTx, ProgressUpdate};
use crate::events::{Event, EventLog};
use crate::janitor;
use crate::memory::{Memory, MemoryStore};
use crate::memory_provider;
use crate::memory_writer;
use crate::metrics::Metrics;
use crate::model::ModelBackend;
use crate::router::{ModelRouter, RouterConfig};
use crate::schedule::ScheduleStore;
use crate::secrets::Secrets;
use crate::session::Message;
use crate::tool::{FrontendContext, ToolRegistry};
use crate::toolbox::Toolbox;

pub struct RuntimeOptions {
    pub toolbox_path: String,
    pub memory_path: String,
    pub log_path: String,
    pub verbose: bool,
    pub secrets_path: String,
    pub spawn_janitor: bool,
    pub schedule_path: String,
}

pub struct Runtime {
    router: Arc<ModelRouter>,
    registry: Arc<ToolRegistry>,
    memory_store: Arc<MemoryStore>,
    schedule_store: Arc<ScheduleStore>,
    client: Arc<Client>,
    log: Arc<EventLog>,
    secrets: Arc<Secrets>,
    metrics: Arc<Metrics>,
}

async fn load_relevant_memories(
    description: &str,
    store: &MemoryStore,
    backend: &dyn ModelBackend,
) -> Vec<Memory> {
    match memory_provider::select_memories(description, store, backend).await {
        Ok(memories) => memories,
        Err(e) => {
            eprintln!("[marrow] memory retrieval error: {e}");
            store.list().unwrap_or_default()
        }
    }
}

impl Runtime {
    pub async fn from_config(
        config: &RouterConfig,
        options: RuntimeOptions,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let metrics = Arc::new(Metrics::new());
        let router = Arc::new(ModelRouter::from_config_with_metrics(
            config,
            Some(metrics.clone()),
        )?);
        let client = Arc::new(Client::new());
        let toolbox = Toolbox::new(&options.toolbox_path);
        let mut registry = ToolRegistry::new(toolbox, &options.toolbox_path);
        crate::tools::register_all(&mut registry);
        let registry = Arc::new(registry);
        let memory_store = Arc::new(MemoryStore::new(&options.memory_path));
        let schedule_store = Arc::new(ScheduleStore::new(&options.schedule_path));
        let log =
            Arc::new(EventLog::new(Some(PathBuf::from(&options.log_path)), options.verbose).await?);
        let secrets = Arc::new(Secrets::load_or_empty(&options.secrets_path));

        if options.spawn_janitor {
            let janitor_backend = config
                .build_backend("code")
                .or_else(|_| config.build_backend("default"))?;
            let janitor_toolbox = Toolbox::new(&options.toolbox_path);
            let janitor_log = log.clone();
            let janitor_builtins = registry.builtin_info();
            let janitor_memory = MemoryStore::new(&options.memory_path);
            tokio::spawn(async move {
                janitor::run(
                    &janitor_toolbox,
                    janitor_backend.as_ref(),
                    &janitor_log,
                    &janitor_builtins,
                    &janitor_memory,
                )
                .await;
            });
        }

        Ok(Self {
            router,
            registry,
            memory_store,
            schedule_store,
            client,
            log,
            secrets,
            metrics,
        })
    }

    pub fn fast_backend(&self) -> Result<&dyn ModelBackend, Box<dyn Error + Send + Sync>> {
        self.router
            .backend("fast")
            .or_else(|_| self.router.backend("default"))
    }

    pub fn metrics(&self) -> &Metrics {
        self.metrics.as_ref()
    }

    pub fn schedule_store(&self) -> &Arc<ScheduleStore> {
        &self.schedule_store
    }

    pub fn log(&self) -> &Arc<EventLog> {
        &self.log
    }

    /// Run a single janitor pass: review unvalidated tools and clean up the toolbox.
    pub async fn run_janitor_once(&self) -> Result<u32, Box<dyn Error + Send + Sync>> {
        let code_backend = self
            .router
            .backend("code")
            .or_else(|_| self.router.backend("default"))?;
        let builtins = self.registry.builtin_info();
        janitor::run_once(
            self.registry.toolbox(),
            code_backend,
            self.log.as_ref(),
            &builtins,
            self.memory_store.as_ref(),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn run_task(
        &self,
        description: &str,
        frontend: &str,
        conversation: &[Message],
        progress: Option<&ProgressTx>,
        incoming: Option<&mut IncomingRx>,
        formatting_hint: Option<&str>,
        frontend_context: Option<FrontendContext>,
    ) -> Result<String, Box<dyn Error + Send + Sync>> {
        let task_id = uuid::Uuid::new_v4().to_string();

        self.log
            .emit(Event::TaskCreated {
                task_id: task_id.clone(),
                description: description.to_string(),
                role: frontend.to_string(),
            })
            .await;

        let agent_backend = self
            .router
            .backend("agent")
            .or_else(|_| self.router.backend("default"))
            .or_else(|_| self.router.backend("fast"))?;
        let fast_backend = self.fast_backend()?;
        let answer_backend = self
            .router
            .backend("default")
            .or_else(|_| self.router.backend("fast"))?;
        let memories =
            load_relevant_memories(description, self.memory_store.as_ref(), fast_backend).await;

        let answer = agent::run_loop(
            description,
            &task_id,
            agent_backend,
            answer_backend,
            fast_backend,
            self.registry.as_ref(),
            self.client.clone(),
            &memories,
            self.log.as_ref(),
            Some(self.secrets.as_ref()),
            progress,
            conversation,
            incoming,
            formatting_hint,
            Some(self.schedule_store.clone()),
            Some(self.memory_store.clone()),
            frontend_context,
            frontend,
        )
        .await?;

        self.log
            .emit(Event::TaskExecuted {
                task_id: task_id.clone(),
                status: "succeeded".to_string(),
            })
            .await;

        // Run memory writer in the background — don't block the user response.
        let mem_store = self.memory_store.clone();
        let mem_router = self.router.clone();
        let mem_description = description.to_string();
        let mem_answer = answer.clone();
        let mem_progress = progress.cloned();
        tokio::spawn(async move {
            let fast = mem_router
                .backend("fast")
                .or_else(|_| mem_router.backend("default"));
            let Ok(fast) = fast else { return };

            match memory_writer::process_interaction(
                &mem_description,
                &mem_answer,
                mem_store.as_ref(),
                fast,
            )
            .await
            {
                Ok(result) => {
                    if !result.saved.is_empty() {
                        if let Some(ref tx) = mem_progress {
                            let _ = tx.send(ProgressUpdate::MemoryNew);
                        }
                        for fact in &result.saved {
                            eprintln!("[marrow] remembered: {fact}");
                        }
                    }

                    if !result.updated.is_empty()
                        && let Some(ref tx) = mem_progress
                    {
                        let _ = tx.send(ProgressUpdate::MemoryUpdated);
                    }

                    if result.deleted > 0
                        && let Some(ref tx) = mem_progress
                    {
                        let _ = tx.send(ProgressUpdate::MemoryCleared);
                    }
                }
                Err(e) => eprintln!("[marrow] memory writer error: {e}"),
            }
        });

        Ok(answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;
    use tokio::sync::Mutex;

    use crate::memory::{Memory, MemorySource};
    use crate::model::CompletionResult;
    use crate::session::Message;

    struct MockBackend {
        responses: Mutex<Vec<String>>,
    }

    impl MockBackend {
        fn new(responses: Vec<&str>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().map(String::from).collect()),
            }
        }
    }

    impl ModelBackend for MockBackend {
        fn complete(&self, _prompt: String) -> CompletionResult<'_> {
            Box::pin(async {
                let mut queue = self.responses.lock().await;
                if queue.is_empty() {
                    panic!("MockBackend: no more responses queued");
                }
                Ok(queue.remove(0))
            })
        }

        fn complete_chat(&self, _messages: Vec<Message>) -> CompletionResult<'_> {
            Box::pin(async {
                let mut queue = self.responses.lock().await;
                if queue.is_empty() {
                    panic!("MockBackend: no more responses queued");
                }
                Ok(queue.remove(0))
            })
        }
    }

    struct FailingBackend;

    impl ModelBackend for FailingBackend {
        fn complete(&self, _prompt: String) -> CompletionResult<'_> {
            Box::pin(async { Err("backend failed".into()) })
        }

        fn complete_chat(&self, _messages: Vec<Message>) -> CompletionResult<'_> {
            Box::pin(async { Err("backend failed".into()) })
        }
    }

    fn temp_dir(name: &str) -> TempDir {
        tempfile::Builder::new().prefix(name).tempdir().unwrap()
    }

    #[tokio::test]
    async fn load_relevant_memories_selects_matching_facts() {
        let dir = temp_dir("marrow_runtime_mem");
        let store = MemoryStore::new(dir.path());
        let mem = Memory::new("user prefers dark mode", MemorySource::User);
        let id = mem.id;
        store.save(&mem).unwrap();

        let backend = MockBackend::new(vec![&format!(r#"["{id}"]"#)]);
        let selected = load_relevant_memories("theme?", &store, &backend).await;

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].fact, "user prefers dark mode");
    }

    #[tokio::test]
    async fn load_relevant_memories_falls_back_to_all_on_error() {
        let dir = temp_dir("marrow_runtime_mem");
        let store = MemoryStore::new(dir.path());
        let first = Memory::new("user prefers dark mode", MemorySource::User);
        let second = Memory::new("user works in UTC", MemorySource::User);
        store.save(&first).unwrap();
        store.save(&second).unwrap();

        let selected = load_relevant_memories("theme?", &store, &FailingBackend).await;

        assert_eq!(selected.len(), 2);
        let facts: Vec<&str> = selected.iter().map(|m| m.fact.as_str()).collect();
        assert!(facts.contains(&"user prefers dark mode"));
        assert!(facts.contains(&"user works in UTC"));
    }
}

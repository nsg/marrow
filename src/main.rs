use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use reqwest::Client;

use marrow::agent;
use marrow::events::{Event, EventLog};
use marrow::executor::Context;
use marrow::memory::MemoryStore;
use marrow::memory_provider;
use marrow::memory_writer;
use marrow::registry::TaskRegistry;
use marrow::router::{ModelRouter, RouterConfig};
use marrow::session::{ChatSession, Message};
use marrow::task::Task;
use marrow::toolbox::Toolbox;
use marrow::triage;
use marrow::janitor;

#[derive(Parser)]
#[command(
    name = "marrow",
    about = "A lean agent framework for workflow automation"
)]
struct Cli {
    #[arg(short, long, default_value = "config.toml")]
    config: String,

    #[arg(short, long, default_value = "default")]
    role: String,

    /// Run a single prompt and exit
    #[arg(short = 'p', long)]
    prompt: Option<String>,

    /// Path to the toolbox directory
    #[arg(short, long, default_value = "toolbox")]
    toolbox: String,

    /// Path to the memory directory
    #[arg(short, long, default_value = "memory")]
    memory: String,

    /// Show full event stream
    #[arg(short, long)]
    verbose: bool,

    /// Path to event log file
    #[arg(long, default_value = "events.jsonl")]
    log: String,
}

#[allow(clippy::too_many_arguments)]
async fn run_task(
    description: &str,
    role: &str,
    registry: &TaskRegistry,
    router: &ModelRouter,
    toolbox: &Toolbox,
    toolbox_path: &str,
    memory_store: &MemoryStore,
    client: Arc<Client>,
    log: &EventLog,
    session: Option<&ChatSession>,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let mut task = Task::new(description);
    task.model_role = role.to_string();
    let task_id = task.id.to_string();

    log.emit(Event::TaskCreated {
        task_id: task_id.clone(),
        description: description.to_string(),
        role: role.to_string(),
    })
    .await;

    let fast_backend = router
        .backend("fast")
        .or_else(|_| router.backend("default"))?;
    let history_msgs = session.map(|s| s.build_messages(None));
    let history_ref = history_msgs.as_deref();

    // Step 1: Retrieve relevant memories
    let memories =
        memory_provider::select_memories(description, memory_store, fast_backend).await?;

    // Step 2: Triage — does this task need external data?
    let needs_tools =
        triage::needs_external_data(description, fast_backend, history_ref, &memories).await?;

    // Step 3: If tools needed, run agent loop; otherwise direct answer
    let answer = if needs_tools {
        let code_backend = router
            .backend("code")
            .or_else(|_| router.backend("default"))?;

        agent::run_loop(
            description,
            &task_id,
            fast_backend,
            code_backend,
            toolbox,
            toolbox_path,
            client,
            &memories,
            log,
        )
        .await?
    } else {
        // Direct answer — no tools needed
        let memory_context = memory_provider::memories_to_context(&memories);
        let mut context_data = serde_json::Map::new();
        context_data.insert("memories".to_string(), memory_context);
        let context = Context::new(serde_json::Value::Object(context_data));

        let history = session.map(|s| s.build_messages(None));
        let history_slice = history.as_deref();

        let id = registry.create(task.clone()).await;
        let result = registry.run(id, router, &context, history_slice).await;

        let status = if result.is_ok() { "succeeded" } else { "failed" };
        log.emit(Event::TaskExecuted {
            task_id: task_id.clone(),
            status: status.to_string(),
        })
        .await;

        match result {
            Ok(val) => val.as_str().unwrap_or("").to_string(),
            Err(e) => return Err(e.into()),
        }
    };

    log.emit(Event::TaskExecuted {
        task_id: task_id.clone(),
        status: "succeeded".to_string(),
    })
    .await;

    // Step 4: Post-task memory writer
    if let Err(e) =
        memory_writer::process_interaction(description, &answer, memory_store, fast_backend).await
    {
        eprintln!("[marrow] memory writer error: {e}");
    }

    Ok(serde_json::Value::String(answer))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cli = Cli::parse();

    let config = RouterConfig::from_file(&cli.config)?;
    let router = ModelRouter::from_config(&config)?;
    let registry = TaskRegistry::new();
    let client = Arc::new(Client::new());
    let toolbox = Toolbox::new(&cli.toolbox);
    let memory_store = MemoryStore::new(&cli.memory);
    let log = EventLog::new(Some(PathBuf::from(&cli.log)), cli.verbose).await?;

    // Spawn janitor in background
    let janitor_backend = config
        .build_backend("code")
        .or_else(|_| config.build_backend("default"))?;
    let janitor_toolbox = Toolbox::new(&cli.toolbox);
    let janitor_log = log.clone();
    tokio::spawn(async move {
        janitor::run(&janitor_toolbox, janitor_backend.as_ref(), &janitor_log).await;
    });

    if let Some(prompt) = cli.prompt {
        match run_task(
            &prompt,
            &cli.role,
            &registry,
            &router,
            &toolbox,
            &cli.toolbox,
            &memory_store,
            client,
            &log,
            None,
        )
        .await
        {
            Ok(output) => {
                if let Some(text) = output.as_str() {
                    println!("{text}");
                } else {
                    println!("{output}");
                }
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    } else {
        println!("marrow ready (role: {})", cli.role);
        println!("type 'quit' to exit\n");

        let mut session = ChatSession::new();
        let fast_backend = router
            .backend("fast")
            .or_else(|_| router.backend("default"))?;

        let stdin = io::stdin();
        loop {
            print!("> ");
            io::stdout().flush()?;

            let mut input = String::new();
            let bytes = stdin.read_line(&mut input)?;
            if bytes == 0 {
                break;
            }
            let input = input.trim();

            if input.is_empty() {
                continue;
            }
            if input == "quit" {
                break;
            }

            match run_task(
                input,
                &cli.role,
                &registry,
                &router,
                &toolbox,
                &cli.toolbox,
                &memory_store,
                client.clone(),
                &log,
                Some(&session),
            )
            .await
            {
                Ok(output) => {
                    let text = output.as_str().unwrap_or("").to_string();
                    println!("\n{text}\n");

                    session.append(Message::user(input));
                    session.append(Message::assistant(&text));

                    if session.needs_summarization()
                        && let Err(e) = session.summarize(fast_backend).await
                    {
                        eprintln!("[marrow] summarization error: {e}");
                    }
                }
                Err(e) => eprintln!("\nerror: {e}\n"),
            }
        }
    }

    Ok(())
}

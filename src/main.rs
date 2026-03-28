mod backends;
mod codegen;
mod context;
mod events;
mod executor;
mod janitor;
mod memory;
mod memory_provider;
mod memory_writer;
mod model;
mod persistence;
mod registry;
mod router;
mod sandbox;
mod sandbox_host;
mod session;
mod task;
mod tool_selection;
mod toolbox;

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use reqwest::Client;

use context::ContextAssembler;
use events::{Event, EventLog};
use memory::MemoryStore;
use registry::TaskRegistry;
use router::{ModelRouter, RouterConfig};
use session::{ChatSession, Message};
use task::Task;
use toolbox::Toolbox;

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

    // Step 1: Get toolbox manifest
    let available_tools = toolbox.list_tools().unwrap_or_default();

    // Step 2: Ask the "fast" model which tools to use
    let fast_backend = router
        .backend("fast")
        .or_else(|_| router.backend("default"))?;
    let selected =
        tool_selection::select_tools(description, &available_tools, fast_backend).await?;

    log.emit(Event::ToolSelected {
        task_id: task_id.clone(),
        tools: selected.clone(),
    })
    .await;

    // Step 3: If no tools selected, generate a new one
    let selected = if selected.is_empty() && !description.trim().is_empty() {
        let code_backend = router
            .backend("code")
            .or_else(|_| router.backend("default"))?;
        match codegen::generate_provider(description, code_backend, toolbox, client.clone()).await {
            Ok(name) => {
                log.emit(Event::ToolGenerated {
                    name: name.clone(),
                    description: description.to_string(),
                })
                .await;
                vec![name]
            }
            Err(e) => {
                eprintln!("[marrow] code generation failed: {e}");
                Vec::new()
            }
        }
    } else {
        selected
    };

    // Step 4: Retrieve relevant memories
    let memories =
        memory_provider::select_memories(description, memory_store, fast_backend).await?;
    let memory_context = memory_provider::memories_to_context(&memories);

    // Step 5: Assemble context from selected providers + memories
    let mut assembler = ContextAssembler::new(client);
    for name in &selected {
        match toolbox.load_provider(name) {
            Ok(provider) => assembler.add_provider(provider),
            Err(e) => eprintln!("[marrow] failed to load provider '{name}': {e}"),
        }
    }

    let mut context = assembler.assemble(description, &selected).await?;

    if let Some(obj) = context.data.as_object_mut() {
        obj.insert("memories".to_string(), memory_context);
    }

    log.emit(Event::ContextAssembled {
        task_id: task_id.clone(),
        providers: selected,
    })
    .await;

    // Step 6: Execute task with assembled context + session history
    let history = session.map(|s| s.build_messages(None));
    let history_slice = history.as_deref();

    let id = registry.create(task).await;
    let result = registry.run(id, router, &context, history_slice).await;

    let status = if result.is_ok() {
        "succeeded"
    } else {
        "failed"
    };

    log.emit(Event::TaskExecuted {
        task_id: task_id.clone(),
        status: status.to_string(),
    })
    .await;

    // Step 7: Post-task memory writer
    if let Ok(ref output) = result {
        let response_text = output.as_str().unwrap_or("");
        if let Err(e) = memory_writer::process_interaction(
            description,
            response_text,
            memory_store,
            fast_backend,
        )
        .await
        {
            eprintln!("[marrow] memory writer error: {e}");
        }
    }

    result.map_err(|e| e.into())
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

                    // Append to session history
                    session.append(Message::user(input));
                    session.append(Message::assistant(&text));

                    // Summarize if needed
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

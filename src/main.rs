mod backends;
mod codegen;
mod context;
mod events;
mod executor;
mod janitor;
mod model;
mod persistence;
mod registry;
mod router;
mod sandbox;
mod sandbox_host;
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
use registry::TaskRegistry;
use router::{ModelRouter, RouterConfig};
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

    /// Show full event stream
    #[arg(short, long)]
    verbose: bool,

    /// Path to event log file
    #[arg(long, default_value = "events.jsonl")]
    log: String,
}

async fn run_task(
    description: &str,
    role: &str,
    registry: &TaskRegistry,
    router: &ModelRouter,
    toolbox: &Toolbox,
    client: Arc<Client>,
    log: &EventLog,
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
        match codegen::generate_provider(description, code_backend, toolbox).await {
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

    // Step 4: Assemble context from selected providers
    let mut assembler = ContextAssembler::new(client);
    for name in &selected {
        match toolbox.load_provider(name) {
            Ok(provider) => assembler.add_provider(provider),
            Err(e) => eprintln!("[marrow] failed to load provider '{name}': {e}"),
        }
    }

    let context = assembler.assemble(description, &selected).await?;

    log.emit(Event::ContextAssembled {
        task_id: task_id.clone(),
        providers: selected,
    })
    .await;

    // Step 5: Execute task with assembled context
    let id = registry.create(task).await;
    let result = registry.run(id, router, &context).await;

    log.emit(Event::TaskExecuted {
        task_id: task_id.clone(),
        status: if result.is_ok() {
            "succeeded".to_string()
        } else {
            "failed".to_string()
        },
    })
    .await;

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
            &prompt, &cli.role, &registry, &router, &toolbox, client, &log,
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
                client.clone(),
                &log,
            )
            .await
            {
                Ok(output) => {
                    if let Some(text) = output.as_str() {
                        println!("\n{text}\n");
                    } else {
                        println!("\n{output}\n");
                    }
                }
                Err(e) => eprintln!("\nerror: {e}\n"),
            }
        }
    }

    Ok(())
}

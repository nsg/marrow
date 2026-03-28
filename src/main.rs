use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use reqwest::Client;

use marrow::agent;
use marrow::events::{Event, EventLog};
use marrow::metrics::Metrics;
use marrow::memory::MemoryStore;
use marrow::memory_writer;
use marrow::router::{ModelRouter, RouterConfig};
use marrow::session::{ChatSession, Message};
use marrow::toolbox::Toolbox;
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
    router: &ModelRouter,
    toolbox: &Toolbox,
    toolbox_path: &str,
    memory_store: &MemoryStore,
    client: Arc<Client>,
    log: &EventLog,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let task_id = uuid::Uuid::new_v4().to_string();

    log.emit(Event::TaskCreated {
        task_id: task_id.clone(),
        description: description.to_string(),
        role: "agent".to_string(),
    })
    .await;

    let fast_backend = router
        .backend("fast")
        .or_else(|_| router.backend("default"))?;
    let code_backend = router
        .backend("code")
        .or_else(|_| router.backend("default"))?;

    // Step 1: Load all memories
    let memories = memory_store.list().unwrap_or_default();

    // Step 2: Agent loop — model decides whether to use tools or answer directly
    let answer = agent::run_loop(
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
    .await?;

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
    let metrics = Arc::new(Metrics::new());
    let router = ModelRouter::from_config_with_metrics(&config, Some(metrics.clone()))?;
    let client = Arc::new(Client::new());
    let toolbox = Toolbox::new(&cli.toolbox);
    let memory_store = MemoryStore::new(&cli.memory);
    let log = EventLog::new(Some(PathBuf::from(&cli.log)), cli.verbose).await?;
    let verbose = cli.verbose;

    // Spawn janitor in background (no metrics for janitor — it's background work)
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
            &router,
            &toolbox,
            &cli.toolbox,
            &memory_store,
            client,
            &log,
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
                &router,
                &toolbox,
                &cli.toolbox,
                &memory_store,
                client.clone(),
                &log,
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

    if verbose {
        metrics.display();
    }

    Ok(())
}

use clap::Parser;
use marrow::memory::MemoryStore;
use marrow::router::RouterConfig;
use marrow::runtime::{Runtime, RuntimeOptions};
use marrow::session::{ChatSession, Message};
use marrow::tool::ToolRegistry;
use marrow::toolbox::Toolbox;
use std::io::{self, Write};
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(
    name = "marrow",
    about = "A lean agent framework for workflow automation"
)]
struct Cli {
    #[arg(short, long, default_value = "config.toml")]
    config: String,

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

    /// Run a single janitor pass (review + cleanup) and exit
    #[arg(long)]
    janitor: bool,

    /// List all tools (built-in and Lua) and exit
    #[arg(long)]
    list_tools: bool,

    /// List all stored memories and exit
    #[arg(long)]
    list_memories: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cli = Cli::parse();

    if cli.list_tools {
        let toolbox = Toolbox::new(&cli.toolbox);
        let mut registry = ToolRegistry::new(toolbox, &cli.toolbox);
        marrow::tools::register_all(&mut registry);

        let tools = registry.list_all();
        if tools.is_empty() {
            println!("(no tools)");
        } else {
            for tool in &tools {
                let status = if tool.builtin { "built-in" } else { "lua" };
                println!("{} [{}] — {}", tool.name, status, tool.description);
            }
        }
        return Ok(());
    }

    if cli.list_memories {
        let store = MemoryStore::new(&cli.memory);
        match store.list() {
            Ok(memories) if memories.is_empty() => println!("(no memories)"),
            Ok(memories) => {
                for mem in &memories {
                    println!("{} [{:?}] — {}", mem.id, mem.source, mem.fact);
                }
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    let config = RouterConfig::from_file(&cli.config)?;
    let runtime = Runtime::from_config(
        &config,
        RuntimeOptions {
            toolbox_path: cli.toolbox.clone(),
            memory_path: cli.memory.clone(),
            log_path: cli.log.clone(),
            verbose: cli.verbose,
            secrets_path: "secrets.toml".to_string(),
            spawn_janitor: false,
        },
    )
    .await?;
    let verbose = cli.verbose;

    if cli.janitor {
        eprintln!("[marrow] running janitor pass...");
        match runtime.run_janitor_once().await {
            Ok(count) => eprintln!("[marrow] janitor done — {count} tool(s) reviewed"),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    } else if let Some(prompt) = cli.prompt {
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<String>();
        let progress_handle = tokio::spawn(async move {
            while let Some(status) = progress_rx.recv().await {
                eprintln!("[progress] {status}");
            }
        });

        match runtime
            .run_task(&prompt, "cli", &[], Some(&progress_tx), None, None)
            .await
        {
            Ok(output) => {
                drop(progress_tx);
                let _ = progress_handle.await;
                println!("{output}");
            }
            Err(e) => {
                drop(progress_tx);
                let _ = progress_handle.await;
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    } else {
        println!("marrow ready");
        println!("type 'quit' to exit\n");

        let mut session = ChatSession::new();
        let fast_backend = runtime.fast_backend()?;

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

            let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<String>();
            let progress_handle = tokio::spawn(async move {
                while let Some(status) = progress_rx.recv().await {
                    eprintln!("[progress] {status}");
                }
            });

            let conversation = session.build_messages(None);
            match runtime
                .run_task(input, "cli", &conversation, Some(&progress_tx), None, None)
                .await
            {
                Ok(text) => {
                    drop(progress_tx);
                    let _ = progress_handle.await;
                    println!("\n{text}\n");

                    session.append(Message::user(input));
                    session.append(Message::assistant(&text));

                    if session.needs_summarization()
                        && let Err(e) = session.summarize(fast_backend).await
                    {
                        eprintln!("[marrow] summarization error: {e}");
                    }
                }
                Err(e) => {
                    drop(progress_tx);
                    let _ = progress_handle.await;
                    eprintln!("\nerror: {e}\n");
                }
            }
        }
    }

    if verbose {
        runtime.metrics().display();
    }

    Ok(())
}

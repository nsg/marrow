use clap::Parser;
use marrow::agent::{Outcome, ProgressUpdate};
use marrow::heartbeat;
use marrow::memory::MemoryStore;
use marrow::router::RouterConfig;
use marrow::runtime::{Runtime, RuntimeOptions};
use marrow::schedule::ScheduleStore;
use marrow::session::{ChatSession, Message};
use marrow::tool::ToolRegistry;
use marrow::toolbox::Toolbox;
use std::io::{self, Write};
use std::sync::Arc;
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

    /// Path to raw request/response log file
    #[arg(long, default_value = "raw_requests.log")]
    raw_log: String,

    /// Run a single janitor pass (review + cleanup) and exit
    #[arg(long)]
    janitor: bool,

    /// List all tools (built-in and Lua) and exit
    #[arg(long)]
    list_tools: bool,

    /// List all stored memories and exit
    #[arg(long)]
    list_memories: bool,

    /// List all schedules and exit
    #[arg(long)]
    list_schedules: bool,

    /// Run a single heartbeat pass (execute due schedules) and exit
    #[arg(long)]
    run_schedules: bool,

    /// Run as a long-lived daemon with the heartbeat scheduler active
    #[arg(long)]
    daemon: bool,

    /// Check for updates and apply if available
    #[arg(long)]
    update: bool,

    /// Path to the schedules directory
    #[arg(long, default_value = "schedules")]
    schedules: String,

    /// Path to the skills directory
    #[arg(long, default_value = "skills")]
    skills: String,
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
        let store = MemoryStore::new(&cli.memory)?;
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

    if cli.list_schedules {
        let store = ScheduleStore::new(&cli.schedules);
        match store.list() {
            Ok(schedules) if schedules.is_empty() => println!("(no schedules)"),
            Ok(schedules) => {
                for s in &schedules {
                    let status = if s.enabled { "enabled" } else { "disabled" };
                    let last = s.last_run.as_deref().unwrap_or("never");
                    println!(
                        "{} [{}] {} — {} (last run: {})",
                        s.id,
                        status,
                        s.repeat.display(),
                        s.description,
                        last
                    );
                }
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    if cli.update {
        match marrow::update::check_and_update().await {
            Ok(true) => {
                eprintln!("[marrow] restart to use the new version");
                return Ok(());
            }
            Ok(false) => return Ok(()),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }

    let config = RouterConfig::from_file(&cli.config)?;
    let spawn_janitor = cli.daemon;
    let runtime = Runtime::from_config(
        &config,
        RuntimeOptions {
            toolbox_path: cli.toolbox.clone(),
            memory_path: cli.memory.clone(),
            log_path: cli.log.clone(),
            raw_log_path: cli.raw_log.clone(),
            verbose: cli.verbose,
            secrets_path: "secrets.toml".to_string(),
            spawn_janitor,
            schedule_path: cli.schedules.clone(),
            skills_path: cli.skills.clone(),
        },
    )
    .await?;
    let verbose = cli.verbose;

    if cli.run_schedules {
        eprintln!("[marrow] running heartbeat pass...");
        let (tx, _rx) = mpsc::unbounded_channel::<heartbeat::ScheduleResult>();
        let count =
            heartbeat::run_once(&runtime, runtime.schedule_store(), runtime.log(), &tx).await;
        eprintln!("[marrow] heartbeat done — {count} schedule(s) executed");
        if verbose {
            runtime.metrics().display();
        }
        return Ok(());
    }

    if cli.daemon {
        eprintln!("[marrow] starting daemon mode...");
        let runtime = Arc::new(runtime);
        let hb_runtime = runtime.clone();
        let hb_store = runtime.schedule_store().clone();
        let hb_log = runtime.log().clone();
        let tick = config.scheduler.as_ref().map(|s| s.tick()).unwrap_or(60);

        let (schedule_tx, mut schedule_rx) = mpsc::unbounded_channel::<heartbeat::ScheduleResult>();

        // Result receiver — prints to stdout (skips dismissed results)
        tokio::spawn(async move {
            while let Some(result) = schedule_rx.recv().await {
                if let Outcome::Answer(ref answer) = result.outcome {
                    let status = if result.success { "ok" } else { "err" };
                    println!("[schedule:{status}] {} — {answer}", result.description);
                }
            }
        });

        // Heartbeat loop
        tokio::spawn(async move {
            heartbeat::run(hb_runtime, hb_store, hb_log, schedule_tx, tick).await;
        });

        eprintln!("[marrow] heartbeat active (tick: {tick}s). Press Ctrl+C to stop.");
        tokio::signal::ctrl_c().await?;
        eprintln!("[marrow] shutting down.");
        if verbose {
            runtime.metrics().display();
        }
        return Ok(());
    }

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
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<ProgressUpdate>();
        let progress_handle = tokio::spawn(async move {
            while let Some(status) = progress_rx.recv().await {
                eprintln!("[progress] {status}");
            }
        });

        match runtime
            .run_task(&prompt, "cli", &[], Some(&progress_tx), None, None, None)
            .await
        {
            Ok(result) => {
                drop(progress_tx);
                let _ = progress_handle.await;
                if let Outcome::Answer(answer) = &result.outcome {
                    println!("{answer}");
                }
                if verbose {
                    result.metrics.display();
                }
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

            let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<ProgressUpdate>();
            let progress_handle = tokio::spawn(async move {
                while let Some(status) = progress_rx.recv().await {
                    eprintln!("[progress] {status}");
                }
            });

            let conversation = session.build_messages(None);
            match runtime
                .run_task(
                    input,
                    "cli",
                    &conversation,
                    Some(&progress_tx),
                    None,
                    None,
                    None,
                )
                .await
            {
                Ok(result) => {
                    drop(progress_tx);
                    let _ = progress_handle.await;
                    match &result.outcome {
                        Outcome::Answer(answer) => {
                            println!("\n{answer}\n");
                            session.append(Message::user(input));
                            session.append(Message::assistant(answer));
                        }
                        Outcome::Dismissed => {
                            // Nothing to show — don't pollute session history
                        }
                    }
                    if verbose {
                        result.metrics.display();
                    }

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

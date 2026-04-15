use clap::Parser;
use marrow::router::RouterConfig;
use marrow::runtime::{Runtime, RuntimeOptions};
use marrow::session::{ChatSession, Message};
use std::io::{self, Write};

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
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cli = Cli::parse();

    let config = RouterConfig::from_file(&cli.config)?;
    let runtime = Runtime::from_config(
        &config,
        RuntimeOptions {
            toolbox_path: cli.toolbox.clone(),
            memory_path: cli.memory.clone(),
            log_path: cli.log.clone(),
            verbose: cli.verbose,
            secrets_path: "secrets.toml".to_string(),
        },
    )
    .await?;
    let verbose = cli.verbose;

    if let Some(prompt) = cli.prompt {
        match runtime
            .run_task(&prompt, "agent", &[], None, None, None)
            .await
        {
            Ok(output) => {
                println!("{output}");
            }
            Err(e) => {
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

            let conversation = session.build_messages(None);
            match runtime
                .run_task(input, "agent", &conversation, None, None, None)
                .await
            {
                Ok(text) => {
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
        runtime.metrics().display();
    }

    Ok(())
}

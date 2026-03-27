mod backends;
mod context;
mod executor;
mod model;
mod persistence;
mod registry;
mod router;
mod sandbox;
mod sandbox_host;
mod task;
mod toolbox;

use std::io::{self, Write};
use std::sync::Arc;

use clap::Parser;
use reqwest::Client;

use context::ContextAssembler;
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
}

async fn run_task(
    description: &str,
    role: &str,
    registry: &TaskRegistry,
    router: &ModelRouter,
    assembler: &ContextAssembler,
) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let mut task = Task::new(description);
    task.model_role = role.to_string();

    let context = assembler.assemble(description, &task.context_refs).await?;
    let id = registry.create(task).await;
    let output = registry.run(id, router, &context).await?;
    Ok(output)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cli = Cli::parse();

    let config = RouterConfig::from_file(&cli.config)?;
    let router = ModelRouter::from_config(&config)?;
    let registry = TaskRegistry::new();
    let client = Arc::new(Client::new());

    // Load toolbox providers
    let tb = Toolbox::new(&cli.toolbox);
    let mut assembler = ContextAssembler::new(client);
    for provider in tb.load_all_providers().unwrap_or_default() {
        assembler.add_provider(provider);
    }

    if let Some(prompt) = cli.prompt {
        match run_task(&prompt, &cli.role, &registry, &router, &assembler).await {
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

            match run_task(input, &cli.role, &registry, &router, &assembler).await {
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

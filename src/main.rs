mod backends;
mod executor;
mod model;
mod persistence;
mod registry;
mod router;
mod task;

use std::io::{self, Write};

use clap::Parser;

use executor::Context;
use registry::TaskRegistry;
use router::{ModelRouter, RouterConfig};
use task::Task;

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
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cli = Cli::parse();

    let config = RouterConfig::from_file(&cli.config)?;
    let router = ModelRouter::from_config(&config)?;
    let registry = TaskRegistry::new();

    if let Some(prompt) = cli.prompt {
        let mut task = Task::new(&prompt);
        task.model_role = cli.role.clone();
        let id = registry.create(task).await;

        match registry.run(id, &router, &Context::empty()).await {
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

            let mut task = Task::new(input);
            task.model_role = cli.role.clone();

            let id = registry.create(task).await;

            match registry.run(id, &router, &Context::empty()).await {
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

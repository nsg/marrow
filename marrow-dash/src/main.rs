use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use clap::Parser;

mod api;
mod data;
mod frontend;
mod state;

#[derive(Parser)]
#[command(name = "marrow-dash", about = "Read-only dashboard for marrow")]
struct Args {
    #[arg(long, default_value = "config.toml")]
    config: PathBuf,
    #[arg(long, default_value = "events.jsonl")]
    log: PathBuf,
    #[arg(long, default_value = "toolbox")]
    toolbox: PathBuf,
    #[arg(long, default_value = "memory")]
    memory: PathBuf,
    #[arg(long, default_value = "schedules")]
    schedules: PathBuf,
    #[arg(long, default_value = "skills")]
    skills: PathBuf,
    #[arg(long, default_value_t = 3000)]
    port: u16,
    #[arg(long, default_value = "127.0.0.1")]
    bind: String,

    /// Check for updates and apply if available
    #[arg(long)]
    update: bool,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    if args.update {
        match marrow::update::check_and_update().await {
            Ok(true) => {
                eprintln!("[marrow-dash] restart to use the new version");
                return;
            }
            Ok(false) => return,
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }

    let state = Arc::new(state::AppState::load(
        &args.log,
        &args.toolbox,
        &args.memory,
        &args.schedules,
        &args.skills,
        &args.config,
    ));
    let app = Router::new()
        .merge(frontend::routes())
        .nest("/api", api::routes())
        .with_state(state.clone());

    // Background refresh task
    let refresh_state = state.clone();
    let log_path = args.log.clone();
    let toolbox_path = args.toolbox.clone();
    let memory_path = args.memory.clone();
    let schedules_path = args.schedules.clone();
    let skills_path = args.skills.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            interval.tick().await;
            refresh_state.refresh(
                &log_path,
                &toolbox_path,
                &memory_path,
                &schedules_path,
                &skills_path,
            );
        }
    });

    let addr: SocketAddr = format!("{}:{}", args.bind, args.port).parse().unwrap();
    eprintln!("marrow-dash listening on http://{addr}");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

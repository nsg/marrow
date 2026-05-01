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
    #[arg(long, default_value = "raw_requests.log")]
    raw_log: PathBuf,

    /// Listen port (overrides config)
    #[arg(long)]
    port: Option<u16>,
    /// Bind address (overrides config)
    #[arg(long)]
    bind: Option<String>,

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

    // Load config for dash settings
    let dash_config = marrow::router::RouterConfig::from_file(&args.config)
        .ok()
        .and_then(|c| c.dash);

    // CLI args trump config values, config trumps defaults
    let bind = args.bind.unwrap_or_else(|| {
        dash_config
            .as_ref()
            .and_then(|d| d.bind.clone())
            .unwrap_or_else(|| "127.0.0.1".to_string())
    });
    let port = args
        .port
        .unwrap_or_else(|| dash_config.as_ref().and_then(|d| d.port).unwrap_or(3000));
    let debug_token = dash_config.as_ref().and_then(|d| d.debug_token.clone());

    let state = Arc::new(state::AppState::load(
        &args.log,
        &args.toolbox,
        &args.memory,
        &args.schedules,
        &args.skills,
        &args.config,
    ));
    let mut app = Router::new()
        .merge(frontend::routes())
        .nest("/api", api::routes())
        .with_state(state.clone());

    // Mount debug endpoints only when a debug token is configured
    if let Some(token) = debug_token {
        let debug_state = api::debug::DebugState {
            token,
            events_path: args.log.clone(),
            raw_log_path: args.raw_log.clone(),
        };
        app = app.merge(axum::Router::new().nest("/debug", api::debug::routes(debug_state)));
        eprintln!("[marrow-dash] debug endpoints enabled at /debug/events and /debug/raw");
    }

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

    let addr: SocketAddr = format!("{bind}:{port}").parse().unwrap();
    eprintln!("marrow-dash listening on http://{addr}");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

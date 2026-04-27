use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::state::AppState;

#[derive(Serialize)]
struct ToolsResponse {
    tools: Vec<crate::data::toolbox::ToolInfo>,
    janitor_history: Vec<serde_json::Value>,
}

async fn list_tools(State(state): State<Arc<AppState>>) -> Json<ToolsResponse> {
    let tools = state.toolbox.read().unwrap().clone();
    let janitor_history = state.events.read().unwrap().janitor_history();
    Json(ToolsResponse {
        tools,
        janitor_history,
    })
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/tools", get(list_tools))
}

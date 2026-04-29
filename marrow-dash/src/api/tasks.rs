use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

#[derive(Deserialize)]
struct Pagination {
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
}

fn default_limit() -> usize {
    50
}

#[derive(Serialize)]
struct TasksResponse {
    tasks: Vec<crate::data::events::TaskSummary>,
    total: usize,
}

async fn list_tasks(
    State(state): State<Arc<AppState>>,
    Query(params): Query<Pagination>,
) -> Json<TasksResponse> {
    let events = state.events.read().unwrap_or_else(|e| e.into_inner());
    let (tasks, total) = events.tasks(params.limit, params.offset);
    Json(TasksResponse { tasks, total })
}

async fn get_task(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
) -> Json<serde_json::Value> {
    let events = state.events.read().unwrap_or_else(|e| e.into_inner());
    match events.task_detail(&task_id) {
        Some(detail) => Json(serde_json::to_value(detail).unwrap_or_default()),
        None => Json(serde_json::json!({"error": "task not found"})),
    }
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/tasks", get(list_tasks))
        .route("/tasks/{task_id}", get(get_task))
}

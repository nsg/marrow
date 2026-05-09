use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::state::AppState;

#[derive(Serialize)]
struct SchedulesResponse {
    schedules: Vec<crate::data::schedules::ScheduleInfo>,
    janitor_history: Vec<serde_json::Value>,
}

async fn list_schedules(State(state): State<Arc<AppState>>) -> Json<SchedulesResponse> {
    let schedules = state
        .schedules
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let janitor_history = state
        .events
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .schedule_history();
    Json(SchedulesResponse {
        schedules,
        janitor_history,
    })
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/schedules", get(list_schedules))
}

use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};

use crate::state::AppState;

async fn list_schedules(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<crate::data::schedules::ScheduleInfo>> {
    let schedules = state.schedules.read().unwrap().clone();
    Json(schedules)
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/schedules", get(list_schedules))
}

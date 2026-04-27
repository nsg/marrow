use std::sync::Arc;

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

#[derive(Deserialize)]
struct EventQuery {
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
    #[serde(default)]
    r#type: Option<String>,
}

fn default_limit() -> usize {
    100
}

#[derive(Serialize)]
struct EventsResponse {
    events: Vec<serde_json::Value>,
    total: usize,
}

async fn list_events(
    State(state): State<Arc<AppState>>,
    Query(params): Query<EventQuery>,
) -> Json<EventsResponse> {
    let events = state.events.read().unwrap();
    let (page, total) = events.events_page(params.limit, params.offset, params.r#type.as_deref());
    Json(EventsResponse {
        events: page,
        total,
    })
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/events", get(list_events))
}

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;

use crate::data::backend_errors::BackendErrorsResponse;
use crate::state::AppState;

#[derive(Deserialize)]
struct ErrorQuery {
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
    #[serde(default)]
    status: Option<u16>,
    #[serde(default)]
    role: Option<String>,
}

fn default_limit() -> usize {
    100
}

async fn list_backend_errors(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ErrorQuery>,
) -> Json<BackendErrorsResponse> {
    let data = crate::data::backend_errors::BackendErrorData::load(&state.error_log_path);
    Json(data.query(
        params.limit,
        params.offset,
        params.status,
        params.role.as_deref(),
    ))
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/backend-errors", get(list_backend_errors))
}

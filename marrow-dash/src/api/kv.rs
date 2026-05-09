use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};

use crate::state::AppState;

async fn list_kv(State(state): State<Arc<AppState>>) -> Json<crate::data::kv::KvData> {
    let kv = state.kv.read().unwrap_or_else(|e| e.into_inner()).clone();
    Json(kv)
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/kv", get(list_kv))
}

use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;

const TAIL_BYTES: u64 = 2 * 1024 * 1024; // 2 MB

pub struct DebugState {
    pub token: String,
    pub events_path: PathBuf,
    pub raw_log_path: PathBuf,
}

#[derive(Deserialize)]
struct TokenQuery {
    #[serde(default)]
    token: String,
}

fn read_tail(path: &PathBuf) -> Result<String, StatusCode> {
    let mut file = std::fs::File::open(path).map_err(|_| StatusCode::NOT_FOUND)?;
    let metadata = file
        .metadata()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let file_len = metadata.len();

    if file_len > TAIL_BYTES {
        file.seek(SeekFrom::Start(file_len - TAIL_BYTES))
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        let mut buf = String::new();
        file.read_to_string(&mut buf)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        // Skip to the first newline to avoid a partial line at the start
        if let Some(pos) = buf.find('\n') {
            buf.drain(..=pos);
        }

        Ok(buf)
    } else {
        let mut buf = String::new();
        file.read_to_string(&mut buf)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        Ok(buf)
    }
}

async fn debug_events(
    State(state): State<Arc<DebugState>>,
    Query(params): Query<TokenQuery>,
) -> Response {
    if params.token != state.token {
        return StatusCode::FORBIDDEN.into_response();
    }
    match read_tail(&state.events_path) {
        Ok(content) => (
            StatusCode::OK,
            [("content-type", "text/plain; charset=utf-8")],
            content,
        )
            .into_response(),
        Err(status) => status.into_response(),
    }
}

async fn debug_raw(
    State(state): State<Arc<DebugState>>,
    Query(params): Query<TokenQuery>,
) -> Response {
    if params.token != state.token {
        return StatusCode::FORBIDDEN.into_response();
    }
    match read_tail(&state.raw_log_path) {
        Ok(content) => (
            StatusCode::OK,
            [("content-type", "text/plain; charset=utf-8")],
            content,
        )
            .into_response(),
        Err(status) => status.into_response(),
    }
}

pub fn routes(state: DebugState) -> Router {
    Router::new()
        .route("/events", get(debug_events))
        .route("/raw", get(debug_raw))
        .with_state(Arc::new(state))
}

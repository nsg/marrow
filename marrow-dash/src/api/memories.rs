use std::sync::Arc;

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::data::memory::{MemoryRow, MemoryStats};
use crate::state::AppState;

#[derive(Deserialize)]
struct MemoryQuery {
    #[serde(default)]
    search: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
}

fn default_limit() -> usize {
    50
}

#[derive(Serialize)]
struct MemoriesResponse {
    memories: Vec<MemoryRow>,
    total: usize,
    stats: MemoryStatsResponse,
    search_mode: &'static str,
}

#[derive(Serialize)]
struct MemoryStatsResponse {
    total: usize,
    auto_count: usize,
    user_count: usize,
    embedded_count: usize,
}

async fn list_memories(
    State(state): State<Arc<AppState>>,
    Query(params): Query<MemoryQuery>,
) -> Json<MemoriesResponse> {
    // Grab stats snapshot without holding lock across awaits
    let (stats, search_mode, page, total) = {
        let mem = state.memory.read().unwrap_or_else(|e| e.into_inner());
        let stats = MemoryStatsResponse {
            total: mem.total,
            auto_count: mem.auto_count,
            user_count: mem.user_count,
            embedded_count: mem.embedded_count,
        };

        match &params.search {
            Some(q) if !q.is_empty() => {
                // Text search — no await needed
                let results = mem.search(q);
                let total = results.len();
                let page: Vec<MemoryRow> = results
                    .into_iter()
                    .skip(params.offset)
                    .take(params.limit)
                    .cloned()
                    .collect();
                (stats, "text", page, total)
            }
            _ => {
                let total = mem.memories.len();
                let page: Vec<MemoryRow> = mem
                    .memories
                    .iter()
                    .skip(params.offset)
                    .take(params.limit)
                    .cloned()
                    .collect();
                (stats, "none", page, total)
            }
        }
    };
    // Lock is dropped here

    // Try vector search if we have a query and an embedding backend
    if let Some(ref q) = params.search
        && !q.is_empty()
        && let Some(embed) = &state.embed_backend
        && let Ok(embeddings) = embed.embed(vec![q.clone()]).await
        && let Some(query_vec) = embeddings.first()
    {
        let results = MemoryStats::search_by_embedding(
            &state.memory_dir,
            query_vec,
            params.limit + params.offset,
        );
        if !results.is_empty() {
            let total = results.len();
            let page: Vec<MemoryRow> = results.into_iter().skip(params.offset).collect();
            return Json(MemoriesResponse {
                memories: page,
                total,
                stats,
                search_mode: "vector",
            });
        }
    }

    Json(MemoriesResponse {
        memories: page,
        total,
        stats,
        search_mode,
    })
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/memories", get(list_memories))
}

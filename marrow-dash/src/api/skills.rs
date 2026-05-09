use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::state::AppState;

#[derive(Serialize)]
struct SkillsResponse {
    skills: Vec<crate::data::skills::SkillInfo>,
    janitor_history: Vec<serde_json::Value>,
}

async fn list_skills(State(state): State<Arc<AppState>>) -> Json<SkillsResponse> {
    let skills = state
        .skills
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let janitor_history = state
        .events
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .skills_history();
    Json(SkillsResponse {
        skills,
        janitor_history,
    })
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/skills", get(list_skills))
}

use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};

use crate::state::AppState;

async fn list_skills(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<crate::data::skills::SkillInfo>> {
    let skills = state
        .skills
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    Json(skills)
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/skills", get(list_skills))
}

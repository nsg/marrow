use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::state::AppState;

#[derive(Serialize)]
struct OverviewResponse {
    #[serde(flatten)]
    stats: crate::data::events::OverviewStats,
    memory: MemoryOverview,
    toolbox: ToolboxOverview,
    schedules: ScheduleOverview,
    skills_count: usize,
    config: crate::data::config::ConfigInfo,
}

#[derive(Serialize)]
struct MemoryOverview {
    total: usize,
    auto_count: usize,
    user_count: usize,
    embedded_count: usize,
}

#[derive(Serialize)]
struct ToolboxOverview {
    total: usize,
    validated: usize,
    unvalidated: usize,
}

#[derive(Serialize)]
struct ScheduleOverview {
    total: usize,
    enabled: usize,
    disabled: usize,
}

async fn overview(State(state): State<Arc<AppState>>) -> Json<OverviewResponse> {
    let stats = state
        .events
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .overview();

    let mem = state.memory.read().unwrap_or_else(|e| e.into_inner());
    let memory = MemoryOverview {
        total: mem.total,
        auto_count: mem.auto_count,
        user_count: mem.user_count,
        embedded_count: mem.embedded_count,
    };

    let tb = state.toolbox.read().unwrap_or_else(|e| e.into_inner());
    let validated = tb.iter().filter(|t| t.validated).count();
    let toolbox = ToolboxOverview {
        total: tb.len(),
        validated,
        unvalidated: tb.len() - validated,
    };

    let sc = state.schedules.read().unwrap_or_else(|e| e.into_inner());
    let enabled = sc.iter().filter(|s| s.enabled).count();
    let schedules_overview = ScheduleOverview {
        total: sc.len(),
        enabled,
        disabled: sc.len() - enabled,
    };

    let skills_count = state.skills.read().unwrap_or_else(|e| e.into_inner()).len();

    // Clone config for serialization
    let config = crate::data::config::ConfigInfo {
        roles: state
            .config
            .roles
            .iter()
            .map(|r| crate::data::config::RoleInfo {
                name: r.name.clone(),
                provider: r.provider.clone(),
                model: r.model.clone(),
            })
            .collect(),
    };

    Json(OverviewResponse {
        stats,
        memory,
        toolbox,
        schedules: schedules_overview,
        skills_count,
        config,
    })
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/overview", get(overview))
}

use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::state::AppState;

#[derive(Serialize)]
struct OverviewResponse {
    version: &'static str,
    #[serde(flatten)]
    stats: crate::data::events::OverviewStats,
    memory: MemoryOverview,
    toolbox: ToolboxOverview,
    schedules: ScheduleOverview,
    skills_count: usize,
    kv_count: usize,
    events_count: usize,
    backend_errors_count: usize,
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
    let events = crate::data::events::EventData::load(&state.log_path);
    let mem = crate::data::memory::MemoryStats::load(&state.memory_path);
    let tb = crate::data::toolbox::load(&state.toolbox_path);
    let sc = crate::data::schedules::load(&state.schedules_path);
    let skills = crate::data::skills::load(&state.skills_path);
    let kv = crate::data::kv::KvData::load(&state.memory_path);
    let backend_errors = crate::data::backend_errors::BackendErrorData::load(&state.error_log_path);

    let stats = events.overview();

    let memory = MemoryOverview {
        total: mem.total,
        auto_count: mem.auto_count,
        user_count: mem.user_count,
        embedded_count: mem.embedded_count,
    };

    let validated = tb.iter().filter(|t| t.validated).count();
    let toolbox = ToolboxOverview {
        total: tb.len(),
        validated,
        unvalidated: tb.len() - validated,
    };

    let enabled = sc.iter().filter(|s| s.enabled).count();
    let schedules_overview = ScheduleOverview {
        total: sc.len(),
        enabled,
        disabled: sc.len() - enabled,
    };

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
        version: env!("CARGO_PKG_VERSION"),
        stats,
        memory,
        toolbox,
        schedules: schedules_overview,
        skills_count: skills.len(),
        kv_count: kv.entries.len(),
        events_count: events.entries.len(),
        backend_errors_count: backend_errors.total(),
        config,
    })
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/overview", get(overview))
}

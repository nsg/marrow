use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

mod events;
mod memories;
mod overview;
mod schedules;
mod tasks;
mod tools;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .merge(overview::routes())
        .merge(tasks::routes())
        .merge(tools::routes())
        .merge(memories::routes())
        .merge(schedules::routes())
        .merge(events::routes())
}

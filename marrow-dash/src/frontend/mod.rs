use std::sync::Arc;

use axum::Router;
use axum::response::Html;
use axum::routing::get;

use crate::state::AppState;

const INDEX_HTML: &str = include_str!("index.html");

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/", get(index))
}

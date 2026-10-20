//! Web admin (placeholder until the UI module is written).

use axum::Router;

use crate::app::AppRef;

pub fn router() -> Router<AppRef> {
    Router::new()
}

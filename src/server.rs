//! HTTP server: routes for the reader APIs, cached images, icons, health and admin UI.

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use rusqlite::OptionalExtension;
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;

use crate::api::{greader, ttrss};
use crate::app::AppRef;
use crate::util;

pub fn router(app: AppRef) -> Router {
    let gr = any(greader::handle);
    let tt = any(ttrss::handle);
    let api = Router::new()
        // Google Reader API (FreshRSS / FreshAPI / root styles)
        .route("/api/greader.php", gr.clone())
        .route("/api/greader.php/", gr.clone())
        .route("/api/greader.php/{*rest}", gr.clone())
        .route("/plugins.local/freshapi/api/greader.php", gr.clone())
        .route("/plugins.local/freshapi/api/greader.php/{*rest}", gr.clone())
        .route("/tt-rss/plugins.local/freshapi/api/greader.php", gr.clone())
        .route("/tt-rss/plugins.local/freshapi/api/greader.php/{*rest}", gr.clone())
        .route("/accounts/{*rest}", gr.clone())
        .route("/reader/{*rest}", gr)
        // TT-RSS API
        .route("/api", tt.clone())
        .route("/api/", tt.clone())
        .route("/api/index.php", tt.clone())
        .route("/tt-rss/api", tt.clone())
        .route("/tt-rss/api/", tt.clone())
        .route("/tt-rss/api/index.php", tt)
        // Icons (TT-RSS conventions + GReader iconUrl)
        .route("/feed-icons/{name}", get(feed_icon))
        .route("/tt-rss/feed-icons/{name}", get(feed_icon))
        .route("/public.php", get(public_php))
        .route("/tt-rss/public.php", get(public_php))
        // Cached images
        .route("/img/{prefix}/{name}", get(image))
        .route("/healthz", get(health));
    Router::new()
        .merge(api)
        .merge(crate::web::router())
        .layer(axum::extract::DefaultBodyLimit::max(32 * 1024 * 1024))
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(app)
}

async fn health(State(app): State<AppRef>) -> Response {
    match app.db.call(|c| Ok(c.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))?)).await {
        Ok(_) => (StatusCode::OK, "ok").into_response(),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, format!("db error: {e}")).into_response(),
    }
}

async fn serve_icon(app: &AppRef, id: i64, headers: &HeaderMap) -> Response {
    let icon: Option<(String, Vec<u8>, String)> = app
        .db
        .call(move |c| {
            Ok(c.query_row("SELECT mime, data, hash FROM feed_icons WHERE feed_id = ? AND length(data) > 0", [id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .optional()?)
        })
        .await
        .ok()
        .flatten();
    let Some((mime, data, hash)) = icon else {
        return (StatusCode::NOT_FOUND, "File not found.").into_response();
    };
    let etag = format!("\"{hash}\"");
    if headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) == Some(etag.as_str()) {
        return StatusCode::NOT_MODIFIED.into_response();
    }
    let mut r = Response::new(Body::from(data));
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_str(&mime).unwrap_or(HeaderValue::from_static("image/x-icon")));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=86400"));
    h.insert(header::ETAG, HeaderValue::from_str(&etag).unwrap_or(HeaderValue::from_static("\"0\"")));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    r
}

async fn feed_icon(State(app): State<AppRef>, Path(name): Path<String>, headers: HeaderMap) -> Response {
    let id: i64 = name.split('.').next().and_then(|s| s.parse().ok()).unwrap_or(0);
    serve_icon(&app, id, &headers).await
}

#[derive(serde::Deserialize)]
struct PublicQuery {
    op: Option<String>,
    id: Option<String>,
}

async fn public_php(State(app): State<AppRef>, Query(q): Query<PublicQuery>, headers: HeaderMap) -> Response {
    if q.op.as_deref() == Some("feed_icon") {
        let id: i64 = q.id.as_deref().and_then(|s| s.split('.').next()).and_then(|s| s.parse().ok()).unwrap_or(0);
        return serve_icon(&app, id, &headers).await;
    }
    (StatusCode::NOT_FOUND, "File not found.").into_response()
}

async fn image(State(app): State<AppRef>, Path((prefix, name)): Path<(String, String)>, headers: HeaderMap) -> Response {
    let Some((path, mime)) = app.images.resolve_request(&name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !name.starts_with(&prefix) || prefix.len() != 2 {
        return StatusCode::NOT_FOUND.into_response();
    }
    let etag = format!("\"{}\"", &name[..name.find('.').unwrap_or(name.len())]);
    if headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) == Some(etag.as_str()) {
        return StatusCode::NOT_MODIFIED.into_response();
    }
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let mut r = Response::new(Body::from(bytes));
            let h = r.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=31536000, immutable"));
            h.insert(header::ETAG, HeaderValue::from_str(&etag).unwrap_or(HeaderValue::from_static("\"0\"")));
            h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
            h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'none'; sandbox"));
            h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
            r
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Run the HTTP server and background workers until a shutdown signal arrives.
pub async fn run(app: AppRef) -> anyhow::Result<()> {
    if !app.cfg.no_workers {
        tokio::spawn(crate::feeds::fetcher::run_scheduler(app.clone()));
        tokio::spawn(crate::pipeline::run_dispatcher(app.clone()));
        tokio::spawn(crate::pipeline::run_page_checker(app.clone()));
        tokio::spawn(crate::maintenance::run_maintenance(app.clone()));
    }
    let listener = tokio::net::TcpListener::bind(app.cfg.listen).await?;
    tracing::info!("RillRSS {} listening on http://{}", env!("CARGO_PKG_VERSION"), listener.local_addr()?);
    app.event(crate::app::Level::Info, "system", Default::default(), format!("服务启动（v{}）", env!("CARGO_PKG_VERSION"))).await;
    let shutdown = app.shutdown.clone();
    let app2 = app.clone();
    axum::serve(listener, router(app.clone()).into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("shutting down");
            app2.shutdown.cancel();
        })
        .await?;
    shutdown.cancel();
    // Give in-flight work a moment to record its state (leases make the rest safe).
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _ = app.db.call(|c| Ok(c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?)).await;
    tracing::info!("stopped at {}", util::fmt_ts(Some(util::now())));
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}

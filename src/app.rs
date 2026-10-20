//! Shared application state.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Result;
use rusqlite::params;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::content::convert::Converter;
use crate::content::images::ImageStore;
use crate::content::translate::Translator;
use crate::db::Db;
use crate::http::HttpClients;
use crate::settings::SettingsStore;
use crate::util;

pub struct App {
    pub cfg: Config,
    pub db: Db,
    pub settings: SettingsStore,
    pub http: HttpClients,
    pub translator: Translator,
    pub converter: Converter,
    pub images: ImageStore,
    /// Wakes the processing dispatcher (new revisions).
    pub wake_pipeline: Notify,
    /// Wakes the feed scheduler (feed added / refresh requested).
    pub wake_fetcher: Notify,
    pub shutdown: CancellationToken,
    pub started_at: i64,
    /// Failed login attempts per client IP (brute-force throttling).
    pub login_failures: Mutex<HashMap<IpAddr, (u32, Instant)>>,
}

pub type AppRef = Arc<App>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn as_str(&self) -> &'static str {
        match self {
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
}

/// Target of an event log line.
#[derive(Debug, Clone, Copy, Default)]
pub struct EventRef {
    pub feed_id: Option<i64>,
    pub story_id: Option<i64>,
    pub item_id: Option<i64>,
}

impl EventRef {
    pub fn feed(id: i64) -> EventRef {
        EventRef { feed_id: Some(id), ..Default::default() }
    }
    pub fn story(feed_id: i64, story_id: i64) -> EventRef {
        EventRef { feed_id: Some(feed_id), story_id: Some(story_id), item_id: None }
    }
}

impl App {
    pub async fn new(cfg: Config) -> Result<AppRef> {
        std::fs::create_dir_all(&cfg.data_dir)?;
        let db = Db::open(&cfg.db_path())?;
        let settings = SettingsStore::load(db.clone(), &cfg).await?;
        let http = HttpClients::new(cfg.allow_private_networks)?;
        let translator = Translator::new(db.clone(), http.api.clone());
        let images = ImageStore::new(cfg.images_dir(), db.clone())?;
        let app = Arc::new(App {
            cfg,
            db,
            settings,
            http,
            translator,
            converter: Converter::default(),
            images,
            wake_pipeline: Notify::new(),
            wake_fetcher: Notify::new(),
            shutdown: CancellationToken::new(),
            started_at: util::now(),
            login_failures: Mutex::new(HashMap::new()),
        });
        crate::auth::bootstrap(&app).await?;
        Ok(app)
    }

    /// Record a diagnostic event (also traced).
    pub async fn event(&self, level: Level, kind: &str, r: EventRef, message: impl Into<String>) {
        let message: String = message.into();
        match level {
            Level::Info => tracing::info!(kind, feed = ?r.feed_id, story = ?r.story_id, "{message}"),
            Level::Warn => tracing::warn!(kind, feed = ?r.feed_id, story = ?r.story_id, "{message}"),
            Level::Error => tracing::error!(kind, feed = ?r.feed_id, story = ?r.story_id, "{message}"),
        }
        let kind = kind.to_string();
        let msg = util::truncate_chars(&message, 2000);
        let res = self
            .db
            .call(move |c| {
                c.execute(
                    "INSERT INTO events (ts, level, kind, feed_id, story_id, item_id, message) VALUES (?, ?, ?, ?, ?, ?, ?)",
                    params![util::now(), level.as_str(), kind, r.feed_id, r.story_id, r.item_id, msg],
                )?;
                Ok(())
            })
            .await;
        if let Err(e) = res {
            tracing::warn!("failed to record event: {e:#}");
        }
    }

    /// Public base URL: settings override, env, or None (derive from request).
    pub async fn configured_base_url(&self) -> Option<String> {
        let s = self.settings.get().await;
        let b = if !s.base_url.trim().is_empty() { s.base_url.trim().to_string() } else { self.cfg.base_url.trim().to_string() };
        if b.is_empty() { None } else { Some(b.trim_end_matches('/').to_string()) }
    }
}

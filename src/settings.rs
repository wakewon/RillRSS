//! Runtime settings (stored as JSON in the `settings` table, editable in the web admin)
//! and per-feed options.

use std::sync::Arc;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::config::Config;
use crate::db::Db;

pub const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (compatible; RillRSS/0.1; +https://github.com/rillrss/rillrss)";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
#[derive(Default)]
pub struct Settings {
    /// Public base URL override (takes precedence over RILL_BASE_URL when set).
    pub base_url: String,
    pub fetch: FetchSettings,
    pub retention: RetentionSettings,
    pub fulltext: FulltextSettings,
    pub translation: TranslationSettings,
    pub images: ImageSettings,
    pub updates: UpdateSettings,
}


#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct FetchSettings {
    /// Default polling interval in minutes.
    pub default_interval_min: u32,
    /// Lower bound for any feed interval, in minutes.
    pub min_interval_min: u32,
    /// Concurrent feed fetches.
    pub concurrency: u32,
    /// Concurrent requests to the same host (feeds + article pages).
    pub per_host_concurrency: u32,
    /// Minimum delay between two article-page requests to the same host (ms).
    pub per_host_delay_ms: u32,
    /// Concurrent article page fetches (full text / update checks).
    pub page_concurrency: u32,
    pub timeout_secs: u32,
    pub user_agent: String,
    pub max_feed_mb: u32,
    pub max_page_mb: u32,
    /// Number of entries processed when a feed is fetched for the first time.
    pub initial_items: u32,
    /// Entries older than this (by publication date) are ignored. 0 = no limit.
    pub max_entry_age_days: u32,
    /// Upper bound of the error back-off, in hours.
    pub max_backoff_hours: u32,
}

impl Default for FetchSettings {
    fn default() -> Self {
        FetchSettings {
            default_interval_min: 30,
            min_interval_min: 5,
            concurrency: 6,
            per_host_concurrency: 2,
            per_host_delay_ms: 500,
            page_concurrency: 4,
            timeout_secs: 30,
            user_agent: DEFAULT_USER_AGENT.into(),
            max_feed_mb: 20,
            max_page_mb: 10,
            initial_items: 30,
            max_entry_age_days: 14,
            max_backoff_hours: 24,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RetentionSettings {
    /// Delete articles older than N days (by arrival time). 0 = keep forever.
    pub days: u32,
    /// Never delete unread articles.
    pub keep_unread: bool,
    /// Keep at most N articles per feed (oldest deleted first). 0 = unlimited.
    pub max_items_per_feed: u32,
    /// Delete superseded (old) versions of updated stories after N days.
    pub superseded_days: u32,
    /// Drop translation cache entries unused for N days.
    pub translation_cache_days: u32,
    /// Keep at most N diagnostic events.
    pub event_rows: u32,
}

impl Default for RetentionSettings {
    fn default() -> Self {
        RetentionSettings {
            days: 14,
            keep_unread: false,
            max_items_per_feed: 0,
            superseded_days: 14,
            translation_cache_days: 30,
            event_rows: 5000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct FulltextSettings {
    /// External extractor endpoint, `{url}` is replaced by the URL-encoded article URL.
    /// Response may be HTML or JSON with a `content`/`html` field.
    pub external_url: String,
    /// Extracted text shorter than this is considered a failure (falls back to feed content).
    pub min_text_chars: u32,
}

impl Default for FulltextSettings {
    fn default() -> Self {
        FulltextSettings { external_url: String::new(), min_text_chars: 200 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TranslationSettings {
    /// OpenAI-compatible base URL, e.g. https://api.openai.com/v1
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    /// Maximum concurrent requests to the provider.
    pub max_concurrency: u32,
    /// Request rate limit (requests per minute). 0 = unlimited.
    pub requests_per_minute: u32,
    pub timeout_secs: u32,
    /// Sampling temperature; negative = do not send.
    pub temperature: f32,
    /// Maximum source characters per request.
    pub max_batch_chars: u32,
    /// Maximum segments per request.
    pub max_batch_segments: u32,
    /// `max_tokens` sent with each request (0 = do not send).
    pub max_tokens: u32,
    /// Ask for `response_format: json_object`.
    pub json_mode: bool,
    /// Extra instructions appended to the system prompt.
    pub extra_prompt: String,
    /// Extra JSON object merged into every request body (e.g. {"enable_thinking": false}).
    pub extra_body: String,
    /// Attempts per article before applying the failure policy.
    pub max_attempts: u32,
    /// What to do when translation keeps failing: "publish_original" or "hold".
    pub on_failure: String,
}

impl Default for TranslationSettings {
    fn default() -> Self {
        TranslationSettings {
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            max_concurrency: 2,
            requests_per_minute: 30,
            timeout_secs: 180,
            temperature: 0.2,
            max_batch_chars: 4000,
            max_batch_segments: 16,
            max_tokens: 0,
            json_mode: false,
            extra_prompt: String::new(),
            extra_body: String::new(),
            max_attempts: 6,
            on_failure: "publish_original".into(),
        }
    }
}

impl TranslationSettings {
    pub fn is_configured(&self) -> bool {
        !self.base_url.trim().is_empty() && !self.model.trim().is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ImageSettings {
    pub max_mb: u32,
    pub max_per_article: u32,
    /// Cache size cap (MB). Past it the images of the oldest articles are dropped (those
    /// articles point at the original images again); new images are still cached.
    /// 0 = unlimited.
    pub max_total_mb: u32,
    pub timeout_secs: u32,
    pub concurrency: u32,
    /// Images smaller than this in both dimensions (tracking pixels) are dropped.
    pub min_dimension: u32,
}

impl Default for ImageSettings {
    fn default() -> Self {
        ImageSettings {
            max_mb: 15,
            max_per_article: 40,
            max_total_mb: 4096,
            timeout_secs: 30,
            concurrency: 4,
            min_dimension: 3,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct UpdateSettings {
    /// Minimum minutes between two published versions of one story.
    pub min_interval_min: u32,
    /// Maximum published versions per story (including the first).
    pub max_versions: u32,
    /// Only track updates for stories first seen less than N hours ago.
    pub track_hours: u32,
    /// Original-page re-check schedule (minutes after first seen) for "page" mode.
    pub page_check_minutes: Vec<u32>,
    pub min_change_ratio: f64,
    pub min_changed_tokens: u32,
    pub major_added_tokens: u32,
    /// A changed title alone (without content change) triggers a new version.
    pub title_only_triggers: bool,
    /// Prefix added to the title of updated versions.
    pub title_prefix: String,
    /// Insert a small "updated" notice at the top of updated versions.
    pub notice: bool,
    /// Text of the notice.
    pub notice_label: String,
    /// Label for the previous version's date in the notice.
    pub previous_label: String,
    /// Updated versions are unread even if the previous version was read.
    pub mark_unread: bool,
    /// Use the update time as the publication date of updated versions (so they sort
    /// to the top in readers). When false the original date is kept.
    pub redate: bool,
    /// Hide superseded versions from article lists (they stay fetchable by id). Off by
    /// default: some clients (Read You, Newsboat) only learn that the old version is
    /// read while it is still listed.
    pub hide_superseded: bool,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        UpdateSettings {
            min_interval_min: 30,
            max_versions: 5,
            track_hours: 48,
            page_check_minutes: vec![30, 90, 240, 600, 1440, 2880],
            min_change_ratio: 0.10,
            min_changed_tokens: 25,
            major_added_tokens: 80,
            title_only_triggers: false,
            title_prefix: "[更新]".into(),
            notice: true,
            notice_label: "本文已更新".into(),
            previous_label: "首发于".into(),
            mark_unread: true,
            redate: true,
            hide_superseded: false,
        }
    }
}

impl UpdateSettings {
    pub fn thresholds(&self) -> crate::util::text::ChangeThresholds {
        crate::util::text::ChangeThresholds {
            min_change_ratio: self.min_change_ratio,
            min_changed_tokens: self.min_changed_tokens as usize,
            major_added_tokens: self.major_added_tokens as usize,
        }
    }
}

// ---------------------------------------------------------------------------
// Per-feed options
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum FulltextMode {
    #[default]
    Off,
    /// Built-in Readability port.
    Readability,
    /// CSS selector(s) given in `selector`.
    Selector,
    /// External extractor service (Settings.fulltext.external_url).
    External,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum TranslateMode {
    /// Replace the text with the translation.
    #[default]
    Replace,
    /// Keep the original and add the translation after each paragraph.
    Bilingual,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum UpdateMode {
    /// Never publish new versions.
    Off,
    /// Detect changes of the feed entry (title/content); with full text enabled the
    /// original page is re-extracted when the entry changes.
    #[default]
    Feed,
    /// Additionally re-check the original page on a schedule.
    Page,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct FeedOptions {
    pub fulltext: FulltextMode,
    /// CSS selector for `selector` mode (comma separated list allowed).
    pub selector: String,
    /// CSS selector of elements removed from the extracted content.
    pub remove_selector: String,
    /// OpenCC conversion: "" (off), t2s, tw2s, hk2s, tw2sp, s2t, s2tw, s2hk, s2twp, t2tw, t2hk.
    pub opencc: String,
    pub translate: bool,
    /// Target language (e.g. "zh-CN", "en", "ja").
    pub target_lang: String,
    pub translate_mode: TranslateMode,
    pub translate_title: bool,
    pub cache_images: bool,
    pub updates: UpdateMode,
    /// Override retention days (None = global).
    pub retention_days: Option<u32>,
    /// Custom User-Agent for this feed and its pages.
    pub user_agent: String,
}

impl Default for FeedOptions {
    fn default() -> Self {
        FeedOptions {
            fulltext: FulltextMode::Off,
            selector: String::new(),
            remove_selector: String::new(),
            opencc: String::new(),
            translate: false,
            target_lang: "zh-CN".into(),
            translate_mode: TranslateMode::Replace,
            translate_title: true,
            cache_images: false,
            updates: UpdateMode::Feed,
            retention_days: None,
            user_agent: String::new(),
        }
    }
}

impl FeedOptions {
    pub fn from_json(s: &str) -> FeedOptions {
        serde_json::from_str(s).unwrap_or_default()
    }
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }
    pub fn opencc_enabled(&self) -> bool {
        !self.opencc.trim().is_empty() && self.opencc != "off"
    }
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

const SETTINGS_KEY: &str = "app";

pub fn get_raw(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM settings WHERE key = ?", [key], |r| r.get(0))
        .optional()?)
}

pub fn set_raw(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO settings(key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// Shared, hot-reloadable settings.
#[derive(Clone)]
pub struct SettingsStore {
    inner: Arc<RwLock<Arc<Settings>>>,
    db: Db,
}

impl SettingsStore {
    pub async fn load(db: Db, cfg: &Config) -> Result<SettingsStore> {
        let cfg = cfg.clone();
        let settings = db
            .call(move |conn| {
                let mut s: Settings = match get_raw(conn, SETTINGS_KEY)? {
                    Some(v) => serde_json::from_str(&v).unwrap_or_default(),
                    None => Settings::default(),
                };
                // Environment provides defaults for translation credentials.
                if s.translation.base_url.is_empty() && !cfg.translate_base_url.is_empty() {
                    s.translation.base_url = cfg.translate_base_url.clone();
                }
                if s.translation.api_key.is_empty() && !cfg.translate_api_key.is_empty() {
                    s.translation.api_key = cfg.translate_api_key.clone();
                }
                if s.translation.model.is_empty() && !cfg.translate_model.is_empty() {
                    s.translation.model = cfg.translate_model.clone();
                }
                Ok(s)
            })
            .await?;
        Ok(SettingsStore { inner: Arc::new(RwLock::new(Arc::new(settings))), db })
    }

    pub async fn get(&self) -> Arc<Settings> {
        self.inner.read().await.clone()
    }

    pub async fn save(&self, s: Settings) -> Result<()> {
        let json = serde_json::to_string(&s)?;
        self.db.call(move |conn| set_raw(conn, SETTINGS_KEY, &json)).await?;
        *self.inner.write().await = Arc::new(s);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_options_roundtrip_and_defaults() {
        let o = FeedOptions::from_json("{\"translate\":true,\"fulltext\":\"readability\"}");
        assert!(o.translate);
        assert_eq!(o.fulltext, FulltextMode::Readability);
        assert_eq!(o.target_lang, "zh-CN");
        assert_eq!(o.updates, UpdateMode::Feed);
        let back = FeedOptions::from_json(&o.to_json());
        assert_eq!(back, o);
        assert_eq!(FeedOptions::from_json("garbage"), FeedOptions::default());
    }

    #[test]
    fn settings_partial_json() {
        let s: Settings = serde_json::from_str("{\"fetch\":{\"concurrency\":3}}").unwrap();
        assert_eq!(s.fetch.concurrency, 3);
        assert_eq!(s.fetch.default_interval_min, 30);
        assert_eq!(s.updates.max_versions, 5);
    }
}

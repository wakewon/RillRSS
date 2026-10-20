//! Feed scheduling and fetching.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{OptionalExtension, params};
use tokio::sync::Semaphore;
use url::Url;

use super::ingest::{IngestContext, IngestReport, ingest_entries};
use super::parse::{ParsedFeed, parse_feed};
use crate::app::{AppRef, EventRef, Level};
use crate::http::{self, Fetched};
use crate::settings::{FeedOptions, Settings};
use crate::util::{self, urlnorm};

const FEED_ACCEPT: &str = "application/rss+xml, application/atom+xml, application/feed+json, application/json;q=0.9, application/xml;q=0.9, text/xml;q=0.8, */*;q=0.5";

#[derive(Debug, Clone)]
pub struct FeedRow {
    pub id: i64,
    pub url: String,
    pub title: String,
    pub title_locked: bool,
    pub site_url: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub fetch_interval: Option<i64>,
    pub error_count: i64,
    pub options: FeedOptions,
}

pub fn load_feed(conn: &rusqlite::Connection, id: i64) -> Result<Option<FeedRow>> {
    Ok(conn
        .query_row(
            "SELECT id, url, title, title_locked, site_url, etag, last_modified, fetch_interval, fetch_error_count, options FROM feeds WHERE id = ?",
            [id],
            |r| {
                Ok(FeedRow {
                    id: r.get(0)?,
                    url: r.get(1)?,
                    title: r.get(2)?,
                    title_locked: r.get::<_, i64>(3)? != 0,
                    site_url: r.get(4)?,
                    etag: r.get(5)?,
                    last_modified: r.get(6)?,
                    fetch_interval: r.get(7)?,
                    error_count: r.get(8)?,
                    options: FeedOptions::from_json(&r.get::<_, String>(9)?),
                })
            },
        )
        .optional()?)
}

pub fn user_agent(settings: &Settings, opts: &FeedOptions) -> String {
    if !opts.user_agent.trim().is_empty() {
        opts.user_agent.trim().to_string()
    } else if !settings.fetch.user_agent.trim().is_empty() {
        settings.fetch.user_agent.trim().to_string()
    } else {
        crate::settings::DEFAULT_USER_AGENT.to_string()
    }
}

fn interval_secs(settings: &Settings, feed_interval: Option<i64>) -> i64 {
    let min = settings.fetch.min_interval_min.max(1) as i64;
    let mins = feed_interval.unwrap_or(settings.fetch.default_interval_min as i64).max(min);
    mins * 60
}

fn jitter(secs: i64) -> i64 {
    let span = (secs / 10).max(1);
    secs + (rand::random::<u64>() % (2 * span as u64 + 1)) as i64 - span
}

#[derive(Debug, Default)]
pub struct FetchOutcome {
    pub status: u16,
    pub not_modified: bool,
    pub report: IngestReport,
}

/// Fetch, parse and ingest one feed. Updates the feed's schedule and error state.
pub async fn fetch_feed(app: &AppRef, feed_id: i64) -> Result<FetchOutcome> {
    let settings = app.settings.get().await;
    let feed = app
        .db
        .call(move |c| load_feed(c, feed_id))
        .await?
        .ok_or_else(|| anyhow!("feed {feed_id} not found"))?;
    let started = std::time::Instant::now();
    let result = fetch_and_ingest(app, &settings, &feed).await;
    let now = util::now();
    let elapsed_ms = started.elapsed().as_millis() as i64;
    let interval = interval_secs(&settings, feed.fetch_interval);

    match result {
        Ok((outcome, meta)) => {
            let next = now + jitter(interval);
            let status = outcome.status as i64;
            app.db
                .call(move |c| {
                    c.execute(
                        "UPDATE feeds SET last_fetch_at = ?, last_success_at = ?, next_fetch_at = ?, fetch_error_count = 0,
                             last_error = NULL, last_status = ?, last_duration_ms = ?, lease_until = NULL,
                             etag = COALESCE(?, etag), last_modified = COALESCE(?, last_modified), updated_at = ?
                         WHERE id = ?",
                        params![now, now, next, status, elapsed_ms, meta.etag, meta.last_modified, now, feed_id],
                    )?;
                    Ok(())
                })
                .await?;
            let r = &outcome.report;
            if r.new_stories + r.updates > 0 {
                app.wake_pipeline.notify_one();
            }
            if r.new_stories + r.updates + r.skipped > 0 {
                app.event(
                    Level::Info,
                    "fetch",
                    EventRef::feed(feed_id),
                    format!(
                        "fetched {} entries: {} new, {} updated, {} skipped, {} ignored changes ({} ms)",
                        r.entries, r.new_stories, r.updates, r.skipped, r.ignored_changes, elapsed_ms
                    ),
                )
                .await;
            }
            Ok(outcome)
        }
        Err(err) => {
            let errors = feed.error_count + 1;
            let max_backoff = settings.fetch.max_backoff_hours.max(1) as i64 * 3600;
            let mut delay = (interval * 2i64.pow(errors.clamp(0, 8) as u32 - 1)).min(max_backoff).max(interval);
            if let Some(ra) = err.downcast_ref::<RetryAfter>() {
                delay = delay.max(ra.0 as i64).min(max_backoff.max(ra.0 as i64));
            }
            let next = now + jitter(delay);
            let msg = format!("{err:#}");
            let status = err.downcast_ref::<HttpStatus>().map(|s| s.0 as i64);
            let msg2 = msg.clone();
            app.db
                .call(move |c| {
                    c.execute(
                        "UPDATE feeds SET last_fetch_at = ?, next_fetch_at = ?, fetch_error_count = ?, last_error = ?,
                             last_status = ?, last_duration_ms = ?, lease_until = NULL, updated_at = ?
                         WHERE id = ?",
                        params![now, next, errors, msg2, status, elapsed_ms, now, feed_id],
                    )?;
                    Ok(())
                })
                .await?;
            app.event(
                if errors >= 3 { Level::Error } else { Level::Warn },
                "fetch",
                EventRef::feed(feed_id),
                format!("fetch failed ({errors} in a row, next try in {} min): {msg}", delay / 60),
            )
            .await;
            Err(err)
        }
    }
}

#[derive(Debug)]
struct HttpStatus(u16);
impl std::fmt::Display for HttpStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {}", self.0)
    }
}
impl std::error::Error for HttpStatus {}

#[derive(Debug)]
struct RetryAfter(u64);
impl std::fmt::Display for RetryAfter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "retry after {} s", self.0)
    }
}
impl std::error::Error for RetryAfter {}

#[derive(Default)]
struct CacheMeta {
    etag: Option<String>,
    last_modified: Option<String>,
}

async fn download(app: &AppRef, settings: &Settings, feed: &FeedRow) -> Result<Fetched> {
    let url = Url::parse(&feed.url).with_context(|| format!("invalid feed URL {}", feed.url))?;
    http::check_url(&url, false)?;
    let host = url.host_str().unwrap_or("").to_string();
    let _permit = app
        .http
        .hosts
        .acquire(&host, settings.fetch.per_host_concurrency, Duration::from_millis(200))
        .await;
    let mut req = app
        .http
        .feed
        .get(url)
        .header(reqwest::header::USER_AGENT, user_agent(settings, &feed.options))
        .header(reqwest::header::ACCEPT, FEED_ACCEPT)
        .timeout(Duration::from_secs(settings.fetch.timeout_secs.max(5) as u64));
    if let Some(e) = &feed.etag {
        req = req.header(reqwest::header::IF_NONE_MATCH, e);
    }
    if let Some(lm) = &feed.last_modified {
        req = req.header(reqwest::header::IF_MODIFIED_SINCE, lm);
    }
    let resp = req.send().await.context("request failed")?;
    http::read_limited(resp, settings.fetch.max_feed_mb.max(1) as usize * 1024 * 1024).await
}

async fn fetch_and_ingest(app: &AppRef, settings: &Arc<Settings>, feed: &FeedRow) -> Result<(FetchOutcome, CacheMeta)> {
    let f = download(app, settings, feed).await?;
    if f.status == 304 {
        return Ok((FetchOutcome { status: 304, not_modified: true, ..Default::default() }, CacheMeta::default()));
    }
    if f.status == 429 || f.status == 503 {
        let ra = f.header("retry-after").and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0);
        return Err(anyhow!(RetryAfter(ra)).context(HttpStatus(f.status)));
    }
    if f.status >= 400 {
        return Err(anyhow!(HttpStatus(f.status)));
    }
    let meta = CacheMeta { etag: f.header("etag"), last_modified: f.header("last-modified") };
    let base = f.final_url.to_string();
    let body = f.body;
    let parsed: ParsedFeed = tokio::task::spawn_blocking(move || parse_feed(&body, &base)).await??;

    let now = util::now();
    let feed_id = feed.id;
    let opts = feed.options.clone();
    let settings2 = settings.clone();
    let title_locked = feed.title_locked;
    let report = app
        .db
        .write(move |tx| {
            // Feed metadata
            if !title_locked && !parsed.title.trim().is_empty() {
                tx.execute("UPDATE feeds SET title = ? WHERE id = ?", params![parsed.title.trim(), feed_id])?;
            }
            tx.execute(
                "UPDATE feeds SET site_url = COALESCE(?, site_url), description = COALESCE(?, description) WHERE id = ?",
                params![parsed.site_url, parsed.description.as_deref().map(|d| util::truncate_chars(d, 1000)), feed_id],
            )?;
            let ctx = IngestContext { feed_id, opts: &opts, settings: &settings2, now };
            ingest_entries(tx, &ctx, &parsed.entries)
        })
        .await?;

    // Fetch the favicon occasionally (in the background).
    let app2 = app.clone();
    let feed2 = feed.clone();
    tokio::spawn(async move {
        if let Err(e) = super::icons::refresh_icon_if_stale(&app2, &feed2).await {
            tracing::debug!(feed = feed2.id, "icon refresh failed: {e:#}");
        }
    });

    Ok((FetchOutcome { status: 200, not_modified: false, report }, meta))
}

/// Scheduler loop: picks due feeds and fetches them with bounded concurrency.
pub async fn run_scheduler(app: AppRef) {
    let mut sem_size = 0usize;
    let mut sem = Arc::new(Semaphore::new(1));
    // Reset leases left over from a previous run.
    let _ = app.db.call(|c| Ok(c.execute("UPDATE feeds SET lease_until = NULL", [])?)).await;
    loop {
        if app.shutdown.is_cancelled() {
            break;
        }
        let settings = app.settings.get().await;
        let want = settings.fetch.concurrency.clamp(1, 64) as usize;
        if want != sem_size {
            sem = Arc::new(Semaphore::new(want));
            sem_size = want;
        }
        let available = sem.available_permits();
        if available > 0 {
            let now = util::now();
            let due: Vec<i64> = app
                .db
                .call(move |c| {
                    let mut st = c.prepare(
                        "SELECT id FROM feeds WHERE enabled = 1 AND next_fetch_at <= ? AND (lease_until IS NULL OR lease_until < ?)
                         ORDER BY next_fetch_at LIMIT ?",
                    )?;
                    let ids = st.query_map(params![now, now, available as i64], |r| r.get(0))?.collect::<Result<Vec<i64>, _>>()?;
                    for id in &ids {
                        c.execute("UPDATE feeds SET lease_until = ? WHERE id = ?", params![now + 600, id])?;
                    }
                    Ok(ids)
                })
                .await
                .unwrap_or_default();
            for id in due {
                let permit = sem.clone().acquire_owned().await.expect("semaphore");
                let app2 = app.clone();
                tokio::spawn(async move {
                    let _p = permit;
                    if let Err(e) = fetch_feed(&app2, id).await {
                        tracing::debug!(feed = id, "fetch error: {e:#}");
                    }
                });
            }
        }
        tokio::select! {
            _ = app.shutdown.cancelled() => break,
            _ = app.wake_fetcher.notified() => {},
            _ = tokio::time::sleep(Duration::from_secs(10)) => {},
        }
    }
}

/// Request an immediate refresh of one feed (or all when `None`).
pub async fn request_refresh(app: &AppRef, feed_id: Option<i64>) -> Result<()> {
    app.db
        .call(move |c| {
            match feed_id {
                Some(id) => c.execute("UPDATE feeds SET next_fetch_at = 0 WHERE id = ?", [id])?,
                None => c.execute("UPDATE feeds SET next_fetch_at = 0 WHERE enabled = 1", [])?,
            };
            Ok(())
        })
        .await?;
    app.wake_fetcher.notify_one();
    Ok(())
}

// ---------------------------------------------------------------------------
// Subscribing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Discovered {
    pub url: String,
    pub title: String,
}

/// Find the feed behind `url`: either the URL itself parses as a feed, or it is an HTML
/// page advertising feeds via <link rel="alternate">.
pub async fn discover(app: &AppRef, url: &str) -> Result<Discovered> {
    let settings = app.settings.get().await;
    let url = url.trim();
    let parsed = Url::parse(url).or_else(|_| Url::parse(&format!("https://{url}"))).context("invalid URL")?;
    let ua = user_agent(&settings, &FeedOptions::default());
    let timeout = Duration::from_secs(settings.fetch.timeout_secs.max(5) as u64);
    let max = settings.fetch.max_feed_mb.max(1) as usize * 1024 * 1024;
    let f = http::get(&app.http.feed, parsed.as_str(), &ua, timeout, max, false, FEED_ACCEPT).await?;
    if f.status >= 400 {
        bail!("HTTP {} for {}", f.status, parsed);
    }
    let final_url = f.final_url.to_string();
    if let Ok(feed) = parse_feed(&f.body, &final_url) {
        return Ok(Discovered { url: final_url, title: feed.title });
    }
    // HTML page: look for alternate feed links.
    let html = http::decode_html(&f.body, f.content_type().as_deref());
    let base = Url::parse(&final_url).ok();
    let candidates: Vec<String> = {
        let doc = dom_query::Document::from(html.as_str());
        doc.select("link[rel~=alternate]")
            .nodes()
            .iter()
            .filter(|n| {
                let t = n.attr("type").map(|t| t.to_ascii_lowercase()).unwrap_or_default();
                t.contains("rss") || t.contains("atom") || t.contains("feed+json") || t.contains("application/json")
            })
            .filter_map(|n| n.attr("href").and_then(|h| urlnorm::resolve(base.as_ref(), &h)))
            .collect()
    };
    for c in candidates.iter().take(3) {
        if let Ok(f2) = http::get(&app.http.feed, c, &ua, timeout, max, false, FEED_ACCEPT).await
            && f2.status < 400
            && let Ok(feed) = parse_feed(&f2.body, &f2.final_url.to_string())
        {
            return Ok(Discovered { url: f2.final_url.to_string(), title: feed.title });
        }
    }
    bail!("no feed found at {parsed}")
}

/// Add a subscription. Returns (feed id, created).
pub async fn add_feed(
    app: &AppRef,
    url: &str,
    title: Option<&str>,
    category_id: Option<i64>,
    options: Option<FeedOptions>,
    discover_first: bool,
) -> Result<(i64, bool)> {
    let (feed_url, discovered_title) = if discover_first {
        let d = discover(app, url).await?;
        (d.url, Some(d.title))
    } else {
        (url.trim().to_string(), None)
    };
    let title_locked = title.is_some_and(|t| !t.trim().is_empty());
    let title = title
        .filter(|t| !t.trim().is_empty())
        .map(|t| t.trim().to_string())
        .or(discovered_title.filter(|t| !t.trim().is_empty()))
        .unwrap_or_else(|| urlnorm::host_of(&feed_url).unwrap_or_else(|| feed_url.clone()));
    let opts = options.unwrap_or_default().to_json();
    let now = util::now();
    let (id, created) = app
        .db
        .write(move |tx| {
            if let Some(id) = tx.query_row("SELECT id FROM feeds WHERE url = ?", [&feed_url], |r| r.get::<_, i64>(0)).optional()? {
                return Ok((id, false));
            }
            tx.execute(
                "INSERT INTO feeds (url, title, title_locked, category_id, options, next_fetch_at, created_at, updated_at) VALUES (?, ?, ?, ?, ?, 0, ?, ?)",
                params![feed_url, title, title_locked as i64, category_id, opts, now, now],
            )?;
            Ok((tx.last_insert_rowid(), true))
        })
        .await?;
    if created {
        app.wake_fetcher.notify_one();
        app.event(Level::Info, "system", EventRef::feed(id), "subscribed").await;
    }
    Ok((id, created))
}

/// Delete a feed and everything belonging to it.
pub async fn delete_feed(app: &AppRef, feed_id: i64) -> Result<()> {
    app.db
        .write(move |tx| {
            tx.execute("DELETE FROM story_keys WHERE feed_id = ?", [feed_id])?;
            tx.execute("DELETE FROM feeds WHERE id = ?", [feed_id])?;
            Ok(())
        })
        .await?;
    app.event(Level::Info, "system", EventRef::default(), format!("feed {feed_id} deleted")).await;
    Ok(())
}

/// Get or create a category by name.
pub fn ensure_category(tx: &rusqlite::Connection, name: &str) -> Result<Option<i64>> {
    let name = name.trim();
    if name.is_empty() {
        return Ok(None);
    }
    if let Some(id) = tx.query_row("SELECT id FROM categories WHERE name = ? COLLATE NOCASE", [name], |r| r.get(0)).optional()? {
        return Ok(Some(id));
    }
    tx.execute("INSERT INTO categories (name, created_at) VALUES (?, ?)", params![name, util::now()])?;
    Ok(Some(tx.last_insert_rowid()))
}

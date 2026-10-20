//! Revision processing pipeline.
//!
//! Every pending revision moves through stages:
//!   extract (full text) -> transform (sanitize, lead image, OpenCC) -> translate ->
//!   images -> publish
//! Stages are claimed with a lease (crash recovery) and advanced with a
//! compare-and-set on (state, stage), so a revision that became obsolete while it was
//! being processed (a newer source arrived) can never be published — an old
//! translation can't overwrite a new version.
//!
//! Publishing allocates the public item id. Updates of an already published story are
//! published as a *new* item (new id); the previous item is hidden, marked read and
//! its star/labels/note move to the new item.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures::FutureExt;
use rusqlite::{OptionalExtension, Transaction, params};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::app::{AppRef, EventRef, Level};
use crate::content::translate::{ArticleTranslation, TranslateError};
use crate::content::{extract, sanitize};
use crate::feeds::fetcher::{FeedRow, load_feed, user_agent};
use crate::feeds::ingest::{self, assess_change};
use crate::feeds::parse::Media;
use crate::http;
use crate::settings::{FeedOptions, FulltextMode, Settings, TranslateMode, UpdateMode};
use crate::util::html::escape;
use crate::util::{self, text};

pub const STAGES: [&str; 5] = ["extract", "transform", "translate", "images", "publish"];

/// Time unit of retry back-off steps in milliseconds (1000 = seconds; tests lower it).
pub static RETRY_UNIT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1000);
/// Dispatcher polling interval in milliseconds (tests lower it).
pub static DISPATCH_TICK_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(3000);

/// Unexpected errors (or panics) in one stage before the revision is given up as 'failed'.
const MAX_STAGE_ERRORS: i64 = 5;
/// Runs of one stage that may start without recording an outcome (the process crashed or
/// was killed mid-stage) before the revision is given up instead of crashing it again.
const MAX_UNFINISHED_RUNS: i64 = 3;
/// Extraction attempts for an entry update: the page is the only way to confirm it, and
/// article pages are often briefly unavailable while being edited (≈ 4 h of back-off).
const UPDATE_PAGE_ATTEMPTS: i64 = 7;
/// Stories queued per transaction by a bulk reprocess.
const REPROCESS_CHUNK: usize = 200;

#[derive(Debug, Clone)]
pub struct Rev {
    pub id: i64,
    pub story_id: i64,
    pub feed_id: i64,
    pub reason: String,
    pub stage: String,
    pub guid: String,
    pub title: String,
    pub link: Option<String>,
    pub author: Option<String>,
    pub published_at: Option<i64>,
    pub entry_html: Option<String>,
    pub media: Media,
    pub content_html: Option<String>,
    pub title_out: Option<String>,
    pub source_text: Option<String>,
    pub lang: Option<String>,
    pub notes: Vec<String>,
    pub attempts: i64,
    pub created_at: i64,
    /// The text was extracted from the article page.
    pub from_page: bool,
    /// Runs of the current stage that started without recording an outcome (incl. this one).
    pub unfinished: i64,
}

fn load_rev(conn: &rusqlite::Connection, id: i64) -> Result<Option<Rev>> {
    Ok(conn
        .query_row(
            "SELECT id, story_id, feed_id, reason, stage, guid, title, link, author, published_at, entry_html, media,
                    content_html, title_out, source_text, lang, notes, attempts, created_at, from_page, unfinished
             FROM revisions WHERE id = ? AND state = 'pending'",
            [id],
            |r| {
                Ok(Rev {
                    id: r.get(0)?,
                    story_id: r.get(1)?,
                    feed_id: r.get(2)?,
                    reason: r.get(3)?,
                    stage: r.get(4)?,
                    guid: r.get(5)?,
                    title: r.get(6)?,
                    link: r.get(7)?,
                    author: r.get(8)?,
                    published_at: r.get(9)?,
                    entry_html: r.get(10)?,
                    media: Media::from_json(r.get::<_, Option<String>>(11)?.as_deref()),
                    content_html: r.get(12)?,
                    title_out: r.get(13)?,
                    source_text: r.get(14)?,
                    lang: r.get(15)?,
                    notes: serde_json::from_str(&r.get::<_, String>(16)?).unwrap_or_default(),
                    attempts: r.get(17)?,
                    created_at: r.get(18)?,
                    from_page: r.get::<_, i64>(19)? != 0,
                    unfinished: r.get(20)?,
                })
            },
        )
        .optional()?)
}

/// Result of running one stage.
enum Outcome {
    Advance(Advance),
    /// Try this stage again later.
    Retry { delay: Duration, error: String, count_attempt: bool },
    /// Change turned out not to be substantive: drop the revision.
    Unchanged(String),
    /// Give up on this revision.
    Failed(String),
    /// Revision is obsolete / disappeared.
    Gone,
}

#[derive(Default)]
struct Advance {
    next: &'static str,
    /// Replacement media JSON (e.g. a better lead image found on the page).
    media: Option<String>,
    content_html: Option<String>,
    title_out: Option<String>,
    source_text: Option<String>,
    lang: Option<String>,
    notes: Vec<String>,
    /// Set when the text now comes from the article page.
    from_page: Option<bool>,
}

fn next_after_transform(o: &FeedOptions) -> &'static str {
    if o.translate {
        "translate"
    } else if o.cache_images {
        "images"
    } else {
        "publish"
    }
}

fn next_after_translate(o: &FeedOptions) -> &'static str {
    if o.cache_images { "images" } else { "publish" }
}

fn stage_concurrency(s: &Settings, stage: &str) -> usize {
    match stage {
        "extract" => s.fetch.page_concurrency.clamp(1, 32) as usize,
        "transform" => 4,
        // Articles in flight: enough to keep the main engine's request slots busy.
        "translate" => {
            let t = &s.translation;
            let slots = if t.engine == "llm" { t.max_concurrency } else { t.mt_max_concurrency };
            (slots.clamp(1, 32) * 2) as usize
        }
        "images" => s.images.concurrency.clamp(1, 16) as usize,
        _ => 1,
    }
}

fn lease_secs(stage: &str) -> i64 {
    match stage {
        "translate" => 600,
        "extract" | "images" => 300,
        _ => 120,
    }
}

fn retry_backoff(attempts: i64) -> Duration {
    const STEPS: [u64; 7] = [60, 300, 900, 1800, 3600, 7200, 14400];
    let unit = RETRY_UNIT_MS.load(std::sync::atomic::Ordering::Relaxed);
    Duration::from_millis(STEPS[(attempts.max(0) as usize).min(STEPS.len() - 1)] * unit)
}

/// Dispatcher loop.
pub async fn run_dispatcher(app: AppRef) {
    let _ = app
        .db
        .call(|c| Ok(c.execute("UPDATE revisions SET lease_until = NULL WHERE state = 'pending'", [])?))
        .await;
    let mut sems: HashMap<&'static str, (usize, Arc<Semaphore>)> = HashMap::new();
    loop {
        if app.shutdown.is_cancelled() {
            break;
        }
        let settings = app.settings.get().await;
        for stage in STAGES {
            let want = stage_concurrency(&settings, stage);
            let entry = sems.entry(stage).or_insert_with(|| (want, Arc::new(Semaphore::new(want))));
            if entry.0 != want {
                *entry = (want, Arc::new(Semaphore::new(want)));
            }
            let sem = entry.1.clone();
            let avail = sem.available_permits();
            if avail == 0 {
                continue;
            }
            // Don't claim translation work while every engine is known to be down.
            if stage == "translate" && app.translator.paused_for(&settings.translation).is_some() {
                continue;
            }
            let ids = match claim(&app, stage, avail).await {
                Ok(ids) => ids,
                Err(e) => {
                    tracing::warn!("claim {stage}: {e:#}");
                    continue;
                }
            };
            for id in ids {
                let permit = sem.clone().acquire_owned().await.expect("semaphore");
                let app2 = app.clone();
                tokio::spawn(async move {
                    run_stage(app2, id, stage, permit).await;
                });
            }
        }
        let tick = DISPATCH_TICK_MS.load(std::sync::atomic::Ordering::Relaxed);
        tokio::select! {
            _ = app.shutdown.cancelled() => break,
            _ = app.wake_pipeline.notified() => {},
            _ = tokio::time::sleep(Duration::from_millis(tick)) => {},
        }
    }
}

async fn claim(app: &AppRef, stage: &'static str, n: usize) -> Result<Vec<i64>> {
    let now = util::now();
    let lease = now + lease_secs(stage);
    app.db
        .call(move |c| {
            // `unfinished` is counted before the stage runs: a revision that crashes the
            // process is not retried forever (startup clears the leases).
            let mut st = c.prepare_cached(
                "UPDATE revisions SET lease_until = ?1, unfinished = unfinished + 1 WHERE id IN (
                    SELECT id FROM revisions
                    WHERE state = 'pending' AND stage = ?2 AND next_attempt_at <= ?3
                      AND (lease_until IS NULL OR lease_until < ?3)
                    ORDER BY priority DESC, id ASC LIMIT ?4)
                 RETURNING id",
            )?;
            let ids = st.query_map(params![lease, stage, now, n as i64], |r| r.get(0))?.collect::<Result<Vec<i64>, _>>()?;
            Ok(ids)
        })
        .await
}

/// Keeps extending a revision's lease while its stage runs.
fn heartbeat(app: &AppRef, id: i64, stage: &'static str) -> CancellationToken {
    let token = CancellationToken::new();
    let t2 = token.clone();
    let app = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = t2.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(60)) => {
                    // Only while this stage still holds the lease: a late renewal must not
                    // lease the revision again after its outcome was recorded.
                    let lease = util::now() + lease_secs(stage);
                    let _ = app.db.call(move |c| Ok(c.execute(
                        "UPDATE revisions SET lease_until = ? WHERE id = ? AND state = 'pending' AND stage = ? AND lease_until IS NOT NULL",
                        params![lease, id, stage])?)).await;
                }
            }
        }
    });
    token
}

async fn run_stage(app: AppRef, id: i64, stage: &'static str, _permit: OwnedSemaphorePermit) {
    // Dropping the guard stops the heartbeat, also when this task unwinds.
    let hb = heartbeat(&app, id, stage).drop_guard();
    let outcome = match AssertUnwindSafe(process_stage(&app, id, stage)).catch_unwind().await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => stage_error(&app, id, stage, format!("{e:#}")).await,
        Err(panic) => {
            let msg = panic_message(panic.as_ref());
            tracing::error!(revision = id, stage, "stage panicked: {msg}");
            stage_error(&app, id, stage, format!("处理时发生内部错误：{msg}")).await
        }
    };
    drop(hb);
    if let Err(e) = apply_outcome(&app, id, stage, outcome).await {
        tracing::error!(revision = id, stage, "failed to record stage outcome: {e:#}");
    }
    app.wake_pipeline.notify_one();
}

fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "panic".into())
}

/// Outcome for an unexpected error: retry with back-off, but a stage that keeps failing
/// ends 'failed' instead of retrying forever.
async fn stage_error(app: &AppRef, id: i64, stage: &'static str, error: String) -> Outcome {
    let attempts = app
        .db
        .call(move |c| {
            Ok(c.query_row("SELECT attempts FROM revisions WHERE id = ? AND state = 'pending' AND stage = ?", params![id, stage], |r| r.get::<_, i64>(0))
                .optional()?)
        })
        .await;
    match attempts {
        Ok(Some(a)) if a + 1 >= MAX_STAGE_ERRORS => Outcome::Failed(format!("{error}（已尝试 {} 次）", a + 1)),
        Ok(Some(a)) => Outcome::Retry { delay: retry_backoff(a), error, count_attempt: true },
        Ok(None) => Outcome::Gone,
        Err(_) => Outcome::Retry { delay: retry_backoff(0), error, count_attempt: true },
    }
}

async fn process_stage(app: &AppRef, id: i64, stage: &'static str) -> Result<Outcome> {
    let Some(rev) = app.db.call(move |c| load_rev(c, id)).await? else {
        return Ok(Outcome::Gone);
    };
    if rev.stage != stage {
        return Ok(Outcome::Gone);
    }
    if rev.unfinished > MAX_UNFINISHED_RUNS {
        return Ok(Outcome::Failed(format!("此阶段已有 {} 次处理未能完成（进程中断或崩溃），已放弃", rev.unfinished - 1)));
    }
    let feed_id = rev.feed_id;
    let Some(feed) = app.db.call(move |c| load_feed(c, feed_id)).await? else {
        return Ok(Outcome::Failed("订阅已删除".into()));
    };
    let settings = app.settings.get().await;
    match stage {
        "extract" => stage_extract(app, &settings, &feed, &rev).await,
        "transform" => stage_transform(app, &settings, &feed, &rev).await,
        "translate" => stage_translate(app, &settings, &feed, &rev).await,
        "images" => stage_images(app, &settings, &feed, &rev).await,
        "publish" => stage_publish(app, &settings, &feed, &rev).await,
        _ => Ok(Outcome::Failed(format!("未知阶段 {stage}"))),
    }
}

async fn apply_outcome(app: &AppRef, id: i64, stage: &'static str, outcome: Outcome) -> Result<()> {
    let now = util::now();
    match outcome {
        Outcome::Gone => {
            app.db
                .call(move |c| Ok(c.execute("UPDATE revisions SET lease_until = NULL, unfinished = 0 WHERE id = ? AND stage = ?", params![id, stage])?))
                .await?;
        }
        Outcome::Advance(a) => {
            let notes = a.notes;
            app.db
                .call(move |c| {
                    let existing: Option<String> = c
                        .query_row("SELECT notes FROM revisions WHERE id = ?", [id], |r| r.get(0))
                        .optional()?;
                    let mut all: Vec<String> = existing.and_then(|e| serde_json::from_str(&e).ok()).unwrap_or_default();
                    all.extend(notes);
                    c.execute(
                        "UPDATE revisions SET stage = ?, content_html = COALESCE(?, content_html), title_out = COALESCE(?, title_out),
                             source_text = COALESCE(?, source_text), lang = COALESCE(?, lang), media = COALESCE(?, media),
                             from_page = COALESCE(?, from_page), notes = ?, attempts = 0, unfinished = 0,
                             next_attempt_at = 0, lease_until = NULL, last_error = NULL, updated_at = ?
                         WHERE id = ? AND state = 'pending' AND stage = ?",
                        params![
                            a.next,
                            a.content_html,
                            a.title_out,
                            a.source_text,
                            a.lang,
                            a.media,
                            a.from_page.map(|b| b as i64),
                            serde_json::to_string(&all)?,
                            now,
                            id,
                            stage
                        ],
                    )?;
                    Ok(())
                })
                .await?;
        }
        Outcome::Retry { delay, error, count_attempt } => {
            let next = now + delay.as_secs() as i64;
            let inc = if count_attempt { 1 } else { 0 };
            let error_msg = error.clone();
            app.db
                .call(move |c| {
                    c.execute(
                        "UPDATE revisions SET attempts = attempts + ?, total_attempts = total_attempts + 1, next_attempt_at = ?,
                             lease_until = NULL, unfinished = 0, last_error = ?, updated_at = ?
                         WHERE id = ? AND state = 'pending' AND stage = ?",
                        params![inc, next, util::truncate_chars(&error_msg, 1000), now, id, stage],
                    )?;
                    Ok(())
                })
                .await?;
            tracing::debug!(revision = id, stage, "retry in {delay:?}: {error}");
        }
        Outcome::Unchanged(why) => {
            let why2 = why.clone();
            let story: Option<(i64, i64)> = app
                .db
                .call(move |c| {
                    let s = c.query_row("SELECT feed_id, story_id FROM revisions WHERE id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
                    c.execute(
                        "UPDATE revisions SET state = 'unchanged', stage = 'done', lease_until = NULL, unfinished = 0, content_html = NULL,
                             last_error = ?, updated_at = ? WHERE id = ? AND state = 'pending' AND stage = ?",
                        params![why2, now, id, stage],
                    )?;
                    Ok(s)
                })
                .await?;
            if let Some((f, s)) = story {
                tracing::debug!(feed = f, story = s, "update not substantive: {why}");
            }
        }
        Outcome::Failed(why) => {
            let why2 = why.clone();
            let story: Option<(i64, i64)> = app
                .db
                .call(move |c| {
                    let s = c.query_row("SELECT feed_id, story_id FROM revisions WHERE id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
                    c.execute(
                        "UPDATE revisions SET state = 'failed', lease_until = NULL, unfinished = 0, last_error = ?, updated_at = ?
                         WHERE id = ? AND state = 'pending' AND stage = ?",
                        params![why2, now, id, stage],
                    )?;
                    Ok(s)
                })
                .await?;
            if let Some((f, s)) = story {
                app.event(Level::Error, stage, EventRef::story(f, s), format!("处理失败：{why}")).await;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// extract
// ---------------------------------------------------------------------------

/// Result of fetching + extracting an article page.
pub struct PageExtract {
    pub html: String,
    pub lang: Option<String>,
    /// Lead image advertised by the page (og:image / JSON-LD).
    pub image: Option<String>,
}

/// Errors from page extraction, classified for retry decisions.
pub enum PageError {
    Transient(String),
    Permanent(String),
}

/// Fetch the article page and extract its content using the feed's mode.
pub async fn extract_page(app: &AppRef, settings: &Settings, feed: &FeedRow, link: &str) -> std::result::Result<PageExtract, PageError> {
    let opts = &feed.options;
    let timeout = Duration::from_secs(settings.fetch.timeout_secs.max(5) as u64);
    if opts.fulltext == FulltextMode::External {
        return match extract::external(&app.http, &settings.fulltext.external_url, link, timeout * 2).await {
            Ok(e) => Ok(PageExtract { html: e.html, lang: e.lang, image: e.image }),
            Err(e) => Err(PageError::Transient(format!("external extractor: {e:#}"))),
        };
    }
    let url = Url::parse(link).map_err(|e| PageError::Permanent(format!("invalid link: {e}")))?;
    let host = url.host_str().unwrap_or("").to_string();
    let _permit = app
        .http
        .hosts
        .acquire(&host, settings.fetch.per_host_concurrency, Duration::from_millis(settings.fetch.per_host_delay_ms as u64))
        .await;
    let ua = user_agent(settings, opts);
    let max = settings.fetch.max_page_mb.max(1) as usize * 1024 * 1024;
    let f = http::get(&app.http.content, link, &ua, timeout, max, app.http.public_only, "text/html,application/xhtml+xml;q=0.9,*/*;q=0.5")
        .await
        .map_err(|e| {
            let msg = format!("{e:#}");
            if msg.contains("blocked") || msg.contains("unsupported URL") || msg.contains("too large") {
                PageError::Permanent(msg)
            } else {
                PageError::Transient(msg)
            }
        })?;
    if f.status >= 400 {
        let msg = format!("HTTP {}", f.status);
        return Err(if f.status == 429 || f.status >= 500 { PageError::Transient(msg) } else { PageError::Permanent(msg) });
    }
    let html = http::decode_html(&f.body, f.content_type().as_deref());
    let final_url = f.final_url.to_string();
    let mode = opts.fulltext;
    let selector = opts.selector.clone();
    let res = tokio::task::spawn_blocking(move || -> Result<extract::Extracted> {
        match mode {
            FulltextMode::Selector => extract::by_selector(&html, &selector, &final_url).or_else(|_| extract::readability(&html, &final_url)),
            _ => extract::readability(&html, &final_url),
        }
    })
    .await
    .map_err(|e| PageError::Transient(format!("extractor panicked: {e}")))?;
    match res {
        Ok(e) => Ok(PageExtract { html: e.html, lang: e.lang, image: e.image }),
        Err(e) => Err(PageError::Permanent(format!("{e:#}"))),
    }
}

fn text_len(html: &str) -> usize {
    text::html_to_text(html).chars().filter(|c| !c.is_whitespace()).count()
}

/// Why the article page could not be used.
enum PageProblem {
    Fetch(String),
    TooShort(usize),
}

impl PageProblem {
    fn describe(&self) -> String {
        match self {
            PageProblem::Fetch(e) => e.clone(),
            PageProblem::TooShort(got) => format!("提取的正文过短（{got} 字）"),
        }
    }
}

/// Was a reprocess of this story queued after its last publish and then replaced by this
/// newer revision (ingest replaces pending revisions)? Then this one still owes it.
async fn replaced_reprocess(app: &AppRef, rev: &Rev) -> Result<bool> {
    let (sid, id) = (rev.story_id, rev.id);
    app.db
        .call(move |c| {
            Ok(c.query_row(
                "SELECT EXISTS(SELECT 1 FROM revisions WHERE story_id = ?1 AND reason = 'reprocess' AND state = 'obsolete' AND id < ?2
                     AND id > COALESCE((SELECT MAX(id) FROM revisions WHERE story_id = ?1 AND state = 'published'), 0))",
                params![sid, id],
                |r| r.get(0),
            )?)
        })
        .await
}

/// Turn a pending revision into a reprocess (published without the update marks).
async fn make_reprocess(app: &AppRef, id: i64) -> Result<()> {
    app.db.call(move |c| Ok(c.execute("UPDATE revisions SET reason = 'reprocess' WHERE id = ? AND state = 'pending'", [id])?)).await?;
    Ok(())
}

/// The story's current version (and its change-detection baseline) came from the page.
async fn story_from_page(app: &AppRef, story_id: i64) -> Result<bool> {
    app.db
        .call(move |c| Ok(c.query_row("SELECT from_page FROM stories WHERE id = ?", [story_id], |r| r.get::<_, i64>(0)).optional()?.unwrap_or(0) != 0))
        .await
}

/// The page could not be used. A new article falls back to the feed content; an update
/// can only be confirmed on the page (retry for a few hours, then drop it); a reprocess
/// never replaces a version that came from the page with the feed content.
async fn page_problem(app: &AppRef, rev: &Rev, problem: PageProblem) -> Result<Outcome> {
    let why = problem.describe();
    let ev = EventRef::story(rev.feed_id, rev.story_id);
    let mut reprocess = rev.reason == "reprocess";
    if rev.reason == "entry_update" {
        if rev.attempts + 1 < UPDATE_PAGE_ATTEMPTS {
            return Ok(Outcome::Retry { delay: retry_backoff(rev.attempts), error: format!("原文无法获取，稍后重试：{why}"), count_attempt: true });
        }
        if !replaced_reprocess(app, rev).await? {
            app.event(Level::Warn, "extract", ev, format!("原文无法获取，无法确认更新：{why}")).await;
            return Ok(Outcome::Unchanged(format!("原文无法获取：{why}")));
        }
        make_reprocess(app, rev.id).await?;
        reprocess = true;
    }
    if reprocess && story_from_page(app, rev.story_id).await? {
        app.event(Level::Warn, "extract", ev, format!("原文页面无法获取，保留当前版本：{why}")).await;
        return Ok(Outcome::Unchanged(format!("原文页面无法获取，保留当前版本：{why}")));
    }
    let note = match &problem {
        PageProblem::Fetch(e) => {
            app.event(Level::Warn, "extract", ev, format!("全文提取失败，改用订阅内容：{e}")).await;
            format!("全文提取失败：{e}")
        }
        PageProblem::TooShort(_) => format!("{why}，改用订阅内容"),
    };
    Ok(Outcome::Advance(Advance { next: "transform", notes: vec![note], ..Default::default() }))
}

async fn stage_extract(app: &AppRef, settings: &Settings, feed: &FeedRow, rev: &Rev) -> Result<Outcome> {
    let Some(link) = rev.link.clone() else {
        return Ok(Outcome::Advance(Advance { next: "transform", notes: vec!["全文提取：没有原文链接".into()], ..Default::default() }));
    };
    let entry_html = rev.entry_html.clone().unwrap_or_default();
    let extracted = match extract_page(app, settings, feed, &link).await {
        Ok(p) => p,
        Err(PageError::Transient(e)) if rev.attempts < 3 && rev.reason != "entry_update" => {
            return Ok(Outcome::Retry { delay: retry_backoff(rev.attempts), error: format!("全文提取：{e}"), count_attempt: true });
        }
        Err(PageError::Transient(e)) | Err(PageError::Permanent(e)) => return page_problem(app, rev, PageProblem::Fetch(e)).await,
    };
    let html = extract::apply_remove(&extracted.html, &feed.options.remove_selector);
    let got = text_len(&html);
    let entry_len = text_len(&entry_html);
    let min = settings.fulltext.min_text_chars as usize;
    if got < min.min(entry_len.max(1) * 3) || (entry_len > 300 && got * 2 < entry_len) {
        return page_problem(app, rev, PageProblem::TooShort(got)).await;
    }
    let src = ingest::source_text(&html);
    // Confirm that an entry update really changed the article.
    if rev.reason == "entry_update" {
        let (old_title, old_text, from_page): (Option<String>, Option<String>, bool) = {
            let sid = rev.story_id;
            app.db
                .call(move |c| {
                    Ok(c.query_row("SELECT source_title, source_text, from_page FROM stories WHERE id = ?", [sid], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0))
                    })?)
                })
                .await?
        };
        // Compare like with like: a current version that came from the feed entry
        // (extraction failed back then, or full text was enabled later) has the entry text
        // as baseline, so the new entry is what is compared — not the page.
        let new_text = if from_page { src.clone() } else { ingest::feed_text(&entry_html, &feed.options) };
        let a = assess_change(old_title.as_deref().unwrap_or(""), old_text.as_deref().unwrap_or(""), &rev.title, &new_text, &settings.updates);
        if a.substantive {
            app.event(Level::Info, "update", EventRef::story(rev.feed_id, rev.story_id), format!("确认实质更新：{}", a.summary)).await;
        } else if replaced_reprocess(app, rev).await? {
            // Not news, but it took the place of a queued reprocess: publish as that.
            make_reprocess(app, rev.id).await?;
        } else {
            return Ok(Outcome::Unchanged(format!("原文没有实质修改：{}", a.summary)));
        }
    }
    // Prefer the page's own lead image (usually high resolution) over the feed thumbnail.
    let media = match extracted.image.as_deref().map(str::trim).filter(|i| i.starts_with("http")) {
        Some(img) => {
            let mut m = rev.media.clone();
            m.thumbnail = Some(img.to_string());
            m.to_json()
        }
        None => None,
    };
    Ok(Outcome::Advance(Advance {
        next: "transform",
        media,
        content_html: Some(html),
        source_text: Some(src),
        lang: extracted.lang,
        from_page: Some(true),
        ..Default::default()
    }))
}

// ---------------------------------------------------------------------------
// transform
// ---------------------------------------------------------------------------

async fn stage_transform(app: &AppRef, _settings: &Settings, feed: &FeedRow, rev: &Rev) -> Result<Outcome> {
    let raw = rev.content_html.clone().or_else(|| rev.entry_html.clone()).unwrap_or_default();
    let base = rev.link.clone().or_else(|| feed.site_url.clone()).unwrap_or_else(|| feed.url.clone());
    let opts = feed.options.clone();
    let thumb = rev.media.thumbnail.clone();
    let title = rev.title.clone();
    let source_known = rev.source_text.clone();
    let conv = app.converter.clone();
    let res = tokio::task::spawn_blocking(move || -> Result<(String, String, String)> {
        let base_url = Url::parse(&base).ok();
        let cleaned = extract::apply_remove(&raw, &opts.remove_selector);
        let mut html = sanitize::sanitize(&cleaned, base_url.as_ref());
        if let Some(t) = thumb
            && !html.contains("<img")
            && let Some(u) = crate::util::urlnorm::resolve(base_url.as_ref(), &t)
        {
            html = format!("<p><img src=\"{}\" alt=\"\"></p>{html}", escape(&u));
        }
        let src = source_known.unwrap_or_else(|| ingest::source_text(&html));
        let mut title = title;
        if opts.opencc_enabled() {
            html = conv.convert_html(&opts.opencc, &html)?;
            title = conv.convert_text(&opts.opencc, &title)?;
        }
        Ok((html, title, src))
    })
    .await??;
    let (html, title, src) = res;
    if html.trim().is_empty() && title.trim().is_empty() {
        return Ok(Outcome::Failed("文章内容为空".into()));
    }
    Ok(Outcome::Advance(Advance {
        next: next_after_transform(&feed.options),
        content_html: Some(html),
        title_out: Some(title),
        source_text: Some(src),
        ..Default::default()
    }))
}

// ---------------------------------------------------------------------------
// translate
// ---------------------------------------------------------------------------

async fn stage_translate(app: &AppRef, settings: &Settings, feed: &FeedRow, rev: &Rev) -> Result<Outcome> {
    let opts = &feed.options;
    let ts = &settings.translation;
    let html = rev.content_html.clone().unwrap_or_default();
    let title = rev.title_out.clone().unwrap_or_else(|| rev.title.clone());
    let next = next_after_translate(opts);
    let max_attempts = ts.max_attempts.max(1) as i64;
    let last_attempt = rev.attempts + 1 >= max_attempts;
    let bilingual = opts.translate_mode == TranslateMode::Bilingual;
    let title_in = if opts.translate_title { Some(title.as_str()) } else { None };
    let ev = EventRef::story(rev.feed_id, rev.story_id);

    // When translation keeps failing (or stays incomplete): "hold" waits up to 72 h;
    // otherwise the original is published — but a reprocess never replaces the current
    // version with an untranslated or partly translated one.
    let hold = ts.on_failure == "hold" && util::now() - rev.created_at < 72 * 3600;
    let reprocess = rev.reason == "reprocess";
    let give_up = |why: String| -> Outcome {
        if hold {
            Outcome::Retry { delay: Duration::from_secs(6 * 3600), error: why, count_attempt: false }
        } else if reprocess {
            Outcome::Failed(format!("翻译失败，保留当前版本：{why}"))
        } else {
            Outcome::Advance(Advance { next, notes: vec![format!("翻译失败，已发布原文：{why}")], ..Default::default() })
        }
    };

    // Segments refused by content moderation are refused again on every attempt: when
    // they are the only failures, the mixed result (translated where allowed, source
    // text elsewhere) is final and is published now, also under "hold". A reprocess
    // still keeps the current version rather than replace it with one translated nowhere.
    let settled = |t: &ArticleTranslation| t.failed_segments == t.filtered_segments && !(reprocess && t.filtered_segments >= t.total_segments);
    // The fallback engine takes over an article the main engine keeps failing on, at
    // the last attempt (before the failure policy applies).
    match app.translator.translate_article_with(ts, &opts.target_lang, title_in, &html, bilingual, last_attempt).await {
        Ok(t) if t.complete || settled(&t) || (last_attempt && !hold && !reprocess) => {
            let mut notes = Vec::new();
            if t.skipped_same_language {
                notes.push("跳过翻译：已是目标语言".to_string());
            }
            if let (Some(why), Some(engine)) = (&t.fallback_reason, t.engine) {
                notes.push(format!("主翻译引擎失败，已改用{}：{why}", engine.label()));
                app.event(Level::Warn, "translate", ev, format!("已改用{}翻译（主引擎失败）", engine.label())).await;
            } else if let Some(fb) = t.fallback_engine {
                notes.push(format!("{} / {} 段由{}补译", t.fallback_segments, t.total_segments, fb.label()));
            }
            if t.filtered_segments > 0 {
                notes.push(format!("{} / {} 段被翻译服务的内容审核拒绝，保留原文", t.filtered_segments, t.total_segments));
            }
            if t.failed_segments > t.filtered_segments {
                notes.push(format!("翻译不完整：{} / {} 段保留原文", t.failed_segments - t.filtered_segments, t.total_segments));
            }
            if !t.complete {
                app.event(Level::Warn, "translate", ev, format!("已发布部分翻译的文章（{} 段保留原文）", t.failed_segments)).await;
            }
            if t.degraded_segments > 0 {
                notes.push(format!("{} 段丢失了行内格式", t.degraded_segments));
            }
            Ok(Outcome::Advance(Advance {
                next,
                content_html: Some(t.html),
                title_out: if opts.translate_title { t.title } else { None },
                notes,
                ..Default::default()
            }))
        }
        Ok(t) => {
            let why = format!("翻译不完整（{} / {} 段失败：{}）", t.failed_segments, t.total_segments, t.problems.join(", "));
            if last_attempt {
                return Ok(give_up(why));
            }
            Ok(Outcome::Retry { delay: retry_backoff(rev.attempts), error: why, count_attempt: true })
        }
        Err(TranslateError::CircuitOpen(d)) => Ok(Outcome::Retry {
            delay: d.max(Duration::from_millis(30 * RETRY_UNIT_MS.load(std::sync::atomic::Ordering::Relaxed))),
            error: "翻译服务暂停中（熔断）".into(),
            count_attempt: false,
        }),
        Err(e) => {
            let msg = e.to_string();
            if last_attempt {
                app.event(Level::Error, "translate", ev, format!("翻译 {max_attempts} 次后仍失败：{msg}")).await;
                return Ok(give_up(msg));
            }
            let delay = match &e {
                TranslateError::NotConfigured => Duration::from_millis(600 * RETRY_UNIT_MS.load(std::sync::atomic::Ordering::Relaxed)),
                TranslateError::Api(api) => api.retry_after().unwrap_or_default().max(retry_backoff(rev.attempts)),
                _ => retry_backoff(rev.attempts),
            };
            Ok(Outcome::Retry { delay, error: msg, count_attempt: true })
        }
    }
}

// ---------------------------------------------------------------------------
// images
// ---------------------------------------------------------------------------

async fn stage_images(app: &AppRef, settings: &Settings, feed: &FeedRow, rev: &Rev) -> Result<Outcome> {
    let html = rev.content_html.clone().unwrap_or_default();
    let ua = user_agent(settings, &feed.options);
    let referer = rev.link.as_deref();
    match app.images.cache_html(&app.http, &settings.images, &html, referer, &ua).await {
        Ok(r) => {
            let mut notes = Vec::new();
            if r.failed > 0 {
                notes.push(format!("{} 张图片下载失败，保留原地址（original URL）", r.failed));
            }
            Ok(Outcome::Advance(Advance { next: "publish", content_html: Some(r.html), notes, ..Default::default() }))
        }
        Err(e) if rev.attempts < 3 => Ok(Outcome::Retry { delay: retry_backoff(rev.attempts), error: format!("图片缓存：{e:#}"), count_attempt: true }),
        Err(e) => Ok(Outcome::Advance(Advance { next: "publish", notes: vec![format!("图片缓存失败：{e:#}")], ..Default::default() })),
    }
}

// ---------------------------------------------------------------------------
// publish
// ---------------------------------------------------------------------------

/// Hashes of cached images referenced by stored HTML.
pub fn image_hashes(html: &str) -> Vec<String> {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"rill-base://local/img/[0-9a-f]{2}/([0-9a-f]{64})\.[a-z]+").unwrap());
    let mut v: Vec<String> = RE.captures_iter(html).map(|c| c[1].to_string()).collect();
    v.sort();
    v.dedup();
    v
}

/// Schedule of the next original-page check for a story (None = no more checks).
pub fn next_page_check(settings: &Settings, opts: &FeedOptions, first_seen_at: i64, check_count: i64) -> Option<i64> {
    if opts.updates != UpdateMode::Page || opts.fulltext == FulltextMode::Off {
        return None;
    }
    let sched = &settings.updates.page_check_minutes;
    let idx = check_count.max(0) as usize;
    let mins = *sched.get(idx)?;
    let t = first_seen_at + mins as i64 * 60;
    if settings.updates.track_hours > 0 && t > first_seen_at + settings.updates.track_hours as i64 * 3600 {
        return None;
    }
    Some(t.max(util::now()))
}

struct PublishInfo {
    item_id: i64,
    /// Version number as news (see `stories.news_versions`).
    version: i64,
    title: String,
    previous: Option<i64>,
    reprocess: bool,
}

fn publish_tx(tx: &Transaction, settings: &Settings, feed: &FeedRow, rev: &Rev) -> Result<Option<PublishInfo>> {
    // The revision must still be the newest pending one of its story.
    let still: Option<String> = tx
        .query_row("SELECT stage FROM revisions WHERE id = ? AND state = 'pending'", [rev.id], |r| r.get(0))
        .optional()?;
    if still.as_deref() != Some("publish") {
        return Ok(None);
    }
    let newer: i64 = tx.query_row(
        "SELECT COUNT(*) FROM revisions WHERE story_id = ? AND state = 'pending' AND id > ?",
        params![rev.story_id, rev.id],
        |r| r.get(0),
    )?;
    if newer > 0 {
        tx.execute("UPDATE revisions SET state = 'obsolete', stage = 'done', lease_until = NULL WHERE id = ?", [rev.id])?;
        return Ok(None);
    }
    #[allow(clippy::type_complexity)]
    let (version, news_versions, current_item, first_seen_at, check_count): (i64, i64, Option<i64>, i64, i64) = tx.query_row(
        "SELECT version, news_versions, current_item_id, first_seen_at, check_count FROM stories WHERE id = ?",
        [rev.story_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    #[allow(clippy::type_complexity)]
    let old: Option<(i64, i64, i64, i64, Option<String>, i64, Option<i64>)> = match current_item {
        Some(iid) => tx
            .query_row(
                "SELECT id, is_read, is_starred, is_published, note, published_at, read_at FROM items WHERE id = ?",
                [iid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
            )
            .optional()?,
        None => None,
    };
    let now = util::now();
    let reprocess = rev.reason == "reprocess";
    if reprocess && current_item.is_some() && old.is_none() {
        // The version to re-render was deleted (retention) while the reprocess waited:
        // publishing would bring the article back as a new, unread one.
        tx.execute(
            "UPDATE revisions SET state = 'obsolete', stage = 'done', lease_until = NULL, last_error = ?, updated_at = ? WHERE id = ?",
            params!["当前版本已被清理，取消重新处理", now, rev.id],
        )?;
        return Ok(None);
    }
    // Strictly increasing insertion time (clients use it as a sync cursor); guards
    // against clock steps.
    let last_us: i64 = tx.query_row("SELECT COALESCE(MAX(created_at), 0) FROM items", [], |r| r.get(0))?;
    let now_us = util::now_us().max(last_us + 1);
    let is_update = old.is_some();
    let real_update = is_update && !reprocess;
    let us = &settings.updates;
    let new_version = version + 1;
    // A re-rendered version is not news: it counts neither towards max_versions nor in
    // the version shown in update notices (`version`, the internal counter, still makes
    // the item guid unique).
    let counts_as_news = !(reprocess && is_update);
    let news_version = if counts_as_news { news_versions + 1 } else { news_versions };

    let mut title = rev.title_out.clone().unwrap_or_else(|| rev.title.clone());
    if title.trim().is_empty() {
        title = rev.link.clone().unwrap_or_else(|| "(untitled)".into());
    }
    let prefix = us.title_prefix.trim_end();
    if real_update && !prefix.is_empty() && !title.starts_with(prefix) {
        title = format!("{} {}", prefix, title);
    }
    let mut content = rev.content_html.clone().unwrap_or_default();
    if real_update && us.notice {
        let when = chrono::DateTime::from_timestamp(now, 0).map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_default();
        // Original publication date of the story (first version).
        let first_published: Option<i64> = tx
            .query_row("SELECT published_at FROM items WHERE story_id = ? ORDER BY id ASC LIMIT 1", [rev.story_id], |r| r.get(0))
            .optional()?
            .or(old.as_ref().map(|o| o.5));
        let orig = first_published
            .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
            .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_default();
        content = format!(
            "<p><em>🔄 {} · v{} · {}{}</em></p><hr>{content}",
            escape(us.notice_label.trim()),
            news_version,
            when,
            if orig.is_empty() { String::new() } else { format!(" · {} {}", escape(us.previous_label.trim()), orig) }
        );
    }
    let published_at = if real_update && us.redate {
        now
    } else if let Some(o) = old.as_ref().filter(|_| reprocess) {
        // A reprocess keeps the date of the version it re-renders.
        o.5
    } else {
        rev.published_at.unwrap_or(now).min(now + 60)
    };
    let excerpt = text::excerpt(&content, 300);
    // An inherited read state keeps its read_at (TT-RSS "recently read" lists by it).
    let (is_read, read_at, is_starred, is_published, note) = match &old {
        Some((_, _, s, p, n, _, _)) if real_update && us.mark_unread => (0, None, *s, *p, n.clone()),
        Some((_, r, s, p, n, _, ra)) => (*r, if *r != 0 { ra.or(Some(now)) } else { None }, *s, *p, n.clone()),
        None => (0, None, 0, 0, None),
    };
    let item_guid = if new_version > 1 { format!("{}#rill-v{}", rev.guid, new_version) } else { rev.guid.clone() };
    tx.execute(
        "INSERT INTO items (feed_id, story_id, revision_id, version, guid, link, title, author, content, excerpt, media, lang,
             published_at, updated_at, created_at, is_read, read_at, is_starred, is_published, note, starred_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            feed.id,
            rev.story_id,
            rev.id,
            new_version,
            item_guid,
            rev.link,
            title,
            rev.author,
            content,
            excerpt,
            rev.media.to_json(),
            rev.lang.as_deref().or(if feed.options.translate { Some(feed.options.target_lang.as_str()) } else { None }),
            published_at,
            now,
            now_us,
            is_read,
            read_at,
            is_starred,
            is_published,
            note,
            if is_starred != 0 { Some(now) } else { None },
        ],
    )?;
    let item_id = tx.last_insert_rowid();
    for h in image_hashes(&content) {
        tx.execute("INSERT OR IGNORE INTO item_images (item_id, hash) VALUES (?, ?)", params![item_id, h])?;
    }
    let previous = old.as_ref().map(|o| o.0);
    if let Some(old_id) = previous {
        tx.execute("INSERT OR IGNORE INTO item_labels (item_id, label_id) SELECT ?, label_id FROM item_labels WHERE item_id = ?", params![item_id, old_id])?;
        // The old version leaves the unread list (marked read, read_at = now so that
        // "read since" queries pick it up) but stays listed and keeps its star: several
        // clients only learn read state from listings and unstar anything that vanishes
        // from the starred list. Hiding it from listings is optional.
        // A reprocess (new settings, not news) of an article that was already read hides
        // the old copy: every client already knows it is read, and lists don't double up.
        let old_was_read = old.as_ref().is_some_and(|o| o.1 != 0);
        let hide = us.hide_superseded || (reprocess && old_was_read);
        tx.execute(
            "UPDATE items SET superseded_by = ?, hidden = ?, read_at = CASE WHEN is_read = 0 THEN ? ELSE read_at END, is_read = 1 WHERE id = ?",
            params![item_id, hide as i64, now, old_id],
        )?;
        // Older generations point at the newest version too.
        tx.execute("UPDATE items SET superseded_by = ? WHERE story_id = ? AND superseded_by = ?", params![item_id, rev.story_id, old_id])?;
    }
    let next_check = next_page_check(settings, &feed.options, first_seen_at, check_count);
    tx.execute(
        "UPDATE stories SET version = ?, news_versions = ?, current_item_id = ?, last_published_at = ?, source_title = ?,
             source_text = COALESCE(?, source_text), from_page = ?, next_check_at = COALESCE(next_check_at, ?),
             state = CASE WHEN ? AND ? > 0 AND ? >= ? THEN 'frozen' ELSE state END
         WHERE id = ?",
        params![
            new_version,
            news_version,
            item_id,
            now,
            rev.title,
            rev.source_text,
            rev.from_page as i64,
            next_check,
            counts_as_news,
            us.max_versions,
            news_version,
            us.max_versions,
            rev.story_id
        ],
    )?;
    tx.execute(
        "UPDATE revisions SET state = 'published', stage = 'done', item_id = ?, lease_until = NULL, content_html = NULL, source_text = NULL, updated_at = ? WHERE id = ?",
        params![item_id, now, rev.id],
    )?;
    Ok(Some(PublishInfo { item_id, version: news_version, title, previous, reprocess }))
}

async fn stage_publish(app: &AppRef, settings: &Settings, feed: &FeedRow, rev: &Rev) -> Result<Outcome> {
    let s = Arc::new(settings.clone());
    let f = feed.clone();
    let r = rev.clone();
    let info = app.db.write(move |tx| publish_tx(tx, &s, &f, &r)).await.context("publish")?;
    match info {
        None => Ok(Outcome::Gone),
        Some(p) => {
            let mut ev = EventRef::story(rev.feed_id, rev.story_id);
            ev.item_id = Some(p.item_id);
            let msg = match p.previous {
                Some(old) if p.reprocess => format!("重新处理后发布（文章 #{}，取代 #{old}）：{}", p.item_id, util::truncate_chars(&p.title, 120)),
                Some(old) => format!("发布第 {} 版（文章 #{}，取代 #{old}）：{}", p.version, p.item_id, util::truncate_chars(&p.title, 120)),
                None => format!("发布文章 #{}：{}", p.item_id, util::truncate_chars(&p.title, 120)),
            };
            let news_update = p.previous.is_some() && !p.reprocess;
            app.event(Level::Info, if news_update { "update" } else { "publish" }, ev, msg).await;
            Ok(Outcome::Gone)
        }
    }
}

// ---------------------------------------------------------------------------
// Original-page update checks
// ---------------------------------------------------------------------------

/// Periodically re-extract recent stories of feeds in "page" update mode.
pub async fn run_page_checker(app: AppRef) {
    loop {
        tokio::select! {
            _ = app.shutdown.cancelled() => break,
            _ = tokio::time::sleep(Duration::from_secs(60)) => {},
        }
        if let Err(e) = check_due_pages(&app).await {
            tracing::warn!("page checker: {e:#}");
        }
    }
}

pub async fn check_due_pages(app: &AppRef) -> Result<usize> {
    let now = util::now();
    let due: Vec<(i64, i64)> = app
        .db
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT id, feed_id FROM stories WHERE next_check_at IS NOT NULL AND next_check_at <= ? AND state = 'active'
                 ORDER BY next_check_at LIMIT 20",
            )?;
            let v = st.query_map([now], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(v)
        })
        .await?;
    let settings = app.settings.get().await;
    let sem = Arc::new(Semaphore::new(settings.fetch.page_concurrency.clamp(1, 8) as usize));
    let mut handles = Vec::new();
    for (story_id, feed_id) in due.iter().copied() {
        let permit = sem.clone().acquire_owned().await?;
        let app2 = app.clone();
        let settings2 = settings.clone();
        handles.push(tokio::spawn(async move {
            let _p = permit;
            if let Err(e) = check_story_page(&app2, &settings2, feed_id, story_id).await {
                tracing::debug!(story = story_id, "page check failed: {e:#}");
            }
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    Ok(due.len())
}

async fn check_story_page(app: &AppRef, settings: &Settings, feed_id: i64, story_id: i64) -> Result<()> {
    let feed = app.db.call(move |c| load_feed(c, feed_id)).await?.ok_or_else(|| anyhow!("feed gone"))?;
    #[allow(clippy::type_complexity)]
    let story: (Option<String>, String, Option<String>, Option<String>, i64, i64, Option<i64>, i64, i64, bool, i64) = app
        .db
        .call(move |c| {
            Ok(c.query_row(
                "SELECT link, title, source_title, source_text, version, first_seen_at, last_published_at, check_count,
                        (SELECT COUNT(*) FROM revisions WHERE story_id = stories.id AND state = 'pending'), from_page, news_versions
                 FROM stories WHERE id = ?",
                [story_id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get::<_, i64>(9)? != 0,
                        r.get(10)?,
                    ))
                },
            )?)
        })
        .await?;
    let (link, title, source_title, source_text, version, first_seen_at, last_published_at, check_count, pending, from_page, news_versions) = story;
    let now = util::now();
    let next_check = next_page_check(settings, &feed.options, first_seen_at, check_count + 1);
    let reschedule = |app: &AppRef, next: Option<i64>, inc: i64| {
        let app = app.clone();
        async move {
            app.db
                .call(move |c| {
                    c.execute("UPDATE stories SET next_check_at = ?, check_count = check_count + ? WHERE id = ?", params![next, inc, story_id])?;
                    Ok(())
                })
                .await
        }
    };
    // The baseline must be page text too (not the feed entry of a version whose
    // extraction failed): comparing the page with it would always look like an update.
    let eligible = version > 0
        && source_text.is_some()
        && from_page
        && feed.options.updates == UpdateMode::Page
        && feed.options.fulltext != FulltextMode::Off
        && (settings.updates.max_versions == 0 || news_versions < settings.updates.max_versions as i64);
    let Some(link) = link.filter(|_| eligible) else {
        return reschedule(app, None, 0).await;
    };
    if pending > 0 {
        return reschedule(app, Some(now + 600), 0).await;
    }
    let page = match extract_page(app, settings, &feed, &link).await {
        Ok(p) => p,
        Err(PageError::Transient(e)) | Err(PageError::Permanent(e)) => {
            tracing::debug!(story = story_id, "page check fetch failed: {e}");
            return reschedule(app, next_check, 1).await;
        }
    };
    let html = extract::apply_remove(&page.html, &feed.options.remove_selector);
    if text_len(&html) < settings.fulltext.min_text_chars as usize {
        return reschedule(app, next_check, 1).await;
    }
    let src = ingest::source_text(&html);
    let a = assess_change(source_title.as_deref().unwrap_or(&title), source_text.as_deref().unwrap_or(""), &title, &src, &settings.updates);
    if !a.substantive {
        return reschedule(app, next_check, 1).await;
    }
    // Create a page_update revision from the latest entry data + the extracted page.
    let us = settings.updates.clone();
    let lang = page.lang.clone();
    let created = app
        .db
        .write(move |tx| {
            #[allow(clippy::type_complexity)]
            let latest: Option<(String, String, Option<String>, Option<String>, Option<i64>, Option<String>, Option<String>)> = tx
                .query_row(
                    "SELECT guid, title, link, author, published_at, entry_html, media FROM revisions WHERE story_id = ? ORDER BY id DESC LIMIT 1",
                    [story_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
                )
                .optional()?;
            let Some((guid, rtitle, rlink, author, published_at, entry_html, media)) = latest else { return Ok(false) };
            let not_before = last_published_at.unwrap_or(0) + us.min_interval_min as i64 * 60;
            ingest::obsolete_pending(tx, story_id, now)?;
            tx.execute(
                "INSERT INTO revisions (story_id, feed_id, reason, state, stage, guid, title, link, author, published_at, entry_html, media,
                     content_html, source_text, lang, from_page, next_attempt_at, created_at, updated_at)
                 VALUES (?, ?, 'page_update', 'pending', 'transform', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?, ?)",
                params![story_id, feed_id, guid, rtitle, rlink, author, published_at, entry_html, media, html, src, lang, not_before.max(0), now, now],
            )?;
            tx.execute("UPDATE stories SET next_check_at = ?, check_count = check_count + 1 WHERE id = ?", params![next_check, story_id])?;
            Ok(true)
        })
        .await?;
    if created {
        app.event(Level::Info, "update", EventRef::story(feed_id, story_id), format!("原文页面已更新：{}", a.summary)).await;
        app.wake_pipeline.notify_one();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Admin actions
// ---------------------------------------------------------------------------

/// Result of queueing a reprocess for one story.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Queued {
    Inserted,
    /// A revision is already pending: it runs with the current settings anyway.
    Pending,
    /// No source data to reprocess from.
    Missing,
}

/// Insert a 'reprocess' revision for a story from its latest source data. A pending
/// revision (news update, page update, earlier reprocess) is never replaced — a news
/// update would be published silently as a reprocess — only hurried up to `priority`.
fn insert_reprocess(tx: &Transaction, story_id: i64, stage: &str, priority: i64, now: i64) -> Result<Queued> {
    let pending = tx.execute(
        "UPDATE revisions SET priority = MAX(priority, ?) WHERE story_id = ? AND state = 'pending'",
        params![priority, story_id],
    )?;
    if pending > 0 {
        return Ok(Queued::Pending);
    }
    #[allow(clippy::type_complexity)]
    let latest: Option<(i64, String, String, Option<String>, Option<String>, Option<i64>, Option<String>, Option<String>)> = tx
        .query_row(
            "SELECT feed_id, guid, title, link, author, published_at, entry_html, media FROM revisions WHERE story_id = ? ORDER BY id DESC LIMIT 1",
            [story_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)),
        )
        .optional()?;
    let Some((feed_id, guid, title, link, author, published_at, entry_html, media)) = latest else { return Ok(Queued::Missing) };
    tx.execute(
        "INSERT INTO revisions (story_id, feed_id, reason, state, stage, priority, guid, title, link, author, published_at, entry_html, media,
             next_attempt_at, created_at, updated_at)
         VALUES (?, ?, 'reprocess', 'pending', ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?)",
        params![story_id, feed_id, stage, priority, guid, title, link, author, published_at, entry_html, media, now, now],
    )?;
    tx.execute("UPDATE stories SET state = 'active' WHERE id = ? AND state = 'skipped'", [story_id])?;
    Ok(Queued::Inserted)
}

/// Re-run processing for a story from its latest source (publishes a new version).
pub async fn reprocess_story(app: &AppRef, story_id: i64) -> Result<Queued> {
    let now = util::now();
    let feed_id: i64 = app.db.call(move |c| Ok(c.query_row("SELECT feed_id FROM stories WHERE id = ?", [story_id], |r| r.get(0))?)).await?;
    let feed = app.db.call(move |c| load_feed(c, feed_id)).await?.ok_or_else(|| anyhow!("feed gone"))?;
    let stage = ingest::initial_stage(&feed.options);
    let queued = app.db.write(move |tx| insert_reprocess(tx, story_id, stage, 10, now)).await?;
    if queued != Queued::Missing {
        app.wake_pipeline.notify_one();
    }
    Ok(queued)
}

/// Which published articles a bulk reprocess covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReprocessScope {
    /// The N most recently seen stories (item ids change with every version, so they
    /// say nothing about the age of an article).
    Latest(u32),
    /// Articles first seen within the last N days.
    Days(u32),
    /// Every article still stored.
    All,
}

impl ReprocessScope {
    /// Parse "n50", "d7", "all".
    pub fn parse(s: &str) -> ReprocessScope {
        let s = s.trim();
        if s == "all" {
            return ReprocessScope::All;
        }
        if let Some(d) = s.strip_prefix('d').and_then(|x| x.parse::<u32>().ok()) {
            return ReprocessScope::Days(d.max(1));
        }
        if let Some(n) = s.strip_prefix('n').and_then(|x| x.parse::<u32>().ok()) {
            return ReprocessScope::Latest(n.max(1));
        }
        ReprocessScope::Latest(10)
    }

    pub fn label(&self) -> String {
        match self {
            ReprocessScope::Latest(n) => format!("最近 {n} 篇"),
            ReprocessScope::Days(d) => format!("最近 {d} 天"),
            ReprocessScope::All => "全部文章".into(),
        }
    }
}

/// Reprocess published articles of one feed (or all feeds) with the current settings.
/// Each article is re-published as a new version that keeps its read/star state and
/// date. Runs at low priority so fresh articles are not delayed. Stories with a pending
/// revision are left alone (it runs with the current settings; a double click doesn't
/// re-queue). Queued in short transactions. Returns the count.
pub async fn reprocess_published(app: &AppRef, feed_id: Option<i64>, scope: ReprocessScope, unread_only: bool) -> Result<usize> {
    let now = util::now();
    let rows: Vec<(i64, String)> = app
        .db
        .call(move |c| {
            let mut sql = String::from(
                "SELECT s.id, f.options FROM stories s JOIN feeds f ON f.id = s.feed_id JOIN items i ON i.id = s.current_item_id
                 WHERE s.state <> 'skipped'
                   AND NOT EXISTS (SELECT 1 FROM revisions v WHERE v.story_id = s.id AND v.state = 'pending')",
            );
            let mut p: Vec<rusqlite::types::Value> = Vec::new();
            if let Some(f) = feed_id {
                sql.push_str(" AND s.feed_id = ?");
                p.push(f.into());
            }
            if unread_only {
                sql.push_str(" AND i.is_read = 0");
            }
            if let ReprocessScope::Days(d) = scope {
                sql.push_str(" AND s.first_seen_at >= ?");
                p.push((now - d as i64 * 86400).into());
            }
            sql.push_str(" ORDER BY s.first_seen_at DESC, s.id DESC");
            if let ReprocessScope::Latest(n) = scope {
                sql.push_str(" LIMIT ?");
                p.push((n as i64).into());
            }
            let mut st = c.prepare(&sql)?;
            let rows = st.query_map(rusqlite::params_from_iter(p), |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
            Ok(rows)
        })
        .await?;
    let mut n = 0;
    for chunk in rows.chunks(REPROCESS_CHUNK) {
        let chunk = chunk.to_vec();
        n += app
            .db
            .write(move |tx| {
                let mut n = 0;
                for (story_id, opts) in chunk {
                    let stage = ingest::initial_stage(&FeedOptions::from_json(&opts));
                    if insert_reprocess(tx, story_id, stage, -10, now)? == Queued::Inserted {
                        n += 1;
                    }
                }
                Ok(n)
            })
            .await?;
    }
    if n > 0 {
        app.wake_pipeline.notify_one();
        app.event(
            Level::Info,
            "system",
            feed_id.map(EventRef::feed).unwrap_or_default(),
            format!("已安排重新处理 {n} 篇文章（范围：{}{}）", scope.label(), if unread_only { "，仅未读" } else { "" }),
        )
        .await;
    }
    Ok(n)
}

/// Retry failed revisions (all, or of one feed) immediately.
pub async fn retry_failed(app: &AppRef, feed_id: Option<i64>) -> Result<usize> {
    let now = util::now();
    let n = app
        .db
        .call(move |c| {
            let n = match feed_id {
                Some(f) => c.execute(
                    "UPDATE revisions SET state = 'pending', attempts = 0, unfinished = 0, next_attempt_at = 0, lease_until = NULL, updated_at = ?
                     WHERE state = 'failed' AND feed_id = ? AND id = (SELECT MAX(id) FROM revisions r2 WHERE r2.story_id = revisions.story_id)",
                    params![now, f],
                )?,
                None => c.execute(
                    "UPDATE revisions SET state = 'pending', attempts = 0, unfinished = 0, next_attempt_at = 0, lease_until = NULL, updated_at = ?
                     WHERE state = 'failed' AND id = (SELECT MAX(id) FROM revisions r2 WHERE r2.story_id = revisions.story_id)",
                    params![now],
                )?,
            };
            // Also hurry up pending revisions waiting for a retry — including translations
            // deferred by an open circuit breaker, which keep their attempt count. (Only
            // the first stage can be deferred by the update cooldown.)
            match feed_id {
                Some(f) => c.execute(
                    "UPDATE revisions SET next_attempt_at = 0 WHERE state = 'pending' AND feed_id = ? AND (attempts > 0 OR stage = 'translate')",
                    [f],
                )?,
                None => c.execute("UPDATE revisions SET next_attempt_at = 0 WHERE state = 'pending' AND (attempts > 0 OR stage = 'translate')", [])?,
            };
            Ok(n)
        })
        .await?;
    app.translator.reset_circuit();
    app.wake_pipeline.notify_one();
    Ok(n)
}

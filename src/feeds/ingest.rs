//! Turning parsed feed entries into stories and revisions.
//!
//! Identity: a story is matched by its normalized guid (`g:` key). If the guid is new
//! but the canonical link (`u:` key) belongs to an existing story whose guids are no
//! longer present in the document, it is the same story with a churned guid (e.g. feeds
//! whose guid is md5(title)). Links shared by several entries of one document are never
//! used for matching.
//!
//! Change handling: a changed entry of an already published story creates a revision
//! only when the change is substantive (or, with full text enabled, when the page must
//! be re-extracted to find out — the pipeline confirms the change after extraction).

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use rusqlite::{OptionalExtension, Transaction, params};

use super::parse::ParsedEntry;
use crate::settings::{FeedOptions, FulltextMode, Settings, UpdateMode, UpdateSettings};
use crate::util::text::{self, ChangeStats};
use crate::util::{self, urlnorm};

pub struct IngestContext<'a> {
    pub feed_id: i64,
    pub opts: &'a FeedOptions,
    pub settings: &'a Settings,
    pub now: i64,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct IngestReport {
    pub entries: usize,
    pub new_stories: usize,
    pub skipped: usize,
    pub unchanged: usize,
    pub updates: usize,
    pub ignored_changes: usize,
}

/// Outcome of comparing two versions of a story.
#[derive(Debug, Clone)]
pub struct ChangeAssessment {
    pub substantive: bool,
    pub title_changed: bool,
    pub stats: ChangeStats,
    pub summary: String,
}

pub fn assess_change(old_title: &str, old_text: &str, new_title: &str, new_text: &str, us: &UpdateSettings) -> ChangeAssessment {
    let stats = text::change_stats(old_text, new_text);
    let content_sub = stats.is_substantive(&us.thresholds());
    let title_changed = text::title_key(old_title) != text::title_key(new_title);
    let title_sim = text::title_similarity(old_title, new_title);
    let title_sub = title_changed && title_sim < 0.7 && (us.title_only_triggers || stats.added + stats.removed >= 10);
    let substantive = content_sub || title_sub;
    let summary = format!(
        "similarity {:.2}, +{} / -{} shingles, title {}",
        stats.similarity,
        stats.added,
        stats.removed,
        if title_changed { format!("changed ({title_sim:.2})") } else { "same".into() }
    );
    ChangeAssessment { substantive, title_changed, stats, summary }
}

/// Normalized source text of an HTML fragment (used for change detection).
pub fn source_text(html: &str) -> String {
    text::normalize_for_compare(&text::html_to_text(html))
}

pub fn initial_stage(opts: &FeedOptions) -> &'static str {
    if opts.fulltext != FulltextMode::Off { "extract" } else { "transform" }
}

fn entry_hash(e: &ParsedEntry) -> String {
    util::short_hash(&format!("{}\n{}\n{}", e.title, e.link.as_deref().unwrap_or(""), e.html))
}

struct Prepared<'a> {
    e: &'a ParsedEntry,
    gkey: String,
    ukey: String,
    published: i64,
}

#[derive(Debug)]
#[allow(dead_code)]
struct StoryRow {
    id: i64,
    title: String,
    state: String,
    entry_hash: Option<String>,
    source_title: Option<String>,
    source_text: Option<String>,
    version: i64,
    first_seen_at: i64,
    last_published_at: Option<i64>,
}

fn load_story(tx: &Transaction, id: i64) -> Result<StoryRow> {
    Ok(tx.query_row(
        "SELECT id, title, state, entry_hash, source_title, source_text, version, first_seen_at, last_published_at FROM stories WHERE id = ?",
        [id],
        |r| {
            Ok(StoryRow {
                id: r.get(0)?,
                title: r.get(1)?,
                state: r.get(2)?,
                entry_hash: r.get(3)?,
                source_title: r.get(4)?,
                source_text: r.get(5)?,
                version: r.get(6)?,
                first_seen_at: r.get(7)?,
                last_published_at: r.get(8)?,
            })
        },
    )?)
}

/// Insert a pending revision for a story from a feed entry.
#[allow(clippy::too_many_arguments)]
pub fn insert_revision(
    tx: &Transaction,
    story_id: i64,
    feed_id: i64,
    reason: &str,
    stage: &str,
    e: &ParsedEntry,
    published_at: i64,
    next_attempt_at: i64,
    now: i64,
) -> Result<i64> {
    tx.execute(
        "INSERT INTO revisions (story_id, feed_id, reason, state, stage, guid, title, link, author, published_at,
             entry_updated_at, entry_html, media, next_attempt_at, created_at, updated_at)
         VALUES (?, ?, ?, 'pending', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            story_id,
            feed_id,
            reason,
            stage,
            e.guid,
            e.title,
            e.link,
            e.author,
            published_at,
            e.updated,
            e.html,
            e.media.to_json(),
            next_attempt_at,
            now,
            now
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

/// Mark all pending revisions of a story obsolete. Returns how many were affected.
pub fn obsolete_pending(tx: &Transaction, story_id: i64, now: i64) -> Result<usize> {
    Ok(tx.execute(
        "UPDATE revisions SET state = 'obsolete', stage = 'done', lease_until = NULL, updated_at = ?
         WHERE story_id = ? AND state = 'pending'",
        params![now, story_id],
    )?)
}

fn add_keys(tx: &Transaction, feed_id: i64, story_id: i64, gkey: &str, ukey: Option<&str>) -> Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO story_keys(feed_id, key, story_id) VALUES (?, ?, ?)",
        params![feed_id, format!("g:{gkey}"), story_id],
    )?;
    if let Some(u) = ukey.filter(|u| !u.is_empty()) {
        tx.execute(
            "INSERT OR IGNORE INTO story_keys(feed_id, key, story_id) VALUES (?, ?, ?)",
            params![feed_id, format!("u:{u}"), story_id],
        )?;
    }
    Ok(())
}

/// Ingest the entries of one fetched feed document.
pub fn ingest_entries(tx: &Transaction, ctx: &IngestContext, entries: &[ParsedEntry]) -> Result<IngestReport> {
    let now = ctx.now;
    let mut report = IngestReport { entries: entries.len(), ..Default::default() };
    let fetch = &ctx.settings.fetch;
    let us = &ctx.settings.updates;

    // 1. Prepare and de-duplicate entries within the document.
    let mut seen_g: HashSet<String> = HashSet::new();
    let mut ukey_guids: HashMap<String, HashSet<String>> = HashMap::new();
    let mut prepared: Vec<Prepared> = Vec::new();
    for e in entries {
        let gkey = urlnorm::guid_key(&e.guid);
        let ukey = e.link.as_deref().map(urlnorm::url_key).unwrap_or_default();
        if !ukey.is_empty() {
            ukey_guids.entry(ukey.clone()).or_default().insert(gkey.clone());
        }
        if !seen_g.insert(gkey.clone()) {
            continue;
        }
        let mut published = e.published.or(e.updated).unwrap_or(now);
        if published > now + 86400 || published <= 0 {
            published = now;
        }
        prepared.push(Prepared { e, gkey, ukey, published });
    }
    let shared_ukeys: HashSet<String> =
        ukey_guids.iter().filter(|(_, g)| g.len() > 1).map(|(u, _)| u.clone()).collect();
    let doc_gkeys: HashSet<String> = prepared.iter().map(|p| format!("g:{}", p.gkey)).collect();

    // 2. First fetch? Then only the newest `initial_items` entries are processed.
    let existing: i64 = tx.query_row("SELECT COUNT(*) FROM stories WHERE feed_id = ?", [ctx.feed_id], |r| r.get(0))?;
    let mut process_allowed: HashSet<String> = HashSet::new();
    if existing == 0 && fetch.initial_items > 0 {
        let mut by_date: Vec<&Prepared> = prepared.iter().collect();
        by_date.sort_by(|a, b| b.published.cmp(&a.published));
        for p in by_date.into_iter().take(fetch.initial_items as usize) {
            process_allowed.insert(p.gkey.clone());
        }
    } else {
        process_allowed.extend(prepared.iter().map(|p| p.gkey.clone()));
    }
    let min_published = if fetch.max_entry_age_days > 0 { now - fetch.max_entry_age_days as i64 * 86400 } else { i64::MIN };

    // 3. Process oldest first so that publication order roughly follows story order.
    prepared.sort_by(|a, b| a.published.cmp(&b.published));
    let track_updates = ctx.opts.updates != UpdateMode::Off;
    let fulltext = ctx.opts.fulltext != FulltextMode::Off;

    for p in &prepared {
        let e = p.e;
        let gk = format!("g:{}", p.gkey);
        let uk = if p.ukey.is_empty() || shared_ukeys.contains(&p.ukey) { None } else { Some(p.ukey.clone()) };

        // Match by guid, then by link (guid churn).
        let mut story_id: Option<i64> = tx
            .query_row("SELECT story_id FROM story_keys WHERE feed_id = ? AND key = ?", params![ctx.feed_id, gk], |r| r.get(0))
            .optional()?;
        if story_id.is_none()
            && let Some(u) = &uk
        {
            let cand: Option<i64> = tx
                .query_row(
                    "SELECT story_id FROM story_keys WHERE feed_id = ? AND key = ?",
                    params![ctx.feed_id, format!("u:{u}")],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(cid) = cand {
                // Same story only if none of its guids is still present in this document.
                let mut stmt = tx.prepare_cached("SELECT key FROM story_keys WHERE story_id = ? AND key LIKE 'g:%'")?;
                let keys: Vec<String> = stmt.query_map([cid], |r| r.get(0))?.collect::<Result<_, _>>()?;
                if !keys.iter().any(|k| doc_gkeys.contains(k)) {
                    story_id = Some(cid);
                }
            }
        }

        let ehash = entry_hash(e);
        match story_id {
            None => {
                // New story.
                let skip = !process_allowed.contains(&p.gkey) || p.published < min_published;
                tx.execute(
                    "INSERT INTO stories (feed_id, guid, link, title, published_at, first_seen_at, last_seen_at, entry_hash, state)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    params![ctx.feed_id, e.guid, e.link, e.title, p.published, now, now, ehash, if skip { "skipped" } else { "active" }],
                )?;
                let sid = tx.last_insert_rowid();
                add_keys(tx, ctx.feed_id, sid, &p.gkey, uk.as_deref())?;
                if skip {
                    report.skipped += 1;
                } else {
                    insert_revision(tx, sid, ctx.feed_id, "new", initial_stage(ctx.opts), e, p.published, 0, now)?;
                    report.new_stories += 1;
                }
            }
            Some(sid) => {
                let s = load_story(tx, sid)?;
                tx.execute(
                    "UPDATE stories SET last_seen_at = ?, guid = ?, link = COALESCE(?, link) WHERE id = ?",
                    params![now, e.guid, e.link, sid],
                )?;
                add_keys(tx, ctx.feed_id, sid, &p.gkey, uk.as_deref())?;
                if s.entry_hash.as_deref() == Some(ehash.as_str()) || s.state == "skipped" {
                    report.unchanged += 1;
                    continue;
                }
                tx.execute("UPDATE stories SET entry_hash = ?, title = ? WHERE id = ?", params![ehash, e.title, sid])?;

                if s.version == 0 {
                    // Not published yet: restart processing with the newest source.
                    let pending: i64 = tx.query_row(
                        "SELECT COUNT(*) FROM revisions WHERE story_id = ? AND state = 'pending'",
                        [sid],
                        |r| r.get(0),
                    )?;
                    if pending > 0 || s.state == "active" {
                        obsolete_pending(tx, sid, now)?;
                        insert_revision(tx, sid, ctx.feed_id, "new", initial_stage(ctx.opts), e, p.published, 0, now)?;
                        report.updates += 1;
                    } else {
                        report.unchanged += 1;
                    }
                    continue;
                }

                // Published story: is it eligible for an update?
                let too_old = us.track_hours > 0 && s.first_seen_at < now - us.track_hours as i64 * 3600;
                let max_reached = us.max_versions > 0 && s.version >= us.max_versions as i64;
                // Without a baseline (dropped after the tracking window) nothing can be compared.
                let no_baseline = s.source_text.is_none();
                if !track_updates || too_old || max_reached || no_baseline || s.state != "active" {
                    report.ignored_changes += 1;
                    continue;
                }

                if !fulltext {
                    // Confirm right away using the feed content.
                    let new_text = source_text(&e.html);
                    let a = assess_change(
                        s.source_title.as_deref().unwrap_or(&s.title),
                        s.source_text.as_deref().unwrap_or(""),
                        &e.title,
                        &new_text,
                        us,
                    );
                    if !a.substantive {
                        report.ignored_changes += 1;
                        continue;
                    }
                    tracing::debug!(story = sid, "entry update: {}", a.summary);
                }

                // Respect the minimum interval between versions (coalesces bursts).
                let not_before = s.last_published_at.unwrap_or(0) + us.min_interval_min as i64 * 60;
                let next_attempt = if not_before > now { not_before } else { 0 };
                obsolete_pending(tx, sid, now)?;
                insert_revision(tx, sid, ctx.feed_id, "entry_update", initial_stage(ctx.opts), e, p.published, next_attempt, now)?;
                report.updates += 1;
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::feeds::parse::Media;

    fn entry(guid: &str, title: &str, link: &str, html: &str, published: i64) -> ParsedEntry {
        ParsedEntry {
            guid: guid.into(),
            title: title.into(),
            link: Some(link.into()),
            author: None,
            html: html.into(),
            published: Some(published),
            updated: None,
            media: Media::default(),
        }
    }

    fn setup() -> (tempfile::TempDir, Db, i64) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let conn = db.conn().unwrap();
        conn.execute(
            "INSERT INTO feeds (url, title, created_at, updated_at) VALUES ('https://x/rss', 'X', 0, 0)",
            [],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        (dir, db, id)
    }

    fn run(db: &Db, feed_id: i64, opts: &FeedOptions, settings: &Settings, now: i64, entries: &[ParsedEntry]) -> IngestReport {
        let mut conn = db.conn().unwrap();
        let tx = conn.transaction().unwrap();
        let r = ingest_entries(&tx, &IngestContext { feed_id, opts, settings, now }, entries).unwrap();
        tx.commit().unwrap();
        r
    }

    fn publish_all(db: &Db, now: i64) {
        // Simulate the pipeline publishing every pending revision.
        let conn = db.conn().unwrap();
        let revs: Vec<(i64, i64, String, String)> = conn
            .prepare("SELECT id, story_id, title, entry_html FROM revisions WHERE state = 'pending'")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        for (rid, sid, title, html) in revs {
            conn.execute("UPDATE revisions SET state = 'published', stage = 'done' WHERE id = ?", [rid]).unwrap();
            conn.execute(
                "UPDATE stories SET version = version + 1, last_published_at = ?, source_title = ?, source_text = ? WHERE id = ?",
                params![now, title, source_text(&html), sid],
            )
            .unwrap();
        }
    }

    fn count(db: &Db, sql: &str) -> i64 {
        db.conn().unwrap().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    const BODY: &str = "<p>French authorities have imposed overnight curfews in parts of central Paris and other cities after a weekend of riots and looting across the country.</p><p>Police in Paris said a curfew would be in force between 23:00 local time and 06:00 in Place de la Republique.</p>";

    #[test]
    fn bbc_style_fragment_guids_do_not_duplicate() {
        let (_d, db, fid) = setup();
        let opts = FeedOptions::default();
        let settings = Settings::default();
        let now = 1_800_000_000;
        let e1 = entry("https://www.bbc.co.uk/news/articles/abc#1", "Curfew in Paris", "https://www.bbc.co.uk/news/articles/abc?at_medium=RSS", BODY, now - 100);
        let r = run(&db, fid, &opts, &settings, now, &[e1]);
        assert_eq!(r.new_stories, 1);
        publish_all(&db, now);
        // Same article moved to another front-page section: '#2'
        let e2 = entry("https://www.bbc.co.uk/news/articles/abc#2", "Curfew in Paris", "https://www.bbc.co.uk/news/articles/abc?at_medium=RSS&at_campaign=rss", BODY, now - 100);
        let r = run(&db, fid, &opts, &settings, now + 600, &[e2.clone()]);
        assert_eq!(r.new_stories, 0);
        assert_eq!(r.updates, 0);
        // Both '#1' and '#3' in the same document -> still one story.
        let e3 = entry("https://www.bbc.co.uk/news/articles/abc#3", "Curfew in Paris", "https://www.bbc.co.uk/news/articles/abc", BODY, now - 100);
        let r = run(&db, fid, &opts, &settings, now + 1200, &[e2, e3]);
        assert_eq!(r.new_stories, 0);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM stories"), 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM revisions"), 1);
    }

    #[test]
    fn guid_churn_with_same_link_is_same_story() {
        let (_d, db, fid) = setup();
        let opts = FeedOptions::default();
        let settings = Settings::default();
        let now = 1_800_000_000;
        let link = "https://www.bbc.com/zhongwen/articles/cwp9g811d5ydo/trad?at_medium=RSS&at_campaign=rss";
        run(&db, fid, &opts, &settings, now, &[entry("md5-title-1", "法國騷亂：巴黎局部宵禁", link, BODY, now - 10)]);
        publish_all(&db, now);
        // Title edited => guid (md5 of title) changes, content unchanged.
        let r = run(&db, fid, &opts, &settings, now + 3600, &[entry("md5-title-2", "法國騷亂：巴黎局部宵禁 被劫商品現身二手平台", link, BODY, now - 10)]);
        assert_eq!(r.new_stories, 0);
        assert_eq!(r.updates, 0, "title-only change must not create a new version");
        assert_eq!(count(&db, "SELECT COUNT(*) FROM stories"), 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM story_keys"), 3);
    }

    #[test]
    fn shared_links_are_not_merged() {
        let (_d, db, fid) = setup();
        let opts = FeedOptions::default();
        let settings = Settings::default();
        let now = 1_800_000_000;
        let a = entry("post-1", "Post one", "https://example.com/live", "<p>one</p>", now - 20);
        let b = entry("post-2", "Post two", "https://example.com/live", "<p>two</p>", now - 10);
        let r = run(&db, fid, &opts, &settings, now, &[a.clone(), b]);
        assert_eq!(r.new_stories, 2);
        let c = entry("post-3", "Post three", "https://example.com/live", "<p>three</p>", now);
        let r = run(&db, fid, &opts, &settings, now + 60, &[a, c]);
        assert_eq!(r.new_stories, 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM stories"), 3);
    }

    #[test]
    fn substantive_update_creates_revision_with_cooldown() {
        let (_d, db, fid) = setup();
        let opts = FeedOptions::default();
        let settings = Settings::default();
        let now = 1_800_000_000;
        let link = "https://example.com/a";
        run(&db, fid, &opts, &settings, now, &[entry("g1", "Quake hits city", link, BODY, now)]);
        publish_all(&db, now);
        let longer = format!(
            "{BODY}<p>More than 2,500 people have been arrested, officials said, and about 1,000 police officers were injured in clashes over the weekend. The prime minister announced a fund for affected businesses and promised a new law to make rioters pay for damage.</p>"
        );
        let r = run(&db, fid, &opts, &settings, now + 300, &[entry("g1", "Quake hits city, dozens hurt", link, &longer, now)]);
        assert_eq!(r.updates, 1);
        let (reason, next): (String, i64) = db
            .conn()
            .unwrap()
            .query_row("SELECT reason, next_attempt_at FROM revisions WHERE state = 'pending'", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!(reason, "entry_update");
        assert_eq!(next, now + 30 * 60, "deferred until the minimum interval passed");

        // A second change while pending replaces the pending revision.
        let longer2 = format!("{longer}<p>Another paragraph with significant new information about the government's response and the opposition's demands for an inquiry into police tactics.</p>");
        let r = run(&db, fid, &opts, &settings, now + 400, &[entry("g1", "Quake hits city, dozens hurt", link, &longer2, now)]);
        assert_eq!(r.updates, 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM revisions WHERE state = 'pending'"), 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM revisions WHERE state = 'obsolete'"), 1);
    }

    #[test]
    fn trivial_change_ignored_and_limits_respected() {
        let (_d, db, fid) = setup();
        let opts = FeedOptions::default();
        let mut settings = Settings::default();
        let now = 1_800_000_000;
        let link = "https://example.com/a";
        run(&db, fid, &opts, &settings, now, &[entry("g1", "Title", link, BODY, now)]);
        publish_all(&db, now);
        let tweaked = BODY.replace("weekend", "week-end");
        let r = run(&db, fid, &opts, &settings, now + 60, &[entry("g1", "Title", link, &tweaked, now)]);
        assert_eq!(r.updates, 0);
        assert_eq!(r.ignored_changes, 1);

        // Too old for tracking
        settings.updates.track_hours = 1;
        let big = format!("{BODY}{}", "<p>Completely new paragraph number with lots of fresh words to make the change substantive and obvious.</p>".repeat(3));
        let r = run(&db, fid, &opts, &settings, now + 7200, &[entry("g1", "Title", link, &big, now)]);
        assert_eq!(r.updates, 0);
    }

    #[test]
    fn first_fetch_limits_and_age() {
        let (_d, db, fid) = setup();
        let opts = FeedOptions::default();
        let mut settings = Settings::default();
        settings.fetch.initial_items = 2;
        settings.fetch.max_entry_age_days = 7;
        let now = 1_800_000_000;
        let entries: Vec<ParsedEntry> = (0..5)
            .map(|i| entry(&format!("g{i}"), &format!("T{i}"), &format!("https://e.com/{i}"), "<p>x</p>", now - i * 60))
            .collect();
        let r = run(&db, fid, &opts, &settings, now, &entries);
        assert_eq!(r.new_stories, 2);
        assert_eq!(r.skipped, 3);
        // Skipped stories stay skipped on later fetches.
        let r = run(&db, fid, &opts, &settings, now + 60, &entries);
        assert_eq!(r.new_stories, 0);
        // A new entry older than max age is skipped.
        let old = entry("old", "Old", "https://e.com/old", "<p>x</p>", now - 30 * 86400);
        let r = run(&db, fid, &opts, &settings, now + 120, &[old]);
        assert_eq!(r.skipped, 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM revisions"), 2);
    }
}

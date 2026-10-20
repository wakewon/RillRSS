//! Periodic cleanup: article retention, superseded versions, old revisions, orphaned
//! stories, image cache GC, translation cache pruning, event log trimming.

use std::time::Duration;

use anyhow::Result;
use rusqlite::params;
use serde::Serialize;

use crate::app::{AppRef, EventRef, Level};
use crate::settings::FeedOptions;
use crate::util;

#[derive(Debug, Default, Clone, Serialize)]
pub struct CleanupReport {
    pub items_deleted: usize,
    pub superseded_deleted: usize,
    pub revisions_deleted: usize,
    pub stories_deleted: usize,
    pub images_deleted: usize,
    pub image_bytes_freed: i64,
    /// Dropped to stay under the cache size cap (their articles use the originals again).
    pub images_evicted: usize,
    pub translations_deleted: usize,
    pub events_deleted: usize,
    pub sessions_deleted: usize,
}

impl CleanupReport {
    pub fn summary(&self) -> String {
        format!(
            "删除文章 {}、旧版本 {}、处理记录 {}、新闻条目 {}、图片 {}（{}{}）、翻译缓存 {}、日志 {}、会话 {}",
            self.items_deleted,
            self.superseded_deleted,
            self.revisions_deleted,
            self.stories_deleted,
            self.images_deleted + self.images_evicted,
            util::human_bytes(self.image_bytes_freed.max(0) as u64),
            if self.images_evicted > 0 { format!("，其中 {} 张因超出缓存上限，相关文章改回原图地址", self.images_evicted) } else { String::new() },
            self.translations_deleted,
            self.events_deleted,
            self.sessions_deleted
        )
    }
}

/// `now - days` in unix seconds, saturating (day counts come from user settings).
fn days_ago(now: i64, days: u32) -> i64 {
    now.saturating_sub((days as i64).saturating_mul(86400))
}

/// `days_ago` in microseconds (the unit of `items.created_at`).
fn days_ago_us(now: i64, days: u32) -> i64 {
    days_ago(now, days).saturating_mul(1_000_000)
}

pub async fn run_maintenance(app: AppRef) {
    // First run shortly after start, then hourly.
    let mut delay = Duration::from_secs(300);
    loop {
        tokio::select! {
            _ = app.shutdown.cancelled() => break,
            _ = tokio::time::sleep(delay) => {},
        }
        delay = Duration::from_secs(3600);
        match cleanup(&app).await {
            Ok(r) => {
                if r.items_deleted + r.superseded_deleted + r.images_deleted + r.images_evicted + r.stories_deleted > 0 {
                    app.event(Level::Info, "cleanup", EventRef::default(), r.summary()).await;
                }
            }
            Err(e) => app.event(Level::Error, "cleanup", EventRef::default(), format!("清理失败：{e:#}")).await,
        }
    }
}

/// Run all cleanup tasks once.
pub async fn cleanup(app: &AppRef) -> Result<CleanupReport> {
    let settings = app.settings.get().await;
    let r = settings.retention.clone();
    let now = util::now();
    let mut report = CleanupReport::default();

    // Per-feed retention.
    let feeds: Vec<(i64, String)> = app
        .db
        .call(|c| {
            let mut st = c.prepare("SELECT id, options FROM feeds")?;
            let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(v)
        })
        .await?;
    for (feed_id, opts) in feeds {
        let o = FeedOptions::from_json(&opts);
        let days = o.retention_days.unwrap_or(r.days);
        let keep_unread = r.keep_unread;
        let max_items = r.max_items_per_feed;
        let n = app
            .db
            .write(move |tx| {
                let mut n = 0;
                if days > 0 {
                    let cutoff_us = days_ago_us(now, days);
                    n += tx.execute(
                        "DELETE FROM items WHERE feed_id = ? AND created_at < ? AND is_starred = 0 AND (? = 0 OR is_read = 1 OR hidden = 1)",
                        params![feed_id, cutoff_us, keep_unread as i64],
                    )?;
                }
                if max_items > 0 {
                    n += tx.execute(
                        "DELETE FROM items WHERE feed_id = ?1 AND is_starred = 0 AND id NOT IN (
                             SELECT id FROM items WHERE feed_id = ?1 AND hidden = 0 ORDER BY id DESC LIMIT ?2)
                           AND (?3 = 0 OR is_read = 1 OR hidden = 1)",
                        params![feed_id, max_items as i64, keep_unread as i64],
                    )?;
                }
                Ok(n)
            })
            .await?;
        report.items_deleted += n;
    }

    // Superseded versions (0 = no extra limit: they follow the article retention, like
    // `days = 0` means "keep").
    if r.superseded_days > 0 {
        let sup_cutoff = days_ago_us(now, r.superseded_days);
        report.superseded_deleted = app
            .db
            .write(move |tx| Ok(tx.execute("DELETE FROM items WHERE hidden = 1 AND superseded_by IS NOT NULL AND is_starred = 0 AND created_at < ?", [sup_cutoff])?))
            .await?;
    }

    // Finished revisions (keep the newest per story: it carries the entry data used for
    // reprocessing and page updates).
    let rev_cutoff = now - 7 * 86400;
    report.revisions_deleted = app
        .db
        .write(move |tx| {
            Ok(tx.execute(
                "DELETE FROM revisions WHERE state <> 'pending' AND updated_at < ?
                   AND id NOT IN (SELECT MAX(id) FROM revisions GROUP BY story_id)",
                [rev_cutoff],
            )?)
        })
        .await?;

    // Stories that left the feed long ago and have no articles left.
    let story_cutoff = days_ago(now, r.days.max(7).saturating_add(7));
    report.stories_deleted = app
        .db
        .write(move |tx| {
            Ok(tx.execute(
                "DELETE FROM stories WHERE last_seen_at < ?
                   AND NOT EXISTS (SELECT 1 FROM items i WHERE i.story_id = stories.id)
                   AND NOT EXISTS (SELECT 1 FROM revisions v WHERE v.story_id = stories.id AND v.state = 'pending')",
                [story_cutoff],
            )?)
        })
        .await?;

    // Drop change-detection baselines of stories past the update-tracking window
    // (track_hours = 0 tracks without a time limit: keep them).
    let track = settings.updates.track_hours as i64;
    if track > 0 {
        let baseline_cutoff = now.saturating_sub(track.saturating_mul(3600)).saturating_sub(86400);
        app.db
            .write(move |tx| {
                Ok(tx.execute(
                    "UPDATE stories SET source_text = NULL, next_check_at = NULL WHERE first_seen_at < ? AND source_text IS NOT NULL",
                    [baseline_cutoff],
                )?)
            })
            .await?;
    }

    // Image cache.
    let (n, freed) = app.images.gc(86400).await?;
    report.images_deleted = n;
    report.image_bytes_freed = freed;
    let cap = settings.images.max_total_mb as i64 * 1024 * 1024;
    let (n, freed) = app.images.evict(cap).await?;
    report.images_evicted = n;
    report.image_bytes_freed += freed;
    // Failed downloads, and sources of dropped images (kept for a while so that `/img/`
    // can redirect copies that clients already have).
    let sources_cutoff = now - 30 * 86400;
    app.db
        .call(move |c| {
            Ok(c.execute(
                "DELETE FROM image_sources WHERE fetched_at < ?
                   AND (status <> 'ok' OR NOT EXISTS (SELECT 1 FROM images i WHERE i.hash = image_sources.hash))",
                [sources_cutoff],
            )?)
        })
        .await?;

    // Translation cache.
    if r.translation_cache_days > 0 {
        report.translations_deleted = app.translator.prune_cache(r.translation_cache_days).await?;
    }

    // Event log and sessions.
    let keep = r.event_rows.max(100) as i64;
    report.events_deleted = app
        .db
        .call(move |c| Ok(c.execute("DELETE FROM events WHERE id <= (SELECT MAX(id) FROM events) - ?", [keep])?))
        .await?;
    let api_cutoff = now - 365 * 86400;
    report.sessions_deleted = app
        .db
        .call(move |c| {
            Ok(c.execute(
                "DELETE FROM sessions WHERE (expires_at IS NOT NULL AND expires_at < ?) OR (kind <> 'web' AND last_used_at < ?)",
                params![now, api_cutoff],
            )?)
        })
        .await?;

    app.db
        .call(|c| {
            c.execute_batch("PRAGMA incremental_vacuum(4000); PRAGMA optimize;")?;
            let _ = c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
            Ok(())
        })
        .await?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cutoffs_saturate() {
        let now = 1_800_000_000;
        assert_eq!(days_ago(now, 1), now - 86400);
        assert_eq!(days_ago_us(now, 30), (now - 30 * 86400) * 1_000_000);
        // Absurd day counts give a cutoff far in the past, never a wrapped future one.
        assert!(days_ago_us(now, 999_999_999) < 0);
        assert!(days_ago_us(now, u32::MAX) < 0);
    }
}

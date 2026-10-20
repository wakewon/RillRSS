//! News update handling: substantive revisions are re-published under a new id, the old
//! version leaves the unread list, stars/labels carry over, trivial edits are ignored.

mod common;

use common::*;
use serde_json::json;

#[tokio::test]
async fn substantive_update_republishes_with_new_id() {
    let mock = start_mock().await;
    let t = start_app(|_| {}).await;
    let body1 = paragraphs("Original", 3);
    mock.set("/feed.xml", "application/rss+xml", rss("News", &[entry("story-1", "Quake hits city", "https://news.example.com/a/1", &body1)]));
    let fid = t.add_feed(&mock.url("/feed.xml"), json!({})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 1);
    let v1: i64 = t.count("SELECT id FROM items").await;

    // User stars and labels v1 (but leaves it unread).
    let c = t.app.db.conn().unwrap();
    rillrss::store::set_starred(&c, &[v1], true).unwrap();
    let l = rillrss::store::ensure_label(&c, "Follow").unwrap();
    rillrss::store::set_label(&c, &[v1], l, true).unwrap();
    drop(c);

    // The story is substantively updated in the feed.
    let body2 = format!("{body1}{}", paragraphs("Breaking update", 3));
    mock.set(
        "/feed.xml",
        "application/rss+xml",
        rss("News", &[entry("story-1", "Quake hits city, dozens injured", "https://news.example.com/a/1", &body2)]),
    );
    t.fetch(fid).await;
    t.wait_idle().await;

    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 2);
    let v2: i64 = t.count("SELECT MAX(id) FROM items").await;
    assert!(v2 > v1);
    let (title, is_read, starred, version): (String, i64, i64, i64) = t
        .query("SELECT title, is_read, is_starred, version FROM items ORDER BY id DESC LIMIT 1", |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .await;
    assert!(title.starts_with("[更新]"), "{title}");
    assert!(title.contains("dozens injured"));
    assert_eq!(is_read, 0, "new version is unread");
    assert_eq!(starred, 1, "star carried over");
    assert_eq!(version, 2);
    let content: String = t.query("SELECT content FROM items ORDER BY id DESC LIMIT 1", |r| r.get(0)).await;
    assert!(content.contains("本文已更新") && content.contains("v2"), "update notice");
    assert!(content.contains("Breaking update"));
    // Old version: read, still listed (not hidden), still starred, points at the new one.
    let (old_read, old_hidden, old_star, sup): (i64, i64, i64, i64) = t
        .query("SELECT is_read, hidden, is_starred, superseded_by FROM items ORDER BY id ASC LIMIT 1", |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .await;
    assert_eq!((old_read, old_hidden, old_star, sup), (1, 0, 1, v2));
    // Labels copied.
    assert_eq!(t.count("SELECT COUNT(*) FROM item_labels").await, 2);

    // Re-fetching the same document does nothing.
    t.fetch(fid).await;
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 2);
}

#[tokio::test]
async fn trivial_edits_and_guid_churn_do_not_duplicate() {
    let mock = start_mock().await;
    let t = start_app(|_| {}).await;
    let body = paragraphs("Stable", 4);
    // BBC style: guid carries a section index fragment; links carry tracking params.
    mock.set(
        "/bbc.xml",
        "application/rss+xml",
        rss("BBC", &[entry("https://www.bbc.co.uk/news/articles/abc#1", "Curfew in Paris", "https://www.bbc.co.uk/news/articles/abc?at_medium=RSS", &body)]),
    );
    let fid = t.add_feed(&mock.url("/bbc.xml"), json!({})).await;
    t.fetch(fid).await;
    t.wait_idle().await;

    // Moved to another section (#2), tracking params changed, a typo fixed.
    let tweaked = body.replacen(": ", " - ", 1);
    mock.set(
        "/bbc.xml",
        "application/rss+xml",
        rss(
            "BBC",
            &[
                entry("https://www.bbc.co.uk/news/articles/abc#2", "Curfew in Paris", "https://www.bbc.co.uk/news/articles/abc?at_medium=RSS&at_campaign=rss", &tweaked),
                entry("https://www.bbc.co.uk/news/articles/abc#4", "Curfew in Paris", "https://www.bbc.co.uk/news/articles/abc", &tweaked),
            ],
        ),
    );
    t.fetch(fid).await;
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 1);
    assert_eq!(t.count("SELECT COUNT(*) FROM stories").await, 1);

    // md5(title)-style guid churn: new guid, same link, title tweak only.
    mock.set(
        "/bbc.xml",
        "application/rss+xml",
        rss("BBC", &[entry("0123456789abcdef", "Curfew imposed in Paris", "https://www.bbc.co.uk/news/articles/abc?at_medium=RSS", &tweaked)]),
    );
    t.fetch(fid).await;
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 1, "title-only change must not create a new version");
    assert_eq!(t.count("SELECT COUNT(*) FROM stories").await, 1);
}

#[tokio::test]
async fn cooldown_coalesces_and_max_versions_caps() {
    let mock = start_mock().await;
    let t = start_app(|s| {
        s.updates.min_interval_min = 60;
        s.updates.max_versions = 2;
    })
    .await;
    let feed = |extra: usize| rss("Live", &[entry("live-1", "Live updates", "https://news.example.com/live", &paragraphs(&format!("Update {extra}"), 2 + extra))]);
    mock.set("/live.xml", "application/rss+xml", feed(0));
    let fid = t.add_feed(&mock.url("/live.xml"), json!({})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 1);

    // Two quick updates within the cooldown: one pending revision, deferred.
    mock.set("/live.xml", "application/rss+xml", feed(3));
    t.fetch(fid).await;
    mock.set("/live.xml", "application/rss+xml", feed(6));
    t.fetch(fid).await;
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 1, "still in cooldown");
    assert_eq!(t.count("SELECT COUNT(*) FROM revisions WHERE state = 'pending'").await, 1);
    assert_eq!(t.count("SELECT COUNT(*) FROM revisions WHERE state = 'obsolete'").await, 1);

    // Cooldown over: the newest source is published once.
    t.app.db.call(|c| Ok(c.execute("UPDATE revisions SET next_attempt_at = 0 WHERE state = 'pending'", [])?)).await.unwrap();
    t.app.wake_pipeline.notify_one();
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 2);
    let content: String = t.query("SELECT content FROM items ORDER BY id DESC LIMIT 1", |r| r.get(0)).await;
    assert!(content.contains("Update 6"), "the newest source wins");

    // max_versions = 2 reached: further updates are ignored.
    t.app.db.call(|c| Ok(c.execute("UPDATE stories SET last_published_at = 0", [])?)).await.unwrap();
    mock.set("/live.xml", "application/rss+xml", feed(10));
    t.fetch(fid).await;
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 2);
    assert_eq!(t.count("SELECT COUNT(*) FROM revisions WHERE state = 'pending'").await, 0);
}

#[tokio::test]
async fn original_page_updates_are_detected() {
    let mock = start_mock().await;
    let t = start_app(|s| {
        s.updates.page_check_minutes = vec![0, 0, 0];
        s.fulltext.min_text_chars = 50;
    })
    .await;
    let page = |body: &str| {
        format!(
            "<html><head><title>Story</title></head><body><nav><a href='/'>Home</a></nav><article><h1>Story</h1>{body}</article><footer>(c)</footer></body></html>"
        )
    };
    mock.set("/article/1", "text/html; charset=utf-8", page(&paragraphs("First draft", 5)));
    let link = mock.url("/article/1");
    mock.set("/full.xml", "application/rss+xml", rss("Full", &[entry("full-1", "Developing story", &link, "<p>Short summary.</p>")]));
    let fid = t.add_feed(&mock.url("/full.xml"), json!({"fulltext": "readability", "updates": "page"})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 1);
    let content: String = t.query("SELECT content FROM items", |r| r.get(0)).await;
    assert!(content.contains("First draft paragraph 4"), "full text extracted: {content}");

    // Page unchanged -> check does nothing.
    rillrss::pipeline::check_due_pages(&t.app).await.unwrap();
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 1);

    // The article page gets substantial new paragraphs (feed entry unchanged).
    t.app.db.call(|c| Ok(c.execute("UPDATE stories SET next_check_at = 0", [])?)).await.unwrap();
    mock.set("/article/1", "text/html; charset=utf-8", page(&format!("{}{}", paragraphs("First draft", 5), paragraphs("Late breaking", 4))));
    rillrss::pipeline::check_due_pages(&t.app).await.unwrap();
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 2);
    let content: String = t.query("SELECT content FROM items ORDER BY id DESC LIMIT 1", |r| r.get(0)).await;
    assert!(content.contains("Late breaking paragraph 3"));
    assert_eq!(t.count("SELECT COUNT(*) FROM items WHERE is_read = 1").await, 1);
}

#[tokio::test]
async fn entry_change_with_fulltext_confirms_on_page() {
    let mock = start_mock().await;
    let t = start_app(|s| s.fulltext.min_text_chars = 50).await;
    let page = |body: &str| format!("<html><body><article><h1>T</h1>{body}</article></body></html>");
    mock.set("/p/2", "text/html", page(&paragraphs("Body", 5)));
    let link = mock.url("/p/2");
    mock.set("/f.xml", "application/rss+xml", rss("F", &[entry("g2", "Title", &link, "<p>Summary A.</p>")]));
    let fid = t.add_feed(&mock.url("/f.xml"), json!({"fulltext": "readability"})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    // Summary changes but the page does not: the change is not confirmed.
    mock.set("/f.xml", "application/rss+xml", rss("F", &[entry("g2", "Title", &link, "<p>Summary B, reworded.</p>")]));
    t.fetch(fid).await;
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 1);
    assert_eq!(t.count("SELECT COUNT(*) FROM revisions WHERE state = 'unchanged'").await, 1);
}

#[tokio::test]
async fn bulk_reprocess_scopes_keep_read_state() {
    use rillrss::pipeline::{ReprocessScope, reprocess_published};
    let mock = start_mock().await;
    let t = start_app(|_| {}).await;
    let entries: Vec<Entry> = (0..5)
        .map(|i| {
            let mut e = entry(&format!("t{i}"), &format!("法國新聞 {i}"), &format!("https://n.example.com/t{i}"), "<p>法國多地上週末爆發騷亂及搶掠，當局其後實施夜間宵禁。</p>");
            e.pub_date = format!("Tue, 20 Oct 2026 0{i}:00:00 GMT");
            e
        })
        .collect();
    mock.set("/tw.xml", "application/rss+xml", rss("TW", &entries));
    let fid = t.add_feed(&mock.url("/tw.xml"), json!({})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 5);
    // Read the two oldest articles, star one.
    t.app
        .db
        .call(|c| {
            c.execute("UPDATE items SET is_read = 1, read_at = 1 WHERE title IN ('法國新聞 0', '法國新聞 1')", [])?;
            c.execute("UPDATE items SET is_starred = 1 WHERE title = '法國新聞 0'", [])?;
            Ok(())
        })
        .await
        .unwrap();
    let published_before: i64 = t.count("SELECT published_at FROM items WHERE title = '法國新聞 0'").await;
    // Settings change: enable Traditional -> Simplified conversion.
    t.app.db.call(move |c| Ok(c.execute("UPDATE feeds SET options = '{\"opencc\":\"t2s\"}' WHERE id = ?", [fid])?)).await.unwrap();

    // Latest 2 only.
    assert_eq!(reprocess_published(&t.app, Some(fid), ReprocessScope::Latest(2), false).await.unwrap(), 2);
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 7);
    assert_eq!(t.count("SELECT COUNT(*) FROM items WHERE title LIKE '法国新闻%'").await, 2, "newest two converted");

    // Unread only: the remaining unread unconverted article (and the two converted ones).
    let n = reprocess_published(&t.app, Some(fid), ReprocessScope::All, true).await.unwrap();
    assert_eq!(n, 3);
    t.wait_idle().await;

    // Everything else.
    let n = reprocess_published(&t.app, Some(fid), ReprocessScope::All, false).await.unwrap();
    assert_eq!(n, 5);
    t.wait_idle().await;
    let visible_current: i64 = t.count("SELECT COUNT(*) FROM items WHERE superseded_by IS NULL").await;
    assert_eq!(visible_current, 5);
    assert_eq!(t.count("SELECT COUNT(*) FROM items WHERE superseded_by IS NULL AND title LIKE '法国新闻%'").await, 5);
    // Read state and stars are kept, no "[更新]" prefix, original dates kept.
    let (read, starred, title, published): (i64, i64, String, i64) = t
        .query("SELECT is_read, is_starred, title, published_at FROM items WHERE superseded_by IS NULL AND title = '法国新闻 0'", |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .await;
    assert_eq!((read, starred, title.as_str()), (1, 1, "法国新闻 0"));
    assert_eq!(published, published_before);
    assert_eq!(t.count("SELECT COUNT(*) FROM items WHERE superseded_by IS NULL AND is_read = 0").await, 3, "unread stay unread");
    // Old copies of already-read articles are hidden; unread ones stay listed (as read).
    assert!(t.count("SELECT COUNT(*) FROM items WHERE superseded_by IS NOT NULL AND hidden = 0 AND is_read = 1").await >= 3);
    assert!(t.count("SELECT COUNT(*) FROM items i WHERE superseded_by IS NOT NULL AND hidden = 1").await >= 2);
}

#[test]
fn reprocess_scope_parsing() {
    use rillrss::pipeline::ReprocessScope;
    assert_eq!(ReprocessScope::parse("all"), ReprocessScope::All);
    assert_eq!(ReprocessScope::parse("d7"), ReprocessScope::Days(7));
    assert_eq!(ReprocessScope::parse("n200"), ReprocessScope::Latest(200));
    assert_eq!(ReprocessScope::parse("bogus"), ReprocessScope::Latest(10));
}

async fn exec(t: &TestApp, sql: impl Into<String>) {
    let sql = sql.into();
    t.app.db.call(move |c| Ok(c.execute_batch(&sql)?)).await.unwrap();
}

fn one_story(title: &str, body: &str) -> String {
    rss("News", &[entry("s1", title, "https://news.example.com/a/1", body)])
}

#[tokio::test]
async fn reprocess_never_replaces_a_pending_update() {
    use rillrss::pipeline::{ReprocessScope, reprocess_published, reprocess_story};
    let mock = start_mock().await;
    let t = start_app(|s| s.updates.min_interval_min = 60).await;
    let body1 = paragraphs("Original", 3);
    mock.set("/f.xml", "application/rss+xml", one_story("Quake hits city", &body1));
    let fid = t.add_feed(&mock.url("/f.xml"), json!({})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    exec(&t, "UPDATE items SET is_read = 1, read_at = 1").await;

    // A substantive update waits for the cooldown.
    let body2 = format!("{body1}{}", paragraphs("Breaking update", 3));
    mock.set("/f.xml", "application/rss+xml", one_story("Quake hits city, dozens injured", &body2));
    t.fetch(fid).await;
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM revisions WHERE state = 'pending' AND reason = 'entry_update'").await, 1);

    // Bulk reprocess (clicked twice) leaves it alone; a single-story reprocess only hurries it.
    assert_eq!(reprocess_published(&t.app, Some(fid), ReprocessScope::Latest(10), false).await.unwrap(), 0);
    assert_eq!(reprocess_published(&t.app, None, ReprocessScope::All, false).await.unwrap(), 0);
    assert!(reprocess_story(&t.app, 1).await.unwrap());
    assert_eq!(t.count("SELECT COUNT(*) FROM revisions").await, 2, "nothing re-queued");
    assert_eq!(t.count("SELECT priority FROM revisions WHERE state = 'pending'").await, 10);

    // After the cooldown it is published as the news update it is.
    exec(&t, "UPDATE revisions SET next_attempt_at = 0 WHERE state = 'pending'").await;
    t.app.wake_pipeline.notify_one();
    t.wait_idle().await;
    let (title, is_read, content): (String, i64, String) =
        t.query("SELECT title, is_read, content FROM items ORDER BY id DESC LIMIT 1", |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).await;
    assert!(title.starts_with("[更新]"), "{title}");
    assert_eq!(is_read, 0);
    assert!(content.contains("Breaking update") && content.contains(" · v2 · "), "{content}");

    // With nothing pending, a bulk reprocess queues the story once (double click: no-op).
    assert_eq!(reprocess_published(&t.app, Some(fid), ReprocessScope::All, false).await.unwrap(), 1);
    assert_eq!(reprocess_published(&t.app, Some(fid), ReprocessScope::All, false).await.unwrap(), 0);
    t.wait_idle().await;
}

#[tokio::test]
async fn reprocessing_does_not_use_up_update_versions() {
    use rillrss::pipeline::{ReprocessScope, reprocess_published};
    let mock = start_mock().await;
    let t = start_app(|s| s.updates.max_versions = 3).await;
    let body1 = paragraphs("Original", 3);
    mock.set("/f.xml", "application/rss+xml", one_story("Quake hits city", &body1));
    let fid = t.add_feed(&mock.url("/f.xml"), json!({})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    for _ in 0..3 {
        assert_eq!(reprocess_published(&t.app, Some(fid), ReprocessScope::Latest(10), false).await.unwrap(), 1);
        t.wait_idle().await;
    }
    let (version, news, state): (i64, i64, String) =
        t.query("SELECT version, news_versions, state FROM stories", |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).await;
    assert_eq!((version, news, state.as_str()), (4, 1, "active"));
    let body2 = format!("{body1}{}", paragraphs("Breaking update", 3));
    mock.set("/f.xml", "application/rss+xml", one_story("Quake hits city, dozens injured", &body2));
    t.fetch(fid).await;
    t.wait_idle().await;
    let (title, content): (String, String) = t.query("SELECT title, content FROM items ORDER BY id DESC LIMIT 1", |r| Ok((r.get(0)?, r.get(1)?))).await;
    assert!(title.starts_with("[更新]"), "the real update is published: {title}");
    assert!(content.contains(" · v2 · "), "shown as the second version: {content}");
}

#[tokio::test]
async fn reprocess_keeps_date_and_read_time_of_current_version() {
    use rillrss::pipeline::{ReprocessScope, reprocess_published};
    let mock = start_mock().await;
    let t = start_app(|_| {}).await;
    let body1 = paragraphs("Original", 3);
    mock.set("/f.xml", "application/rss+xml", one_story("Quake hits city", &body1));
    let fid = t.add_feed(&mock.url("/f.xml"), json!({})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    let body2 = format!("{body1}{}", paragraphs("Breaking update", 3));
    mock.set("/f.xml", "application/rss+xml", one_story("Quake hits city, dozens injured", &body2));
    t.fetch(fid).await;
    t.wait_idle().await;
    // v2 is dated at the update (redate) and has been read.
    exec(&t, "UPDATE items SET is_read = 1, read_at = 1792500000 WHERE version = 2").await;
    let v2_date = t.count("SELECT published_at FROM items WHERE version = 2").await;
    assert_ne!(v2_date, t.count("SELECT published_at FROM items WHERE version = 1").await);
    reprocess_published(&t.app, Some(fid), ReprocessScope::Latest(10), false).await.unwrap();
    t.wait_idle().await;
    let (date, is_read, read_at): (i64, i64, Option<i64>) =
        t.query("SELECT published_at, is_read, read_at FROM items WHERE version = 3", |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).await;
    assert_eq!(date, v2_date, "keeps the date of the version it re-renders");
    assert_eq!((is_read, read_at), (1, Some(1792500000)), "read state and read time inherited");
}

#[tokio::test]
async fn queued_reprocess_of_a_deleted_article_is_dropped() {
    use rillrss::pipeline::{ReprocessScope, reprocess_published};
    let mock = start_mock().await;
    let dir = tempfile::tempdir().unwrap();
    mock.set("/f.xml", "application/rss+xml", one_story("Old story", &paragraphs("Old", 3)));
    let fid;
    {
        let t = start_app_in(dir.path().to_path_buf(), true, |_| {}).await;
        fid = t.add_feed(&mock.url("/f.xml"), json!({})).await;
        t.fetch(fid).await;
        t.wait_idle().await;
        exec(&t, "UPDATE items SET is_read = 1, read_at = 1").await;
        t.app.shutdown.cancel();
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    // The reprocess waits in the queue while retention deletes the (old, read) article.
    let t = start_app_in(dir.path().to_path_buf(), false, |_| {}).await;
    assert_eq!(reprocess_published(&t.app, Some(fid), ReprocessScope::All, false).await.unwrap(), 1);
    exec(&t, format!("UPDATE items SET created_at = {}", (rillrss::util::now() - 40 * 86400) * 1_000_000)).await;
    assert_eq!(rillrss::maintenance::cleanup(&t.app).await.unwrap().items_deleted, 1);
    tokio::spawn(rillrss::pipeline::run_dispatcher(t.app.clone()));
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 0, "not brought back as a new article");
    let err: String = t.query("SELECT last_error FROM revisions WHERE reason = 'reprocess'", |r| r.get(0)).await;
    assert!(err.contains("已被清理"), "{err}");
    t.app.shutdown.cancel();
}

#[tokio::test]
async fn latest_n_reprocesses_the_most_recent_stories() {
    use rillrss::pipeline::{ReprocessScope, reprocess_published};
    let mock = start_mock().await;
    let t = start_app(|_| {}).await;
    let entries: Vec<Entry> = (0..6)
        .map(|i| {
            let mut e = entry(&format!("t{i}"), &format!("Story {i}"), &format!("https://n.example.com/t{i}"), &paragraphs(&format!("S{i}"), 1));
            e.pub_date = format!("Mon, 19 Oct 2026 0{i}:00:00 GMT");
            e
        })
        .collect();
    mock.set("/f.xml", "application/rss+xml", rss("F", &entries));
    let fid = t.add_feed(&mock.url("/f.xml"), json!({})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    // A bulk reprocess gives every story a new item id, in processing order.
    assert_eq!(reprocess_published(&t.app, Some(fid), ReprocessScope::All, false).await.unwrap(), 6);
    t.wait_idle().await;
    let before = t.count("SELECT MAX(id) FROM revisions").await;
    assert_eq!(reprocess_published(&t.app, Some(fid), ReprocessScope::Latest(2), false).await.unwrap(), 2);
    let picked: Vec<String> = t
        .app
        .db
        .call(move |c| {
            let mut st = c.prepare("SELECT title FROM revisions WHERE id > ? ORDER BY id")?;
            let v = st.query_map([before], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
            Ok(v)
        })
        .await
        .unwrap();
    assert_eq!(picked, vec!["Story 5".to_string(), "Story 4".to_string()]);
    t.wait_idle().await;
}

#[tokio::test]
async fn queued_reprocess_survives_a_trivial_entry_change() {
    use rillrss::pipeline::{ReprocessScope, reprocess_published};
    let mock = start_mock().await;
    let dir = tempfile::tempdir().unwrap();
    let page = |body: &str| format!("<html><body><article><h1>T</h1>{body}</article></body></html>");
    mock.set("/p/2", "text/html", page(&paragraphs("Body", 5)));
    let link = mock.url("/p/2");
    mock.set("/f.xml", "application/rss+xml", rss("F", &[entry("g2", "Title", &link, "<p>Summary A.</p>")]));
    let fid;
    {
        let t = start_app_in(dir.path().to_path_buf(), true, |s| s.fulltext.min_text_chars = 50).await;
        fid = t.add_feed(&mock.url("/f.xml"), json!({"fulltext": "readability"})).await;
        t.fetch(fid).await;
        t.wait_idle().await;
        assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 1);
        t.app.shutdown.cancel();
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let t = start_app_in(dir.path().to_path_buf(), false, |s| s.fulltext.min_text_chars = 50).await;
    assert_eq!(reprocess_published(&t.app, Some(fid), ReprocessScope::Latest(10), false).await.unwrap(), 1);
    // The feed summary is reworded while the reprocess waits: the entry update replaces it,
    // turns out not to be news, and carries out the reprocess instead.
    mock.set("/f.xml", "application/rss+xml", rss("F", &[entry("g2", "Title", &link, "<p>Summary B, reworded.</p>")]));
    rillrss::feeds::fetcher::fetch_feed(&t.app, fid).await.unwrap();
    tokio::spawn(rillrss::pipeline::run_dispatcher(t.app.clone()));
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 2, "reprocess carried out");
    let (title, is_read): (String, i64) = t.query("SELECT title, is_read FROM items ORDER BY id DESC LIMIT 1", |r| Ok((r.get(0)?, r.get(1)?))).await;
    assert!(!title.starts_with("[更新]"), "not presented as news: {title}");
    assert_eq!(is_read, 0);
    assert_eq!(t.count("SELECT COUNT(*) FROM revisions WHERE state = 'published' AND reason = 'reprocess'").await, 1);
    t.app.shutdown.cancel();
}

#[tokio::test]
async fn removed_boxes_rotating_in_the_feed_are_not_updates() {
    let mock = start_mock().await;
    let t = start_app(|_| {}).await;
    let related = |k: usize| -> String {
        (0..12).map(|i| format!("<li><a href='/r{i}'>Related headline {} about some other topic entirely</a></li>", i + k)).collect()
    };
    let body = |k: usize| format!("{}<div class=\"related\"><ul>{}</ul></div>", paragraphs("Main", 4), related(k));
    mock.set("/f.xml", "application/rss+xml", rss("F", &[entry("g1", "Stable story", "https://n.example.com/a", &body(0))]));
    let fid = t.add_feed(&mock.url("/f.xml"), json!({"remove_selector": ".related"})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    // Only the removed "related" box changes.
    mock.set("/f.xml", "application/rss+xml", rss("F", &[entry("g1", "Stable story", "https://n.example.com/a", &body(1))]));
    t.fetch(fid).await;
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 1);
    assert_eq!(t.count("SELECT COUNT(*) FROM revisions").await, 1);
    // A real change of the article still is one.
    mock.set(
        "/f.xml",
        "application/rss+xml",
        rss("F", &[entry("g1", "Stable story", "https://n.example.com/a", &format!("{}{}", body(2), paragraphs("Fresh facts", 3)))]),
    );
    t.fetch(fid).await;
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 2);
}

#[tokio::test]
async fn page_check_ignores_a_truncated_page() {
    let mock = start_mock().await;
    let t = start_app(|s| {
        s.updates.page_check_minutes = vec![0, 0, 0];
        s.fulltext.min_text_chars = 200;
    })
    .await;
    let page = |body: &str| format!("<html><body><article><h1>Story</h1>{body}</article></body></html>");
    mock.set("/article/1", "text/html", page(&paragraphs("Full", 6)));
    let link = mock.url("/article/1");
    mock.set("/full.xml", "application/rss+xml", rss("Full", &[entry("full-1", "Developing story", &link, "<p>Short summary.</p>")]));
    let fid = t.add_feed(&mock.url("/full.xml"), json!({"fulltext": "readability", "updates": "page"})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    // Metered paywall: two paragraphs and a subscription prompt.
    exec(&t, "UPDATE stories SET next_check_at = 0").await;
    mock.set(
        "/article/1",
        "text/html",
        page(&format!("{}<p>Subscribe to continue reading this article. Already a subscriber? Sign in to read the full story and get unlimited access.</p>", paragraphs("Full", 2))),
    );
    rillrss::pipeline::check_due_pages(&t.app).await.unwrap();
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 1, "the teaser is not an update");
    assert_eq!(t.count("SELECT COUNT(*) FROM revisions").await, 1);
}

/// Mock site whose article page fails a given number of times (then serves `page`).
#[derive(Default)]
struct FlakySite {
    feed: std::sync::Mutex<String>,
    page: std::sync::Mutex<String>,
    failures: std::sync::atomic::AtomicUsize,
}

async fn start_flaky_site() -> (String, std::sync::Arc<FlakySite>) {
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;
    use std::sync::atomic::Ordering;
    let site = std::sync::Arc::new(FlakySite::default());
    let (s1, s2) = (site.clone(), site.clone());
    let app = axum::Router::new()
        .route(
            "/f.xml",
            axum::routing::get(move || {
                let s = s1.clone();
                async move { ([(header::CONTENT_TYPE, "application/rss+xml")], s.feed.lock().unwrap().clone()).into_response() }
            }),
        )
        .route(
            "/p/1",
            axum::routing::get(move || {
                let s = s2.clone();
                async move {
                    if s.failures.load(Ordering::SeqCst) > 0 {
                        s.failures.fetch_sub(1, Ordering::SeqCst);
                        return (StatusCode::NOT_FOUND, "gone for a moment").into_response();
                    }
                    ([(header::CONTENT_TYPE, "text/html")], s.page.lock().unwrap().clone()).into_response()
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), site)
}

#[tokio::test]
async fn entry_update_waits_for_a_briefly_missing_page() {
    let (base, site) = start_flaky_site().await;
    let t = start_app(|s| s.fulltext.min_text_chars = 50).await;
    let page = |body: &str| format!("<html><body><article><h1>T</h1>{body}</article></body></html>");
    let link = format!("{base}/p/1");
    *site.page.lock().unwrap() = page(&paragraphs("Body", 5));
    *site.feed.lock().unwrap() = rss("F", &[entry("g1", "Title", &link, &paragraphs("Summary", 1))]);
    let fid = t.add_feed(&format!("{base}/f.xml"), json!({"fulltext": "readability"})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 1);
    // The story is updated; the page is unavailable for the next three requests.
    *site.page.lock().unwrap() = page(&format!("{}{}", paragraphs("Body", 5), paragraphs("New facts", 4)));
    site.failures.store(3, std::sync::atomic::Ordering::SeqCst);
    *site.feed.lock().unwrap() = rss("F", &[entry("g1", "Title, updated", &link, &format!("{}{}", paragraphs("Summary", 1), paragraphs("New facts", 2)))]);
    t.fetch(fid).await;
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 2, "update published once the page is back");
    let content: String = t.query("SELECT content FROM items ORDER BY id DESC LIMIT 1", |r| r.get(0)).await;
    assert!(content.contains("New facts paragraph 3"));
}

#[tokio::test]
async fn reprocess_keeps_full_text_when_the_page_is_gone() {
    use rillrss::pipeline::{ReprocessScope, reprocess_published};
    let mock = start_mock().await;
    let t = start_app(|s| s.fulltext.min_text_chars = 50).await;
    let page = |body: &str| format!("<html><body><article><h1>T</h1>{body}</article></body></html>");
    mock.set("/p/1", "text/html", page(&paragraphs("Fulltext", 5)));
    let (l1, l2) = (mock.url("/p/1"), mock.url("/p/2"));
    // Article 2's page never worked: its version is the feed text.
    mock.set(
        "/f.xml",
        "application/rss+xml",
        rss("F", &[entry("g1", "One", &l1, "<p>Teaser one.</p>"), entry("g2", "Two", &l2, "<p>法國多地上週末爆發騷亂。</p>")]),
    );
    let fid = t.add_feed(&mock.url("/f.xml"), json!({"fulltext": "readability"})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 2);
    // Settings change (OpenCC) and reprocess, but article 1's page is gone now.
    mock.remove("/p/1");
    exec(&t, format!("UPDATE feeds SET options = '{{\"fulltext\":\"readability\",\"opencc\":\"t2s\"}}' WHERE id = {fid}")).await;
    assert_eq!(reprocess_published(&t.app, Some(fid), ReprocessScope::All, false).await.unwrap(), 2);
    t.wait_idle().await;
    let current: Vec<(String, String)> = t
        .app
        .db
        .call(|c| {
            let mut st = c.prepare("SELECT title, content FROM items WHERE superseded_by IS NULL ORDER BY title")?;
            let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(v)
        })
        .await
        .unwrap();
    assert_eq!(current.len(), 2);
    assert_eq!(current[0].0, "One");
    assert!(current[0].1.contains("Fulltext paragraph 4"), "full text kept: {}", current[0].1);
    assert!(current[1].1.contains("法国"), "feed-text version re-rendered: {}", current[1].1);
    let err: String = t.query("SELECT last_error FROM revisions WHERE reason = 'reprocess' AND state = 'unchanged'", |r| r.get(0)).await;
    assert!(err.contains("保留当前版本"), "{err}");
}

#[tokio::test]
async fn reprocess_never_publishes_a_failed_translation() {
    use rillrss::pipeline::{ReprocessScope, reprocess_published};
    let mock = start_mock().await;
    let t = start_app(|s| {
        s.translation.base_url = mock.url("/v1");
        s.translation.model = "mock-model".into();
        s.translation.api_key = "test-key".into();
        s.translation.max_attempts = 2;
    })
    .await;
    mock.set("/en.xml", "application/rss+xml", rss("English", &[entry("e1", "Taxes to rise", "https://news.example.com/e1", &paragraphs("Taxes", 2))]));
    let fid = t.add_feed(&mock.url("/en.xml"), json!({"translate": true})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    let before: String = t.query("SELECT content FROM items", |r| r.get(0)).await;
    assert!(!before.contains("Taxes paragraph"), "translated");
    // New glossary (cache miss), but the provider now answers garbage.
    let mut s = (*t.app.settings.get().await).clone();
    s.translation.extra_prompt = "Use the glossary.".into();
    t.app.settings.save(s).await.unwrap();
    mock.script_llm(vec![LlmMode::Garbage; 1000]);
    assert_eq!(reprocess_published(&t.app, Some(fid), ReprocessScope::All, false).await.unwrap(), 1);
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 1, "current version kept");
    let err: String = t.query("SELECT last_error FROM revisions WHERE reason = 'reprocess'", |r| r.get(0)).await;
    assert!(err.contains("保留当前版本"), "{err}");
    assert_eq!(t.count("SELECT COUNT(*) FROM revisions WHERE reason = 'reprocess' AND state = 'failed'").await, 1);
}

#[tokio::test]
async fn hold_policy_also_holds_incomplete_translations() {
    let mock = start_mock().await;
    let t = start_app(|s| {
        s.translation.base_url = mock.url("/v1");
        s.translation.model = "mock-model".into();
        s.translation.api_key = "test-key".into();
        s.translation.max_attempts = 2;
        s.translation.on_failure = "hold".into();
    })
    .await;
    mock.script_llm(vec![LlmMode::Garbage; 1000]);
    mock.set("/en.xml", "application/rss+xml", rss("English", &[entry("e1", "Taxes to rise", "https://news.example.com/e1", &paragraphs("Taxes", 2))]));
    let fid = t.add_feed(&mock.url("/en.xml"), json!({"translate": true})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 0, "not published partly translated");
    assert_eq!(t.count("SELECT COUNT(*) FROM revisions WHERE state = 'pending' AND stage = 'translate'").await, 1);
    // Retrying (e.g. after fixing the provider) resumes it.
    mock.script_llm(vec![]);
    rillrss::pipeline::retry_failed(&t.app, None).await.unwrap();
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 1);
}

#[tokio::test]
async fn retry_resumes_translations_deferred_by_the_circuit_breaker() {
    let mock = start_mock().await;
    let dir = tempfile::tempdir().unwrap();
    let t = start_app_in(dir.path().to_path_buf(), false, |_| {}).await;
    mock.set("/en.xml", "application/rss+xml", rss("English", &[entry("e1", "Taxes to rise", "https://news.example.com/e1", &paragraphs("Taxes", 2))]));
    let fid = t.add_feed(&mock.url("/en.xml"), json!({"translate": true})).await;
    rillrss::feeds::fetcher::fetch_feed(&t.app, fid).await.unwrap();
    // Deferred while the circuit was open: no attempt counted.
    exec(&t, format!("UPDATE revisions SET stage = 'translate', attempts = 0, next_attempt_at = {}", rillrss::util::now() + 1800)).await;
    rillrss::pipeline::retry_failed(&t.app, Some(fid)).await.unwrap();
    assert_eq!(t.count("SELECT next_attempt_at FROM revisions").await, 0);
}

#[tokio::test]
async fn stories_stay_seen_while_the_feed_answers_304() {
    use axum::http::{HeaderMap, StatusCode, header};
    use axum::response::IntoResponse;
    let body = "<?xml version=\"1.0\"?><rss version=\"2.0\"><channel><title>Q</title><link>https://q.example.com/</link><item><guid>q1</guid><title>Quiet blog post</title><link>https://q.example.com/p/1</link><description>Hello there, a short post.</description></item></channel></rss>";
    let app = axum::Router::new().route(
        "/q.xml",
        axum::routing::get(move |h: HeaderMap| async move {
            if h.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) == Some("\"v1\"") {
                return StatusCode::NOT_MODIFIED.into_response();
            }
            ([(header::ETAG, "\"v1\""), (header::CONTENT_TYPE, "application/rss+xml")], body).into_response()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let t = start_app(|_| {}).await;
    let fid = t.add_feed(&format!("http://{addr}/q.xml"), json!({})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    let long_ago = rillrss::util::now() - 40 * 86400;
    exec(&t, format!("UPDATE stories SET last_seen_at = {long_ago}")).await;
    let o = rillrss::feeds::fetcher::fetch_feed(&t.app, fid).await.unwrap();
    assert!(o.not_modified);
    assert!(t.count("SELECT last_seen_at FROM stories").await > long_ago, "a 304 keeps the listed stories seen");
    // So the cleanup keeps the story (its key) even once retention removed the article.
    exec(&t, format!("UPDATE items SET created_at = {}", long_ago * 1_000_000)).await;
    let r = rillrss::maintenance::cleanup(&t.app).await.unwrap();
    assert_eq!((r.items_deleted, r.stories_deleted), (1, 0));
}

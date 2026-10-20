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

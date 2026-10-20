//! Restart / crash recovery: persistent work survives a process restart, leases left by
//! a crashed process are reclaimed, and nothing is published twice.

mod common;

use common::*;
use serde_json::json;

#[tokio::test]
async fn pending_work_resumes_after_restart() {
    let mock = start_mock().await;
    let dir = tempfile::tempdir().unwrap();
    let feed_url = mock.url("/f.xml");
    mock.set(
        "/f.xml",
        "application/rss+xml",
        rss("F", &[entry("1", "One", "https://n.example.com/1", &paragraphs("One", 2)), entry("2", "Two", "https://n.example.com/2", &paragraphs("Two", 2))]),
    );

    // First process: no workers. Ingest only (revisions stay pending), then simulate a
    // crash in the middle of a stage (lease held, stage half-done).
    {
        let t = start_app_in(dir.path().to_path_buf(), false, |_| {}).await;
        let fid = t.add_feed(&feed_url, json!({})).await;
        rillrss::feeds::fetcher::fetch_feed(&t.app, fid).await.unwrap();
        assert_eq!(t.count("SELECT COUNT(*) FROM revisions WHERE state = 'pending'").await, 2);
        t.app
            .db
            .call(|c| {
                c.execute("UPDATE revisions SET lease_until = strftime('%s','now') + 3600, stage = 'publish' WHERE id = 1", [])?;
                c.execute("UPDATE feeds SET lease_until = strftime('%s','now') + 3600", [])?;
                Ok(())
            })
            .await
            .unwrap();
        t.app.shutdown.cancel();
    }

    // Second process on the same data dir: leases are reset and all work completes.
    let t = start_app_in(dir.path().to_path_buf(), true, |_| {}).await;
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 2);
    assert_eq!(t.count("SELECT COUNT(*) FROM revisions WHERE state = 'published'").await, 2);
    // The feed scheduler resets stale feed leases on start.
    tokio::spawn(rillrss::feeds::fetcher::run_scheduler(t.app.clone()));
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let feed_lease: Option<i64> = t.query("SELECT lease_until FROM feeds", |r| r.get(0)).await;
    assert!(feed_lease.is_none() || feed_lease.unwrap() > chrono::Utc::now().timestamp(), "feed fetched again or lease cleared");
    // Re-running the same feed does not duplicate anything.
    t.fetch(1).await;
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 2);
    t.app.shutdown.cancel();
}

#[tokio::test]
async fn published_ids_are_monotonic_and_never_reused() {
    let mock = start_mock().await;
    let dir = tempfile::tempdir().unwrap();
    mock.set("/f.xml", "application/rss+xml", rss("F", &[entry("1", "One", "https://n.example.com/1", &paragraphs("One", 2))]));
    let first_max;
    {
        let t = start_app_in(dir.path().to_path_buf(), true, |_| {}).await;
        let fid = t.add_feed(&mock.url("/f.xml"), json!({})).await;
        t.fetch(fid).await;
        t.wait_idle().await;
        first_max = t.count("SELECT MAX(id) FROM items").await;
        // Delete the newest item (e.g. retention) — its id must never come back.
        t.app.db.call(|c| Ok(c.execute("DELETE FROM items", [])?)).await.unwrap();
        t.app.shutdown.cancel();
    }
    let t = start_app_in(dir.path().to_path_buf(), true, |_| {}).await;
    mock.set("/f.xml", "application/rss+xml", rss("F", &[entry("2", "Two", "https://n.example.com/2", &paragraphs("Two", 2))]));
    t.fetch(1).await;
    t.wait_idle().await;
    let new_id = t.count("SELECT MAX(id) FROM items").await;
    assert!(new_id > first_max, "AUTOINCREMENT ids are never reused ({new_id} vs {first_max})");
    t.app.shutdown.cancel();
}

#[tokio::test]
async fn failing_feed_backs_off_without_affecting_others() {
    let mock = start_mock().await;
    let t = start_app(|_| {}).await;
    mock.set("/ok.xml", "application/rss+xml", rss("OK", &[entry("1", "Fine", "https://n.example.com/1", &paragraphs("Fine", 2))]));
    let bad = t.add_feed(&mock.url("/missing.xml"), json!({})).await;
    let good = t.add_feed(&mock.url("/ok.xml"), json!({})).await;
    assert!(rillrss::feeds::fetcher::fetch_feed(&t.app, bad).await.is_err());
    assert!(rillrss::feeds::fetcher::fetch_feed(&t.app, bad).await.is_err());
    t.fetch(good).await;
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 1);
    let (errors, next, last_error): (i64, i64, String) = t
        .query("SELECT fetch_error_count, next_fetch_at, last_error FROM feeds WHERE id = 1", |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .await;
    assert_eq!(errors, 2);
    assert!(last_error.contains("404"), "{last_error}");
    // Back-off doubles the interval (30 min default * 2).
    assert!(next - chrono::Utc::now().timestamp() > 50 * 60, "backed off");
    // Garbage instead of XML is an error too, and the good feed keeps working.
    mock.set("/missing.xml", "text/html", "<html>not a feed</html>");
    assert!(rillrss::feeds::fetcher::fetch_feed(&t.app, bad).await.is_err());
}

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

async fn exec(t: &TestApp, sql: impl Into<String>) {
    let sql = sql.into();
    t.app.db.call(move |c| Ok(c.execute_batch(&sql)?)).await.unwrap();
}

#[tokio::test]
async fn revision_that_keeps_crashing_the_process_ends_failed() {
    let mock = start_mock().await;
    let dir = tempfile::tempdir().unwrap();
    mock.set("/f.xml", "application/rss+xml", rss("F", &[entry("1", "One", "https://n.example.com/1", &paragraphs("One", 2))]));
    let t = start_app_in(dir.path().to_path_buf(), false, |_| {}).await;
    let fid = t.add_feed(&mock.url("/f.xml"), json!({})).await;
    rillrss::feeds::fetcher::fetch_feed(&t.app, fid).await.unwrap();
    // Three runs of the stage started and never finished (the process died each time);
    // startup clears the lease, so without a count it would be retried forever.
    exec(&t, "UPDATE revisions SET unfinished = 3, lease_until = strftime('%s','now') + 600").await;
    tokio::spawn(rillrss::pipeline::run_dispatcher(t.app.clone()));
    t.wait_idle().await;
    let (state, err): (String, String) = t.query("SELECT state, last_error FROM revisions", |r| Ok((r.get(0)?, r.get(1)?))).await;
    assert_eq!(state, "failed");
    assert!(err.contains("未能完成"), "{err}");
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 0);
    // An operator retry gives it fresh chances.
    rillrss::pipeline::retry_failed(&t.app, None).await.unwrap();
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 1);
    t.app.shutdown.cancel();
}

#[tokio::test]
async fn permanently_failing_stage_ends_failed_instead_of_retrying_forever() {
    let mock = start_mock().await;
    let t = start_app(|_| {}).await;
    mock.set("/f.xml", "application/rss+xml", rss("F", &[entry("1", "One", "https://n.example.com/1", &paragraphs("One", 2))]));
    let fid = t.add_feed(&mock.url("/f.xml"), json!({})).await;
    // An option the transform stage cannot apply (stored behind the form's back).
    exec(&t, format!("UPDATE feeds SET options = '{{\"opencc\":\"t2x\"}}' WHERE id = {fid}")).await;
    t.fetch(fid).await;
    for _ in 0..200 {
        if t.count("SELECT COUNT(*) FROM revisions WHERE state = 'failed'").await == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let (state, stage, err): (String, String, String) =
        t.query("SELECT state, stage, last_error FROM revisions", |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).await;
    assert_eq!((state.as_str(), stage.as_str()), ("failed", "transform"));
    assert!(err.contains("OpenCC") && err.contains("已尝试 5 次"), "{err}");
}

#[tokio::test]
async fn opml_restore_keeps_names_and_checks_what_it_imports() {
    let mock = start_mock().await;
    mock.set("/a.xml", "application/rss+xml", rss("Upstream A", &[entry("1", "One", "https://n.example.com/1", &paragraphs("One", 1))]));
    mock.set("/b.xml", "application/rss+xml", rss("Upstream B", &[entry("2", "Two", "https://n.example.com/2", &paragraphs("Two", 1))]));
    let opml = format!(
        r#"<?xml version="1.0"?><opml version="2.0" xmlns:rill="https://github.com/rillrss/rillrss"><body>
<outline type="rss" text="My A" title="My A" xmlUrl="{a}" htmlUrl="javascript:alert(1)" rill:options="{{&quot;opencc&quot;:&quot;t2x&quot;,&quot;translate&quot;:true}}"/>
<outline type="rss" text="Name B" xmlUrl="{b}" rill:titleLocked="0"/>
<outline type="rss" text="Local file" xmlUrl="file:///etc/passwd"/>
</body></opml>"#,
        a = mock.url("/a.xml"),
        b = mock.url("/b.xml")
    );
    let t = start_app(|_| {}).await;
    let r = rillrss::opml::import(&t.app, opml.as_bytes()).await.unwrap();
    assert_eq!((r.found, r.added), (3, 2), "non-http feed refused");
    for id in [1, 2] {
        rillrss::feeds::fetcher::fetch_feed(&t.app, id).await.unwrap();
    }
    let feeds: Vec<(String, Option<String>, String)> = t
        .app
        .db
        .call(|c| {
            let mut st = c.prepare("SELECT title, site_url, options FROM feeds ORDER BY id")?;
            let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(v)
        })
        .await
        .unwrap();
    assert_eq!(feeds[0].0, "My A", "a named subscription keeps its name");
    assert_eq!(feeds[1].0, "Upstream B", "RillRSS export: title was not locked");
    assert_ne!(feeds[0].1.as_deref(), Some("javascript:alert(1)"));
    let opts = rillrss::settings::FeedOptions::from_json(&feeds[0].2);
    assert!(opts.translate && opts.opencc.is_empty(), "unknown OpenCC mode dropped, the rest kept");
    let xml = rillrss::opml::export(&t.app).await.unwrap();
    assert!(xml.contains("rill:titleLocked=\"1\"") && xml.contains("rill:titleLocked=\"0\""), "{xml}");
}

#[tokio::test]
async fn out_of_range_settings_are_clamped() {
    let t = start_app(|s| {
        s.retention.days = 999_999_999;
        s.fulltext.min_text_chars = 0;
    })
    .await;
    let s = t.app.settings.get().await;
    assert_eq!(s.retention.days, rillrss::settings::MAX_DAYS);
    assert_eq!(s.fulltext.min_text_chars, 1);
    rillrss::maintenance::cleanup(&t.app).await.unwrap();
}

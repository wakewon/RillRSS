//! Web admin: script policy, external links, dead links, OPML upload and settings.

mod common;

use std::collections::HashSet;

use common::*;
use rusqlite::params;
use serde_json::json;

/// A logged-in admin browser: session cookie and CSRF token.
struct Admin {
    base: String,
    client: reqwest::Client,
    cookie: String,
    csrf: String,
}

/// A test instance without background workers (nothing is fetched or processed).
async fn app() -> TestApp {
    let dir = tempfile::tempdir().unwrap();
    let mut t = start_app_in(dir.path().to_path_buf(), false, |_| {}).await;
    t.dir = Some(dir);
    t
}

async fn login(t: &TestApp) -> Admin {
    let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let r = client.post(t.url("/admin/login")).form(&[("username", "admin"), ("password", "password123")]).send().await.unwrap();
    assert_eq!(r.status(), 303);
    let cookie = r.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_string();
    let mut a = Admin { base: t.base.clone(), client, cookie, csrf: String::new() };
    let page = a.get("/admin").await.2;
    a.csrf = between(&page, "name=\"csrf\" value=\"", "\"").to_string();
    a
}

fn between<'a>(s: &'a str, start: &str, end: &str) -> &'a str {
    let from = s.find(start).unwrap_or_else(|| panic!("{start:?} not found")) + start.len();
    &s[from..from + s[from..].find(end).unwrap()]
}

fn location(r: &reqwest::Response) -> String {
    r.headers().get("location").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default()
}

impl Admin {
    async fn get(&self, path: &str) -> (u16, reqwest::header::HeaderMap, String) {
        let r = self.client.get(format!("{}{path}", self.base)).header("cookie", &self.cookie).send().await.unwrap();
        (r.status().as_u16(), r.headers().clone(), r.text().await.unwrap())
    }

    /// Submit a form (with the CSRF token); returns the status and redirect target.
    async fn post(&self, path: &str, form: &[(&str, &str)]) -> (u16, String) {
        let mut fields = vec![("csrf", self.csrf.as_str())];
        fields.extend_from_slice(form);
        let r = self.client.post(format!("{}{path}", self.base)).header("cookie", &self.cookie).form(&fields).send().await.unwrap();
        (r.status().as_u16(), location(&r))
    }

    /// Submit a multipart form as is (no token added).
    async fn upload(&self, path: &str, fields: &[(&str, &[u8])]) -> (u16, String) {
        let boundary = "rill-test-boundary-5f2c";
        let mut body = Vec::new();
        for (name, value) in fields {
            body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes());
            body.extend_from_slice(value);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        let r = self
            .client
            .post(format!("{}{path}", self.base))
            .header("cookie", &self.cookie)
            .header("content-type", format!("multipart/form-data; boundary={boundary}"))
            .body(body)
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), location(&r))
    }
}

/// The nonce of a CSP whose scripts are allowed by nonce only.
fn script_nonce(csp: &str) -> String {
    let script = csp.split(';').map(str::trim).find(|d| d.starts_with("script-src")).expect("script-src");
    script.strip_prefix("script-src 'nonce-").and_then(|n| n.strip_suffix('\'')).unwrap_or_else(|| panic!("not nonce-only: {script}")).to_string()
}

async fn insert_item(t: &TestApp, feed_id: i64, link: &str, superseded_by: Option<i64>) -> i64 {
    let link = link.to_string();
    t.app
        .db
        .call(move |c| {
            c.execute(
                "INSERT INTO items (feed_id, guid, title, link, content, published_at, updated_at, created_at, superseded_by)
                 VALUES (?, ?, 'An article', ?, '<p>Body</p>', 1700000000, 1700000000, 1700000000000000, ?)",
                params![feed_id, format!("g-{link}-{superseded_by:?}"), link, superseded_by],
            )?;
            Ok(c.last_insert_rowid())
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn pages_run_only_their_own_scripts() {
    let t = app().await;
    let a = login(&t).await;
    let fid = t.add_feed("http://127.0.0.1:9/feed.xml", json!({})).await;
    let item = insert_item(&t, fid, "https://news.example.com/a", None).await;
    let mut nonces = HashSet::new();
    let mut pages = vec![("/admin/login".to_string(), {
        let r = a.client.get(t.url("/admin/login")).send().await.unwrap();
        (r.status().as_u16(), r.headers().clone(), r.text().await.unwrap())
    })];
    for p in ["/admin", "/admin/feeds", "/admin/categories", "/admin/items", "/admin/tasks", "/admin/events", "/admin/settings", "/admin/tools", "/admin/opml", "/admin/account"]
        .into_iter()
        .map(String::from)
        .chain([format!("/admin/feeds/{fid}"), format!("/admin/items/{item}")])
    {
        let page = a.get(&p).await;
        pages.push((p, page));
    }
    for (path, (status, headers, body)) in pages {
        assert_eq!(status, 200, "{path}");
        let csp = headers["content-security-policy"].to_str().unwrap();
        // Styles stay inline (the stylesheet is embedded in every page).
        assert!(csp.contains("style-src 'self' 'unsafe-inline'"), "{path}: {csp}");
        let nonce = script_nonce(csp);
        assert!(nonces.insert(nonce.clone()), "{path}: nonce reused");
        let scripts = body.matches("<script").count();
        assert!(scripts > 0, "{path}");
        assert_eq!(body.matches(&format!("<script nonce=\"{nonce}\">")).count(), scripts, "{path}");
    }
}

#[tokio::test]
async fn only_http_addresses_are_saved_or_linked() {
    let t = app().await;
    let a = login(&t).await;
    let fid = t.add_feed("http://127.0.0.1:9/feed.xml", json!({})).await;
    let page = format!("/admin/feeds/{fid}");

    for bad in ["javascript:alert(1)", "data:text/html,x", "ftp://example.com/feed.xml"] {
        let (status, to) = a.post(&page, &[("title", "Feed"), ("url", bad), ("enabled", "on")]).await;
        assert_eq!(status, 303);
        assert!(to.contains("err="), "{bad}: {to}");
    }
    assert_eq!(t.query("SELECT url FROM feeds", |r| r.get::<_, String>(0)).await, "http://127.0.0.1:9/feed.xml");
    let (_, to) = a.post(&page, &[("title", "Feed"), ("url", "https://example.com/rss.xml"), ("enabled", "on")]).await;
    assert!(to.contains("msg="), "{to}");
    let body = a.get(&page).await.2;
    assert!(body.contains("href=\"https://example.com/rss.xml\""));

    // Addresses stored elsewhere (OPML htmlUrl, a feed's links, older data) are shown
    // as text at most, never as script links.
    t.app.db.call(|c| Ok(c.execute("UPDATE feeds SET url = 'javascript:alert(2)', site_url = 'javascript:alert(3)'", [])?)).await.unwrap();
    let body = a.get(&page).await.2;
    assert!(body.contains("javascript:alert(2)"));
    assert!(!body.contains("href=\"javascript:"));
    assert!(!body.contains("网站"));

    let bad = insert_item(&t, fid, "javascript:alert(4)", None).await;
    let body = a.get(&format!("/admin/items/{bad}")).await.2;
    assert!(!body.contains("javascript:") && !body.contains("原文"));
    let good = insert_item(&t, fid, "https://news.example.com/a", None).await;
    let body = a.get(&format!("/admin/items/{good}")).await.2;
    assert!(body.contains("href=\"https://news.example.com/a\""));
}

#[tokio::test]
async fn links_to_deleted_feeds_and_items_become_text() {
    let t = app().await;
    let a = login(&t).await;
    let fid = t.add_feed("http://127.0.0.1:9/feed.xml", json!({})).await;
    let newer = insert_item(&t, fid, "https://news.example.com/a", None).await;
    let older = insert_item(&t, fid, "https://news.example.com/a", Some(newer)).await;
    t.app
        .db
        .call(move |c| {
            c.execute(
                "INSERT INTO events (ts, level, kind, feed_id, item_id, message) VALUES (strftime('%s','now'), 'info', 'publish', ?, ?, 'published')",
                [fid, newer],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let (feed_link, newer_link) = (format!("href=\"/admin/feeds/{fid}\""), format!("href=\"/admin/items/{newer}\""));
    let events = a.get("/admin/events").await.2;
    assert!(events.contains(&feed_link) && events.contains(&newer_link));
    assert!(a.get(&format!("/admin/items/{older}")).await.2.contains(&newer_link));
    assert!(a.get("/admin/items").await.2.contains(&newer_link));

    // Cleanup removes the newer version; the older one stays (e.g. starred).
    t.app.db.call(move |c| Ok(c.execute("DELETE FROM items WHERE id = ?", [newer])?)).await.unwrap();
    let body = a.get(&format!("/admin/items/{older}")).await.2;
    assert!(!body.contains(&newer_link) && body.contains(&format!("#{newer}（已删除）")));
    let body = a.get("/admin/items").await.2;
    assert!(!body.contains(&newer_link) && body.contains("旧版"));

    let (status, _) = a.post(&format!("/admin/feeds/{fid}/delete"), &[]).await;
    assert_eq!(status, 303);
    let events = a.get("/admin/events").await.2;
    assert!(!events.contains(&feed_link) && !events.contains(&newer_link));
    assert!(events.contains(&format!("订阅 #{fid}（已删除）")) && events.contains(&format!("文章 #{newer}（已删除）")));
}

#[tokio::test]
async fn next_fetch_is_queued_or_not_shown() {
    let t = app().await;
    let a = login(&t).await;
    // A new feed is due at once (next_fetch_at = 0), not "20000 days ago".
    let fid = t.add_feed("http://127.0.0.1:9/feed.xml", json!({})).await;
    let page = format!("/admin/feeds/{fid}");
    assert!(a.get(&page).await.2.contains("下次 排队中"));
    let (_, to) = a.post(&page, &[("title", "Feed"), ("url", "http://127.0.0.1:9/feed.xml")]).await;
    assert!(to.contains("msg="), "{to}");
    assert!(!a.get(&page).await.2.contains("下次"));
}

#[tokio::test]
async fn reprocessing_a_deleted_story_is_a_notice() {
    let t = app().await;
    let a = login(&t).await;
    let (status, to) = a.post("/admin/stories/987654/reprocess", &[("item", "12")]).await;
    assert_eq!(status, 303);
    assert!(to.starts_with("/admin/items/12?err="), "{to}");
}

#[tokio::test]
async fn opml_import_takes_the_token_from_the_form() {
    let t = app().await;
    let a = login(&t).await;
    let opml = |url: &str, pad: usize| {
        format!(
            "<?xml version=\"1.0\"?><opml version=\"2.0\"><head><title>t</title></head><body><!--{}--><outline text=\"Imported\" type=\"rss\" xmlUrl=\"{url}\"/></body></opml>",
            " ".repeat(pad)
        )
    };
    let doc = opml("https://example.com/a.xml", 0);
    // In the URL (where it ends up in logs and history) or wrong: refused.
    let (status, _) = a.upload(&format!("/admin/opml/import?csrf={}", a.csrf), &[("text", doc.as_bytes())]).await;
    assert_eq!(status, 403);
    let (status, _) = a.upload("/admin/opml/import", &[("csrf", b"0123".as_slice()), ("text", doc.as_bytes())]).await;
    assert_eq!(status, 403);
    assert_eq!(t.count("SELECT COUNT(*) FROM feeds").await, 0);

    let (status, to) = a.upload("/admin/opml/import", &[("csrf", a.csrf.as_bytes()), ("text", doc.as_bytes())]).await;
    assert_eq!(status, 303);
    assert!(to.starts_with("/admin/feeds?msg="), "{to}");
    // Files beyond the server-wide body limit (2 MiB) are fine on this route.
    let big = opml("https://example.com/b.xml", 3 * 1024 * 1024);
    let (status, to) = a.upload("/admin/opml/import", &[("csrf", a.csrf.as_bytes()), ("file", big.as_bytes()), ("text", b"".as_slice())]).await;
    assert_eq!(status, 303);
    assert!(to.starts_with("/admin/feeds?msg="), "{to}");
    assert_eq!(t.count("SELECT COUNT(*) FROM feeds").await, 2);
}

#[tokio::test]
async fn settings_check_addresses_and_resume_deferred_translations() {
    let t = app().await;
    let a = login(&t).await;
    for (key, value) in [
        ("base_url", "javascript:alert(1)"),
        ("base_url", "https://rss.example.com/\"onerror=\"alert(1)"),
        ("translation.base_url", "ftp://example.com/v1"),
        ("fulltext.external_url", "file:///etc/passwd"),
    ] {
        let (status, to) = a.post("/admin/settings", &[(key, value)]).await;
        assert_eq!(status, 303);
        assert!(to.contains("err="), "{key}={value}: {to}");
    }
    assert_eq!(t.app.settings.get().await.base_url, "");
    let (_, to) = a.post("/admin/settings", &[("base_url", "https://rss.example.com"), ("fulltext.external_url", "https://x.example.com/?url={url}")]).await;
    assert!(to.contains("msg="), "{to}");
    assert_eq!(t.app.settings.get().await.base_url, "https://rss.example.com");

    // A translation put off while the service was failing runs once settings are saved.
    let fid = t.add_feed("http://127.0.0.1:9/feed.xml", json!({"translate": true})).await;
    t.app
        .db
        .call(move |c| {
            c.execute("INSERT INTO stories (feed_id, guid, first_seen_at, last_seen_at) VALUES (?, 'g', 1, 1)", [fid])?;
            c.execute(
                "INSERT INTO revisions (story_id, feed_id, reason, state, stage, guid, title, next_attempt_at, created_at, updated_at)
                 VALUES (?, ?, 'new', 'pending', 'translate', 'g', 'Title', strftime('%s','now') + 3600, 1, 1)",
                [c.last_insert_rowid(), fid],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let (_, to) = a.post("/admin/settings", &[]).await;
    assert!(to.contains("msg="), "{to}");
    assert_eq!(t.count("SELECT next_attempt_at FROM revisions").await, 0);
}

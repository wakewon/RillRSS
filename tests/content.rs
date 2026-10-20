//! Content fetching and processing safety: the private-network guard (with proxies and
//! redirects), deeply nested HTML, and relative URLs of extracted pages.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use common::*;
use rillrss::http::{self, Fetched, GuardedClient, HttpClients};
use rillrss::util::urlnorm::resolve;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::Url;

const T: Duration = Duration::from_secs(10);

async fn fetch(client: &GuardedClient, url: &str, public_only: bool) -> anyhow::Result<Fetched> {
    http::get(client, url, "test", T, 1 << 20, public_only, "*/*").await
}

/// The error of a request that must fail.
fn error(r: anyhow::Result<Fetched>) -> String {
    match r {
        Ok(f) => panic!("request to {} was not refused", f.final_url),
        Err(e) => e.to_string(),
    }
}

/// A forward proxy that answers every request itself: `/redirect/<url>` redirects to
/// `<url>`, anything else gets "VIA-PROXY". Returns its URL (by host name, which
/// resolves to a private address) and the request targets it received.
async fn fake_proxy() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let log = log.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buf = [0u8; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                let target = String::from_utf8_lossy(&head).split_whitespace().nth(1).unwrap_or("").to_string();
                log.lock().unwrap().push(target.clone());
                let resp = match target.split_once("/redirect/") {
                    Some((_, to)) => format!("HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
                    None => "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 9\r\nConnection: close\r\n\r\nVIA-PROXY".to_string(),
                };
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    (format!("http://localhost:{port}"), seen)
}

/// A small site on 127.0.0.1: redirects, an article page and an extractor endpoint.
async fn start_site() -> String {
    let story = format!(
        "<html><head><title>Story</title></head><body><nav><a href=\"/\">Home</a></nav><article class=\"post\">{}<img src=\"img/a.jpg\"><a href=\"related.html\">related</a></article></body></html>",
        paragraphs("story", 3)
    );
    let extracted = json!({
        "title": "External",
        "content": format!("{}<img src=\"pic.jpg\">", paragraphs("external", 3)),
        "url": "https://www.site.example/news/2024/a.html",
    })
    .to_string();
    let app = Router::new()
        .route("/feed", get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/real")]) }))
        .route("/real", get(|| async { "<rss/>" }))
        .route("/link", get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/2024/10/story.html")]) }))
        .route("/2024/10/story.html", get(move || std::future::ready(([(header::CONTENT_TYPE, "text/html")], story.clone()).into_response())))
        .route("/extract", get(move || std::future::ready(([(header::CONTENT_TYPE, "application/json")], extracted.clone()).into_response())));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn proxied_requests_are_checked_before_sending() {
    let (proxy, seen) = fake_proxy().await;
    let c = HttpClients::with_proxy(false, Some(&proxy)).unwrap();
    // The proxy (a name resolving to a private address) is used; targets are ours to check.
    let f = fetch(&c.content, "http://198.18.0.1/page", true).await.unwrap();
    assert_eq!(f.body, b"VIA-PROXY");
    // Private targets never reach the proxy, however they are spelled ...
    for u in ["http://localhost./x", "http://localhost:8080/x", "http://127.0.0.1/x", "http://0x7f.1/x", "http://[::127.0.0.1]/x"] {
        let e = error(fetch(&c.content, u, true).await);
        assert!(e.contains("blocked"), "{u}: {e}");
    }
    // ... nor do redirects into private space; public redirects are followed.
    for to in ["http://127.0.0.1:9/secret", "http://localhost.:9/secret", "http://[::ffff:169.254.169.254]/latest"] {
        let e = error(fetch(&c.content, &format!("http://198.18.0.1/redirect/{to}"), true).await);
        assert!(e.contains("blocked"), "{to}: {e}");
    }
    let f = fetch(&c.content, "http://198.18.0.1/redirect/http://198.18.0.2/ok", true).await.unwrap();
    assert_eq!(f.final_url.as_str(), "http://198.18.0.2/ok");
    assert_eq!(f.body, b"VIA-PROXY");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 6, "{seen:?}");
    assert!(seen.iter().all(|t| t.starts_with("http://198.18.")), "{seen:?}");
}

#[tokio::test]
async fn feeds_reach_private_addresses_only_from_private_urls() {
    let lan = start_site().await;
    let c = HttpClients::new(false).unwrap();
    // A LAN feed may redirect within the LAN ...
    let f = fetch(&c.feed, &format!("{lan}/feed"), false).await.unwrap();
    assert_eq!((f.status, f.final_url.path(), f.private_ok), (200, "/real", true));
    let resp = c.feed.get(Url::parse(&format!("{lan}/feed")).unwrap()).timeout(T).send().await.unwrap();
    assert_eq!(resp.url().path(), "/real");
    // ... but feeds discovered on a public page are fetched public-only ...
    assert!(fetch(&c.feed, &format!("{lan}/feed"), true).await.is_err());
    // ... and content requests (pages, images) are public-only anyway.
    assert!(fetch(&c.content, &format!("{lan}/real"), false).await.is_err());

    // A public feed must not redirect into private space (the proxy plays the public site).
    let (proxy, _) = fake_proxy().await;
    let c = HttpClients::with_proxy(false, Some(&proxy)).unwrap();
    let e = error(fetch(&c.feed, "http://198.18.0.1/redirect/http://127.0.0.1:9/feed", false).await);
    assert!(e.contains("blocked"), "{e}");
    let f = fetch(&c.feed, "http://198.18.0.1/feed", false).await.unwrap();
    assert_eq!((f.status, f.private_ok), (200, false));
}

#[tokio::test]
async fn deeply_nested_pages_and_entries_do_not_crash_the_pipeline() {
    let mock = start_mock().await;
    let t = start_app(|s| {
        s.translation.base_url = mock.url("/v1");
        s.translation.model = "mock-model".into();
        s.translation.api_key = "test-key".into();
    })
    .await;
    let deep = |n: usize| format!("{}deep text{}", "<div>".repeat(n), "</div>".repeat(n));
    // Full text of a page nested 20k levels deep (Readability used to overflow its stack).
    mock.set(
        "/deep.html",
        "text/html",
        format!("<html><head><title>Deep</title></head><body><article>{}{}</article></body></html>", paragraphs("page", 4), deep(20_000)),
    );
    mock.set("/full.xml", "application/rss+xml", rss("Full", &[entry("d1", "Deep page", &mock.url("/deep.html"), &paragraphs("summary", 2))]));
    let full = t.add_feed(&mock.url("/full.xml"), json!({"fulltext": "readability"})).await;
    // A translated entry nested 8k levels deep (segmentation used to overflow its stack).
    let body = format!("{}{}", paragraphs("nested", 2), deep(8_000));
    mock.set("/tr.xml", "application/rss+xml", rss("Tr", &[entry("t1", "Nested entry", "https://n.example.com/t1", &body)]));
    let tr = t.add_feed(&mock.url("/tr.xml"), json!({"translate": true})).await;
    t.fetch(full).await;
    t.fetch(tr).await;
    t.wait_idle().await;

    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 2);
    let content: String = t.query("SELECT content FROM items WHERE title = 'Deep page'", |r| r.get(0)).await;
    assert!(content.contains("summary paragraph 0"), "falls back to the feed content: {}", &content[..200]);
    let notes: String = t.query("SELECT notes FROM revisions WHERE title = 'Deep page'", |r| r.get(0)).await;
    assert!(notes.contains("nested too deeply"), "{notes}");
    let translated: String = t.query("SELECT content FROM items WHERE feed_id = 2", |r| r.get(0)).await;
    assert!(translated.matches("<div>").count() <= 200, "nesting capped");
}

#[tokio::test]
async fn extracted_pages_resolve_relative_urls_against_their_final_address() {
    let site = start_site().await;
    let mock = start_mock().await;
    let ext = format!("{site}/extract?u={{url}}");
    let t = start_app(move |s| {
        s.fulltext.min_text_chars = 50;
        s.fulltext.external_url = ext;
    })
    .await;
    // The article link redirects; selector mode used to resolve against the link.
    mock.set("/sel.xml", "application/rss+xml", rss("Sel", &[entry("s1", "Story", &format!("{site}/link"), "<p>short summary</p>")]));
    let sel = t.add_feed(&mock.url("/sel.xml"), json!({"fulltext": "selector", "selector": "article.post"})).await;
    mock.set("/ext.xml", "application/rss+xml", rss("Ext", &[entry("x1", "External", "https://n.example.com/x1", "<p>short summary</p>")]));
    let ext = t.add_feed(&mock.url("/ext.xml"), json!({"fulltext": "external"})).await;
    t.fetch(sel).await;
    t.fetch(ext).await;
    t.wait_idle().await;

    let content: String = t.query("SELECT content FROM items WHERE title = 'Story'", |r| r.get(0)).await;
    assert!(content.contains(&format!("\"{site}/2024/10/img/a.jpg\"")), "{content}");
    assert!(content.contains(&format!("\"{site}/2024/10/related.html\"")), "{content}");
    assert!(!content.contains("<base"), "{content}");
    let content: String = t.query("SELECT content FROM items WHERE title = 'External'", |r| r.get(0)).await;
    assert!(content.contains("\"https://www.site.example/news/2024/pic.jpg\""), "{content}");
}

#[test]
fn resolve_returns_only_http_urls() {
    let base = Url::parse("https://example.com/news/a.html").unwrap();
    assert_eq!(resolve(Some(&base), "b.png").as_deref(), Some("https://example.com/news/b.png"));
    assert_eq!(resolve(Some(&base), "//cdn.example.com/c.png").as_deref(), Some("https://cdn.example.com/c.png"));
    assert_eq!(resolve(None, "HTTP://Example.com/x").as_deref(), Some("http://example.com/x"));
    for bad in ["JavaScript:alert(1)", " javascript:alert(1)", "java\tscript:alert(1)", "DATA:text/html,x", "vbscript:x", "mailto:a@b.example", "ftp://x.example/a"] {
        assert_eq!(resolve(Some(&base), bad), None, "{bad:?}");
    }
}

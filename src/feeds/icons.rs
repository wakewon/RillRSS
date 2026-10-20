//! Feed favicons (served to readers via /feed-icons/<id>.ico and GReader iconUrl).

use std::time::Duration;

use anyhow::{Result, bail};
use rusqlite::{OptionalExtension, params};
use url::Url;

use super::fetcher::FeedRow;
use crate::app::AppRef;
use crate::http;
use crate::util::{self, urlnorm};

const REFRESH_SECS: i64 = 14 * 86400;
const RETRY_SECS: i64 = 86400;

fn sniff_icon(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(&[0, 0, 1, 0]) {
        return Some("image/x-icon");
    }
    crate::content::images::sniff(b).map(|(m, _)| m)
}

pub async fn refresh_icon_if_stale(app: &AppRef, feed: &FeedRow, feed_image: Option<&str>) -> Result<()> {
    let id = feed.id;
    let fetched: Option<(i64, usize)> = app
        .db
        .call(move |c| {
            Ok(c.query_row("SELECT fetched_at, length(data) FROM feed_icons WHERE feed_id = ?", [id], |r| {
                Ok((r.get(0)?, r.get::<_, i64>(1)? as usize))
            })
            .optional()?)
        })
        .await?;
    let now = util::now();
    match fetched {
        Some((t, len)) if len > 0 && now - t < REFRESH_SECS => return Ok(()),
        Some((t, 0)) if now - t < RETRY_SECS => return Ok(()),
        _ => {}
    }
    let site = feed
        .site_url
        .clone()
        .or_else(|| Url::parse(&feed.url).ok().map(|u| format!("{}://{}/", u.scheme(), u.host_str().unwrap_or(""))))
        .unwrap_or_default();
    let result = fetch_icon(app, &site, feed_image).await;
    let (mime, data) = match result {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(feed = id, "no icon: {e:#}");
            (String::from("application/octet-stream"), Vec::new())
        }
    };
    let hash = blake3::hash(&data).to_hex()[..16].to_string();
    app.db
        .call(move |c| {
            c.execute(
                "INSERT INTO feed_icons (feed_id, mime, data, hash, fetched_at) VALUES (?, ?, ?, ?, ?)
                 ON CONFLICT(feed_id) DO UPDATE SET mime = excluded.mime, data = excluded.data, hash = excluded.hash, fetched_at = excluded.fetched_at",
                params![id, mime, data, hash, now],
            )?;
            Ok(())
        })
        .await
}

async fn fetch_icon(app: &AppRef, site: &str, feed_image: Option<&str>) -> Result<(String, Vec<u8>)> {
    let settings = app.settings.get().await;
    let ua = settings.fetch.user_agent.clone();
    let timeout = Duration::from_secs(15);
    let base = Url::parse(site)?;
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(f) = http::get(&app.http.content, base.as_str(), &ua, timeout, 2 * 1024 * 1024, app.http.public_only, "text/html,*/*;q=0.5").await
        && f.status < 400
    {
        let html = http::decode_html(&f.body, f.content_type().as_deref());
        let doc = dom_query::Document::from(html.as_str());
        let mut links: Vec<(i32, String)> = Vec::new();
        for n in doc.select("link[rel]").nodes() {
            let rel = n.attr("rel").map(|r| r.to_ascii_lowercase()).unwrap_or_default();
            let Some(href) = n.attr("href") else { continue };
            let score = if rel.contains("apple-touch-icon") {
                3
            } else if rel.split_whitespace().any(|r| r == "icon") {
                2
            } else {
                continue;
            };
            if href.ends_with(".svg") {
                continue;
            }
            if let Some(u) = urlnorm::resolve(Some(&f.final_url), &href) {
                links.push((score, u));
            }
        }
        links.sort_by_key(|l| std::cmp::Reverse(l.0));
        candidates.extend(links.into_iter().map(|(_, u)| u));
    }
    if let Ok(u) = base.join("/favicon.ico") {
        candidates.push(u.to_string());
    }
    // The feed's own image/logo as a last resort.
    if let Some(img) = feed_image.filter(|i| i.starts_with("http")) {
        candidates.push(img.to_string());
    }
    candidates.dedup();
    for c in candidates.iter().take(5) {
        if let Ok(f) = http::get(&app.http.content, c, &ua, timeout, 512 * 1024, app.http.public_only, "image/*").await
            && f.status < 400
            && let Some(mime) = sniff_icon(&f.body)
        {
            return Ok((mime.to_string(), f.body));
        }
    }
    bail!("no usable icon")
}

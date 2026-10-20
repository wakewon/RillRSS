//! Image cache: downloads article images, stores them content-addressed on disk and
//! rewrites `<img src>` to this server.
//!
//! Safety limits: http(s) only, public addresses only (unless allowed), size cap,
//! magic-byte type check (raster formats only — SVG is never cached), tracking-pixel
//! removal, per-article image cap and per-host politeness. Past the total size cap the
//! images of the oldest articles are dropped (see `ImageStore::evict`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use futures::StreamExt;
use rusqlite::{OptionalExtension, params};

use super::dom::{fragment_html, parse_fragment};
use crate::db::Db;
use crate::http::{self, HttpClients};
use crate::settings::ImageSettings;
use crate::util::html::BASE_MARKER;
use crate::util::{self, urlnorm};

#[derive(Clone)]
pub struct ImageStore {
    pub dir: PathBuf,
    db: Db,
    total_bytes: Arc<AtomicI64>,
    /// An eviction (see `evict`) is running in the background.
    evicting: Arc<AtomicBool>,
}

#[derive(Debug, Default)]
pub struct CacheResult {
    pub html: String,
    pub hashes: Vec<String>,
    pub cached: usize,
    pub failed: usize,
    pub removed_pixels: usize,
}

/// Detected raster image formats we are willing to cache.
pub fn sniff(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(("image/jpeg", "jpg"));
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(("image/png", "png"));
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(("image/gif", "gif"));
    }
    if bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some(("image/webp", "webp"));
    }
    if bytes.len() > 12 && &bytes[4..8] == b"ftyp" {
        let brand = &bytes[8..12];
        if brand == b"avif" || brand == b"avis" {
            return Some(("image/avif", "avif"));
        }
        if brand == b"heic" || brand == b"heix" || brand == b"mif1" {
            return Some(("image/heic", "heic"));
        }
    }
    if bytes.starts_with(b"BM") {
        return Some(("image/bmp", "bmp"));
    }
    None
}

pub fn mime_for_ext(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "jpg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "heic" => "image/heic",
        "bmp" => "image/bmp",
        _ => return None,
    })
}

/// Public path of a cached image.
pub fn public_path(hash: &str, ext: &str) -> String {
    format!("/img/{}/{hash}.{ext}", &hash[..2])
}

impl ImageStore {
    pub fn new(dir: PathBuf, db: Db) -> Result<ImageStore> {
        std::fs::create_dir_all(&dir)?;
        let total: i64 = db.conn()?.query_row("SELECT COALESCE(SUM(size), 0) FROM images", [], |r| r.get(0))?;
        Ok(ImageStore { dir, db, total_bytes: Arc::new(AtomicI64::new(total)), evicting: Arc::new(AtomicBool::new(false)) })
    }

    pub fn file_path(&self, hash: &str, ext: &str) -> PathBuf {
        self.dir.join(&hash[..2]).join(format!("{hash}.{ext}"))
    }

    pub fn total_bytes(&self) -> i64 {
        self.total_bytes.load(Ordering::Relaxed)
    }

    pub fn sub_bytes(&self, n: i64) {
        self.total_bytes.fetch_sub(n, Ordering::Relaxed);
    }

    /// Validate a request path segment like `<hash>.<ext>` and return the file path.
    pub fn resolve_request(&self, name: &str) -> Option<(PathBuf, &'static str)> {
        let (hash, ext) = name.rsplit_once('.')?;
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let mime = mime_for_ext(ext)?;
        let p = self.file_path(hash, ext);
        Some((p, mime))
    }

    async fn write_file(&self, hash: &str, ext: &str, bytes: &[u8]) -> Result<()> {
        let path = self.file_path(hash, ext);
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp = path.with_extension(format!("{ext}.tmp{}", rand::random::<u32>()));
        tokio::fs::write(&tmp, bytes).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(())
    }

    /// Download (or reuse) one image. Returns (hash, ext, width, height).
    async fn fetch_one(
        &self,
        http: &HttpClients,
        cfg: &ImageSettings,
        url: &str,
        referer: Option<&str>,
        user_agent: &str,
    ) -> Result<(String, String, Option<u32>, Option<u32>)> {
        // Already known?
        let u = url.to_string();
        let known: Option<(Option<String>, String, i64, i64)> = self
            .db
            .call(move |c| {
                Ok(c.query_row(
                    "SELECT s.hash, s.status, s.attempts, s.fetched_at FROM image_sources s WHERE s.url = ?",
                    [u],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?)
            })
            .await?;
        if let Some((hash, status, attempts, fetched_at)) = &known {
            if status == "ok"
                && let Some(h) = hash
            {
                let h2 = h.clone();
                let meta: Option<(String, Option<u32>, Option<u32>)> = self
                    .db
                    .call(move |c| {
                        Ok(c.query_row("SELECT ext, width, height FROM images WHERE hash = ?", [h2], |r| {
                            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                        })
                        .optional()?)
                    })
                    .await?;
                if let Some((ext, w, hgt)) = meta
                    && tokio::fs::try_exists(self.file_path(h, &ext)).await.unwrap_or(false)
                {
                    return Ok((h.clone(), ext, w, hgt));
                }
            } else if status != "ok" && *attempts >= 3 && util::now() - fetched_at < 7 * 86400 {
                bail!("previously failed {attempts} times");
            }
        }

        let result = self.download(http, cfg, url, referer, user_agent).await;
        let now = util::now();
        match result {
            Ok((bytes, mime, ext)) => {
                let dims = imagesize::blob_size(&bytes).ok();
                let (w, h) = dims.map(|d| (Some(d.width as u32), Some(d.height as u32))).unwrap_or((None, None));
                let hash = blake3::hash(&bytes).to_hex().to_string();
                self.write_file(&hash, ext, &bytes).await?;
                let size = bytes.len() as i64;
                let (h2, u2, mime2, ext2) = (hash.clone(), url.to_string(), mime.to_string(), ext.to_string());
                let inserted = self
                    .db
                    .call(move |c| {
                        let n = c.execute(
                            "INSERT OR IGNORE INTO images (hash, mime, ext, size, width, height, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
                            params![h2, mime2, ext2, size, w, h, now],
                        )?;
                        c.execute(
                            "INSERT INTO image_sources (url, hash, status, error, attempts, fetched_at) VALUES (?, ?, 'ok', NULL, 1, ?)
                             ON CONFLICT(url) DO UPDATE SET hash = excluded.hash, status = 'ok', error = NULL, attempts = attempts + 1, fetched_at = excluded.fetched_at",
                            params![u2, h2, now],
                        )?;
                        Ok(n)
                    })
                    .await?;
                if inserted > 0 {
                    self.total_bytes.fetch_add(size, Ordering::Relaxed);
                }
                Ok((hash, ext.to_string(), w, h))
            }
            Err(e) => {
                let (u2, msg) = (url.to_string(), e.to_string());
                self.db
                    .call(move |c| {
                        c.execute(
                            "INSERT INTO image_sources (url, hash, status, error, attempts, fetched_at) VALUES (?, NULL, 'failed', ?, 1, ?)
                             ON CONFLICT(url) DO UPDATE SET status = 'failed', error = excluded.error, attempts = attempts + 1, fetched_at = excluded.fetched_at",
                            params![u2, msg, now],
                        )?;
                        Ok(())
                    })
                    .await?;
                Err(e)
            }
        }
    }

    async fn download(
        &self,
        http: &HttpClients,
        cfg: &ImageSettings,
        url: &str,
        referer: Option<&str>,
        user_agent: &str,
    ) -> Result<(Vec<u8>, &'static str, &'static str)> {
        let parsed = url::Url::parse(url)?;
        http::check_url(&parsed, http.public_only)?;
        let host = parsed.host_str().unwrap_or("").to_string();
        let _permit = http.hosts.acquire(&host, 4, Duration::from_millis(50)).await;
        let mut req = http
            .content
            .get(parsed)
            .header(reqwest::header::USER_AGENT, user_agent)
            .header(reqwest::header::ACCEPT, "image/avif,image/webp,image/png,image/jpeg,image/*;q=0.8")
            .timeout(Duration::from_secs(cfg.timeout_secs.max(5) as u64));
        if let Some(r) = referer {
            req = req.header(reqwest::header::REFERER, r);
        }
        let resp = req.send().await?;
        let f = http::read_limited(resp, cfg.max_mb.max(1) as usize * 1024 * 1024).await?;
        if f.status >= 400 {
            bail!("HTTP {}", f.status);
        }
        let (mime, ext) = sniff(&f.body).ok_or_else(|| {
            anyhow!("not a supported raster image ({})", f.content_type().unwrap_or_else(|| "unknown type".into()))
        })?;
        Ok((f.body, mime, ext))
    }

    /// Cache all images of an HTML fragment and rewrite their URLs.
    pub async fn cache_html(
        &self,
        http: &HttpClients,
        cfg: &ImageSettings,
        html: &str,
        referer: Option<&str>,
        user_agent: &str,
    ) -> Result<CacheResult> {
        // Collect unique absolute image URLs (in order).
        let urls: Vec<String> = {
            let doc = parse_fragment(html);
            let mut v: Vec<String> = Vec::new();
            for n in doc.select("img").nodes() {
                if let Some(src) = n.attr("src") {
                    let s = src.trim().to_string();
                    if (s.starts_with("http://") || s.starts_with("https://")) && !v.contains(&s) {
                        v.push(s);
                    }
                }
            }
            v.truncate(cfg.max_per_article.max(1) as usize);
            v
        };
        type Fetched = Result<(String, String, Option<u32>, Option<u32>)>;
        let mut results: HashMap<String, Fetched> = HashMap::new();
        let conc = cfg.concurrency.max(1) as usize;
        let fetched: Vec<(String, Fetched)> = futures::stream::iter(urls)
            .map(|u| async move {
                let r = self.fetch_one(http, cfg, &u, referer, user_agent).await;
                (u, r)
            })
            .buffer_unordered(conc)
            .collect()
            .await;
        for (u, r) in fetched {
            results.insert(u, r);
        }

        let doc = parse_fragment(html);
        let mut out = CacheResult::default();
        for n in doc.select("img").nodes().to_vec() {
            let Some(src) = n.attr("src").map(|s| s.trim().to_string()) else { continue };
            match results.get(&src) {
                Some(Ok((hash, ext, w, h))) => {
                    let min = cfg.min_dimension;
                    if let (Some(w), Some(h)) = (w, h)
                        && *w < min
                        && *h < min
                    {
                        n.remove_from_parent();
                        out.removed_pixels += 1;
                        continue;
                    }
                    n.set_attr("src", &format!("{BASE_MARKER}{}", public_path(hash, ext)));
                    n.remove_attr("srcset");
                    n.remove_attr("sizes");
                    if n.attr("width").is_none() && let Some(w) = w {
                        n.set_attr("width", &w.to_string());
                    }
                    if n.attr("height").is_none() && let Some(h) = h {
                        n.set_attr("height", &h.to_string());
                    }
                    if !out.hashes.contains(hash) {
                        out.hashes.push(hash.clone());
                        out.cached += 1;
                    }
                }
                Some(Err(e)) => {
                    tracing::debug!(url = %src, "image not cached: {e:#}");
                    out.failed += 1;
                }
                None => {}
            }
        }
        out.html = fragment_html(&doc);
        if cfg.max_total_mb > 0 {
            self.evict_in_background(cfg.max_total_mb as i64 * 1024 * 1024);
        }
        Ok(out)
    }

    /// Start `evict` unless the cache is within `max_bytes` or one is running already.
    fn evict_in_background(&self, max_bytes: i64) {
        if self.total_bytes() <= max_bytes || self.evicting.swap(true, Ordering::AcqRel) {
            return;
        }
        let store = self.clone();
        tokio::spawn(async move {
            match store.evict(max_bytes).await {
                Ok((0, _)) => {}
                Ok((n, freed)) => tracing::info!("image cache over its limit: dropped {n} images of the oldest articles ({freed} bytes)"),
                Err(e) => tracing::warn!("image cache eviction failed: {e:#}"),
            }
            store.evicting.store(false, Ordering::Release);
        });
    }

    /// Bring the cache under `max_bytes`, down to 90% of it so that this does not run
    /// for every new image. Images whose newest article is oldest go first, those of
    /// starred articles last. Articles that used a dropped image get its original
    /// address back, and `/img/` redirects there for copies clients already have.
    /// Returns (images dropped, bytes freed).
    pub async fn evict(&self, max_bytes: i64) -> Result<(usize, i64)> {
        let target = max_bytes / 10 * 9;
        let (mut dropped, mut freed) = (0usize, 0i64);
        while max_bytes > 0 && self.total_bytes() > target {
            let young = util::now() - 86400;
            let batch: Vec<(String, String, i64)> = self
                .db
                .call(move |c| {
                    // An unreferenced new image may belong to an article still being
                    // processed. Without a known source an image could not be restored.
                    let mut st = c.prepare(
                        "SELECT i.hash, i.ext, i.size FROM images i
                         LEFT JOIN item_images ii ON ii.hash = i.hash
                         LEFT JOIN items it ON it.id = ii.item_id
                         WHERE EXISTS (SELECT 1 FROM image_sources s WHERE s.hash = i.hash AND s.status = 'ok')
                         GROUP BY i.hash
                         HAVING COUNT(it.id) > 0 OR i.created_at < ?
                         ORDER BY COALESCE(MAX(it.is_starred), 0), MAX(i.created_at * 1000000, COALESCE(MAX(it.created_at), 0))
                         LIMIT 100",
                    )?;
                    let v = st.query_map([young], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?;
                    Ok(v)
                })
                .await?;
            let mut progressed = false;
            for (hash, ext, size) in batch {
                if self.total_bytes() <= target {
                    break;
                }
                if self.drop_image(&hash, &ext, size).await? {
                    dropped += 1;
                    freed += size;
                    progressed = true;
                }
            }
            if !progressed {
                break;
            }
        }
        Ok((dropped, freed))
    }

    /// Remove one image from the cache and point the articles using it at its original
    /// address. False when it is gone already or its source is unknown.
    async fn drop_image(&self, hash: &str, ext: &str, size: i64) -> Result<bool> {
        let cached = format!("{BASE_MARKER}{}", public_path(hash, ext));
        let (h, now) = (hash.to_string(), util::now());
        let dropped = self
            .db
            .write(move |tx| {
                let original: Option<String> = tx
                    .query_row("SELECT url FROM image_sources WHERE hash = ? AND status = 'ok' ORDER BY fetched_at DESC LIMIT 1", [&h], |r| r.get(0))
                    .optional()?;
                let Some(original) = original else { return Ok(false) };
                let original = html_escape::encode_double_quoted_attribute(&original).into_owned();
                let items: Vec<i64> = {
                    let mut st = tx.prepare("SELECT item_id FROM item_images WHERE hash = ?")?;
                    st.query_map([&h], |r| r.get(0))?.collect::<Result<_, _>>()?
                };
                for id in items {
                    let content: String = tx.query_row("SELECT content FROM items WHERE id = ?", [id], |r| r.get(0))?;
                    if content.contains(&cached) {
                        tx.execute("UPDATE items SET content = ? WHERE id = ?", params![content.replace(&cached, &original), id])?;
                    }
                }
                tx.execute("DELETE FROM item_images WHERE hash = ?", [&h])?;
                // The source row stays (as of now) for the `/img/` redirect; maintenance
                // drops it later.
                tx.execute("UPDATE image_sources SET fetched_at = ? WHERE hash = ? AND status = 'ok'", params![now, h])?;
                Ok(tx.execute("DELETE FROM images WHERE hash = ?", [&h])? > 0)
            })
            .await?;
        if dropped {
            let _ = tokio::fs::remove_file(self.file_path(hash, ext)).await;
            self.sub_bytes(size);
        }
        Ok(dropped)
    }

    /// Original address of a cached (or dropped) image.
    pub async fn source_url(&self, hash: &str) -> Result<Option<String>> {
        let h = hash.to_string();
        self.db
            .call(move |c| {
                Ok(c.query_row("SELECT url FROM image_sources WHERE hash = ? AND status = 'ok' ORDER BY fetched_at DESC LIMIT 1", [h], |r| r.get(0))
                    .optional()?)
            })
            .await
    }

    /// Delete images no longer referenced by any item (older than `grace_secs`).
    /// Returns (files deleted, bytes freed).
    pub async fn gc(&self, grace_secs: i64) -> Result<(usize, i64)> {
        let cutoff = util::now() - grace_secs;
        let orphans: Vec<(String, String, i64)> = self
            .db
            .call(move |c| {
                let mut st = c.prepare(
                    "SELECT i.hash, i.ext, i.size FROM images i
                     WHERE i.created_at < ? AND NOT EXISTS (SELECT 1 FROM item_images ii WHERE ii.hash = i.hash)
                     LIMIT 5000",
                )?;
                let v = st.query_map([cutoff], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?;
                Ok(v)
            })
            .await?;
        let mut freed = 0i64;
        let mut deleted = 0usize;
        for (hash, ext, size) in orphans {
            let p = self.file_path(&hash, &ext);
            let _ = tokio::fs::remove_file(&p).await;
            let h2 = hash.clone();
            self.db
                .call(move |c| {
                    c.execute("DELETE FROM images WHERE hash = ? AND NOT EXISTS (SELECT 1 FROM item_images WHERE hash = ?)", params![h2, h2])?;
                    c.execute("DELETE FROM image_sources WHERE hash = ?", [h2])?;
                    Ok(())
                })
                .await?;
            freed += size;
            deleted += 1;
        }
        self.sub_bytes(freed);
        Ok((deleted, freed))
    }

    /// Size of the cache directory on disk (slow; for diagnostics).
    pub fn disk_usage(dir: &Path) -> u64 {
        fn walk(p: &Path) -> u64 {
            let Ok(rd) = std::fs::read_dir(p) else { return 0 };
            rd.flatten()
                .map(|e| match e.metadata() {
                    Ok(m) if m.is_dir() => walk(&e.path()),
                    Ok(m) => m.len(),
                    Err(_) => 0,
                })
                .sum()
        }
        walk(dir)
    }
}

/// Resolve image URLs of a fragment against a base (used for lead images).
pub fn absolutize(src: &str, base: Option<&url::Url>) -> Option<String> {
    urlnorm::resolve(base, src)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_formats() {
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(("image/jpeg", "jpg")));
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nxxxx"), Some(("image/png", "png")));
        assert_eq!(sniff(b"GIF89a...."), Some(("image/gif", "gif")));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some(("image/webp", "webp")));
        assert_eq!(sniff(b"<svg xmlns=..."), None);
        assert_eq!(sniff(b"<html>"), None);
    }

    #[test]
    fn request_path_validation() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let store = ImageStore::new(dir.path().join("img"), db).unwrap();
        let h = "a".repeat(64);
        assert!(store.resolve_request(&format!("{h}.jpg")).is_some());
        assert!(store.resolve_request(&format!("{h}.svg")).is_none());
        assert!(store.resolve_request("../../etc/passwd.jpg").is_none());
        assert!(store.resolve_request(&format!("{}.jpg", "g".repeat(64))).is_none());
    }
}

//! Google Reader API (FreshRSS-compatible).
//!
//! Served at `/api/greader.php/...` (FreshRSS), `/[tt-rss/]plugins.local/freshapi/api/greader.php/...`
//! (FreshAPI) and at the root (`/accounts/ClientLogin`, `/reader/api/0/...`).
//!
//! Item ids: short (decimal) id = GREADER_ID_OFFSET + item id (16 digits), long id =
//! `tag:google.com,2005:reader/item/` + 16 lowercase hex digits of the same number.
//! `timestampUsec`/`crawlTimeMsec` are the insertion (publish) time; `ot`/`nt` filter on it.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::Response;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use super::{Params, api_headers, cap_content, client_ip, request_base};
use crate::app::{App, AppRef};
use crate::auth;
use crate::feeds::fetcher;
use crate::opml;
use crate::store::{self, GREADER_ID_OFFSET, ItemQuery, ItemRow, Order};
use crate::util::{self, html::expand_base};

const LONG_PREFIX: &str = "tag:google.com,2005:reader/item/";
const READING_LIST: &str = "user/-/state/com.google/reading-list";
const STATE_READ: &str = "user/-/state/com.google/read";
const STATE_STARRED: &str = "user/-/state/com.google/starred";
const UNCATEGORIZED: &str = "Uncategorized";
/// Content cap (News+ fails on huge items; FreshRSS uses the same limit).
const MAX_CONTENT_BYTES: usize = 500_000;
/// Slack applied to `ot` (seconds). Zero: some clients (Readrops) store re-sent items as
/// duplicates; insertion times are strictly monotonic at commit instead.
const OT_GRACE_SECS: i64 = 0;

// ---------------------------------------------------------------------------
// Ids
// ---------------------------------------------------------------------------

pub fn short_id(item_id: i64) -> String {
    (GREADER_ID_OFFSET + item_id).to_string()
}

pub fn long_id(item_id: i64) -> String {
    format!("{LONG_PREFIX}{:016x}", (GREADER_ID_OFFSET + item_id) as u64)
}

/// Parse any client item id form into an internal item id.
pub fn parse_item_id(raw: &str) -> Option<i64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let value: i64 = if let Some(hex) = s.strip_prefix(LONG_PREFIX).or_else(|| s.strip_prefix("tag:google.com,2005:reader/item/")) {
        u64::from_str_radix(hex, 16).ok()? as i64
    } else if s.chars().all(|c| c.is_ascii_digit() || c == '-') && (s == "0" || !s.starts_with('0')) {
        s.parse::<i64>().ok()?
    } else if s.len() <= 16 && s.chars().all(|c| c.is_ascii_hexdigit()) {
        u64::from_str_radix(s, 16).ok()? as i64
    } else {
        let last = s.rsplit('/').next().filter(|l| l.len() <= 16 && l.chars().all(|c| c.is_ascii_hexdigit()))?;
        u64::from_str_radix(last, 16).ok()? as i64
    };
    if value >= GREADER_ID_OFFSET {
        Some(value - GREADER_ID_OFFSET)
    } else if value > 0 {
        Some(value) // lenient: raw internal id
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Streams
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Stream {
    ReadingList,
    Starred,
    Read,
    Unread,
    Feed(i64),
    FeedUrl(String),
    Label(String),
    Ignored,
}

pub fn parse_stream(raw: &str) -> Stream {
    let mut s = raw.trim().to_string();
    // user/<anything>/... -> user/-/...
    if let Some(rest) = s.strip_prefix("user/")
        && let Some(pos) = rest.find('/')
    {
        s = format!("user/-/{}", &rest[pos + 1..]);
    }
    if let Some(state) = s.strip_prefix("user/-/state/com.google/") {
        return match state {
            "reading-list" => Stream::ReadingList,
            "starred" => Stream::Starred,
            "read" => Stream::Read,
            "unread" => Stream::Unread,
            _ => Stream::Ignored,
        };
    }
    if let Some(state) = s.strip_prefix("user/-/state/org.freshrss/") {
        return if state == "main" { Stream::ReadingList } else { Stream::Ignored };
    }
    if let Some(name) = s.strip_prefix("user/-/label/") {
        return Stream::Label(name.to_string());
    }
    let mut f = s.as_str();
    let mut is_feed = false;
    while let Some(rest) = f.strip_prefix("feed/") {
        f = rest;
        is_feed = true;
    }
    if is_feed {
        return match f.parse::<i64>() {
            Ok(n) => Stream::Feed(n),
            Err(_) => Stream::FeedUrl(f.to_string()),
        };
    }
    if s.starts_with("http://") || s.starts_with("https://") {
        return Stream::FeedUrl(s);
    }
    Stream::Ignored
}

fn label_id_str(name: &str) -> String {
    format!("user/-/label/{name}")
}

/// Turn a stream into a base query. None = matches nothing.
fn stream_query(conn: &Connection, stream: &Stream) -> anyhow::Result<Option<ItemQuery>> {
    let mut q = ItemQuery::default();
    match stream {
        Stream::ReadingList => {}
        Stream::Starred => q.starred = Some(true),
        Stream::Read => q.read = Some(true),
        Stream::Unread => q.read = Some(false),
        Stream::Feed(id) => q.feed_ids = Some(vec![*id]),
        Stream::FeedUrl(u) => {
            let id: Option<i64> = conn.query_row("SELECT id FROM feeds WHERE url = ?", [u], |r| r.get(0)).optional()?;
            match id {
                Some(id) => q.feed_ids = Some(vec![id]),
                None => return Ok(None),
            }
        }
        Stream::Label(name) => {
            if let Some(cid) = store::find_category(conn, name)? {
                q.category = Some(Some(cid));
            } else if name.eq_ignore_ascii_case(UNCATEGORIZED) {
                q.category = Some(None);
            } else if let Some(lid) = store::find_label(conn, name)? {
                q.label_id = Some(lid);
            } else {
                return Ok(None);
            }
        }
        Stream::Ignored => return Ok(None),
    }
    Ok(Some(q))
}

/// Apply xt / it filters. Returns false if the combination matches nothing.
fn apply_filters(conn: &Connection, q: &mut ItemQuery, xt: &[&str], it: &[&str]) -> anyhow::Result<bool> {
    fn set(slot: &mut Option<bool>, v: bool) -> bool {
        match slot {
            Some(cur) if *cur != v => false,
            _ => {
                *slot = Some(v);
                true
            }
        }
    }
    for x in xt {
        let ok = match parse_stream(x) {
            Stream::Read => set(&mut q.read, false),
            Stream::Unread => set(&mut q.read, true),
            Stream::Starred => set(&mut q.starred, false),
            _ => true,
        };
        if !ok {
            return Ok(false);
        }
    }
    for x in it {
        let ok = match parse_stream(x) {
            Stream::Read => set(&mut q.read, true),
            Stream::Unread => set(&mut q.read, false),
            Stream::Starred => set(&mut q.starred, true),
            Stream::Label(name) => match store::find_label(conn, &name)? {
                Some(l) => {
                    q.label_id = Some(l);
                    true
                }
                None => match store::find_category(conn, &name)? {
                    Some(c) => {
                        q.category = Some(Some(c));
                        true
                    }
                    None => false,
                },
            },
            Stream::Feed(f) => {
                q.feed_ids = Some(vec![f]);
                true
            }
            _ => true,
        };
        if !ok {
            return Ok(false);
        }
    }
    Ok(true)
}

fn parse_secs(v: Option<&str>) -> Option<i64> {
    let v = v?.trim();
    let n: f64 = v.parse().ok()?;
    if !n.is_finite() || n <= 0.0 {
        return None;
    }
    let n = n as i64;
    // Accept milliseconds / microseconds by magnitude.
    Some(if n >= 100_000_000_000_000 { n / 1_000_000 } else if n >= 100_000_000_000 { n / 1000 } else { n })
}

/// Normalize a mark-all-as-read `ts` to microseconds.
fn ts_to_us(v: Option<&str>) -> Option<i64> {
    let n: i64 = v?.trim().parse().ok()?;
    if n <= 0 {
        return None;
    }
    Some(if n >= 100_000_000_000_000_000 {
        n / 1000 // ns
    } else if n >= 100_000_000_000_000 {
        n // µs
    } else if n >= 100_000_000_000 {
        n * 1000 // ms
    } else {
        n * 1_000_000 // s
    })
}

/// Build the stream query from request parameters. None = empty result.
fn build_query(conn: &Connection, stream: &Stream, p: &Params, max_n: usize, default_n: usize) -> anyhow::Result<Option<(ItemQuery, usize)>> {
    let Some(mut q) = stream_query(conn, stream)? else { return Ok(None) };
    if !apply_filters(conn, &mut q, &p.all("xt"), &p.all("it"))? {
        return Ok(None);
    }
    if let Some(ot) = parse_secs(p.get("ot")) {
        q.min_created_us = Some((ot - OT_GRACE_SECS) * 1_000_000);
        if q.read == Some(true) {
            // Read-id queries: also report items marked read since `ot` (e.g. superseded
            // versions of long-running stories).
            q.or_read_since = Some(ot - OT_GRACE_SECS);
        }
    }
    if let Some(nt) = parse_secs(p.get("nt")) {
        q.max_created_us = Some(nt * 1_000_000 + 999_999);
    }
    let asc = p.get("r") == Some("o");
    q.order = if asc { Order::IdAsc } else { Order::IdDesc };
    if let Some(c) = p.get("c").and_then(parse_item_id) {
        if asc {
            q.id_gt = Some(c);
        } else {
            q.id_lt = Some(c);
        }
    }
    let n = match p.get("n").and_then(|n| n.trim().parse::<i64>().ok()) {
        Some(n) if n > 0 => (n as usize).min(max_n),
        Some(_) => max_n,
        None => default_n,
    };
    q.limit = Some(n + 1);
    Ok(Some((q, n)))
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

fn respond(status: StatusCode, content_type: &'static str, body: impl Into<axum::body::Body>) -> Response {
    let mut r = Response::new(body.into());
    *r.status_mut() = status;
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    api_headers(r.headers_mut());
    r
}

fn json_resp(v: &Value) -> Response {
    respond(StatusCode::OK, "application/json; charset=utf-8", serde_json::to_vec(v).unwrap_or_default())
}

fn text(status: StatusCode, s: &str) -> Response {
    respond(status, "text/plain; charset=utf-8", s.to_string())
}

fn ok() -> Response {
    text(StatusCode::OK, "OK")
}

fn bad_request() -> Response {
    text(StatusCode::BAD_REQUEST, "Bad Request!")
}

fn unauthorized() -> Response {
    let mut r = text(StatusCode::UNAUTHORIZED, "Unauthorized!");
    r.headers_mut().insert("Google-Bad-Token", HeaderValue::from_static("true"));
    r.headers_mut().insert("X-Reader-Google-Bad-Token", HeaderValue::from_static("true"));
    r
}

fn server_error(e: anyhow::Error) -> Response {
    tracing::error!("greader: {e:#}");
    text(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error!")
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

const PREFIXES: &[&str] = &["/tt-rss", "/plugins.local/freshapi", "/api/greader.php", "/greader.php", "/api"];

/// Strip API roots from the raw path, collapse `//` and a trailing `/`.
pub fn api_path(raw: &str) -> String {
    let mut p = raw.to_string();
    while p.contains("//") {
        p = p.replace("//", "/");
    }
    loop {
        let mut changed = false;
        for pre in PREFIXES {
            if p == *pre || p.starts_with(&format!("{pre}/")) {
                p = p[pre.len()..].to_string();
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    if p.len() > 1 && p.ends_with('/') {
        p.pop();
    }
    if p == "/" {
        p.clear();
    }
    p
}

pub async fn handle(
    State(app): State<AppRef>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    body: Bytes,
) -> Response {
    if method == Method::OPTIONS {
        return respond(StatusCode::NO_CONTENT, "text/plain; charset=utf-8", "");
    }
    let path = api_path(uri.path());
    let ctype = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    let form_body = ctype.is_empty() || ctype.starts_with("application/x-www-form-urlencoded") || ctype.starts_with("text/plain");
    let is_import = path.ends_with("/subscription/import");
    let params = Params::parse(uri.query(), if form_body && !is_import { Some(&body) } else { None });
    let resp = route(&app, &method, &path, &params, &headers, peer, &body).await;
    match resp {
        Ok(r) => {
            if method == Method::HEAD {
                let (parts, _) = r.into_parts();
                Response::from_parts(parts, axum::body::Body::empty())
            } else {
                r
            }
        }
        Err(e) => server_error(e),
    }
}

async fn authenticate(app: &App, headers: &HeaderMap, params: &Params, method: &Method) -> anyhow::Result<bool> {
    let expected = auth::greader_token(app).await?;
    if let Some(v) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        let v = v.trim();
        let token = if v.len() > 11 && v[..11].eq_ignore_ascii_case("googlelogin") {
            let rest = v[11..].trim();
            rest.split_once('=').map(|(k, t)| (k.trim().eq_ignore_ascii_case("auth"), t)).filter(|(ok, _)| *ok).map(|(_, t)| t.to_string())
        } else if v.len() > 6 && v[..6].eq_ignore_ascii_case("bearer") {
            Some(v[6..].trim().to_string())
        } else {
            None
        };
        if let Some(t) = token {
            let t = t.trim().trim_matches('"');
            let t = percent_encoding::percent_decode_str(t).decode_utf8_lossy().to_string();
            // FreshRSS-style "user/token" is accepted too.
            let t = t.rsplit('/').next().unwrap_or(&t).to_string();
            return Ok(auth::ct_eq(&t, &expected));
        }
    }
    // Miniflux style: POST authenticated by T = auth token.
    if *method == Method::POST
        && let Some(t) = params.last("T")
    {
        return Ok(auth::ct_eq(t.trim(), &expected));
    }
    Ok(false)
}

async fn route(app: &AppRef, method: &Method, path: &str, p: &Params, headers: &HeaderMap, peer: SocketAddr, body: &Bytes) -> anyhow::Result<Response> {
    if path.is_empty() {
        return Ok(ok());
    }
    if path == "/check/compatibility" || path == "/check%2Fcompatibility" {
        return Ok(text(StatusCode::OK, "PASS"));
    }
    if path == "/accounts/ClientLogin" {
        return client_login(app, p, headers, peer, body).await;
    }
    if !authenticate(app, headers, p, method).await? {
        return Ok(unauthorized());
    }
    if path == "/reader/ping" {
        return Ok(ok());
    }
    let Some(rest) = path.strip_prefix("/reader/api/0") else { return Ok(bad_request()) };
    // stream/contents carries the stream id in the (raw) path.
    if let Some(sid) = rest.strip_prefix("/stream/contents") {
        let raw = sid.trim_start_matches('/');
        let stream_id = if raw.is_empty() {
            p.get("s").unwrap_or(READING_LIST).to_string()
        } else {
            percent_encoding::percent_decode_str(raw).decode_utf8_lossy().to_string()
        };
        return stream_contents(app, headers, &stream_id, p).await;
    }
    match rest {
        "/token" => {
            let t = auth::greader_token(app).await?;
            Ok(text(StatusCode::OK, &format!("{}\n", auth::greader_action_token(&t))))
        }
        "/user-info" => {
            let u = &app.cfg.username;
            Ok(json_resp(&json!({"userId": u, "userName": u, "userProfileId": u, "userEmail": ""})))
        }
        "/tag/list" => tag_list(app).await,
        "/subscription/list" => subscription_list(app, headers).await,
        "/subscription/edit" => subscription_edit(app, p).await,
        "/subscription/quickadd" => quickadd(app, p).await,
        "/subscription/export" => {
            let xml = opml::export(app).await?;
            let mut r = respond(StatusCode::OK, "application/xml; charset=utf-8", xml);
            r.headers_mut().insert(
                header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&format!("attachment; filename=\"feeds_{}.opml.xml\"", chrono::Utc::now().format("%Y-%m-%d")))?,
            );
            Ok(r)
        }
        "/subscription/import" => {
            if body.is_empty() {
                return Ok(bad_request());
            }
            match opml::import(app, body).await {
                Ok(_) => Ok(ok()),
                Err(e) => {
                    tracing::warn!("OPML import failed: {e:#}");
                    Ok(bad_request())
                }
            }
        }
        "/subscribed" => {
            let s = p.get("s").map(parse_stream);
            let found = match s {
                Some(Stream::Feed(id)) => app.db.call(move |c| Ok(c.query_row("SELECT 1 FROM feeds WHERE id = ?", [id], |_| Ok(())).optional()?.is_some())).await?,
                Some(Stream::FeedUrl(u)) => app.db.call(move |c| Ok(c.query_row("SELECT 1 FROM feeds WHERE url = ?", [u], |_| Ok(())).optional()?.is_some())).await?,
                _ => false,
            };
            Ok(text(StatusCode::OK, if found { "true" } else { "false" }))
        }
        "/unread-count" => unread_count(app).await,
        "/stream/items/ids" => items_ids(app, p).await,
        "/stream/items/contents" => items_contents(app, headers, p).await,
        "/stream/items/count" => {
            let stream = parse_stream(p.get("s").unwrap_or(READING_LIST));
            let p2 = p.clone();
            let n = app
                .db
                .call(move |c| {
                    Ok(match build_query(c, &stream, &p2, usize::MAX, usize::MAX)? {
                        Some((mut q, _)) => {
                            q.limit = None;
                            q.id_lt = None;
                            q.id_gt = None;
                            q.count(c)?
                        }
                        None => 0,
                    })
                })
                .await?;
            Ok(text(StatusCode::OK, &n.to_string()))
        }
        "/edit-tag" => edit_tag(app, p).await,
        "/mark-all-as-read" => mark_all_as_read(app, p).await,
        "/rename-tag" => rename_tag(app, p).await,
        "/disable-tag" => disable_tag(app, p).await,
        "/preference/list" => Ok(json_resp(&json!({"prefs": []}))),
        "/preference/stream/list" => Ok(json_resp(&json!({"streamprefs": {}}))),
        _ => Ok(bad_request()),
    }
}

/// Minimal multipart/form-data field parser (some clients post ClientLogin that way).
fn multipart_fields(content_type: &str, body: &[u8]) -> Vec<(String, String)> {
    let Some(boundary) = content_type.split(';').find_map(|p| p.trim().strip_prefix("boundary=")).map(|b| b.trim_matches('"').to_string()) else {
        return Vec::new();
    };
    let body = String::from_utf8_lossy(body);
    let delim = format!("--{boundary}");
    let mut out = Vec::new();
    for part in body.split(&delim) {
        let part = part.trim_start_matches("\r\n");
        let Some((head, value)) = part.split_once("\r\n\r\n").or_else(|| part.split_once("\n\n")) else { continue };
        let name = head
            .split(';')
            .find_map(|x| x.trim().strip_prefix("name="))
            .map(|n| n.trim_matches('"').to_string());
        if let Some(n) = name {
            out.push((n, value.trim_end_matches("\r\n").trim_end_matches('\n').to_string()));
        }
    }
    out
}

async fn client_login(app: &AppRef, p: &Params, headers: &HeaderMap, peer: SocketAddr, body: &Bytes) -> anyhow::Result<Response> {
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    let mut p = p.clone();
    if ct.to_ascii_lowercase().starts_with("multipart/form-data") {
        p.0.extend(multipart_fields(ct, body));
    }
    let (Some(user), Some(pass)) = (p.last("Email"), p.last("Passwd")) else {
        return Ok(text(StatusCode::BAD_REQUEST, "Bad Request!"));
    };
    let ip = client_ip(app, headers, Some(peer));
    let Some(cred) = auth::api_login(app, user, &[pass], ip).await? else {
        return Ok(unauthorized());
    };
    // Bound to the credential that matched: if the password changed meanwhile, the
    // token is already invalid.
    let t = auth::greader_token_for(app, &cred).await?;
    let mut r = text(StatusCode::OK, &format!("SID={t}\nLSID=null\nAuth={t}\n"));
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(r)
}

// ---------------------------------------------------------------------------
// Listings
// ---------------------------------------------------------------------------

fn feed_category_name(f: &store::FeedInfo) -> String {
    f.category.clone().unwrap_or_else(|| UNCATEGORIZED.to_string())
}

async fn tag_list(app: &AppRef) -> anyhow::Result<Response> {
    let (cats, labels, has_uncat) = app
        .db
        .call(|c| {
            let cats = store::list_categories(c)?;
            let labels = store::list_labels(c)?;
            let has_uncat: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM feeds WHERE category_id IS NULL)", [], |r| r.get(0))?;
            Ok((cats, labels, has_uncat))
        })
        .await?;
    let mut tags = vec![json!({"id": STATE_STARRED}), json!({"id": READING_LIST})];
    let mut names: Vec<String> = cats.iter().map(|c| c.name.clone()).collect();
    if has_uncat && !names.iter().any(|n| n.eq_ignore_ascii_case(UNCATEGORIZED)) {
        names.push(UNCATEGORIZED.to_string());
    }
    names.sort_by_key(|n| n.to_lowercase());
    for n in names {
        tags.push(json!({"id": label_id_str(&n), "label": n, "type": "folder"}));
    }
    for l in labels {
        if cats.iter().any(|c| c.name.eq_ignore_ascii_case(&l.name)) {
            continue;
        }
        tags.push(json!({"id": label_id_str(&l.name), "label": l.name, "type": "tag", "unread_count": l.unread}));
    }
    Ok(json_resp(&json!({"tags": tags})))
}

async fn subscription_list(app: &AppRef, headers: &HeaderMap) -> anyhow::Result<Response> {
    let base = request_base(app, headers).await;
    let feeds = app.db.call(|c| store::list_feeds(c)).await?;
    let subs: Vec<Value> = feeds
        .iter()
        .map(|f| {
            let cat = feed_category_name(f);
            json!({
                "id": format!("feed/{}", f.id),
                "title": f.title,
                "categories": [{"id": label_id_str(&cat), "label": cat}],
                "url": f.url,
                "htmlUrl": f.site_url,
                "iconUrl": if f.has_icon { format!("{base}/feed-icons/{}.ico?h={}", f.id, f.icon_hash.clone().unwrap_or_default()) } else { String::new() },
                "sortid": format!("{:08X}", f.id),
                "firstitemmsec": "0",
            })
        })
        .collect();
    Ok(json_resp(&json!({"subscriptions": subs})))
}

async fn unread_count(app: &AppRef) -> anyhow::Result<Response> {
    let feeds = app.db.call(|c| store::list_feeds(c)).await?;
    let mut out: Vec<Value> = Vec::new();
    let mut total = 0i64;
    let mut newest_all = 0i64;
    let mut by_cat: std::collections::BTreeMap<String, (i64, i64)> = Default::default();
    let mut feeds_sorted = feeds.clone();
    feeds_sorted.sort_by_key(|f| (feed_category_name(f).to_lowercase(), f.title.to_lowercase()));
    let mut current_cat: Option<String> = None;
    for f in &feeds_sorted {
        let cat = feed_category_name(f);
        if let Some(prev) = &current_cat
            && *prev != cat
            && let Some((cnt, newest)) = by_cat.get(prev)
        {
            out.push(json!({"id": label_id_str(prev), "count": cnt, "newestItemTimestampUsec": newest.to_string()}));
        }
        current_cat = Some(cat.clone());
        out.push(json!({"id": format!("feed/{}", f.id), "count": f.unread, "newestItemTimestampUsec": f.newest_created_us.to_string()}));
        let e = by_cat.entry(cat).or_insert((0, 0));
        e.0 += f.unread;
        e.1 = e.1.max(f.newest_created_us);
        total += f.unread;
        newest_all = newest_all.max(f.newest_created_us);
    }
    if let Some(prev) = &current_cat
        && let Some((cnt, newest)) = by_cat.get(prev)
    {
        out.push(json!({"id": label_id_str(prev), "count": cnt, "newestItemTimestampUsec": newest.to_string()}));
    }
    let labels = app.db.call(|c| store::list_labels(c)).await?;
    for l in labels {
        out.push(json!({"id": label_id_str(&l.name), "count": l.unread, "newestItemTimestampUsec": newest_all.to_string()}));
    }
    out.push(json!({"id": READING_LIST, "count": total, "newestItemTimestampUsec": newest_all.to_string()}));
    Ok(json_resp(&json!({"max": total, "unreadcounts": out})))
}

// ---------------------------------------------------------------------------
// Items
// ---------------------------------------------------------------------------

pub fn item_json(it: &ItemRow, base: &str) -> Value {
    let cat = it.category.clone().unwrap_or_else(|| UNCATEGORIZED.to_string());
    let mut categories = vec![json!(READING_LIST), json!(label_id_str(&cat))];
    if it.is_read {
        categories.push(json!(STATE_READ));
    }
    if it.is_starred {
        categories.push(json!(STATE_STARRED));
    }
    for (_, l) in &it.labels {
        categories.push(json!(label_id_str(l)));
    }
    let content = cap_content(expand_base(&it.content, base), MAX_CONTENT_BYTES);
    let mut v = json!({
        "id": long_id(it.id),
        "crawlTimeMsec": (it.created_at / 1000).to_string(),
        "timestampUsec": it.created_at.to_string(),
        "published": it.published_at,
        "updated": it.updated_at,
        "title": it.title,
        "canonical": [{"href": it.link}],
        "alternate": [{"href": it.link, "type": "text/html"}],
        "categories": categories,
        "origin": {"streamId": format!("feed/{}", it.feed_id), "htmlUrl": it.site_url, "title": it.feed_title},
        "summary": {"direction": "ltr", "content": content},
    });
    if !it.author.is_empty() {
        v["author"] = json!(it.author);
    }
    let media = crate::feeds::parse::Media::from_json(it.media.as_deref());
    let mut enc: Vec<Value> = media
        .enclosures
        .iter()
        .map(|e| json!({"href": e.url, "type": e.mime, "length": e.length}))
        .collect();
    if let Some(t) = &media.thumbnail {
        enc.push(json!({"href": t, "type": "image/jpeg", "length": 0}));
    }
    if !enc.is_empty() {
        v["enclosure"] = json!(enc);
    }
    v
}

async fn stream_contents(app: &AppRef, headers: &HeaderMap, stream_id: &str, p: &Params) -> anyhow::Result<Response> {
    let base = request_base(app, headers).await;
    let stream = parse_stream(stream_id);
    let p2 = p.clone();
    let (items, more) = app
        .db
        .call(move |c| {
            let Some((q, n)) = build_query(c, &stream, &p2, 1000, 20)? else { return Ok((Vec::new(), false)) };
            let mut ids = q.ids(c)?;
            let more = ids.len() > n;
            ids.truncate(n);
            Ok((store::load_items_ordered(c, &ids, true)?, more))
        })
        .await?;
    let mut v = json!({
        "id": stream_id,
        "updated": util::now(),
        "items": items.iter().map(|it| item_json(it, &base)).collect::<Vec<_>>(),
    });
    if more && let Some(last) = items.last() {
        v["continuation"] = json!(short_id(last.id));
    }
    Ok(json_resp(&v))
}

async fn items_ids(app: &AppRef, p: &Params) -> anyhow::Result<Response> {
    let Some(s) = p.last("s") else { return Ok(bad_request()) };
    let stream = parse_stream(s);
    let p2 = p.clone();
    let (ids, more) = app
        .db
        .call(move |c| {
            let Some((q, n)) = build_query(c, &stream, &p2, 10_000, 10_000)? else { return Ok((Vec::new(), false)) };
            let mut ids = q.ids(c)?;
            let more = ids.len() > n;
            ids.truncate(n);
            Ok((ids, more))
        })
        .await?;
    // Only `id`: some clients (Readrops) parse exactly one key per itemRef.
    let refs: Vec<Value> = ids.iter().map(|id| json!({"id": short_id(*id)})).collect();
    let mut v = json!({"itemRefs": refs});
    if refs.is_empty() && p.get("client") == Some("newsplus") {
        v = json!({"itemRefs": [{"id": "0"}]});
    }
    if more && let Some(last) = ids.last() {
        v["continuation"] = json!(short_id(*last));
    }
    Ok(json_resp(&v))
}

async fn items_contents(app: &AppRef, headers: &HeaderMap, p: &Params) -> anyhow::Result<Response> {
    let base = request_base(app, headers).await;
    let mut seen = HashSet::new();
    let ids: Vec<i64> = p.all("i").iter().filter_map(|s| parse_item_id(s)).filter(|id| seen.insert(*id)).take(10_000).collect();
    let asc = p.get("r") == Some("o");
    let items = app
        .db
        .call(move |c| {
            let mut items = store::load_items(c, &ids, true)?;
            items.sort_by_key(|it| if asc { it.id } else { -it.id });
            Ok(items)
        })
        .await?;
    let v = json!({
        "id": READING_LIST,
        "updated": util::now(),
        "items": items.iter().map(|it| item_json(it, &base)).collect::<Vec<_>>(),
    });
    Ok(json_resp(&v))
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

async fn edit_tag(app: &AppRef, p: &Params) -> anyhow::Result<Response> {
    let ids: Vec<i64> = p.all("i").iter().filter_map(|s| parse_item_id(s)).collect();
    let adds: Vec<Stream> = p.all("a").iter().map(|s| parse_stream(s)).collect();
    let removes: Vec<Stream> = p.all("r").iter().map(|s| parse_stream(s)).collect();
    let raw_adds: Vec<String> = p.all("a").iter().map(|s| s.to_string()).collect();
    if ids.is_empty() {
        return Ok(ok());
    }
    app.db
        .write(move |tx| {
            // Removals first so that an add wins over a conflicting remove.
            for s in &removes {
                match s {
                    Stream::Read => {
                        store::set_read(tx, &ids, false)?;
                    }
                    Stream::Starred => {
                        store::set_starred(tx, &ids, false)?;
                    }
                    Stream::Label(name) => {
                        if let Some(l) = store::find_label(tx, name)? {
                            store::set_label(tx, &ids, l, false)?;
                        }
                    }
                    _ => {}
                }
            }
            for (s, raw) in adds.iter().zip(raw_adds.iter()) {
                match s {
                    Stream::Read => {
                        store::set_read(tx, &ids, true)?;
                    }
                    Stream::Starred => {
                        store::set_starred(tx, &ids, true)?;
                    }
                    Stream::Label(name) if !name.trim().is_empty() => {
                        let l = store::ensure_label(tx, name)?;
                        store::set_label(tx, &ids, l, true)?;
                    }
                    Stream::Ignored if raw.ends_with("/kept-unread") => {
                        store::set_read(tx, &ids, false)?;
                    }
                    _ => {}
                }
            }
            Ok(())
        })
        .await?;
    Ok(ok())
}

async fn mark_all_as_read(app: &AppRef, p: &Params) -> anyhow::Result<Response> {
    let Some(s) = p.last("s") else { return Ok(bad_request()) };
    let stream = parse_stream(s);
    let ts_us = ts_to_us(p.last("ts"));
    app.db
        .write(move |tx| {
            if let Some(mut q) = stream_query(tx, &stream)? {
                q.read = Some(false);
                q.max_created_us = ts_us;
                q.mark_read(tx)?;
            }
            Ok(())
        })
        .await?;
    Ok(ok())
}

async fn rename_tag(app: &AppRef, p: &Params) -> anyhow::Result<Response> {
    let (Some(Stream::Label(old)), Some(Stream::Label(new))) = (p.last("s").map(parse_stream), p.last("dest").map(parse_stream)) else {
        return Ok(bad_request());
    };
    let new = new.trim().to_string();
    if new.is_empty() {
        return Ok(bad_request());
    }
    let found = app
        .db
        .write(move |tx| {
            if let Some(cid) = store::find_category(tx, &old)? {
                tx.execute("UPDATE categories SET name = ? WHERE id = ?", params![new, cid])?;
                return Ok(true);
            }
            if old.eq_ignore_ascii_case(UNCATEGORIZED) {
                let cid = fetcher::ensure_category(tx, &new)?;
                tx.execute("UPDATE feeds SET category_id = ? WHERE category_id IS NULL", [cid])?;
                return Ok(true);
            }
            if let Some(lid) = store::find_label(tx, &old)? {
                tx.execute("UPDATE labels SET name = ? WHERE id = ?", params![new, lid])?;
                return Ok(true);
            }
            Ok(false)
        })
        .await?;
    Ok(if found { ok() } else { bad_request() })
}

async fn disable_tag(app: &AppRef, p: &Params) -> anyhow::Result<Response> {
    let names: Vec<String> = p
        .all("s")
        .iter()
        .filter_map(|s| match parse_stream(s) {
            Stream::Label(n) => Some(n),
            _ => None,
        })
        .collect();
    app.db
        .write(move |tx| {
            for n in names {
                if let Some(cid) = store::find_category(tx, &n)? {
                    tx.execute("UPDATE feeds SET category_id = NULL WHERE category_id = ?", [cid])?;
                    tx.execute("DELETE FROM categories WHERE id = ?", [cid])?;
                } else if let Some(lid) = store::find_label(tx, &n)? {
                    tx.execute("DELETE FROM labels WHERE id = ?", [lid])?;
                }
            }
            Ok(())
        })
        .await?;
    Ok(ok())
}

fn feed_target(s: &str) -> Option<Stream> {
    match parse_stream(s) {
        x @ (Stream::Feed(_) | Stream::FeedUrl(_)) => Some(x),
        _ if s.starts_with("http://") || s.starts_with("https://") => Some(Stream::FeedUrl(s.to_string())),
        _ => None,
    }
}

async fn subscription_edit(app: &AppRef, p: &Params) -> anyhow::Result<Response> {
    let Some(ac) = p.last("ac") else { return Ok(bad_request()) };
    let streams: Vec<&str> = p.all("s");
    if streams.is_empty() {
        return Ok(bad_request());
    }
    let titles: Vec<&str> = if p.all("t").is_empty() { p.all("title") } else { p.all("t") };
    let adds: Vec<String> = p.all("a").iter().filter_map(|a| if let Stream::Label(n) = parse_stream(a) { Some(n) } else { None }).collect();
    let removes: Vec<String> = p.all("r").iter().filter_map(|a| if let Stream::Label(n) = parse_stream(a) { Some(n) } else { None }).collect();
    for (idx, s) in streams.iter().enumerate() {
        let Some(target) = feed_target(s) else { continue };
        let title = titles.get(idx).map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
        match ac {
            "subscribe" => {
                let Stream::FeedUrl(url) = target else { continue };
                let cat_name = adds.first().cloned().filter(|n| !n.eq_ignore_ascii_case(UNCATEGORIZED));
                let cid = match cat_name {
                    Some(n) => app.db.write(move |tx| fetcher::ensure_category(tx, &n)).await?,
                    None => None,
                };
                if let Err(e) = fetcher::add_feed(app, &url, title.as_deref(), cid, None, true).await {
                    tracing::warn!("subscribe {url}: {e:#}");
                    return Ok(bad_request());
                }
            }
            "unsubscribe" => {
                if let Some(id) = resolve_feed(app, target).await? {
                    fetcher::delete_feed(app, id).await?;
                }
            }
            "edit" => {
                let Some(id) = resolve_feed(app, target).await? else { return Ok(bad_request()) };
                let (adds, removes, title) = (adds.clone(), removes.clone(), title.clone());
                app.db
                    .write(move |tx| {
                        let current: Option<String> = tx
                            .query_row("SELECT c.name FROM feeds f LEFT JOIN categories c ON c.id = f.category_id WHERE f.id = ?", [id], |r| r.get(0))?;
                        // Removals first, then additions (NetNewsWire sends r=old&a=new).
                        for r in &removes {
                            let cur = current.clone().unwrap_or_else(|| UNCATEGORIZED.to_string());
                            if cur.eq_ignore_ascii_case(r) && adds.is_empty() {
                                store::set_feed_category(tx, id, None)?;
                            }
                        }
                        if let Some(a) = adds.first() {
                            let cid = if a.eq_ignore_ascii_case(UNCATEGORIZED) { None } else { fetcher::ensure_category(tx, a)? };
                            store::set_feed_category(tx, id, cid)?;
                        }
                        if let Some(t) = &title {
                            store::rename_feed(tx, id, t)?;
                        }
                        Ok(())
                    })
                    .await?;
            }
            _ => return Ok(bad_request()),
        }
    }
    Ok(ok())
}

async fn resolve_feed(app: &AppRef, target: Stream) -> anyhow::Result<Option<i64>> {
    app.db
        .call(move |c| {
            Ok(match target {
                Stream::Feed(id) => c.query_row("SELECT id FROM feeds WHERE id = ?", [id], |r| r.get(0)).optional()?,
                Stream::FeedUrl(u) => c.query_row("SELECT id FROM feeds WHERE url = ?", [u], |r| r.get(0)).optional()?,
                _ => None,
            })
        })
        .await
}

async fn quickadd(app: &AppRef, p: &Params) -> anyhow::Result<Response> {
    let Some(q) = p.last("quickadd") else { return Ok(bad_request()) };
    let url = q.trim().trim_start_matches("feed/").to_string();
    if url::Url::parse(&url).is_err() {
        return Ok(json_resp(&json!({"numResults": 0, "error": format!("`{url}` is not a valid URL")})));
    }
    let u2 = url.clone();
    let existing: Option<i64> = app.db.call(move |c| Ok(c.query_row("SELECT id FROM feeds WHERE url = ?", [u2], |r| r.get(0)).optional()?)).await?;
    if existing.is_some() {
        return Ok(json_resp(&json!({"numResults": 0, "error": format!("Already subscribed! {url}")})));
    }
    match fetcher::add_feed(app, &url, None, None, None, true).await {
        Ok((id, created)) => {
            let (title, feed_url): (String, String) =
                app.db.call(move |c| Ok(c.query_row("SELECT title, url FROM feeds WHERE id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?)))?)).await?;
            if !created {
                return Ok(json_resp(&json!({"numResults": 0, "error": format!("Already subscribed! {feed_url}")})));
            }
            Ok(json_resp(&json!({"numResults": 1, "query": feed_url, "streamId": format!("feed/{id}"), "streamName": title})))
        }
        Err(e) => Ok(json_resp(&json!({"numResults": 0, "error": format!("Feed not added! {url} ({e})")}))),
    }
}

/// Shared handler state type alias for router construction.
pub type Shared = Arc<App>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_roundtrip() {
        assert_eq!(short_id(123), "1000000000000123");
        assert_eq!(long_id(123), "tag:google.com,2005:reader/item/00038d7ea4c6807b");
        for raw in ["1000000000000123", "tag:google.com,2005:reader/item/00038d7ea4c6807b", "00038d7ea4c6807b", "tag:google.com,2005:reader/item/38d7ea4c6807b", "123"] {
            assert_eq!(parse_item_id(raw), Some(123), "{raw}");
        }
        assert_eq!(parse_item_id(""), None);
        assert_eq!(parse_item_id("garbage!"), None);
        assert_eq!(long_id(1).len(), LONG_PREFIX.len() + 16);
    }

    #[test]
    fn streams() {
        assert_eq!(parse_stream("user/-/state/com.google/reading-list"), Stream::ReadingList);
        assert_eq!(parse_stream("user/1234/state/com.google/read"), Stream::Read);
        assert_eq!(parse_stream("user/-/label/Tech News"), Stream::Label("Tech News".into()));
        assert_eq!(parse_stream("feed/12"), Stream::Feed(12));
        assert_eq!(parse_stream("feed/feed/12"), Stream::Feed(12));
        assert_eq!(parse_stream("feed/https://x.com/rss"), Stream::FeedUrl("https://x.com/rss".into()));
        assert_eq!(parse_stream("user/-/state/com.google/like"), Stream::Ignored);
        assert_eq!(parse_stream("user/-/state/org.freshrss/main"), Stream::ReadingList);
    }

    #[test]
    fn paths() {
        assert_eq!(api_path("/api/greader.php/reader/api/0/token"), "/reader/api/0/token");
        assert_eq!(api_path("/api/greader.php/api/greader.php/accounts/ClientLogin/"), "/accounts/ClientLogin");
        assert_eq!(api_path("/tt-rss/plugins.local/freshapi/api/greader.php/reader/api/0/user-info"), "/reader/api/0/user-info");
        assert_eq!(api_path("/reader/api/0//stream/items/ids"), "/reader/api/0/stream/items/ids");
        assert_eq!(api_path("/api/greader.php"), "");
        assert_eq!(api_path("/api/greader.php/"), "");
    }

    #[test]
    fn multipart_login_fields() {
        let body = "--XyZ\r\nContent-Disposition: form-data; name=\"Email\"\r\n\r\nadmin\r\n--XyZ\r\nContent-Disposition: form-data; name=\"Passwd\"\r\n\r\ns3cret\r\n--XyZ--\r\n";
        let f = multipart_fields("multipart/form-data; boundary=XyZ", body.as_bytes());
        assert_eq!(f, vec![("Email".to_string(), "admin".to_string()), ("Passwd".to_string(), "s3cret".to_string())]);
    }

    #[test]
    fn ts_units() {
        assert_eq!(ts_to_us(Some("1792483200")), Some(1_792_483_200_000_000));
        assert_eq!(ts_to_us(Some("1792483200123")), Some(1_792_483_200_123_000));
        assert_eq!(ts_to_us(Some("1792483200123456")), Some(1_792_483_200_123_456));
        assert_eq!(ts_to_us(Some("1792483200123456789")), Some(1_792_483_200_123_456));
        assert_eq!(ts_to_us(Some("0")), None);
        assert_eq!(parse_secs(Some("1792483200123")), Some(1792483200));
    }
}

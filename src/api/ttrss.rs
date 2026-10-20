//! Tiny Tiny RSS JSON API (+ News+ `getCompactHeadlines`).
//!
//! `POST /api/`, `/api/index.php` (and under `/tt-rss`). Article ids are the internal item
//! ids (small integers, fit in 32 bits as several clients require).

use std::net::SocketAddr;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};

use super::{cap_content, client_ip, request_base};
use crate::app::{App, AppRef};
use crate::auth;
use crate::feeds::fetcher;
use crate::store::{self, ItemQuery, ItemRow, Order};
use crate::util::{self, html::expand_base, text};

pub const API_LEVEL: i64 = 25;
const SESSION_TTL_SECS: u64 = 30 * 86400;
const LABEL_BASE: i64 = -1025;
const FRESH_HOURS: i64 = 24;
const RECENT_HOURS: i64 = 24;

// ---------------------------------------------------------------------------
// Parameter coercion (TT-RSS semantics)
// ---------------------------------------------------------------------------

struct Req {
    v: Map<String, Value>,
}

impl Req {
    fn raw(&self, k: &str) -> Option<&Value> {
        self.v.get(k).filter(|v| !v.is_null())
    }
    fn str(&self, k: &str) -> String {
        match self.raw(k) {
            Some(Value::String(s)) => s.trim().to_string(),
            Some(Value::Number(n)) => n.to_string(),
            Some(Value::Bool(b)) => if *b { "1".into() } else { String::new() },
            _ => String::new(),
        }
    }
    fn bool(&self, k: &str) -> bool {
        match self.raw(k) {
            None => false,
            Some(Value::Bool(b)) => *b,
            Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
            Some(Value::String(s)) => {
                let s = s.trim();
                !(s.is_empty() || s == "0" || s == "f" || s == "false")
            }
            Some(Value::Array(a)) => !a.is_empty(),
            Some(_) => true,
        }
    }
    fn int(&self, k: &str, default: i64) -> i64 {
        match self.raw(k) {
            None => default,
            Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).unwrap_or(default),
            Some(Value::Bool(b)) => *b as i64,
            Some(Value::String(s)) => parse_int_prefix(s).unwrap_or(0),
            _ => 0,
        }
    }
    /// Integer list from a comma string or JSON array. None = invalid.
    fn int_list(&self, k: &str) -> Option<Vec<i64>> {
        let items: Vec<String> = match self.raw(k)? {
            Value::Array(a) => a
                .iter()
                .map(|x| match x {
                    Value::Number(n) => n.to_string(),
                    Value::String(s) => s.clone(),
                    _ => "x".into(),
                })
                .collect(),
            Value::Number(n) => vec![n.to_string()],
            Value::String(s) => s.split(',').map(|x| x.to_string()).collect(),
            _ => return None,
        };
        let mut out = Vec::new();
        for i in items {
            let t = i.trim();
            if t.is_empty() {
                continue;
            }
            out.push(t.parse::<i64>().ok()?);
        }
        if out.is_empty() { None } else { Some(out) }
    }
}

fn parse_int_prefix(s: &str) -> Option<i64> {
    let s = s.trim();
    let mut end = 0;
    for (i, c) in s.char_indices() {
        if c.is_ascii_digit() || (i == 0 && (c == '-' || c == '+')) {
            end = i + c.len_utf8();
        } else {
            break;
        }
    }
    s[..end].parse().ok()
}

// ---------------------------------------------------------------------------
// Envelope
// ---------------------------------------------------------------------------

fn envelope(seq: Value, status: i64, content: Value) -> Response {
    let mut body = Map::new();
    body.insert("seq".into(), seq);
    body.insert("status".into(), json!(status));
    body.insert("content".into(), content);
    let bytes = serde_json::to_vec(&Value::Object(body)).unwrap_or_default();
    let len = bytes.len();
    let mut r = Response::new(axum::body::Body::from(bytes));
    *r.status_mut() = StatusCode::OK;
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    h.insert("Api-Content-Length", HeaderValue::from(len));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

fn err(seq: Value, code: &str) -> Response {
    envelope(seq, 1, json!({"error": code}))
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn handle(
    State(app): State<AppRef>,
    uri: Uri,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    body: Bytes,
) -> Response {
    // JSON body (Content-Type ignored); lenient fallback to form/query parameters.
    let mut map: Map<String, Value> = match serde_json::from_slice::<Value>(&body) {
        Ok(Value::Object(m)) => m,
        _ => {
            let mut m = Map::new();
            for (k, v) in super::Params::parse(uri.query(), Some(&body)).0 {
                m.insert(k, Value::String(v));
            }
            m
        }
    };
    if map.is_empty()
        && let Some(q) = uri.query()
    {
        for (k, v) in super::Params::parse(Some(q), None).0 {
            map.insert(k, Value::String(v));
        }
    }
    let req = Req { v: map };
    let op = req.str("op").to_ascii_lowercase();
    let seq_val = json!(req.int("seq", 0));
    match dispatch(&app, &req, &op, seq_val.clone(), &headers, peer).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("ttrss {op}: {e:#}");
            err(seq_val, "E_OPERATION_FAILED")
        }
    }
}

async fn logged_in(app: &App, req: &Req) -> anyhow::Result<bool> {
    let sid = req.str("sid");
    if sid.is_empty() {
        return Ok(false);
    }
    auth::check_session(app, &["ttrss"], &sid).await
}

async fn dispatch(app: &AppRef, req: &Req, op: &str, seq: Value, headers: &HeaderMap, peer: SocketAddr) -> anyhow::Result<Response> {
    match op {
        "login" => return login(app, req, seq, headers, peer).await,
        "isloggedin" => return Ok(envelope(seq, 0, json!({"status": logged_in(app, req).await?}))),
        _ => {}
    }
    if !logged_in(app, req).await? {
        return Ok(err(Value::Null, "NOT_LOGGED_IN"));
    }
    let base = request_base(app, headers).await;
    let ok = |c: Value| envelope(seq.clone(), 0, c);
    Ok(match op {
        "logout" => {
            auth::delete_session(app, &req.str("sid")).await?;
            ok(json!({"status": "OK"}))
        }
        "getapilevel" => ok(json!({"level": API_LEVEL})),
        "getversion" => ok(json!({"version": format!("26.10-rillrss-{}", env!("CARGO_PKG_VERSION"))})),
        "getconfig" => ok(config(app).await?),
        "getunread" => {
            let (fid, is_cat) = (req.raw("feed_id").cloned().unwrap_or(Value::Null), req.bool("is_cat"));
            let n = app.db.call(move |c| global_or_counter(c, &fid, is_cat)).await?;
            ok(json!({"unread": n}))
        }
        "getcounters" => ok(app.db.call(counters).await?),
        "getfeeds" => {
            let (cat, unread_only, limit, offset, nested) =
                (req.int("cat_id", 0), req.bool("unread_only"), req.int("limit", 0), req.int("offset", 0), req.bool("include_nested"));
            let base2 = base.clone();
            ok(app.db.call(move |c| get_feeds(c, cat, unread_only, limit, offset, nested, &base2)).await?)
        }
        "getcategories" => {
            let (unread_only, include_empty) = (req.bool("unread_only"), req.bool("include_empty"));
            ok(app.db.call(move |c| get_categories(c, unread_only, include_empty)).await?)
        }
        "getheadlines" => {
            let Some(fid) = req.raw("feed_id").cloned().filter(|v| !(v.is_string() && v.as_str().unwrap_or("").trim().is_empty())) else {
                return Ok(err(seq, "INCORRECT_USAGE"));
            };
            let p = HeadlineParams::from_req(req, fid);
            let base2 = base.clone();
            ok(app.db.call(move |c| get_headlines(c, &p, &base2)).await?)
        }
        "getcompactheadlines" => {
            let Some(fid) = req.raw("feed_id").cloned().filter(|v| !(v.is_string() && v.as_str().unwrap_or("").trim().is_empty())) else {
                return Ok(err(seq, "INCORRECT_USAGE"));
            };
            let mut p = HeadlineParams::from_req(req, fid);
            p.compact = true;
            ok(app.db.call(move |c| compact_headlines(c, &p)).await?)
        }
        "updatearticle" => {
            let Some(ids) = req.int_list("article_ids") else { return Ok(err(seq, "INCORRECT_USAGE")) };
            let (mode, field, data) = (req.int("mode", 0), req.int("field", 0), req.str("data"));
            if !(0..=2).contains(&mode) || !(0..=4).contains(&field) {
                return Ok(err(seq, "INCORRECT_USAGE"));
            }
            let n = app.db.write(move |tx| update_article(tx, &ids, mode, field, &data)).await?;
            ok(json!({"status": "OK", "updated": n}))
        }
        "getarticle" => {
            let Some(ids) = req.int_list("article_id") else { return Ok(ok(json!([]))) };
            let base2 = base.clone();
            ok(app.db.call(move |c| get_article(c, &ids, &base2)).await?)
        }
        "updatefeed" => {
            let id = req.int("feed_id", 0);
            if id > 0 {
                fetcher::request_refresh(app, Some(id)).await?;
            }
            ok(json!({"status": "OK"}))
        }
        "getpref" => ok(json!({"value": pref(&req.str("pref_name"))})),
        "catchupfeed" => {
            let (fid, is_cat, mode) = (req.raw("feed_id").cloned().unwrap_or(Value::Null), req.bool("is_cat"), req.str("mode"));
            app.db.write(move |tx| catchup(tx, &fid, is_cat, &mode)).await?;
            ok(json!({"status": "OK"}))
        }
        "getlabels" => {
            let aid = req.int("article_id", -1);
            ok(app.db.call(move |c| get_labels(c, aid)).await?)
        }
        "setarticlelabel" => {
            let Some(ids) = req.int_list("article_ids") else { return Ok(err(seq, "INCORRECT_USAGE")) };
            let (label_fid, assign) = (req.int("label_id", 0), req.bool("assign"));
            let label_id = label_fid.abs() - 1025;
            let n = app
                .db
                .write(move |tx| {
                    let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM labels WHERE id = ?)", [label_id], |r| r.get(0))?;
                    if !exists {
                        return Ok(0);
                    }
                    store::set_label(tx, &ids, label_id, assign)?;
                    Ok(ids.len())
                })
                .await?;
            ok(json!({"status": "OK", "updated": n}))
        }
        "sharetopublished" => err(seq, "E_OPERATION_FAILED"),
        "subscribetofeed" => subscribe(app, req, seq).await?,
        "unsubscribefeed" => {
            let id = req.int("feed_id", 0);
            let exists: bool = app.db.call(move |c| Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM feeds WHERE id = ?)", [id], |r| r.get(0))?)).await?;
            if !exists {
                return Ok(err(seq, "E_OPERATION_FAILED"));
            }
            fetcher::delete_feed(app, id).await?;
            ok(json!({"status": "OK"}))
        }
        "getfeedtree" => {
            let include_empty = req.bool("include_empty");
            let base2 = base.clone();
            ok(app.db.call(move |c| feed_tree(c, include_empty, &base2)).await?)
        }
        "getfeedicon" => {
            let id = req.int("id", 0);
            let icon: Option<(String, Vec<u8>)> = app
                .db
                .call(move |c| Ok(c.query_row("SELECT mime, data FROM feed_icons WHERE feed_id = ? AND length(data) > 0", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?))
                .await?;
            match icon {
                Some((mime, data)) => {
                    let mut r = (StatusCode::OK, data).into_response();
                    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_str(&mime).unwrap_or(HeaderValue::from_static("image/x-icon")));
                    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
                    r
                }
                None => err(seq, "E_NOT_FOUND"),
            }
        }
        "getfeedupdateintervals" => ok(json!({"update_intervals": {
            "0": "Default interval", "-1": "Disable updates", "15": "15 minutes", "30": "30 minutes", "60": "Hourly",
            "240": "4 hours", "720": "12 hours", "1440": "Daily", "10080": "Weekly"}})),
        _ => envelope(seq, 1, json!({"error": "UNKNOWN_METHOD", "method": op})),
    })
}

async fn login(app: &AppRef, req: &Req, seq: Value, headers: &HeaderMap, peer: SocketAddr) -> anyhow::Result<Response> {
    let user = req.str("user");
    let password = match req.raw("password") {
        Some(Value::String(s)) => s.clone(),
        _ => req.str("password"),
    };
    let ip = client_ip(app, headers, Some(peer));
    // Very old clients sent the password base64-encoded.
    let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, password.trim())
        .ok()
        .and_then(|d| String::from_utf8(d).ok())
        .filter(|p| *p != password);
    let mut candidates = vec![password.as_str()];
    candidates.extend(decoded.as_deref());
    let Some(cred) = auth::api_login(app, &user, &candidates, ip).await? else {
        return Ok(err(seq, "LOGIN_ERROR"));
    };
    let old = req.str("sid");
    if !old.is_empty() {
        let _ = auth::delete_session(app, &old).await;
    }
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let ttl = Some(std::time::Duration::from_secs(SESSION_TTL_SECS));
    let Some(sid) = auth::create_session(app, "ttrss", ttl, ua, ip, auth::Cred::Api, cred).await? else {
        return Ok(err(seq, "LOGIN_ERROR"));
    };
    let mut content = Map::new();
    content.insert("session_id".into(), json!(sid));
    content.insert("config".into(), config(app).await?);
    content.insert("api_level".into(), json!(API_LEVEL));
    Ok(envelope(seq, 0, Value::Object(content)))
}

async fn config(app: &AppRef) -> anyhow::Result<Value> {
    let n: i64 = app.db.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM feeds", [], |r| r.get(0))?)).await?;
    Ok(json!({
        "icons_dir": "feed-icons",
        "icons_url": "feed-icons",
        "daemon_is_running": true,
        "custom_sort_types": [],
        "num_feeds": n,
    }))
}

fn pref(name: &str) -> Value {
    match name {
        "ENABLE_FEED_CATS" | "SHOW_CONTENT_PREVIEW" | "COMBINED_DISPLAY_MODE" | "CDM_EXPANDED" | "HIDE_READ_SHOWS_SPECIAL" | "CONFIRM_FEED_CATCHUP"
        | "PURGE_UNREAD_ARTICLES" | "ENABLE_API_ACCESS" => json!(true),
        "CDM_AUTO_CATCHUP" | "HIDE_READ_FEEDS" | "FEEDS_SORT_BY_UNREAD" | "REVERSE_HEADLINES" | "STRIP_IMAGES" | "VFEED_GROUP_BY_FEED"
        | "ON_CATCHUP_SHOW_NEXT_FEED" | "DIGEST_ENABLE" | "DIGEST_CATCHUP" | "HEADLINES_NO_DISTINCT" | "WIDESCREEN_MODE" | "CDM_ENABLE_GRID"
        | "_DEFAULT_INCLUDE_CHILDREN" => json!(false),
        "FRESH_ARTICLE_MAX_AGE" => json!(FRESH_HOURS),
        "RECENTLY_READ_MAX_AGE" => json!(RECENT_HOURS),
        "PURGE_OLD_DAYS" => json!(30),
        "DEFAULT_UPDATE_INTERVAL" => json!(30),
        "_DEFAULT_VIEW_LIMIT" => json!(30),
        "DIGEST_MIN_SCORE" => json!(0),
        "_DEFAULT_VIEW_MODE" => json!("adaptive"),
        "_DEFAULT_VIEW_ORDER_BY" => json!("default"),
        "SHORT_DATE_FORMAT" => json!("M d, G:i"),
        "LONG_DATE_FORMAT" => json!("D, M d Y - G:i"),
        "USER_TIMEZONE" => json!("Automatic"),
        "USER_LANGUAGE" | "DEFAULT_SEARCH_LANGUAGE" | "USER_CSS_THEME" | "USER_STYLESHEET" | "SSL_CERT_SERIAL" | "_ENABLED_PLUGINS" => json!(""),
        "DIGEST_PREFERRED_TIME" => json!("00:00"),
        "BLACKLISTED_TAGS" => json!("main, generic, misc, uncategorized, blog, blogroll, general, news"),
        _ => Value::Null,
    }
}

// ---------------------------------------------------------------------------
// Feed / category selection
// ---------------------------------------------------------------------------

/// Feed id meaning.
#[derive(Debug, Clone, PartialEq)]
enum Target {
    Feed(i64),
    Category(i64), // 0 = uncategorized
    Starred,
    Published,
    Fresh,
    All,
    RecentlyRead,
    Archived,
    AnyLabel,
    Label(i64),
    Tag(String),
    Nothing,
}

fn target_of(fid: &Value, is_cat: bool) -> Target {
    let num = match fid {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        Value::Bool(b) => Some(*b as i64),
        _ => None,
    };
    let Some(id) = num else {
        return match fid {
            Value::String(s) if !s.trim().is_empty() => Target::Tag(s.trim().to_string()),
            _ => Target::Nothing,
        };
    };
    if id < LABEL_BASE {
        return Target::Label(-id + LABEL_BASE); // |id| - 1025
    }
    if is_cat {
        return match id {
            i if i >= 0 => Target::Category(i),
            -1 => Target::Starred,
            -2 => Target::AnyLabel,
            -3 => Target::Fresh,
            -4 => Target::All,
            -6 => Target::RecentlyRead,
            _ => Target::Nothing,
        };
    }
    match id {
        i if i > 0 => Target::Feed(i),
        0 => Target::Archived,
        -1 => Target::Starred,
        -2 => Target::Published,
        -3 => Target::Fresh,
        -4 => Target::All,
        -6 => Target::RecentlyRead,
        _ => Target::Nothing,
    }
}

/// Base query for a target. None = empty.
fn target_query(conn: &Connection, t: &Target) -> anyhow::Result<Option<ItemQuery>> {
    let now = util::now();
    let mut q = ItemQuery::default();
    match t {
        Target::Feed(id) => q.feed_ids = Some(vec![*id]),
        Target::Category(0) => q.category = Some(None),
        Target::Category(c) => q.category = Some(Some(*c)),
        Target::Starred => q.starred = Some(true),
        Target::Published => q.published_flag = Some(true),
        Target::Fresh => {
            q.read = Some(false);
            q.min_created_us = Some((now - FRESH_HOURS * 3600) * 1_000_000);
        }
        Target::All => {}
        Target::RecentlyRead => {
            q.read = Some(true);
            q.read_since = Some(now - RECENT_HOURS * 3600);
        }
        Target::Label(l) => q.label_id = Some(*l),
        Target::AnyLabel => {
            let ids: Vec<i64> = {
                let mut st = conn.prepare("SELECT DISTINCT item_id FROM item_labels")?;
                st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?
            };
            q.id_in = Some(ids);
        }
        Target::Archived | Target::Tag(_) | Target::Nothing => return Ok(None),
    }
    Ok(Some(q))
}

fn counter(conn: &Connection, t: &Target) -> anyhow::Result<i64> {
    match t {
        Target::RecentlyRead => Ok(0),
        _ => match target_query(conn, t)? {
            Some(mut q) => {
                if q.read == Some(true) {
                    return Ok(0);
                }
                q.read = Some(false);
                Ok(q.count(conn)?)
            }
            None => Ok(0),
        },
    }
}

fn global_or_counter(conn: &Connection, fid: &Value, is_cat: bool) -> anyhow::Result<i64> {
    let falsy = match fid {
        Value::Null => true,
        Value::Number(n) => n.as_i64() == Some(0),
        Value::String(s) => s.trim().is_empty() || s.trim() == "0",
        Value::Bool(b) => !b,
        _ => false,
    };
    if falsy {
        return ItemQuery { read: Some(false), ..Default::default() }.count(conn);
    }
    counter(conn, &target_of(fid, is_cat))
}

// ---------------------------------------------------------------------------
// Counters, feeds, categories, tree
// ---------------------------------------------------------------------------

fn short_date(ts: Option<i64>) -> String {
    match ts.and_then(|t| chrono::DateTime::from_timestamp(t, 0)) {
        None => "Never".into(),
        Some(d) => {
            let now = chrono::Utc::now();
            if d.date_naive() == now.date_naive() {
                d.format("%H:%M").to_string()
            } else {
                d.format("%b %d, %H:%M").to_string()
            }
        }
    }
}

fn counters(c: &mut Connection) -> anyhow::Result<Value> {
    let feeds = store::list_feeds(c)?;
    let total_unread = ItemQuery { read: Some(false), ..Default::default() }.count(c)?;
    let mut out: Vec<Value> = vec![
        json!({"id": "global-unread", "counter": total_unread}),
        json!({"id": "subscribed-feeds", "counter": feeds.len()}),
        json!({"id": 0, "counter": 0, "auxcounter": 0}),
    ];
    let starred_total = ItemQuery { starred: Some(true), ..Default::default() }.count(c)?;
    let starred_unread = ItemQuery { starred: Some(true), read: Some(false), ..Default::default() }.count(c)?;
    out.push(json!({"id": -1, "counter": starred_unread, "auxcounter": starred_total, "markedcounter": starred_total}));
    let pub_total = ItemQuery { published_flag: Some(true), ..Default::default() }.count(c)?;
    let pub_unread = ItemQuery { published_flag: Some(true), read: Some(false), ..Default::default() }.count(c)?;
    out.push(json!({"id": -2, "counter": pub_unread, "auxcounter": pub_total, "publishedcounter": pub_total}));
    out.push(json!({"id": -3, "counter": counter(c, &Target::Fresh)?, "auxcounter": 0}));
    out.push(json!({"id": -4, "counter": total_unread, "auxcounter": 0}));
    for l in store::list_labels(c)? {
        let starred = ItemQuery { label_id: Some(l.id), starred: Some(true), ..Default::default() }.count(c)?;
        out.push(json!({"id": LABEL_BASE - l.id, "counter": l.unread, "auxcounter": l.total, "markedcounter": starred, "publishedcounter": 0, "description": l.name}));
    }
    let mut starred_by_feed: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    {
        let mut st = c.prepare("SELECT feed_id, COUNT(*) FROM items WHERE is_starred = 1 GROUP BY feed_id")?;
        for r in st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))? {
            let (f, n) = r?;
            starred_by_feed.insert(f, n);
        }
    }
    let mut cat_unread: std::collections::HashMap<i64, (i64, i64)> = std::collections::HashMap::new();
    for f in &feeds {
        if f.newest_created_us > 0 {
            out.push(json!({
                "id": f.id, "title": f.title, "error": f.last_error.clone().unwrap_or_default(), "updated": short_date(f.last_success_at),
                "counter": f.unread, "markedcounter": starred_by_feed.get(&f.id).copied().unwrap_or(0), "publishedcounter": 0,
                "ts": 0, "has_img": f.has_icon as i64,
            }));
        }
        let e = cat_unread.entry(f.category_id.unwrap_or(0)).or_insert((0, 0));
        e.0 += f.unread;
        e.1 += starred_by_feed.get(&f.id).copied().unwrap_or(0);
    }
    out.push(json!({"id": -2, "kind": "cat", "counter": counter(c, &Target::AnyLabel)?}));
    let mut cats: Vec<i64> = store::list_categories(c)?.iter().map(|x| x.id).collect();
    cats.push(0);
    for id in cats {
        let (u, m) = cat_unread.get(&id).copied().unwrap_or((0, 0));
        out.push(json!({"id": id, "kind": "cat", "markedcounter": m, "publishedcounter": 0, "counter": u}));
    }
    Ok(Value::Array(out))
}

fn feed_entry(f: &store::FeedInfo, interval: i64) -> Value {
    json!({
        "feed_url": f.url,
        "title": f.title,
        "id": f.id,
        "unread": f.unread,
        "has_icon": f.has_icon,
        "cat_id": f.category_id.unwrap_or(0),
        "last_updated": f.last_success_at.unwrap_or(0),
        "order_id": f.sort_order,
        "last_error": f.last_error.clone().unwrap_or_default(),
        "update_interval": interval,
    })
}

fn special_title(id: i64) -> &'static str {
    match id {
        -1 => "Starred articles",
        -2 => "Published articles",
        -3 => "Fresh articles",
        -4 => "All articles",
        -6 => "Recently read",
        0 => "Archived articles",
        _ => "",
    }
}

#[allow(clippy::too_many_arguments)]
fn get_feeds(c: &mut Connection, cat_id: i64, unread_only: bool, limit: i64, offset: i64, nested: bool, _base: &str) -> anyhow::Result<Value> {
    let mut out: Vec<Value> = Vec::new();
    if cat_id == -4 || cat_id == -2 {
        for l in store::list_labels(c)? {
            if unread_only && l.unread == 0 {
                continue;
            }
            out.push(json!({"id": LABEL_BASE - l.id, "title": l.name, "unread": l.unread, "cat_id": -2}));
        }
    }
    if cat_id == -4 || cat_id == -1 {
        for id in [-1i64, -2, -3, -4, -6, 0] {
            let t = target_of(&json!(id), false);
            let unread = counter(c, &t)?;
            if unread_only && unread == 0 {
                continue;
            }
            out.push(json!({"id": id, "title": special_title(id), "unread": unread, "cat_id": -1}));
        }
    }
    if nested && cat_id > 0 {
        // No nested categories in this server.
    }
    let intervals: std::collections::HashMap<i64, i64> = {
        let mut st = c.prepare("SELECT id, COALESCE(fetch_interval, 0) FROM feeds")?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?
    };
    let mut feeds: Vec<store::FeedInfo> = store::list_feeds(c)?
        .into_iter()
        .filter(|f| match cat_id {
            -3 | -4 => true,
            -1 | -2 => false,
            0 => f.category_id.is_none(),
            id => f.category_id == Some(id),
        })
        .filter(|f| !unread_only || f.unread > 0)
        .collect();
    feeds.sort_by(|a, b| a.sort_order.cmp(&b.sort_order).then(a.title.to_lowercase().cmp(&b.title.to_lowercase())));
    let start = offset.max(0) as usize;
    let iter = feeds.iter().skip(start);
    let take = if limit > 0 { limit as usize } else { usize::MAX };
    for f in iter.take(take) {
        out.push(feed_entry(f, intervals.get(&f.id).copied().unwrap_or(0)));
    }
    Ok(Value::Array(out))
}

fn get_categories(c: &mut Connection, unread_only: bool, include_empty: bool) -> anyhow::Result<Value> {
    let feeds = store::list_feeds(c)?;
    let mut out = Vec::new();
    for cat in store::list_categories(c)? {
        let n_feeds = feeds.iter().filter(|f| f.category_id == Some(cat.id)).count();
        let unread: i64 = feeds.iter().filter(|f| f.category_id == Some(cat.id)).map(|f| f.unread).sum();
        if (n_feeds == 0 && !include_empty) || (unread_only && unread == 0) {
            continue;
        }
        out.push(json!({"id": cat.id, "title": cat.name, "unread": unread, "order_id": cat.sort_order}));
    }
    let labels = store::list_labels(c)?;
    if !labels.is_empty() || include_empty {
        let u = counter(c, &Target::AnyLabel)?;
        if !(unread_only && u == 0) {
            out.push(json!({"id": -2, "title": "Labels", "unread": u}));
        }
    }
    if !unread_only {
        out.push(json!({"id": -1, "title": "Special", "unread": 0}));
    }
    let uncat: Vec<&store::FeedInfo> = feeds.iter().filter(|f| f.category_id.is_none()).collect();
    if !uncat.is_empty() || include_empty {
        let u: i64 = uncat.iter().map(|f| f.unread).sum();
        if !(unread_only && u == 0) {
            out.push(json!({"id": 0, "title": "Uncategorized", "unread": u}));
        }
    }
    Ok(Value::Array(out))
}

fn feed_tree(c: &mut Connection, include_empty: bool, base: &str) -> anyhow::Result<Value> {
    let feeds = store::list_feeds(c)?;
    let mut items: Vec<Value> = Vec::new();
    let icons = [(-4i64, "inbox"), (-3, "whatshot"), (-1, "star"), (-2, "rss_feed"), (0, "archive"), (-6, "restore")];
    let mut special = Vec::new();
    for (id, icon) in icons {
        let unread = counter(c, &target_of(&json!(id), false))?;
        special.push(json!({"id": format!("FEED:{id}"), "name": special_title(id), "unread": unread, "type": "feed", "error": "", "updated": "", "icon": icon, "bare_id": id, "auxcounter": 0}));
    }
    items.push(json!({"id": "CAT:-1", "items": special, "name": "Special", "type": "category", "unread": -1, "bare_id": -1}));
    let labels = store::list_labels(c)?;
    if !labels.is_empty() {
        let l: Vec<Value> = labels
            .iter()
            .map(|l| json!({"id": format!("FEED:{}", LABEL_BASE - l.id), "name": l.name, "unread": l.unread, "type": "feed", "error": "", "updated": "", "icon": "label", "bare_id": LABEL_BASE - l.id, "auxcounter": 0, "fg_color": "", "bg_color": ""}))
            .collect();
        items.push(json!({"id": "CAT:-2", "items": l, "name": "Labels", "type": "category", "unread": -1, "bare_id": -2}));
    }
    let feed_node = |f: &store::FeedInfo, with_type: bool| -> Value {
        let mut v = json!({
            "id": format!("FEED:{}", f.id), "bare_id": f.id, "auxcounter": -1, "name": f.title, "checkbox": false, "unread": -1,
            "error": f.last_error.clone().unwrap_or_default(),
            "icon": if f.has_icon { format!("{base}/public.php?op=feed_icon&id={}", f.id) } else { String::new() },
            "param": "", "updates_disabled": 0,
        });
        if with_type {
            v["type"] = json!("feed");
        }
        v
    };
    for cat in store::list_categories(c)? {
        let fs: Vec<Value> = feeds.iter().filter(|f| f.category_id == Some(cat.id)).map(|f| feed_node(f, false)).collect();
        if fs.is_empty() && !include_empty {
            continue;
        }
        let n = fs.len();
        items.push(json!({"id": format!("CAT:{}", cat.id), "bare_id": cat.id, "auxcounter": -1, "name": cat.name, "items": fs, "checkbox": false, "type": "category", "unread": -1, "child_unread": -1, "param": format!("({n} feeds)")}));
    }
    let uncat: Vec<Value> = feeds.iter().filter(|f| f.category_id.is_none()).map(|f| feed_node(f, true)).collect();
    if !uncat.is_empty() || include_empty {
        let n = uncat.len();
        items.push(json!({"id": "CAT:0", "bare_id": 0, "auxcounter": -1, "name": "Uncategorized", "items": uncat, "type": "category", "checkbox": false, "unread": -1, "child_unread": -1, "param": format!("({n} feeds)")}));
    }
    Ok(json!({"categories": {"identifier": "id", "label": "name", "items": items}}))
}

// ---------------------------------------------------------------------------
// Headlines
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct HeadlineParams {
    feed_id: Value,
    is_cat: bool,
    limit: i64,
    skip: i64,
    show_excerpt: bool,
    excerpt_length: i64,
    show_content: bool,
    view_mode: String,
    include_attachments: bool,
    since_id: i64,
    order_by: String,
    include_header: bool,
    check_first_id: i64,
    search: String,
    compact: bool,
}

impl HeadlineParams {
    fn from_req(r: &Req, feed_id: Value) -> HeadlineParams {
        HeadlineParams {
            feed_id,
            is_cat: r.bool("is_cat"),
            limit: r.int("limit", 0),
            skip: r.int("skip", 0).max(0),
            show_excerpt: r.bool("show_excerpt"),
            excerpt_length: r.int("excerpt_length", 0),
            show_content: r.bool("show_content"),
            view_mode: r.str("view_mode"),
            include_attachments: r.bool("include_attachments"),
            since_id: r.int("since_id", 0),
            order_by: r.str("order_by"),
            include_header: r.bool("include_header"),
            check_first_id: r.int("check_first_id", 0),
            search: r.str("search"),
            compact: false,
        }
    }
}

/// Build the filtered, ordered query for headlines (without limit/skip).
fn headline_query(c: &Connection, p: &HeadlineParams) -> anyhow::Result<Option<(ItemQuery, Target)>> {
    let t = target_of(&p.feed_id, p.is_cat);
    let Some(mut q) = target_query(c, &t)? else { return Ok(None) };
    match p.view_mode.as_str() {
        "unread" if t != Target::RecentlyRead => q.read = Some(false),
        "marked" => q.starred = Some(true),
        "published" => q.published_flag = Some(true),
        "has_note" => q.has_note = true,
        "adaptive" if p.search.is_empty() && t != Target::Starred
            && counter(c, &t)? > 0 => {
                q.read = Some(false);
            }
        _ => {}
    }
    if p.since_id > 0 {
        q.id_gt = Some(p.since_id);
    }
    if !p.search.is_empty() {
        q.search = Some(p.search.clone());
    }
    q.order = match p.order_by.as_str() {
        "feed_dates" => Order::PublishedDesc,
        "date_reverse" => Order::PublishedAsc,
        "title" => Order::TitleAsc,
        _ => Order::IdDesc,
    };
    Ok(Some((q, t)))
}

fn excerpt_of(content: &str, len: i64) -> String {
    let t = text::html_to_text(content).replace('\n', " ");
    let n = if len <= 0 { 100 } else { len as usize };
    if t.chars().count() > n {
        let s: String = t.chars().take(n).collect();
        format!("{s}&hellip;")
    } else {
        t
    }
}

fn first_image(html: &str) -> String {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#"(?i)<img[^>]+src\s*=\s*["']([^"']+)["']"#).unwrap());
    RE.captures(html).map(|c| html_escape::decode_html_entities(&c[1]).into_owned()).unwrap_or_default()
}

fn attachments(it: &ItemRow) -> Vec<Value> {
    let media = crate::feeds::parse::Media::from_json(it.media.as_deref());
    media
        .enclosures
        .iter()
        .enumerate()
        .map(|(i, e)| json!({"id": it.id * 100 + i as i64, "content_url": e.url, "content_type": e.mime, "title": "", "duration": "", "width": 0, "height": 0, "post_id": it.id}))
        .collect()
}

fn labels_json(it: &ItemRow) -> Vec<Value> {
    let mut l: Vec<&(i64, String)> = it.labels.iter().collect();
    l.sort_by_key(|a| a.1.to_lowercase());
    l.iter().map(|(id, name)| json!([LABEL_BASE - id, name, "", ""])).collect()
}

fn headline(it: &ItemRow, p: &HeadlineParams, base: &str) -> Value {
    let mut m = Map::new();
    m.insert("id".into(), json!(it.id));
    m.insert("guid".into(), json!(it.guid));
    m.insert("unread".into(), json!(!it.is_read));
    m.insert("marked".into(), json!(it.is_starred));
    m.insert("published".into(), json!(it.is_published));
    m.insert("updated".into(), json!(it.published_at));
    m.insert("is_updated".into(), json!(false));
    m.insert("title".into(), json!(it.title));
    m.insert("link".into(), json!(it.link));
    m.insert("feed_id".into(), json!(it.feed_id));
    m.insert("tags".into(), json!([""]));
    if p.include_attachments {
        m.insert("attachments".into(), json!(attachments(it)));
    }
    let content = if p.show_content || p.show_excerpt { expand_base(&it.content, base) } else { String::new() };
    if p.show_excerpt {
        m.insert("excerpt".into(), json!(excerpt_of(&content, p.excerpt_length)));
    }
    if p.show_content {
        m.insert("content".into(), json!(cap_content(content.clone(), 1_000_000)));
    }
    m.insert("labels".into(), json!(labels_json(it)));
    m.insert("feed_title".into(), json!(it.feed_title));
    m.insert("comments_count".into(), json!(0));
    m.insert("comments_link".into(), json!(""));
    m.insert("always_display_attachments".into(), json!(false));
    m.insert("author".into(), json!(it.author));
    m.insert("score".into(), json!(0));
    m.insert("note".into(), it.note.as_ref().map(|n| json!(n)).unwrap_or(Value::Null));
    m.insert("lang".into(), json!(it.lang));
    m.insert("site_url".into(), json!(it.site_url));
    if p.show_content {
        let mut img = first_image(&content);
        if img.is_empty() {
            img = crate::feeds::parse::Media::from_json(it.media.as_deref()).thumbnail.unwrap_or_default();
        }
        m.insert("flavor_image".into(), json!(img));
        m.insert("flavor_stream".into(), json!(""));
    }
    Value::Object(m)
}

fn get_headlines(c: &mut Connection, p: &HeadlineParams, base: &str) -> anyhow::Result<Value> {
    let Some((mut q, _t)) = headline_query(c, p)? else {
        return Ok(if p.include_header { json!([{"id": p.feed_id, "first_id": 0, "is_cat": p.is_cat}, []]) } else { json!([]) });
    };
    // first_id: newest item (current order) entered within the last hour, ignoring view_mode.
    let mut first_id = 0i64;
    if p.search.is_empty() && p.order_by != "date_reverse" {
        let t = target_of(&p.feed_id, p.is_cat);
        if let Some(mut fq) = target_query(c, &t)? {
            fq.min_created_us = Some((util::now() - 3600) * 1_000_000);
            if p.since_id > 0 {
                fq.id_gt = Some(p.since_id);
            }
            fq.order = q.order;
            fq.limit = Some(1);
            first_id = fq.ids(c)?.first().copied().unwrap_or(0);
        }
    }
    let mut header = json!({"id": p.feed_id, "first_id": first_id, "is_cat": p.is_cat});
    if p.skip > 0 && p.check_first_id > 0 && first_id > 0 && first_id != p.check_first_id {
        header["first_id_changed"] = json!(true);
        return Ok(if p.include_header { json!([header, []]) } else { json!([]) });
    }
    if p.limit >= 0 {
        q.limit = Some(if p.limit == 0 || p.limit >= 200 { 200 } else { p.limit as usize });
        q.offset = p.skip as usize;
    }
    let ids = q.ids(c)?;
    let items = store::load_items_ordered(c, &ids, p.show_content || p.show_excerpt)?;
    let list: Vec<Value> = items.iter().map(|it| headline(it, p, base)).collect();
    Ok(if p.include_header { json!([header, list]) } else { Value::Array(list) })
}

fn compact_headlines(c: &mut Connection, p: &HeadlineParams) -> anyhow::Result<Value> {
    let mut p = p.clone();
    let unread_first = p.view_mode == "unread_first";
    if unread_first {
        p.view_mode.clear();
    }
    let Some((mut q, _)) = headline_query(c, &p)? else { return Ok(json!([])) };
    if p.limit > 0 {
        q.limit = Some(p.limit as usize);
        q.offset = p.skip as usize;
    }
    let mut ids = q.ids(c)?;
    if unread_first {
        let unread: std::collections::HashSet<i64> = ItemQuery { read: Some(false), id_in: Some(ids.clone()), ..Default::default() }.ids(c)?.into_iter().collect();
        ids.sort_by_key(|id| !unread.contains(id));
    }
    Ok(Value::Array(ids.into_iter().map(|id| json!({"id": id})).collect()))
}

fn get_article(c: &mut Connection, ids: &[i64], base: &str) -> anyhow::Result<Value> {
    let items = store::load_items_ordered(c, ids, true)?;
    let out: Vec<Value> = items
        .iter()
        .map(|it| {
            let mut m = Map::new();
            m.insert("id".into(), json!(it.id));
            m.insert("guid".into(), json!(it.guid));
            m.insert("title".into(), json!(it.title));
            m.insert("link".into(), json!(it.link));
            m.insert("labels".into(), json!(labels_json(it)));
            m.insert("unread".into(), json!(!it.is_read));
            m.insert("marked".into(), json!(it.is_starred));
            m.insert("published".into(), json!(it.is_published));
            m.insert("comments".into(), json!(""));
            m.insert("author".into(), json!(it.author));
            m.insert("updated".into(), json!(it.published_at - it.published_at.rem_euclid(60)));
            m.insert("feed_id".into(), json!(it.feed_id));
            m.insert("attachments".into(), json!(attachments(it)));
            m.insert("score".into(), json!(0));
            m.insert("feed_title".into(), json!(it.feed_title));
            m.insert("note".into(), it.note.as_ref().map(|n| json!(n)).unwrap_or(Value::Null));
            m.insert("lang".into(), json!(it.lang));
            m.insert("content".into(), json!(cap_content(expand_base(&it.content, base), 1_000_000)));
            Value::Object(m)
        })
        .collect();
    Ok(Value::Array(out))
}

fn update_article(tx: &rusqlite::Transaction, ids: &[i64], mode: i64, field: i64, data: &str) -> anyhow::Result<usize> {
    let existing: Vec<i64> = ItemQuery { id_in: Some(ids.to_vec()), include_hidden: true, ..Default::default() }.ids(tx)?;
    let items = store::load_items(tx, &existing, false)?;
    match field {
        0..=2 => {
            for it in &items {
                let cur = match field {
                    0 => it.is_starred,
                    1 => it.is_published,
                    _ => !it.is_read, // field 2 is "unread"
                };
                let v = match mode {
                    0 => false,
                    1 => true,
                    _ => !cur,
                };
                match field {
                    0 => store::set_starred(tx, &[it.id], v)?,
                    1 => store::set_published_flag(tx, &[it.id], v)?,
                    _ => store::set_read(tx, &[it.id], !v)?,
                };
            }
        }
        3 => {
            store::set_note(tx, &existing, data)?;
        }
        4 => {} // scores are not supported; accept silently
        _ => {}
    }
    Ok(existing.len())
}

fn catchup(tx: &rusqlite::Transaction, fid: &Value, is_cat: bool, mode: &str) -> anyhow::Result<()> {
    let t = target_of(fid, is_cat);
    if matches!(t, Target::RecentlyRead | Target::Archived | Target::Nothing) {
        return Ok(());
    }
    if is_cat && matches!(t, Target::Starred | Target::Fresh | Target::All) {
        return Ok(());
    }
    let Some(mut q) = target_query(tx, &t)? else { return Ok(()) };
    let now = util::now();
    let older = match mode {
        "1day" => Some(86400),
        "1week" => Some(7 * 86400),
        "2week" => Some(14 * 86400),
        _ => None,
    };
    if let Some(secs) = older {
        q.max_created_us = Some((now - secs) * 1_000_000);
    }
    q.read = Some(false);
    q.mark_read(tx)?;
    Ok(())
}

fn get_labels(c: &mut Connection, article_id: i64) -> anyhow::Result<Value> {
    let checked: std::collections::HashSet<i64> = if article_id > 0 {
        let mut st = c.prepare("SELECT label_id FROM item_labels WHERE item_id = ?")?;
        st.query_map([article_id], |r| r.get(0))?.collect::<Result<_, _>>()?
    } else {
        Default::default()
    };
    let out: Vec<Value> = store::list_labels(c)?
        .iter()
        .map(|l| json!({"id": LABEL_BASE - l.id, "caption": l.name, "fg_color": "", "bg_color": "", "checked": checked.contains(&l.id)}))
        .collect();
    Ok(Value::Array(out))
}

async fn subscribe(app: &AppRef, req: &Req, seq: Value) -> anyhow::Result<Response> {
    let url = req.str("feed_url");
    if url.is_empty() {
        return Ok(err(seq, "INCORRECT_USAGE"));
    }
    if url::Url::parse(&url).is_err() {
        return Ok(envelope(seq, 0, json!({"status": {"code": 2}})));
    }
    let cat = req.int("category_id", 0);
    let cat = if cat > 0 {
        app.db.call(move |c| Ok(c.query_row("SELECT id FROM categories WHERE id = ?", [cat], |r| r.get::<_, i64>(0)).optional()?)).await?
    } else {
        None
    };
    let u2 = url.clone();
    if let Some(id) = app.db.call(move |c| Ok(c.query_row("SELECT id FROM feeds WHERE url = ?", [u2], |r| r.get::<_, i64>(0)).optional()?)).await? {
        return Ok(envelope(seq, 0, json!({"status": {"code": 0, "feed_id": id}})));
    }
    let interval = req.int("update_interval", 0);
    match fetcher::add_feed(app, &url, None, cat, None, true).await {
        Ok((id, created)) => {
            if interval > 0 {
                app.db.call(move |c| Ok(c.execute("UPDATE feeds SET fetch_interval = ? WHERE id = ?", params![interval, id])?)).await?;
            } else if interval < 0 {
                app.db.call(move |c| Ok(c.execute("UPDATE feeds SET enabled = 0 WHERE id = ?", params![id])?)).await?;
            }
            Ok(envelope(seq, 0, json!({"status": {"code": if created { 1 } else { 0 }, "feed_id": id}})))
        }
        Err(e) => {
            let msg = format!("{e:#}");
            let code = if msg.contains("no feed found") { 3 } else if msg.contains("HTTP") || msg.contains("request") { 5 } else { 6 };
            Ok(envelope(seq, 0, json!({"status": {"code": code, "message": msg}})))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(v: Value) -> Req {
        Req { v: v.as_object().unwrap().clone() }
    }

    #[test]
    fn coercion() {
        let r = req(json!({"a": "true", "b": "0", "c": "f", "d": 1, "e": "no", "f": "12abc", "g": "1, 2,3", "h": [4, "5"], "i": "1,x"}));
        assert!(r.bool("a"));
        assert!(!r.bool("b"));
        assert!(!r.bool("c"));
        assert!(r.bool("d"));
        assert!(r.bool("e"));
        assert!(!r.bool("missing"));
        assert_eq!(r.int("f", 0), 12);
        assert_eq!(r.int_list("g"), Some(vec![1, 2, 3]));
        assert_eq!(r.int_list("h"), Some(vec![4, 5]));
        assert_eq!(r.int_list("i"), None);
    }

    #[test]
    fn targets() {
        assert_eq!(target_of(&json!(-4), false), Target::All);
        assert_eq!(target_of(&json!("-1"), false), Target::Starred);
        assert_eq!(target_of(&json!(-1026), false), Target::Label(1));
        assert_eq!(target_of(&json!(3), true), Target::Category(3));
        assert_eq!(target_of(&json!(0), true), Target::Category(0));
        assert_eq!(target_of(&json!(0), false), Target::Archived);
        assert_eq!(target_of(&json!(12), false), Target::Feed(12));
        assert_eq!(target_of(&json!("tech"), false), Target::Tag("tech".into()));
        assert_eq!(target_of(&json!(-5), false), Target::Nothing);
    }

    #[test]
    fn envelope_key_order() {
        let r = envelope(json!(3), 0, json!({"x": 1}));
        let b = r.into_body();
        let bytes = futures::executor::block_on(axum::body::to_bytes(b, 1024)).unwrap();
        assert_eq!(std::str::from_utf8(&bytes).unwrap(), r#"{"seq":3,"status":0,"content":{"x":1}}"#);
    }
}

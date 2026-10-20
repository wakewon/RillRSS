//! Web admin: configuration, feed management, task status and diagnostics.
//! Server-rendered (askama), cookie sessions, CSRF tokens on every form.

use std::collections::HashMap;
use std::net::SocketAddr;

use askama::Template;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Multipart, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use rusqlite::{OptionalExtension, params};
use serde_json::Value;

use crate::api::{client_ip, request_base};
use crate::app::{AppRef, EventRef, Level};
use crate::auth;
use crate::content::{convert, extract, sanitize};
use crate::feeds::fetcher;
use crate::settings::{FeedOptions, Settings};
use crate::util::{self, fmt_relative, fmt_ts, html::expand_base, human_bytes, truncate_chars};

const COOKIE: &str = "rill_session";
const SESSION_DAYS: u64 = 30;
const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn router() -> Router<AppRef> {
    Router::new()
        .route("/", get(|| async { Redirect::to("/admin") }))
        .route("/admin/static/admin.css", get(css))
        .route("/admin/login", get(login_page).post(login_post))
        .route("/admin/logout", post(logout))
        .route("/admin", get(dashboard))
        .route("/admin/", get(dashboard))
        .route("/admin/feeds", get(feeds_page).post(feed_add))
        .route("/admin/feeds/refresh-all", post(feeds_refresh_all))
        .route("/admin/feeds/{id}", get(feed_page).post(feed_save))
        .route("/admin/feeds/{id}/refresh", post(feed_refresh))
        .route("/admin/feeds/{id}/reprocess", post(feed_reprocess))
        .route("/admin/feeds/{id}/markread", post(feed_markread))
        .route("/admin/feeds/{id}/delete", post(feed_delete))
        .route("/admin/categories", get(categories_page).post(category_add))
        .route("/admin/categories/{id}/rename", post(category_rename))
        .route("/admin/categories/{id}/delete", post(category_delete))
        .route("/admin/items", get(items_page))
        .route("/admin/items/{id}", get(item_page))
        .route("/admin/stories/{id}/reprocess", post(story_reprocess))
        .route("/admin/tasks", get(tasks_page))
        .route("/admin/tasks/retry", post(tasks_retry))
        .route("/admin/maintenance/cleanup", post(run_cleanup))
        .route("/admin/events", get(events_page))
        .route("/admin/settings", get(settings_page).post(settings_save))
        .route("/admin/tools", get(tools_page))
        .route("/admin/tools/extract", post(tool_extract))
        .route("/admin/tools/translate", post(tool_translate))
        .route("/admin/tools/feed", post(tool_feed))
        .route("/admin/opml", get(opml_page))
        .route("/admin/opml/import", post(opml_import))
        .route("/admin/opml/export", get(opml_export))
        .route("/admin/account", get(account_page))
        .route("/admin/account/password", post(account_password))
        .route("/admin/account/api-password", post(account_api_password))
        .route("/admin/account/revoke", post(account_revoke))
}

// ---------------------------------------------------------------------------
// Plumbing
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Base {
    pub csrf: String,
    pub nav: &'static str,
    pub version: &'static str,
    pub flash: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct Field {
    pub name: String,
    pub label: String,
    pub kind: &'static str,
    pub value: String,
    pub checked: bool,
    pub options: Vec<(String, String)>,
    pub help: String,
}

pub struct Section {
    pub id: String,
    pub title: String,
    pub help: String,
    pub fields: Vec<Field>,
}

struct Session {
    token: String,
    csrf: String,
}

fn csrf_for(token: &str) -> String {
    blake3::hash(format!("csrf:{token}").as_bytes()).to_hex()[..32].to_string()
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    headers.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';')).find_map(|c| {
        let (k, v) = c.trim().split_once('=')?;
        if k == COOKIE { Some(v.to_string()) } else { None }
    })
}

async fn session(app: &AppRef, headers: &HeaderMap) -> Option<Session> {
    let token = cookie_token(headers)?;
    if auth::check_session(app, &["web"], &token).await.ok()? {
        Some(Session { csrf: csrf_for(&token), token })
    } else {
        None
    }
}

fn to_login() -> Response {
    Redirect::to("/admin/login").into_response()
}

fn render<T: Template>(t: T) -> Response {
    match t.render() {
        Ok(html) => {
            let mut r = Html(html).into_response();
            let h = r.headers_mut();
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
            h.insert(header::REFERRER_POLICY, HeaderValue::from_static("same-origin"));
            h.insert(
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static("default-src 'self'; img-src * data:; style-src 'self' 'unsafe-inline'; script-src 'unsafe-inline'; frame-ancestors 'none'; form-action 'self'"),
            );
            r
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("template error: {e}")).into_response(),
    }
}

fn server_error(e: anyhow::Error) -> Response {
    tracing::error!("admin: {e:#}");
    (StatusCode::INTERNAL_SERVER_ERROR, format!("error: {e:#}")).into_response()
}

#[derive(serde::Deserialize, Default)]
struct FlashQuery {
    msg: Option<String>,
    err: Option<String>,
}

fn base(s: &Session, nav: &'static str, q: &FlashQuery) -> Base {
    Base { csrf: s.csrf.clone(), nav, version: VERSION, flash: q.msg.clone(), error: q.err.clone() }
}

fn redirect_msg(path: &str, msg: &str) -> Response {
    let enc = percent_encoding::utf8_percent_encode(msg, percent_encoding::NON_ALPHANUMERIC).to_string();
    let sep = if path.contains('?') { '&' } else { '?' };
    Redirect::to(&format!("{path}{sep}msg={enc}")).into_response()
}

fn redirect_err(path: &str, msg: &str) -> Response {
    let enc = percent_encoding::utf8_percent_encode(msg, percent_encoding::NON_ALPHANUMERIC).to_string();
    let sep = if path.contains('?') { '&' } else { '?' };
    Redirect::to(&format!("{path}{sep}err={enc}")).into_response()
}

/// Parsed urlencoded form (keeps repeated keys).
struct Form(Vec<(String, String)>);

impl Form {
    fn parse(body: &[u8]) -> Form {
        Form(url::form_urlencoded::parse(body).map(|(k, v)| (k.into_owned(), v.into_owned())).collect())
    }
    fn get(&self, k: &str) -> Option<&str> {
        self.0.iter().rev().find(|(key, _)| key == k).map(|(_, v)| v.as_str())
    }
    fn all(&self, k: &str) -> Vec<&str> {
        self.0.iter().filter(|(key, _)| key == k).map(|(_, v)| v.as_str()).collect()
    }
}

/// Authenticate + CSRF check for POST handlers.
async fn post_guard(app: &AppRef, headers: &HeaderMap, form: &Form) -> Result<Session, Response> {
    let Some(s) = session(app, headers).await else { return Err(to_login()) };
    if form.get("csrf").is_none_or(|c| !auth::ct_eq(c, &s.csrf)) {
        return Err((StatusCode::FORBIDDEN, "invalid CSRF token, please reload the page").into_response());
    }
    Ok(s)
}

macro_rules! guard_get {
    ($app:expr, $headers:expr) => {
        match session(&$app, &$headers).await {
            Some(s) => s,
            None => return to_login(),
        }
    };
}

macro_rules! guard_post {
    ($app:expr, $headers:expr, $form:expr) => {
        match post_guard(&$app, &$headers, &$form).await {
            Ok(s) => s,
            Err(r) => return r,
        }
    };
}

async fn css() -> Response {
    let mut r = include_str!("../static/admin.css").into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/css; charset=utf-8"));
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=86400"));
    r
}

// ---------------------------------------------------------------------------
// Login
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "login.html")]
struct LoginT {
    username: String,
    error: Option<String>,
}

async fn login_page(State(app): State<AppRef>, headers: HeaderMap) -> Response {
    if session(&app, &headers).await.is_some() {
        return Redirect::to("/admin").into_response();
    }
    render(LoginT { username: app.cfg.username.clone(), error: None })
}

async fn login_post(State(app): State<AppRef>, headers: HeaderMap, ConnectInfo(peer): ConnectInfo<SocketAddr>, body: Bytes) -> Response {
    let form = Form::parse(&body);
    let user = form.get("username").unwrap_or("").to_string();
    let pass = form.get("password").unwrap_or("");
    let ip = client_ip(&app, &headers, Some(peer));
    let fail = |msg: &str| render(LoginT { username: user.clone(), error: Some(msg.to_string()) });
    if auth::login_blocked(&app, ip) {
        return fail("尝试次数过多，请 15 分钟后再试。");
    }
    let ok = auth::username_matches(&app, &user) && auth::verify_password(&app, pass).await.unwrap_or(false);
    auth::record_login(&app, ip, ok);
    if !ok {
        app.event(Level::Warn, "system", EventRef::default(), format!("failed admin login from {ip:?}")).await;
        return fail("用户名或密码错误。");
    }
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let token = match auth::create_session(&app, "web", Some(std::time::Duration::from_secs(SESSION_DAYS * 86400)), ua, ip).await {
        Ok(t) => t,
        Err(e) => return server_error(e),
    };
    let secure = headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()) == Some("https")
        || app.configured_base_url().await.is_some_and(|b| b.starts_with("https://"));
    let cookie = format!(
        "{COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}{}",
        SESSION_DAYS * 86400,
        if secure { "; Secure" } else { "" }
    );
    let mut r = Redirect::to("/admin").into_response();
    r.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).unwrap());
    r
}

async fn logout(State(app): State<AppRef>, headers: HeaderMap, body: Bytes) -> Response {
    let form = Form::parse(&body);
    let s = guard_post!(app, headers, form);
    let _ = auth::delete_session(&app, &s.token).await;
    let mut r = Redirect::to("/admin/login").into_response();
    r.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_str(&format!("{COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")).unwrap());
    r
}

// ---------------------------------------------------------------------------
// Dashboard
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Stats {
    feeds: i64,
    feeds_error: i64,
    items: i64,
    unread: i64,
    starred: i64,
    pending: i64,
    failed: i64,
    updates: i64,
    images: i64,
    images_size: String,
    db_size: String,
    uptime: String,
    base: String,
    username: String,
    api_password: bool,
    tr_configured: bool,
    tr_model: String,
    tr_circuit: Option<String>,
    tr_requests: u64,
    tr_failures: u64,
    tr_cache_hits: u64,
    tr_tokens: u64,
    tr_in_flight: u32,
    tr_last_error: Option<String>,
}

pub struct EventRow {
    pub time: String,
    pub level: String,
    pub level_class: &'static str,
    pub kind: String,
    pub feed_id: Option<i64>,
    pub feed_title: String,
    pub item_id: Option<i64>,
    pub message: String,
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardT {
    base: Base,
    s: Stats,
    stages: Vec<(String, i64, i64)>,
    events: Vec<EventRow>,
}

fn load_events(c: &rusqlite::Connection, feed: Option<i64>, level: &str, kind: &str, limit: i64) -> anyhow::Result<Vec<EventRow>> {
    let mut sql = String::from("SELECT e.ts, e.level, e.kind, e.feed_id, COALESCE(f.title, ''), e.item_id, e.message FROM events e LEFT JOIN feeds f ON f.id = e.feed_id WHERE 1");
    let mut p: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(f) = feed {
        sql.push_str(" AND e.feed_id = ?");
        p.push(f.into());
    }
    match level {
        "warn" => sql.push_str(" AND e.level IN ('warn', 'error')"),
        "error" => sql.push_str(" AND e.level = 'error'"),
        _ => {}
    }
    if !kind.trim().is_empty() {
        sql.push_str(" AND e.kind = ?");
        p.push(kind.trim().to_string().into());
    }
    sql.push_str(" ORDER BY e.id DESC LIMIT ?");
    p.push(limit.into());
    let mut st = c.prepare(&sql)?;
    let rows = st.query_map(rusqlite::params_from_iter(p), |r| {
        let level: String = r.get(1)?;
        Ok(EventRow {
            time: fmt_ts(r.get(0)?).replace(" UTC", ""),
            level_class: match level.as_str() {
                "error" => "err",
                "warn" => "warn",
                _ => "muted",
            },
            level,
            kind: r.get(2)?,
            feed_id: r.get(3)?,
            feed_title: r.get(4)?,
            item_id: r.get(5)?,
            message: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

async fn dashboard(State(app): State<AppRef>, headers: HeaderMap, Query(q): Query<FlashQuery>) -> Response {
    let s = guard_get!(app, headers);
    let base_url = request_base(&app, &headers).await;
    let data = app
        .db
        .call(|c| {
            let one = |sql: &str| -> rusqlite::Result<i64> { c.query_row(sql, [], |r| r.get(0)) };
            let mut st = Stats {
                feeds: one("SELECT COUNT(*) FROM feeds")?,
                feeds_error: one("SELECT COUNT(*) FROM feeds WHERE fetch_error_count > 0")?,
                items: one("SELECT COUNT(*) FROM items")?,
                unread: one("SELECT COUNT(*) FROM items WHERE is_read = 0 AND hidden = 0")?,
                starred: one("SELECT COUNT(*) FROM items WHERE is_starred = 1")?,
                pending: one("SELECT COUNT(*) FROM revisions WHERE state = 'pending'")?,
                failed: one("SELECT COUNT(*) FROM revisions WHERE state = 'failed'")?,
                updates: one("SELECT COUNT(*) FROM items WHERE version > 1 AND created_at > (strftime('%s','now') - 86400) * 1000000")?,
                images: one("SELECT COUNT(*) FROM images")?,
                ..Default::default()
            };
            st.images_size = human_bytes(one("SELECT COALESCE(SUM(size), 0) FROM images")? as u64);
            let mut stages = Vec::new();
            for stage in crate::pipeline::STAGES {
                let due: i64 = c.query_row(
                    "SELECT COUNT(*) FROM revisions WHERE state = 'pending' AND stage = ? AND next_attempt_at <= strftime('%s','now')",
                    [stage],
                    |r| r.get(0),
                )?;
                let waiting: i64 = c.query_row(
                    "SELECT COUNT(*) FROM revisions WHERE state = 'pending' AND stage = ? AND next_attempt_at > strftime('%s','now')",
                    [stage],
                    |r| r.get(0),
                )?;
                stages.push((stage_name(stage).to_string(), due, waiting));
            }
            let events = load_events(c, None, "", "", 15)?;
            Ok((st, stages, events))
        })
        .await;
    let (mut st, stages, events) = match data {
        Ok(d) => d,
        Err(e) => return server_error(e),
    };
    let settings = app.settings.get().await;
    st.db_size = human_bytes(crate::db::Db::file_size(&app.cfg.db_path()));
    st.uptime = {
        let secs = util::now() - app.started_at;
        if secs < 3600 { format!("{} 分钟", secs / 60) } else if secs < 86400 { format!("{} 小时", secs / 3600) } else { format!("{} 天", secs / 86400) }
    };
    st.base = base_url;
    st.username = app.cfg.username.clone();
    st.api_password = auth::has_api_password(&app).await.unwrap_or(false);
    st.tr_configured = settings.translation.is_configured();
    st.tr_model = settings.translation.model.clone();
    st.tr_circuit = app.translator.circuit_open_for().map(|d| format!("{} 秒", d.as_secs()));
    let ts = app.translator.stats();
    st.tr_requests = ts.requests;
    st.tr_failures = ts.failures;
    st.tr_cache_hits = ts.cache_hits;
    st.tr_tokens = ts.prompt_tokens + ts.completion_tokens;
    st.tr_in_flight = app.translator.in_flight();
    st.tr_last_error = ts.last_error.map(|e| format!("{} ({})", truncate_chars(&e, 200), fmt_relative(ts.last_error_at)));
    render(DashboardT { base: base(&s, "dash", &q), s: st, stages, events })
}

fn stage_name(s: &str) -> &'static str {
    match s {
        "extract" => "全文提取",
        "transform" => "清理/繁简",
        "translate" => "翻译",
        "images" => "图片缓存",
        "publish" => "发布",
        _ => "完成",
    }
}

// ---------------------------------------------------------------------------
// Feeds
// ---------------------------------------------------------------------------

struct FeedListRow {
    id: i64,
    title: String,
    url: String,
    category: String,
    enabled: bool,
    badges: Vec<&'static str>,
    unread: i64,
    items: i64,
    last_fetch: String,
    error: String,
    error_count: i64,
}

struct CatOption {
    id: i64,
    name: String,
}

#[derive(Template)]
#[template(path = "feeds.html")]
struct FeedsT {
    base: Base,
    feeds: Vec<FeedListRow>,
    categories: Vec<CatOption>,
}

pub fn option_badges(o: &FeedOptions) -> Vec<&'static str> {
    let mut b = Vec::new();
    match o.fulltext {
        crate::settings::FulltextMode::Off => {}
        crate::settings::FulltextMode::Readability => b.push("全文"),
        crate::settings::FulltextMode::Selector => b.push("全文·选择器"),
        crate::settings::FulltextMode::External => b.push("全文·外部"),
    }
    if o.opencc_enabled() {
        b.push("繁简");
    }
    if o.translate {
        b.push("翻译");
    }
    if o.cache_images {
        b.push("图片");
    }
    if o.updates == crate::settings::UpdateMode::Page {
        b.push("页面追踪");
    }
    b
}

async fn feeds_page(State(app): State<AppRef>, headers: HeaderMap, Query(q): Query<FlashQuery>) -> Response {
    let s = guard_get!(app, headers);
    let res = app
        .db
        .call(|c| {
            let unread = crate::store::unread_by_feed(c)?;
            let totals: HashMap<i64, i64> = {
                let mut st = c.prepare("SELECT feed_id, COUNT(*) FROM items GROUP BY feed_id")?;
                st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?
            };
            let mut st = c.prepare(
                "SELECT f.id, f.title, f.url, COALESCE(c.name, ''), f.enabled, f.options, f.last_fetch_at, COALESCE(f.last_error, ''), f.fetch_error_count
                 FROM feeds f LEFT JOIN categories c ON c.id = f.category_id ORDER BY c.name COLLATE NOCASE, f.title COLLATE NOCASE",
            )?;
            let rows = st
                .query_map([], |r| {
                    let id: i64 = r.get(0)?;
                    let opts = FeedOptions::from_json(&r.get::<_, String>(5)?);
                    Ok(FeedListRow {
                        id,
                        title: r.get(1)?,
                        url: r.get(2)?,
                        category: r.get(3)?,
                        enabled: r.get::<_, i64>(4)? != 0,
                        badges: option_badges(&opts),
                        unread: 0,
                        items: 0,
                        last_fetch: fmt_relative(r.get(6)?),
                        error: r.get(7)?,
                        error_count: r.get(8)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let rows = rows
                .into_iter()
                .map(|mut f| {
                    f.unread = unread.get(&f.id).copied().unwrap_or(0);
                    f.items = totals.get(&f.id).copied().unwrap_or(0);
                    f
                })
                .collect();
            let cats = crate::store::list_categories(c)?.into_iter().map(|c| CatOption { id: c.id, name: c.name }).collect();
            Ok((rows, cats))
        })
        .await;
    match res {
        Ok((feeds, categories)) => render(FeedsT { base: base(&s, "feeds", &q), feeds, categories }),
        Err(e) => server_error(e),
    }
}

async fn feed_add(State(app): State<AppRef>, headers: HeaderMap, body: Bytes) -> Response {
    let form = Form::parse(&body);
    let _s = guard_post!(app, headers, form);
    let url = form.get("url").unwrap_or("").trim().to_string();
    let category = form.get("category").and_then(|c| c.parse::<i64>().ok());
    let mut opts = FeedOptions::default();
    match form.get("preset").unwrap_or("plain") {
        "fulltext" => opts.fulltext = crate::settings::FulltextMode::Readability,
        "fulltext_tr" => {
            opts.fulltext = crate::settings::FulltextMode::Readability;
            opts.translate = true;
        }
        "tr" => opts.translate = true,
        "t2s" => {
            opts.opencc = "t2s".into();
            opts.cache_images = true;
        }
        _ => {}
    }
    match fetcher::add_feed(&app, &url, None, category, Some(opts), true).await {
        Ok((id, true)) => redirect_msg(&format!("/admin/feeds/{id}"), "已添加订阅，正在抓取。"),
        Ok((id, false)) => redirect_msg(&format!("/admin/feeds/{id}"), "该订阅已存在。"),
        Err(e) => redirect_err("/admin/feeds", &format!("添加失败：{e:#}")),
    }
}

async fn feeds_refresh_all(State(app): State<AppRef>, headers: HeaderMap, body: Bytes) -> Response {
    let form = Form::parse(&body);
    let _s = guard_post!(app, headers, form);
    match fetcher::request_refresh(&app, None).await {
        Ok(_) => redirect_msg("/admin/feeds", "已安排刷新所有订阅。"),
        Err(e) => server_error(e),
    }
}

struct FeedDetail {
    id: i64,
    title: String,
    url: String,
    site_url: String,
    unread: i64,
    items: i64,
    stories: i64,
    versions: i64,
    last_fetch: String,
    next_fetch: String,
    last_status: String,
    duration: String,
    error: String,
    error_count: i64,
}

#[derive(Template)]
#[template(path = "feed_edit.html")]
struct FeedT {
    base: Base,
    feed: FeedDetail,
    sections: Vec<Section>,
    events: Vec<EventRow>,
}

fn field(name: &str, label: &str, kind: &'static str, value: impl ToString, help: &str) -> Field {
    Field { name: name.into(), label: label.into(), kind, value: value.to_string(), checked: false, options: vec![], help: help.into() }
}

fn check(name: &str, label: &str, checked: bool, help: &str) -> Field {
    Field { name: name.into(), label: label.into(), kind: "bool", value: String::new(), checked, options: vec![], help: help.into() }
}

fn select(name: &str, label: &str, value: &str, options: &[(&str, &str)], help: &str) -> Field {
    Field {
        name: name.into(),
        label: label.into(),
        kind: "select",
        value: value.into(),
        checked: false,
        options: options.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(),
        help: help.into(),
    }
}

fn feed_sections(f: &fetcher::FeedRow, enabled: bool, category: Option<i64>, cats: &[CatOption], interval: Option<i64>) -> Vec<Section> {
    let o = &f.options;
    let mut cat_opts: Vec<(String, String)> = vec![(String::new(), "（未分类）".into())];
    cat_opts.extend(cats.iter().map(|c| (c.id.to_string(), c.name.clone())));
    let mut opencc: Vec<(&str, &str)> = vec![("", "关闭")];
    opencc.extend(convert::MODES.iter().copied());
    vec![
        Section {
            id: "basic".into(),
            title: "基本".into(),
            help: String::new(),
            fields: vec![
                field("title", "名称", "text", &f.title, "修改后不再随订阅源标题变化"),
                field("url", "订阅地址", "url", &f.url, ""),
                Field {
                    name: "category".into(),
                    label: "分类".into(),
                    kind: "select",
                    value: category.map(|c| c.to_string()).unwrap_or_default(),
                    checked: false,
                    options: cat_opts,
                    help: String::new(),
                },
                field("fetch_interval", "抓取间隔（分钟）", "number", interval.map(|i| i.to_string()).unwrap_or_default(), "留空使用全局默认值"),
                check("enabled", "启用抓取", enabled, ""),
            ],
        },
        Section {
            id: "fulltext".into(),
            title: "全文提取".into(),
            help: "从原文网页提取正文；提取失败或过短时回退到订阅内容。".into(),
            fields: vec![
                select(
                    "opt.fulltext",
                    "方式",
                    match o.fulltext {
                        crate::settings::FulltextMode::Off => "off",
                        crate::settings::FulltextMode::Readability => "readability",
                        crate::settings::FulltextMode::Selector => "selector",
                        crate::settings::FulltextMode::External => "external",
                    },
                    &[("off", "关闭"), ("readability", "内置 Readability（推荐）"), ("selector", "CSS 选择器"), ("external", "外部提取服务")],
                    "",
                ),
                field("opt.selector", "正文 CSS 选择器", "text", &o.selector, "选择器模式使用，如 article .content"),
                field("opt.remove_selector", "移除元素（CSS 选择器）", "text", &o.remove_selector, "对所有内容生效，如 .ad, [aria-hidden=true]"),
                field("opt.user_agent", "User-Agent", "text", &o.user_agent, "留空使用全局设置"),
            ],
        },
        Section {
            id: "convert".into(),
            title: "繁简转换（OpenCC）".into(),
            help: String::new(),
            fields: vec![select("opt.opencc", "转换", &o.opencc, &opencc, "繁体中文新闻一般选 t2s")],
        },
        Section {
            id: "translate".into(),
            title: "AI 翻译".into(),
            help: "需要在设置中配置 OpenAI 兼容接口。已是目标语言的文章会自动跳过。".into(),
            fields: vec![
                check("opt.translate", "启用翻译", o.translate, ""),
                field("opt.target_lang", "目标语言", "text", &o.target_lang, "如 zh-CN、zh-TW、en、ja"),
                select(
                    "opt.translate_mode",
                    "模式",
                    match o.translate_mode {
                        crate::settings::TranslateMode::Replace => "replace",
                        crate::settings::TranslateMode::Bilingual => "bilingual",
                    },
                    &[("replace", "仅译文"), ("bilingual", "双语对照")],
                    "",
                ),
                check("opt.translate_title", "翻译标题", o.translate_title, ""),
            ],
        },
        Section {
            id: "images".into(),
            title: "图片缓存".into(),
            help: String::new(),
            fields: vec![check("opt.cache_images", "下载正文图片到本服务器", o.cache_images, "阅读器从本服务器加载图片")],
        },
        Section {
            id: "updates".into(),
            title: "新闻更新追踪".into(),
            help: "检测到实质性修改时，以新文章 ID 发布新版本，旧版标为已读。".into(),
            fields: vec![
                select(
                    "opt.updates",
                    "追踪方式",
                    match o.updates {
                        crate::settings::UpdateMode::Off => "off",
                        crate::settings::UpdateMode::Feed => "feed",
                        crate::settings::UpdateMode::Page => "page",
                    },
                    &[("off", "关闭"), ("feed", "订阅条目变化（默认）"), ("page", "条目变化 + 定期复查原文页面（需启用全文）")],
                    "",
                ),
                field("opt.retention_days", "保留天数", "number", o.retention_days.map(|d| d.to_string()).unwrap_or_default(), "留空使用全局设置；收藏不会被删除"),
            ],
        },
    ]
}

async fn feed_page(State(app): State<AppRef>, headers: HeaderMap, Path(id): Path<i64>, Query(q): Query<FlashQuery>) -> Response {
    let s = guard_get!(app, headers);
    let res = app
        .db
        .call(move |c| {
            let Some(f) = fetcher::load_feed(c, id)? else { return Ok(None) };
            #[allow(clippy::type_complexity)]
            let (enabled, category, interval, last_fetch, next_fetch, status, dur, err, errc, site): (i64, Option<i64>, Option<i64>, Option<i64>, i64, Option<i64>, Option<i64>, Option<String>, i64, Option<String>) = c
                .query_row(
                    "SELECT enabled, category_id, fetch_interval, last_fetch_at, next_fetch_at, last_status, last_duration_ms, last_error, fetch_error_count, site_url FROM feeds WHERE id = ?",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?)),
                )?;
            let one = |sql: &str| -> rusqlite::Result<i64> { c.query_row(sql, [id], |r| r.get(0)) };
            let detail = FeedDetail {
                id,
                title: f.title.clone(),
                url: f.url.clone(),
                site_url: site.unwrap_or_default(),
                unread: one("SELECT COUNT(*) FROM items WHERE feed_id = ? AND is_read = 0 AND hidden = 0")?,
                items: one("SELECT COUNT(*) FROM items WHERE feed_id = ?")?,
                stories: one("SELECT COUNT(*) FROM stories WHERE feed_id = ?")?,
                versions: one("SELECT COUNT(*) FROM items WHERE feed_id = ? AND version > 1")?,
                last_fetch: fmt_relative(last_fetch),
                next_fetch: if enabled == 0 { "已停用".into() } else { fmt_relative(Some(next_fetch.max(1))) },
                last_status: status.map(|s| s.to_string()).unwrap_or_else(|| "-".into()),
                duration: dur.map(|s| s.to_string()).unwrap_or_else(|| "-".into()),
                error: err.unwrap_or_default(),
                error_count: errc,
            };
            let cats: Vec<CatOption> = crate::store::list_categories(c)?.into_iter().map(|c| CatOption { id: c.id, name: c.name }).collect();
            let sections = feed_sections(&f, enabled != 0, category, &cats, interval);
            let events = load_events(c, Some(id), "", "", 30)?;
            Ok(Some((detail, sections, events)))
        })
        .await;
    match res {
        Ok(Some((feed, sections, events))) => render(FeedT { base: base(&s, "feeds", &q), feed, sections, events }),
        Ok(None) => (StatusCode::NOT_FOUND, "feed not found").into_response(),
        Err(e) => server_error(e),
    }
}

/// Set a value in a JSON tree at a dotted path, coercing to the existing type.
fn set_path(v: &mut Value, path: &str, raw: &str, is_bool: bool) {
    let mut cur = v;
    let parts: Vec<&str> = path.split('.').collect();
    for (i, p) in parts.iter().enumerate() {
        if i + 1 == parts.len() {
            let existing = cur.get(*p).cloned().unwrap_or(Value::Null);
            let new = if is_bool {
                Value::Bool(matches!(raw, "on" | "true" | "1"))
            } else {
                match existing {
                    Value::Number(n) => {
                        let t = raw.trim();
                        if t.is_empty() {
                            Value::Number(n)
                        } else if n.is_f64() || t.contains('.') {
                            t.parse::<f64>().ok().and_then(serde_json::Number::from_f64).map(Value::Number).unwrap_or(Value::Number(n))
                        } else {
                            t.parse::<i64>().map(|x| Value::Number(x.into())).unwrap_or(Value::Number(n))
                        }
                    }
                    Value::Array(_) => Value::Array(
                        raw.split([',', ' '])
                            .filter(|s| !s.trim().is_empty())
                            .filter_map(|s| s.trim().parse::<i64>().ok())
                            .map(|x| Value::Number(x.into()))
                            .collect(),
                    ),
                    Value::Null => {
                        let t = raw.trim();
                        if t.is_empty() { Value::Null } else { t.parse::<i64>().map(|x| Value::Number(x.into())).unwrap_or(Value::String(t.to_string())) }
                    }
                    _ => Value::String(raw.trim().to_string()),
                }
            };
            if let Some(obj) = cur.as_object_mut() {
                obj.insert(p.to_string(), new);
            }
            return;
        }
        match cur.get_mut(*p) {
            Some(next) => cur = next,
            None => return,
        }
    }
}

async fn feed_save(State(app): State<AppRef>, headers: HeaderMap, Path(id): Path<i64>, body: Bytes) -> Response {
    let form = Form::parse(&body);
    let _s = guard_post!(app, headers, form);
    let Ok(Some(feed)) = app.db.call(move |c| fetcher::load_feed(c, id)).await else {
        return (StatusCode::NOT_FOUND, "feed not found").into_response();
    };
    let bools: Vec<String> = form.all("_bool").iter().map(|s| s.to_string()).collect();
    let mut opts = serde_json::to_value(&feed.options).unwrap_or_default();
    for (k, v) in &form.0 {
        if let Some(p) = k.strip_prefix("opt.") {
            if p == "retention_days" {
                let n = v.trim().parse::<u32>().ok();
                opts["retention_days"] = n.map(|x| Value::Number(x.into())).unwrap_or(Value::Null);
                continue;
            }
            set_path(&mut opts, p, v, bools.contains(k));
        }
    }
    for b in &bools {
        if let Some(p) = b.strip_prefix("opt.")
            && form.get(b).is_none()
        {
            set_path(&mut opts, p, "", true);
        }
    }
    let new_opts: FeedOptions = match serde_json::from_value(opts) {
        Ok(o) => o,
        Err(e) => return redirect_err(&format!("/admin/feeds/{id}"), &format!("选项无效：{e}")),
    };
    if new_opts.opencc_enabled() && app.converter.get(&new_opts.opencc).is_err() {
        return redirect_err(&format!("/admin/feeds/{id}"), "未知的 OpenCC 模式");
    }
    let title = form.get("title").unwrap_or("").trim().to_string();
    let url = form.get("url").unwrap_or("").trim().to_string();
    let category = form.get("category").and_then(|c| c.parse::<i64>().ok());
    let interval = form.get("fetch_interval").and_then(|c| c.trim().parse::<i64>().ok()).filter(|i| *i > 0);
    let enabled = form.get("enabled").is_some();
    if url::Url::parse(&url).is_err() {
        return redirect_err(&format!("/admin/feeds/{id}"), "订阅地址无效");
    }
    let title_changed = title != feed.title;
    let res = app
        .db
        .call(move |c| {
            c.execute(
                "UPDATE feeds SET url = ?, category_id = ?, fetch_interval = ?, enabled = ?, options = ?, updated_at = ? WHERE id = ?",
                params![url, category, interval, enabled as i64, new_opts.to_json(), util::now(), id],
            )?;
            if title_changed && !title.is_empty() {
                crate::store::rename_feed(c, id, &title)?;
            }
            Ok(())
        })
        .await;
    match res {
        Ok(_) => redirect_msg(&format!("/admin/feeds/{id}"), "已保存。新设置对之后处理的文章生效；可用「重新处理」更新已有文章。"),
        Err(e) => redirect_err(&format!("/admin/feeds/{id}"), &format!("保存失败：{e:#}")),
    }
}

async fn feed_refresh(State(app): State<AppRef>, headers: HeaderMap, Path(id): Path<i64>, body: Bytes) -> Response {
    let form = Form::parse(&body);
    let _s = guard_post!(app, headers, form);
    match fetcher::fetch_feed(&app, id).await {
        Ok(o) => redirect_msg(
            &format!("/admin/feeds/{id}"),
            &if o.not_modified {
                "未修改（HTTP 304）。".to_string()
            } else {
                format!("抓取完成：{} 条，新增 {}，更新 {}，跳过 {}。", o.report.entries, o.report.new_stories, o.report.updates, o.report.skipped)
            },
        ),
        Err(e) => redirect_err(&format!("/admin/feeds/{id}"), &format!("抓取失败：{e:#}")),
    }
}

async fn feed_reprocess(State(app): State<AppRef>, headers: HeaderMap, Path(id): Path<i64>, body: Bytes) -> Response {
    let form = Form::parse(&body);
    let _s = guard_post!(app, headers, form);
    let limit: i64 = form.get("limit").and_then(|l| l.parse().ok()).unwrap_or(10).clamp(1, 200);
    let stories: Vec<i64> = match app
        .db
        .call(move |c| {
            let mut st = c.prepare("SELECT id FROM stories WHERE feed_id = ? AND state <> 'skipped' ORDER BY first_seen_at DESC LIMIT ?")?;
            let v = st.query_map(params![id, limit], |r| r.get(0))?.collect::<Result<Vec<i64>, _>>()?;
            Ok(v)
        })
        .await
    {
        Ok(v) => v,
        Err(e) => return server_error(e),
    };
    let mut n = 0;
    for sid in stories {
        if crate::pipeline::reprocess_story(&app, sid).await.unwrap_or(false) {
            n += 1;
        }
    }
    redirect_msg(&format!("/admin/feeds/{id}"), &format!("已安排重新处理 {n} 篇文章。"))
}

async fn feed_markread(State(app): State<AppRef>, headers: HeaderMap, Path(id): Path<i64>, body: Bytes) -> Response {
    let form = Form::parse(&body);
    let _s = guard_post!(app, headers, form);
    let n = app
        .db
        .write(move |tx| crate::store::ItemQuery { feed_ids: Some(vec![id]), read: Some(false), ..Default::default() }.mark_read(tx))
        .await
        .unwrap_or(0);
    redirect_msg(&format!("/admin/feeds/{id}"), &format!("已将 {n} 篇文章标为已读。"))
}

async fn feed_delete(State(app): State<AppRef>, headers: HeaderMap, Path(id): Path<i64>, body: Bytes) -> Response {
    let form = Form::parse(&body);
    let _s = guard_post!(app, headers, form);
    match fetcher::delete_feed(&app, id).await {
        Ok(_) => redirect_msg("/admin/feeds", "订阅已删除。"),
        Err(e) => server_error(e),
    }
}

// ---------------------------------------------------------------------------
// Categories
// ---------------------------------------------------------------------------

struct CatRow {
    id: i64,
    name: String,
    feeds: i64,
}

#[derive(Template)]
#[template(path = "categories.html")]
struct CategoriesT {
    base: Base,
    cats: Vec<CatRow>,
}

async fn categories_page(State(app): State<AppRef>, headers: HeaderMap, Query(q): Query<FlashQuery>) -> Response {
    let s = guard_get!(app, headers);
    let res = app
        .db
        .call(|c| {
            let mut st = c.prepare("SELECT c.id, c.name, (SELECT COUNT(*) FROM feeds f WHERE f.category_id = c.id) FROM categories c ORDER BY c.name COLLATE NOCASE")?;
            let v = st.query_map([], |r| Ok(CatRow { id: r.get(0)?, name: r.get(1)?, feeds: r.get(2)? }))?.collect::<Result<Vec<_>, _>>()?;
            Ok(v)
        })
        .await;
    match res {
        Ok(cats) => render(CategoriesT { base: base(&s, "feeds", &q), cats }),
        Err(e) => server_error(e),
    }
}

async fn category_add(State(app): State<AppRef>, headers: HeaderMap, body: Bytes) -> Response {
    let form = Form::parse(&body);
    let _s = guard_post!(app, headers, form);
    let name = form.get("name").unwrap_or("").trim().to_string();
    if name.is_empty() || name.contains('/') {
        return redirect_err("/admin/categories", "名称不能为空或包含 /");
    }
    match app.db.write(move |tx| fetcher::ensure_category(tx, &name)).await {
        Ok(_) => redirect_msg("/admin/categories", "已创建。"),
        Err(e) => server_error(e),
    }
}

async fn category_rename(State(app): State<AppRef>, headers: HeaderMap, Path(id): Path<i64>, body: Bytes) -> Response {
    let form = Form::parse(&body);
    let _s = guard_post!(app, headers, form);
    let name = form.get("name").unwrap_or("").trim().to_string();
    if name.is_empty() || name.contains('/') {
        return redirect_err("/admin/categories", "名称不能为空或包含 /");
    }
    match app.db.call(move |c| Ok(c.execute("UPDATE categories SET name = ? WHERE id = ?", params![name, id])?)).await {
        Ok(_) => redirect_msg("/admin/categories", "已重命名。"),
        Err(e) => redirect_err("/admin/categories", &format!("重命名失败（名称可能重复）：{e:#}")),
    }
}

async fn category_delete(State(app): State<AppRef>, headers: HeaderMap, Path(id): Path<i64>, body: Bytes) -> Response {
    let form = Form::parse(&body);
    let _s = guard_post!(app, headers, form);
    match app
        .db
        .write(move |tx| {
            tx.execute("UPDATE feeds SET category_id = NULL WHERE category_id = ?", [id])?;
            tx.execute("DELETE FROM categories WHERE id = ?", [id])?;
            Ok(())
        })
        .await
    {
        Ok(_) => redirect_msg("/admin/categories", "已删除。"),
        Err(e) => server_error(e),
    }
}

// ---------------------------------------------------------------------------
// Items
// ---------------------------------------------------------------------------

struct ItemRowV {
    id: i64,
    title: String,
    feed: String,
    version: i64,
    superseded_by: Option<i64>,
    read: bool,
    starred: bool,
    published: String,
    created: String,
}

#[derive(Template)]
#[template(path = "items.html")]
struct ItemsT {
    base: Base,
    items: Vec<ItemRowV>,
    feeds: Vec<(i64, String, bool)>,
    state: String,
    q: String,
    next_before: Option<i64>,
    query_prefix: String,
}

#[derive(serde::Deserialize, Default)]
struct ItemsQuery {
    feed: Option<String>,
    state: Option<String>,
    q: Option<String>,
    before: Option<i64>,
    msg: Option<String>,
    err: Option<String>,
}

async fn items_page(State(app): State<AppRef>, headers: HeaderMap, Query(iq): Query<ItemsQuery>) -> Response {
    let s = guard_get!(app, headers);
    let feed = iq.feed.as_deref().and_then(|f| f.parse::<i64>().ok());
    let state = iq.state.clone().unwrap_or_default();
    let q = iq.q.clone().unwrap_or_default();
    let before = iq.before;
    let (state2, q2) = (state.clone(), q.clone());
    let res = app
        .db
        .call(move |c| {
            let mut sql = String::from(
                "SELECT i.id, i.title, f.title, i.version, i.superseded_by, i.is_read, i.is_starred, i.published_at, i.created_at
                 FROM items i JOIN feeds f ON f.id = i.feed_id WHERE 1",
            );
            let mut p: Vec<rusqlite::types::Value> = Vec::new();
            if let Some(f) = feed {
                sql.push_str(" AND i.feed_id = ?");
                p.push(f.into());
            }
            match state2.as_str() {
                "unread" => sql.push_str(" AND i.is_read = 0 AND i.hidden = 0"),
                "starred" => sql.push_str(" AND i.is_starred = 1"),
                "updated" => sql.push_str(" AND i.version > 1"),
                _ => {}
            }
            if !q2.trim().is_empty() {
                sql.push_str(" AND i.title LIKE ?");
                p.push(format!("%{}%", q2.trim()).into());
            }
            if let Some(b) = before {
                sql.push_str(" AND i.id < ?");
                p.push(b.into());
            }
            sql.push_str(" ORDER BY i.id DESC LIMIT 100");
            let mut st = c.prepare(&sql)?;
            let items = st
                .query_map(rusqlite::params_from_iter(p), |r| {
                    Ok(ItemRowV {
                        id: r.get(0)?,
                        title: r.get(1)?,
                        feed: r.get(2)?,
                        version: r.get(3)?,
                        superseded_by: r.get(4)?,
                        read: r.get::<_, i64>(5)? != 0,
                        starred: r.get::<_, i64>(6)? != 0,
                        published: fmt_ts(r.get(7)?).replace(" UTC", ""),
                        created: fmt_ts(Some(r.get::<_, i64>(8)? / 1_000_000)).replace(" UTC", ""),
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let feeds: Vec<(i64, String)> = {
                let mut st = c.prepare("SELECT id, title FROM feeds ORDER BY title COLLATE NOCASE")?;
                st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?
            };
            Ok((items, feeds))
        })
        .await;
    let fq = FlashQuery { msg: iq.msg, err: iq.err };
    match res {
        Ok((items, feeds)) => {
            let next_before = if items.len() == 100 { items.last().map(|i| i.id) } else { None };
            let mut prefix = String::new();
            if let Some(f) = feed {
                prefix.push_str(&format!("feed={f}&"));
            }
            if !state.is_empty() {
                prefix.push_str(&format!("state={state}&"));
            }
            if !q.is_empty() {
                prefix.push_str(&format!("q={}&", percent_encoding::utf8_percent_encode(&q, percent_encoding::NON_ALPHANUMERIC)));
            }
            let feeds = feeds.into_iter().map(|(id, t)| (id, t, Some(id) == feed)).collect();
            render(ItemsT { base: base(&s, "items", &fq), items, feeds, state, q, next_before, query_prefix: prefix })
        }
        Err(e) => server_error(e),
    }
}

struct ItemDetail {
    id: i64,
    title: String,
    feed: String,
    link: String,
    version: i64,
    published: String,
    created: String,
    superseded_by: Option<i64>,
    story_id: Option<i64>,
    content: String,
}

struct VersionRow {
    id: i64,
    version: i64,
    title: String,
    created: String,
    read: bool,
    starred: bool,
}

struct RevRow {
    id: i64,
    reason: String,
    state: String,
    stage: String,
    attempts: i64,
    notes: String,
    error: String,
    updated: String,
}

#[derive(Template)]
#[template(path = "item.html")]
struct ItemT {
    base: Base,
    it: ItemDetail,
    versions: Vec<VersionRow>,
    revisions: Vec<RevRow>,
}

async fn item_page(State(app): State<AppRef>, headers: HeaderMap, Path(id): Path<i64>, Query(q): Query<FlashQuery>) -> Response {
    let s = guard_get!(app, headers);
    let base_url = request_base(&app, &headers).await;
    let res = app
        .db
        .call(move |c| {
            #[allow(clippy::type_complexity)]
            let row: Option<(String, String, Option<String>, i64, i64, i64, Option<i64>, Option<i64>, String)> = c
                .query_row(
                    "SELECT i.title, f.title, i.link, i.version, i.published_at, i.created_at, i.superseded_by, i.story_id, i.content
                     FROM items i JOIN feeds f ON f.id = i.feed_id WHERE i.id = ?",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?)),
                )
                .optional()?;
            let Some((title, feed, link, version, published, created, sup, story, content)) = row else { return Ok(None) };
            let mut versions = Vec::new();
            let mut revisions = Vec::new();
            if let Some(sid) = story {
                let mut st = c.prepare("SELECT id, version, title, created_at, is_read, is_starred FROM items WHERE story_id = ? ORDER BY id")?;
                versions = st
                    .query_map([sid], |r| {
                        Ok(VersionRow {
                            id: r.get(0)?,
                            version: r.get(1)?,
                            title: r.get(2)?,
                            created: fmt_ts(Some(r.get::<_, i64>(3)? / 1_000_000)),
                            read: r.get::<_, i64>(4)? != 0,
                            starred: r.get::<_, i64>(5)? != 0,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                let mut st = c.prepare("SELECT id, reason, state, stage, total_attempts, notes, COALESCE(last_error, ''), updated_at FROM revisions WHERE story_id = ? ORDER BY id DESC LIMIT 20")?;
                revisions = st
                    .query_map([sid], |r| {
                        let notes: Vec<String> = serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default();
                        Ok(RevRow {
                            id: r.get(0)?,
                            reason: r.get(1)?,
                            state: r.get(2)?,
                            stage: r.get(3)?,
                            attempts: r.get(4)?,
                            notes: notes.join("；"),
                            error: r.get(6)?,
                            updated: fmt_ts(r.get(7)?),
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
            }
            Ok(Some((
                ItemDetail {
                    id,
                    title,
                    feed,
                    link: link.unwrap_or_default(),
                    version,
                    published: fmt_ts(Some(published)),
                    created: fmt_ts(Some(created / 1_000_000)),
                    superseded_by: sup,
                    story_id: story,
                    content,
                },
                versions,
                revisions,
            )))
        })
        .await;
    match res {
        Ok(Some((mut it, versions, revisions))) => {
            it.content = expand_base(&it.content, &base_url);
            render(ItemT { base: base(&s, "items", &q), it, versions, revisions })
        }
        Ok(None) => (StatusCode::NOT_FOUND, "item not found").into_response(),
        Err(e) => server_error(e),
    }
}

async fn story_reprocess(State(app): State<AppRef>, headers: HeaderMap, Path(id): Path<i64>, body: Bytes) -> Response {
    let form = Form::parse(&body);
    let _s = guard_post!(app, headers, form);
    match crate::pipeline::reprocess_story(&app, id).await {
        Ok(true) => redirect_msg("/admin/tasks", "已安排重新处理，完成后会以新版本发布。"),
        Ok(false) => redirect_err("/admin/tasks", "没有可重新处理的源数据。"),
        Err(e) => server_error(e),
    }
}

// ---------------------------------------------------------------------------
// Tasks & events
// ---------------------------------------------------------------------------

struct TaskRow {
    id: i64,
    feed_id: i64,
    feed: String,
    title: String,
    reason: String,
    stage: String,
    attempts: i64,
    next: String,
    error: String,
}

#[derive(Template)]
#[template(path = "tasks.html")]
struct TasksT {
    base: Base,
    pending: Vec<TaskRow>,
    failed: Vec<TaskRow>,
    feed_errors: Vec<(i64, String, i64, String, String)>,
}

async fn tasks_page(State(app): State<AppRef>, headers: HeaderMap, Query(q): Query<FlashQuery>) -> Response {
    let s = guard_get!(app, headers);
    let res = app
        .db
        .call(|c| {
            let load = |sql: &str| -> rusqlite::Result<Vec<TaskRow>> {
                let mut st = c.prepare(sql)?;
                st.query_map([], |r| {
                    let reason: String = r.get(4)?;
                    let stage: String = r.get(5)?;
                    Ok(TaskRow {
                        id: r.get(0)?,
                        feed_id: r.get(1)?,
                        feed: r.get(2)?,
                        title: r.get(3)?,
                        reason,
                        stage: stage_name(&stage).to_string(),
                        attempts: r.get(6)?,
                        next: fmt_relative(r.get(7)?),
                        error: truncate_chars(&r.get::<_, Option<String>>(8)?.unwrap_or_default(), 300),
                    })
                })?
                .collect()
            };
            let pending = load(
                "SELECT r.id, r.feed_id, f.title, r.title, r.reason, r.stage, r.attempts, r.next_attempt_at, r.last_error
                 FROM revisions r JOIN feeds f ON f.id = r.feed_id WHERE r.state = 'pending' ORDER BY r.next_attempt_at, r.id LIMIT 200",
            )?;
            let failed = load(
                "SELECT r.id, r.feed_id, f.title, r.title, r.reason, r.stage, r.attempts, r.updated_at, r.last_error
                 FROM revisions r JOIN feeds f ON f.id = r.feed_id WHERE r.state = 'failed' ORDER BY r.id DESC LIMIT 100",
            )?;
            let mut st = c.prepare("SELECT id, title, fetch_error_count, COALESCE(last_error, ''), next_fetch_at FROM feeds WHERE fetch_error_count > 0 ORDER BY fetch_error_count DESC")?;
            let feed_errors = st
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, fmt_relative(Some(r.get::<_, i64>(4)?)))))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok((pending, failed, feed_errors))
        })
        .await;
    match res {
        Ok((pending, failed, feed_errors)) => render(TasksT { base: base(&s, "tasks", &q), pending, failed, feed_errors }),
        Err(e) => server_error(e),
    }
}

async fn tasks_retry(State(app): State<AppRef>, headers: HeaderMap, body: Bytes) -> Response {
    let form = Form::parse(&body);
    let _s = guard_post!(app, headers, form);
    let feed = form.get("feed").and_then(|f| f.parse::<i64>().ok());
    match crate::pipeline::retry_failed(&app, feed).await {
        Ok(n) => redirect_msg(feed.map(|f| format!("/admin/feeds/{f}")).as_deref().unwrap_or("/admin/tasks"), &format!("已重新排队 {n} 个失败任务，并立即重试等待中的任务。")),
        Err(e) => server_error(e),
    }
}

async fn run_cleanup(State(app): State<AppRef>, headers: HeaderMap, body: Bytes) -> Response {
    let form = Form::parse(&body);
    let _s = guard_post!(app, headers, form);
    match crate::maintenance::cleanup(&app).await {
        Ok(r) => {
            app.event(Level::Info, "cleanup", EventRef::default(), format!("manual cleanup: {}", r.summary())).await;
            redirect_msg("/admin/tasks", &format!("清理完成：{}", r.summary()))
        }
        Err(e) => server_error(e),
    }
}

#[derive(Template)]
#[template(path = "events.html")]
struct EventsT {
    base: Base,
    events: Vec<EventRow>,
    level: String,
    kind: String,
}

#[derive(serde::Deserialize, Default)]
struct EventsQuery {
    level: Option<String>,
    kind: Option<String>,
}

async fn events_page(State(app): State<AppRef>, headers: HeaderMap, Query(eq): Query<EventsQuery>) -> Response {
    let s = guard_get!(app, headers);
    let level = eq.level.unwrap_or_default();
    let kind = eq.kind.unwrap_or_default();
    let (l2, k2) = (level.clone(), kind.clone());
    match app.db.call(move |c| load_events(c, None, &l2, &k2, 300)).await {
        Ok(events) => render(EventsT { base: base(&s, "events", &FlashQuery::default()), events, level, kind }),
        Err(e) => server_error(e),
    }
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsT {
    base: Base,
    sections: Vec<Section>,
}

fn settings_sections(s: &Settings) -> Vec<Section> {
    let n = |name: &str, label: &str, v: &dyn ToString, help: &str| field(name, label, "number", v.to_string(), help);
    let t = |name: &str, label: &str, v: &str, help: &str| field(name, label, "text", v, help);
    vec![
        Section {
            id: "general".into(),
            title: "常规".into(),
            help: String::new(),
            fields: vec![t("base_url", "公开访问地址", &s.base_url, "如 https://rss.example.com，用于缓存图片地址；留空则按请求自动推断（或使用 RILL_BASE_URL）")],
        },
        Section {
            id: "fetch".into(),
            title: "抓取".into(),
            help: String::new(),
            fields: vec![
                n("fetch.default_interval_min", "默认抓取间隔（分钟）", &s.fetch.default_interval_min, ""),
                n("fetch.min_interval_min", "最小抓取间隔（分钟）", &s.fetch.min_interval_min, ""),
                n("fetch.concurrency", "并发抓取订阅数", &s.fetch.concurrency, ""),
                n("fetch.per_host_concurrency", "同一站点并发数", &s.fetch.per_host_concurrency, ""),
                n("fetch.per_host_delay_ms", "同一站点请求间隔（毫秒）", &s.fetch.per_host_delay_ms, "抓取原文网页时的礼貌延迟"),
                n("fetch.page_concurrency", "并发全文提取数", &s.fetch.page_concurrency, ""),
                n("fetch.timeout_secs", "请求超时（秒）", &s.fetch.timeout_secs, ""),
                t("fetch.user_agent", "User-Agent", &s.fetch.user_agent, ""),
                n("fetch.max_feed_mb", "订阅最大体积（MB）", &s.fetch.max_feed_mb, ""),
                n("fetch.max_page_mb", "网页最大体积（MB）", &s.fetch.max_page_mb, ""),
                n("fetch.initial_items", "首次抓取处理条数", &s.fetch.initial_items, "新订阅只处理最新的若干条，避免大量翻译"),
                n("fetch.max_entry_age_days", "忽略早于 N 天的条目", &s.fetch.max_entry_age_days, "0 表示不限制"),
                n("fetch.max_backoff_hours", "出错退避上限（小时）", &s.fetch.max_backoff_hours, ""),
            ],
        },
        Section {
            id: "fulltext".into(),
            title: "全文提取".into(),
            help: String::new(),
            fields: vec![
                t("fulltext.external_url", "外部提取服务地址", &s.fulltext.external_url, "可选。{url} 会替换为编码后的文章地址；返回 HTML 或含 content/html 字段的 JSON"),
                n("fulltext.min_text_chars", "最少正文字数", &s.fulltext.min_text_chars, "少于此值视为提取失败"),
            ],
        },
        Section {
            id: "translation".into(),
            title: "AI 翻译（OpenAI 兼容接口）".into(),
            help: "支持 OpenAI、DeepSeek、通义千问、硅基流动、OpenRouter、Ollama、vLLM 等。".into(),
            fields: vec![
                t("translation.base_url", "API 地址", &s.translation.base_url, "如 https://api.openai.com/v1、https://api.deepseek.com/v1、http://ollama:11434/v1"),
                field("translation.api_key", "API Key", "password", if s.translation.api_key.is_empty() { "" } else { "set" }, ""),
                t("translation.model", "模型", &s.translation.model, ""),
                n("translation.max_concurrency", "最大并发请求", &s.translation.max_concurrency, ""),
                n("translation.requests_per_minute", "每分钟请求上限", &s.translation.requests_per_minute, "0 表示不限制"),
                n("translation.timeout_secs", "单次请求超时（秒）", &s.translation.timeout_secs, ""),
                n("translation.temperature", "Temperature", &s.translation.temperature, "负数表示不发送该参数"),
                n("translation.max_batch_chars", "每批最多字符", &s.translation.max_batch_chars, ""),
                n("translation.max_batch_segments", "每批最多段落", &s.translation.max_batch_segments, ""),
                n("translation.max_tokens", "max_tokens", &s.translation.max_tokens, "0 表示不发送"),
                n("translation.max_attempts", "每篇最多尝试次数", &s.translation.max_attempts, "超过后按失败策略处理"),
                select(
                    "translation.on_failure",
                    "失败策略",
                    &s.translation.on_failure,
                    &[("publish_original", "发布原文（附说明）"), ("hold", "继续等待（最长 72 小时）")],
                    "",
                ),
                check("translation.json_mode", "使用 JSON 输出格式（response_format）", s.translation.json_mode, "默认使用 <seg> 标签格式，截断时可保留已完成段落"),
                field("translation.extra_prompt", "附加提示词", "textarea", &s.translation.extra_prompt, "如术语表、风格要求"),
                field("translation.extra_body", "附加请求参数（JSON）", "textarea", &s.translation.extra_body, "合并到请求体，如 {\"enable_thinking\": false}"),
            ],
        },
        Section {
            id: "images".into(),
            title: "图片缓存".into(),
            help: String::new(),
            fields: vec![
                n("images.max_mb", "单张图片上限（MB）", &s.images.max_mb, ""),
                n("images.max_per_article", "每篇最多图片数", &s.images.max_per_article, ""),
                n("images.max_total_mb", "缓存总上限（MB）", &s.images.max_total_mb, "超过后新图片保留原地址；0 表示不限制"),
                n("images.timeout_secs", "下载超时（秒）", &s.images.timeout_secs, ""),
                n("images.concurrency", "每篇并发下载数", &s.images.concurrency, ""),
                n("images.min_dimension", "过滤小于 N 像素的跟踪图", &s.images.min_dimension, ""),
            ],
        },
        Section {
            id: "updates".into(),
            title: "新闻更新".into(),
            help: "实质性修改以新文章 ID 发布新版本（移动端阅读器不会刷新已缓存的文章）；旧版标为已读但仍保留，以便各客户端同步状态。".into(),
            fields: vec![
                n("updates.min_interval_min", "两次版本最小间隔（分钟）", &s.updates.min_interval_min, "期间的多次修改合并为一次发布"),
                n("updates.max_versions", "每条新闻最多版本数", &s.updates.max_versions, "含首个版本；0 表示不限制"),
                n("updates.track_hours", "追踪时长（小时）", &s.updates.track_hours, "仅追踪首次出现后若干小时内的更新"),
                field("updates.page_check_minutes", "原文复查时间点（分钟）", "text", s.updates.page_check_minutes.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(", "), "「页面追踪」模式下，首次出现后的复查时间"),
                n("updates.min_change_ratio", "最小变化比例", &s.updates.min_change_ratio, "0–1，文本相似度下降超过该值才算修改"),
                n("updates.min_changed_tokens", "最少变化词数", &s.updates.min_changed_tokens, ""),
                n("updates.major_added_tokens", "新增词数阈值", &s.updates.major_added_tokens, "新增超过该值（约一段）总是视为实质更新"),
                check("updates.title_only_triggers", "仅标题明显变化也发布新版本", s.updates.title_only_triggers, ""),
                t("updates.title_prefix", "新版本标题前缀", &s.updates.title_prefix, ""),
                check("updates.notice", "在正文顶部显示更新说明", s.updates.notice, ""),
                t("updates.notice_label", "更新说明文字", &s.updates.notice_label, ""),
                t("updates.previous_label", "首发时间说明文字", &s.updates.previous_label, ""),
                check("updates.mark_unread", "新版本总是未读", s.updates.mark_unread, "关闭则继承旧版的已读状态"),
                check("updates.redate", "新版本使用更新时间作为发布时间", s.updates.redate, "使更新出现在列表顶部（推荐）"),
                check("updates.hide_superseded", "在列表中隐藏旧版本", s.updates.hide_superseded, "不推荐：Read You、Newsboat 等客户端将无法得知旧版已读"),
            ],
        },
        Section {
            id: "retention".into(),
            title: "保留与清理".into(),
            help: "收藏的文章永不删除。".into(),
            fields: vec![
                n("retention.days", "文章保留天数", &s.retention.days, "0 表示永久保留"),
                check("retention.keep_unread", "保留未读文章", s.retention.keep_unread, ""),
                n("retention.max_items_per_feed", "每个订阅最多文章数", &s.retention.max_items_per_feed, "0 表示不限制"),
                n("retention.superseded_days", "旧版本保留天数", &s.retention.superseded_days, "建议 ≥ 31，便于客户端同步已读状态"),
                n("retention.translation_cache_days", "翻译缓存保留天数", &s.retention.translation_cache_days, ""),
                n("retention.event_rows", "日志保留条数", &s.retention.event_rows, ""),
            ],
        },
    ]
}

async fn settings_page(State(app): State<AppRef>, headers: HeaderMap, Query(q): Query<FlashQuery>) -> Response {
    let s = guard_get!(app, headers);
    let settings = app.settings.get().await;
    render(SettingsT { base: base(&s, "settings", &q), sections: settings_sections(&settings) })
}

async fn settings_save(State(app): State<AppRef>, headers: HeaderMap, body: Bytes) -> Response {
    let form = Form::parse(&body);
    let _s = guard_post!(app, headers, form);
    let current = app.settings.get().await;
    let mut v = serde_json::to_value(&*current).unwrap_or_default();
    let bools: Vec<String> = form.all("_bool").iter().map(|s| s.to_string()).collect();
    for (k, val) in &form.0 {
        if k == "csrf" || k == "_bool" {
            continue;
        }
        if k == "translation.api_key" && val.trim().is_empty() {
            continue; // keep existing secret
        }
        set_path(&mut v, k, val, bools.contains(k));
    }
    for b in &bools {
        if form.get(b).is_none() {
            set_path(&mut v, b, "", true);
        }
    }
    let new: Settings = match serde_json::from_value(v) {
        Ok(s) => s,
        Err(e) => return redirect_err("/admin/settings", &format!("设置无效：{e}")),
    };
    if !new.translation.extra_body.trim().is_empty() && serde_json::from_str::<serde_json::Map<String, Value>>(&new.translation.extra_body).is_err() {
        return redirect_err("/admin/settings", "附加请求参数必须是 JSON 对象");
    }
    match app.settings.save(new).await {
        Ok(_) => {
            app.translator.reset_circuit();
            app.wake_fetcher.notify_one();
            app.wake_pipeline.notify_one();
            redirect_msg("/admin/settings", "设置已保存并立即生效。")
        }
        Err(e) => server_error(e),
    }
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "tools.html")]
struct ToolsT {
    base: Base,
    extract_url: String,
    translate_text: String,
    result_title: String,
    result_meta: String,
    result_html: String,
}

fn tools(s: &Session) -> ToolsT {
    ToolsT {
        base: base(s, "tools", &FlashQuery::default()),
        extract_url: String::new(),
        translate_text: "<p>The prime minister said on <b>Monday</b> that taxes would rise next year.</p><p>Opposition parties criticised the plan.</p>".into(),
        result_title: String::new(),
        result_meta: String::new(),
        result_html: String::new(),
    }
}

async fn tools_page(State(app): State<AppRef>, headers: HeaderMap) -> Response {
    let s = guard_get!(app, headers);
    render(tools(&s))
}

async fn tool_extract(State(app): State<AppRef>, headers: HeaderMap, body: Bytes) -> Response {
    let form = Form::parse(&body);
    let s = guard_post!(app, headers, form);
    let url = form.get("url").unwrap_or("").trim().to_string();
    let mut t = tools(&s);
    t.extract_url = url.clone();
    let settings = app.settings.get().await;
    let mut feed = fetcher::FeedRow {
        id: 0,
        url: url.clone(),
        title: String::new(),
        title_locked: false,
        site_url: None,
        etag: None,
        last_modified: None,
        fetch_interval: None,
        error_count: 0,
        options: FeedOptions::default(),
    };
    feed.options.fulltext = match form.get("mode") {
        Some("selector") => crate::settings::FulltextMode::Selector,
        Some("external") => crate::settings::FulltextMode::External,
        _ => crate::settings::FulltextMode::Readability,
    };
    feed.options.selector = form.get("selector").unwrap_or("").to_string();
    let started = std::time::Instant::now();
    match crate::pipeline::extract_page(&app, &settings, &feed, &url).await {
        Ok(p) => {
            let clean = sanitize::sanitize(&p.html, url::Url::parse(&url).ok().as_ref());
            let text_len = crate::util::text::html_to_text(&clean).chars().count();
            t.result_title = "提取结果".into();
            t.result_meta = format!(
                "耗时 {} ms，正文 {} 字，语言 {}，主图 {}",
                started.elapsed().as_millis(),
                text_len,
                p.lang.unwrap_or_else(|| "-".into()),
                p.image.unwrap_or_else(|| "-".into())
            );
            t.result_html = clean;
        }
        Err(crate::pipeline::PageError::Transient(e)) | Err(crate::pipeline::PageError::Permanent(e)) => {
            t.result_title = "提取失败".into();
            t.result_meta = e;
        }
    }
    let _ = extract::apply_remove; // keep the import used in all cfgs
    render(t)
}

async fn tool_translate(State(app): State<AppRef>, headers: HeaderMap, body: Bytes) -> Response {
    let form = Form::parse(&body);
    let s = guard_post!(app, headers, form);
    let text = form.get("text").unwrap_or("").to_string();
    let target = form.get("target").unwrap_or("zh-CN").to_string();
    let mut t = tools(&s);
    t.translate_text = text.clone();
    let html = if text.contains('<') { text.clone() } else { format!("<p>{}</p>", crate::util::html::escape(&text)) };
    let settings = app.settings.get().await;
    let started = std::time::Instant::now();
    match app.translator.test(&settings.translation, &target, &sanitize::sanitize(&html, None)).await {
        Ok(r) => {
            t.result_title = if r.complete { "翻译成功".into() } else { "翻译不完整".into() };
            t.result_meta = format!(
                "耗时 {} ms，请求 {} 次，段落 {}，缓存命中 {}，失败 {}，降级 {}\n标题：{}{}",
                started.elapsed().as_millis(),
                r.requests,
                r.total_segments,
                r.cache_hits,
                r.failed_segments,
                r.degraded_segments,
                r.title.clone().unwrap_or_default(),
                if r.problems.is_empty() { String::new() } else { format!("\n问题：{}", r.problems.join(", ")) }
            );
            t.result_html = r.html;
        }
        Err(e) => {
            t.result_title = "翻译失败".into();
            t.result_meta = e.to_string();
        }
    }
    render(t)
}

async fn tool_feed(State(app): State<AppRef>, headers: HeaderMap, body: Bytes) -> Response {
    let form = Form::parse(&body);
    let s = guard_post!(app, headers, form);
    let url = form.get("url").unwrap_or("").trim().to_string();
    let mut t = tools(&s);
    match fetcher::discover(&app, &url).await {
        Ok(d) => {
            t.result_title = "检测成功".into();
            t.result_meta = format!("订阅地址：{}\n标题：{}", d.url, d.title);
        }
        Err(e) => {
            t.result_title = "检测失败".into();
            t.result_meta = format!("{e:#}");
        }
    }
    render(t)
}

// ---------------------------------------------------------------------------
// OPML
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "opml.html")]
struct OpmlT {
    base: Base,
}

async fn opml_page(State(app): State<AppRef>, headers: HeaderMap, Query(q): Query<FlashQuery>) -> Response {
    let s = guard_get!(app, headers);
    render(OpmlT { base: base(&s, "feeds", &q) })
}

#[derive(serde::Deserialize)]
struct CsrfQuery {
    csrf: Option<String>,
}

async fn opml_import(State(app): State<AppRef>, headers: HeaderMap, Query(cq): Query<CsrfQuery>, mut mp: Multipart) -> Response {
    let Some(s) = session(&app, &headers).await else { return to_login() };
    if cq.csrf.as_deref().is_none_or(|c| !auth::ct_eq(c, &s.csrf)) {
        return (StatusCode::FORBIDDEN, "invalid CSRF token").into_response();
    }
    let mut data: Vec<u8> = Vec::new();
    while let Ok(Some(field)) = mp.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        if let Ok(bytes) = field.bytes().await
            && !bytes.iter().all(|b| b.is_ascii_whitespace())
            && (name == "file" || (name == "text" && data.is_empty()))
        {
            data = bytes.to_vec();
        }
    }
    if data.is_empty() {
        return redirect_err("/admin/opml", "请选择文件或粘贴 OPML 内容。");
    }
    match crate::opml::import(&app, &data).await {
        Ok(r) => redirect_msg("/admin/feeds", &format!("导入完成：发现 {} 个订阅，新增 {}，已存在 {}，新建分类 {}。", r.found, r.added, r.existing, r.categories)),
        Err(e) => redirect_err("/admin/opml", &format!("导入失败：{e:#}")),
    }
}

async fn opml_export(State(app): State<AppRef>, headers: HeaderMap) -> Response {
    let _s = guard_get!(app, headers);
    match crate::opml::export(&app).await {
        Ok(xml) => {
            let mut r = xml.into_response();
            r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/x-opml; charset=utf-8"));
            r.headers_mut().insert(
                header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&format!("attachment; filename=\"rillrss-{}.opml\"", chrono::Utc::now().format("%Y%m%d"))).unwrap(),
            );
            r
        }
        Err(e) => server_error(e),
    }
}

// ---------------------------------------------------------------------------
// Account
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "account.html")]
struct AccountT {
    base: Base,
    api_password: bool,
    sessions: Vec<(String, String, String, String)>,
}

async fn account_page(State(app): State<AppRef>, headers: HeaderMap, Query(q): Query<FlashQuery>) -> Response {
    let s = guard_get!(app, headers);
    let sessions = app
        .db
        .call(|c| {
            let mut st = c.prepare("SELECT kind, COALESCE(user_agent, ''), COALESCE(remote_addr, ''), last_used_at FROM sessions ORDER BY last_used_at DESC LIMIT 50")?;
            let v = st
                .query_map([], |r| {
                    Ok((
                        match r.get::<_, String>(0)?.as_str() {
                            "web" => "网页".to_string(),
                            other => other.to_uppercase(),
                        },
                        r.get(1)?,
                        r.get(2)?,
                        fmt_relative(r.get(3)?),
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(v)
        })
        .await
        .unwrap_or_default();
    let api_password = auth::has_api_password(&app).await.unwrap_or(false);
    render(AccountT { base: base(&s, "account", &q), api_password, sessions })
}

async fn account_password(State(app): State<AppRef>, headers: HeaderMap, body: Bytes) -> Response {
    let form = Form::parse(&body);
    let _s = guard_post!(app, headers, form);
    let current = form.get("current").unwrap_or("");
    let new = form.get("new").unwrap_or("");
    if !auth::verify_password(&app, current).await.unwrap_or(false) {
        return redirect_err("/admin/account", "当前密码错误。");
    }
    if new.chars().count() < 8 {
        return redirect_err("/admin/account", "新密码至少 8 位。");
    }
    match auth::set_password(&app, new).await {
        Ok(_) => {
            app.event(Level::Info, "system", EventRef::default(), "admin password changed").await;
            Redirect::to("/admin/login").into_response()
        }
        Err(e) => server_error(e),
    }
}

async fn account_api_password(State(app): State<AppRef>, headers: HeaderMap, body: Bytes) -> Response {
    let form = Form::parse(&body);
    let _s = guard_post!(app, headers, form);
    let pw = form.get("password").unwrap_or("");
    if !pw.is_empty() && pw.chars().count() < 8 {
        return redirect_err("/admin/account", "API 密码至少 8 位。");
    }
    match auth::set_api_password(&app, if pw.is_empty() { None } else { Some(pw) }).await {
        Ok(_) => redirect_msg("/admin/account", if pw.is_empty() { "已取消 API 专用密码，阅读器请使用管理密码重新登录。" } else { "API 密码已设置，请在阅读器中重新登录。" }),
        Err(e) => server_error(e),
    }
}

async fn account_revoke(State(app): State<AppRef>, headers: HeaderMap, body: Bytes) -> Response {
    let form = Form::parse(&body);
    let _s = guard_post!(app, headers, form);
    match auth::revoke_sessions(&app, &["ttrss"]).await {
        Ok(_) => redirect_msg("/admin/account", "已注销所有 TT-RSS 会话。"),
        Err(e) => server_error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_path_coerces_types() {
        let mut v = serde_json::to_value(Settings::default()).unwrap();
        set_path(&mut v, "fetch.concurrency", "9", false);
        set_path(&mut v, "updates.min_change_ratio", "0.25", false);
        set_path(&mut v, "updates.notice", "", true);
        set_path(&mut v, "updates.page_check_minutes", "10, 20 30", false);
        set_path(&mut v, "translation.model", " gpt-x ", false);
        set_path(&mut v, "fetch.timeout_secs", "", false);
        let s: Settings = serde_json::from_value(v).unwrap();
        assert_eq!(s.fetch.concurrency, 9);
        assert_eq!(s.updates.min_change_ratio, 0.25);
        assert!(!s.updates.notice);
        assert_eq!(s.updates.page_check_minutes, vec![10, 20, 30]);
        assert_eq!(s.translation.model, "gpt-x");
        assert_eq!(s.fetch.timeout_secs, 30);
    }
}

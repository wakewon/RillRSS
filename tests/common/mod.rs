//! Integration test harness: runs a real RillRSS instance (temp data dir, random port)
//! next to a mock upstream web server (feeds, article pages, images) and a mock
//! OpenAI-compatible translation API.
#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, post};
use rillrss::app::{App, AppRef};
use rillrss::config::Config;
use rillrss::settings::Settings;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Mock upstream
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum LlmMode {
    Good,
    Http(u16),
    Garbage,
    Untranslated,
    Truncated,
    /// Answer correctly after a delay.
    Slow(u64),
}

#[derive(Default)]
pub struct MockState {
    pub files: Mutex<HashMap<String, (String, Vec<u8>)>>,
    pub hits: Mutex<HashMap<String, usize>>,
    pub llm_script: Mutex<Vec<LlmMode>>,
    pub llm_calls: AtomicUsize,
    pub llm_segments: AtomicUsize,
}

pub struct Mock {
    pub base: String,
    pub state: Arc<MockState>,
}

impl Mock {
    pub fn set(&self, path: &str, content_type: &str, body: impl Into<Vec<u8>>) {
        self.state.files.lock().unwrap().insert(path.to_string(), (content_type.to_string(), body.into()));
    }
    pub fn remove(&self, path: &str) {
        self.state.files.lock().unwrap().remove(path);
    }
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }
    pub fn hits(&self, path: &str) -> usize {
        self.state.hits.lock().unwrap().get(path).copied().unwrap_or(0)
    }
    pub fn script_llm(&self, modes: Vec<LlmMode>) {
        *self.state.llm_script.lock().unwrap() = modes;
    }
    pub fn llm_calls(&self) -> usize {
        self.state.llm_calls.load(Ordering::SeqCst)
    }
    pub fn llm_segments(&self) -> usize {
        self.state.llm_segments.load(Ordering::SeqCst)
    }
}

async fn serve_file(State(st): State<Arc<MockState>>, uri: Uri) -> Response {
    let path = uri.path().to_string();
    *st.hits.lock().unwrap().entry(path.clone()).or_default() += 1;
    let f = st.files.lock().unwrap().get(&path).cloned();
    match f {
        Some((ct, body)) => ([(header::CONTENT_TYPE, ct)], body).into_response(),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

const HANZI: &[char] = &[
    '政', '府', '宣', '布', '新', '规', '则', '明', '年', '税', '收', '将', '上', '涨', '反', '对', '党', '批', '评', '计', '划', '公', '平', '家',
    '庭', '全', '国', '照', '片', '部', '长', '记', '者', '议', '会', '外', '预', '算', '支', '出', '养', '老', '金', '分', '析', '师', '央', '行',
    '利', '率', '巴', '黎', '宵', '禁', '骚', '乱', '警', '察', '城', '市',
];

/// Deterministic pseudo translation: each source word -> two CJK characters; tags kept.
pub fn pseudo_translate(text: &str) -> String {
    let re = regex::Regex::new(r"(<[^>]+>)|([^<]+)").unwrap();
    let mut out = String::new();
    for c in re.captures_iter(text) {
        if let Some(tag) = c.get(1) {
            out.push_str(tag.as_str());
        } else if let Some(t) = c.get(2) {
            for w in t.as_str().split_whitespace() {
                let h = blake3::hash(w.as_bytes());
                let b = h.as_bytes();
                out.push(HANZI[b[0] as usize % HANZI.len()]);
                out.push(HANZI[b[1] as usize % HANZI.len()]);
            }
            if t.as_str().trim_end().ends_with('.') {
                out.push('。');
            }
        }
    }
    out
}

async fn mock_llm(State(st): State<Arc<MockState>>, body: Bytes) -> Response {
    st.llm_calls.fetch_add(1, Ordering::SeqCst);
    let mode = {
        let mut s = st.llm_script.lock().unwrap();
        if s.is_empty() { LlmMode::Good } else { s.remove(0) }
    };
    let req: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let user = req["messages"][1]["content"].as_str().unwrap_or("").to_string();
    let re = regex::Regex::new(r#"(?s)<seg id="(\d+)">(.*?)</seg>"#).unwrap();
    let segs: Vec<(String, String)> = re.captures_iter(&user).map(|c| (c[1].to_string(), c[2].to_string())).collect();
    st.llm_segments.fetch_add(segs.len(), Ordering::SeqCst);
    let answer = |f: &dyn Fn(&str) -> String| -> String { segs.iter().map(|(id, t)| format!("<seg id=\"{id}\">{}</seg>\n", f(t))).collect() };
    let (content, finish) = match mode {
        LlmMode::Http(code) => {
            return (StatusCode::from_u16(code).unwrap(), json!({"error": {"message": "mock failure"}}).to_string()).into_response();
        }
        LlmMode::Garbage => ("Sorry, I can't help with that.".to_string(), "stop"),
        LlmMode::Untranslated => (answer(&|t| t.to_string()), "stop"),
        LlmMode::Truncated => {
            let full = answer(&pseudo_translate);
            (full.chars().take(full.chars().count() / 2).collect(), "length")
        }
        LlmMode::Slow(ms) => {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            (answer(&pseudo_translate), "stop")
        }
        LlmMode::Good => (answer(&pseudo_translate), "stop"),
    };
    let resp = json!({
        "id": "mock", "object": "chat.completion", "model": "mock-model",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": finish}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 10}
    });
    ([(header::CONTENT_TYPE, "application/json")], resp.to_string()).into_response()
}

pub async fn start_mock() -> Mock {
    let state = Arc::new(MockState::default());
    let app = Router::new()
        .route("/v1/chat/completions", post(mock_llm))
        .route("/{*path}", any(serve_file))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Mock { base: format!("http://{addr}"), state }
}

// ---------------------------------------------------------------------------
// RillRSS instance
// ---------------------------------------------------------------------------

pub struct TestApp {
    pub app: AppRef,
    pub base: String,
    pub dir: Option<tempfile::TempDir>,
    pub client: reqwest::Client,
}

pub fn fast_settings(s: &mut Settings) {
    s.fetch.per_host_delay_ms = 0;
    s.fetch.initial_items = 100;
    s.fetch.max_entry_age_days = 0;
    s.updates.min_interval_min = 0;
    s.translation.requests_per_minute = 0;
    s.translation.max_attempts = 3;
    s.images.min_dimension = 3;
}

pub async fn start_app_in(dir: std::path::PathBuf, workers: bool, tweak: impl FnOnce(&mut Settings)) -> TestApp {
    rillrss::pipeline::RETRY_UNIT_MS.store(1, Ordering::Relaxed);
    rillrss::pipeline::DISPATCH_TICK_MS.store(30, Ordering::Relaxed);
    let mut cfg = Config::for_tests(dir);
    cfg.no_workers = !workers;
    let app = App::new(cfg).await.unwrap();
    app.translator.retry_base_ms.store(1, Ordering::Relaxed);
    let mut s = (*app.settings.get().await).clone();
    fast_settings(&mut s);
    tweak(&mut s);
    app.settings.save(s).await.unwrap();
    if workers {
        tokio::spawn(rillrss::pipeline::run_dispatcher(app.clone()));
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = rillrss::server::router(app.clone());
    tokio::spawn(async move {
        axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();
    });
    TestApp { app, base: format!("http://{addr}"), dir: None, client: reqwest::Client::new() }
}

pub async fn start_app(tweak: impl FnOnce(&mut Settings)) -> TestApp {
    let dir = tempfile::tempdir().unwrap();
    let mut t = start_app_in(dir.path().to_path_buf(), true, tweak).await;
    t.dir = Some(dir);
    t
}

impl TestApp {
    pub fn url(&self, p: &str) -> String {
        format!("{}{}", self.base, p)
    }

    /// Add a feed directly (no discovery) with options JSON.
    pub async fn add_feed(&self, url: &str, options: Value) -> i64 {
        let opts: rillrss::settings::FeedOptions = serde_json::from_value(options).unwrap();
        let (id, _) = rillrss::feeds::fetcher::add_feed(&self.app, url, None, None, Some(opts), false).await.unwrap();
        id
    }

    pub async fn fetch(&self, feed_id: i64) {
        rillrss::feeds::fetcher::fetch_feed(&self.app, feed_id).await.unwrap();
        self.app.wake_pipeline.notify_one();
    }

    /// Wait until no revision is pending and due.
    pub async fn wait_idle(&self) {
        for _ in 0..600 {
            let busy: i64 = self
                .app
                .db
                .call(|c| {
                    Ok(c.query_row(
                        "SELECT COUNT(*) FROM revisions WHERE state = 'pending' AND (next_attempt_at <= strftime('%s','now') + 1 OR lease_until IS NOT NULL)",
                        [],
                        |r| r.get(0),
                    )?)
                })
                .await
                .unwrap();
            if busy == 0 {
                return;
            }
            self.app.wake_pipeline.notify_one();
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("pipeline did not become idle");
    }

    pub async fn query<T: Send + 'static>(&self, sql: &'static str, f: impl FnOnce(&rusqlite::Row) -> rusqlite::Result<T> + Send + 'static) -> T {
        self.app.db.call(move |c| Ok(c.query_row(sql, [], f)?)).await.unwrap()
    }

    pub async fn count(&self, sql: &'static str) -> i64 {
        self.query(sql, |r| r.get(0)).await
    }

    // --- Google Reader helpers
    pub async fn greader_login(&self) -> String {
        let body = self
            .client
            .post(self.url("/api/greader.php/accounts/ClientLogin"))
            .form(&[("Email", "admin"), ("Passwd", "password123")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        body.lines().find_map(|l| l.strip_prefix("Auth=")).unwrap().to_string()
    }

    pub async fn gr_get(&self, auth: &str, path: &str) -> Value {
        let r = self
            .client
            .get(self.url(&format!("/api/greader.php/reader/api/0/{path}")))
            .header("Authorization", format!("GoogleLogin auth={auth}"))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "GET {path}: {}", r.status());
        r.json().await.unwrap()
    }

    pub async fn gr_post(&self, auth: &str, path: &str, form: &[(&str, &str)]) -> (u16, String) {
        let r = self
            .client
            .post(self.url(&format!("/api/greader.php/reader/api/0/{path}")))
            .header("Authorization", format!("GoogleLogin auth={auth}"))
            .form(form)
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.text().await.unwrap())
    }

    // --- TT-RSS helpers
    pub async fn tt(&self, body: Value) -> Value {
        self.client.post(self.url("/api/")).json(&body).send().await.unwrap().json().await.unwrap()
    }

    pub async fn tt_login(&self) -> String {
        let r = self.tt(json!({"op": "login", "user": "admin", "password": "password123"})).await;
        r["content"]["session_id"].as_str().unwrap().to_string()
    }

    pub async fn tt_op(&self, sid: &str, mut body: Value) -> Value {
        body["sid"] = json!(sid);
        let r = self.tt(body).await;
        assert_eq!(r["status"], 0, "{r}");
        r["content"].clone()
    }
}

// ---------------------------------------------------------------------------
// Feed builders
// ---------------------------------------------------------------------------

pub struct Entry {
    pub guid: String,
    pub title: String,
    pub link: String,
    pub html: String,
    pub pub_date: String,
}

pub fn entry(guid: &str, title: &str, link: &str, html: &str) -> Entry {
    Entry { guid: guid.into(), title: title.into(), link: link.into(), html: html.into(), pub_date: "Tue, 20 Oct 2026 01:00:00 GMT".into() }
}

pub fn rss(title: &str, entries: &[Entry]) -> String {
    let mut s = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><rss version=\"2.0\" xmlns:content=\"http://purl.org/rss/1.0/modules/content/\"><channel><title>{title}</title><link>https://site.example.com/</link>"
    );
    for e in entries {
        s.push_str(&format!(
            "<item><guid isPermaLink=\"false\">{}</guid><title><![CDATA[{}]]></title><link>{}</link><pubDate>{}</pubDate><description><![CDATA[{}]]></description></item>",
            e.guid.replace('&', "&amp;"), e.title, e.link.replace('&', "&amp;"), e.pub_date, e.html
        ));
    }
    s.push_str("</channel></rss>");
    s
}

const WORDS: &[&str] = &[
    "government", "minister", "police", "officials", "protesters", "city", "council", "budget", "economy", "inflation", "workers",
    "union", "court", "judge", "election", "voters", "parliament", "agency", "report", "investigation", "hospital", "patients",
    "doctors", "school", "teachers", "students", "river", "storm", "flooding", "rescue", "teams", "airport", "flights", "border",
    "talks", "agreement", "sanctions", "market", "prices", "energy", "climate", "scientists", "study", "results", "company",
    "shares", "investors", "village", "residents", "museum", "festival", "football", "coach", "season", "victory", "defeat",
    "announced", "warned", "confirmed", "denied", "promised", "criticised", "welcomed", "rejected", "approved", "delayed",
    "on", "monday", "tuesday", "wednesday", "after", "before", "during", "despite", "amid", "following", "new", "former",
    "local", "national", "regional", "senior", "several", "dozens", "hundreds", "thousands", "early", "late", "major", "minor",
];

/// Distinct pseudo-news prose: `n` paragraphs whose words depend on `seed`.
pub fn paragraphs(seed: &str, n: usize) -> String {
    let mut state = u64::from_le_bytes(blake3::hash(seed.as_bytes()).as_bytes()[..8].try_into().unwrap());
    let mut next = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (state >> 33) as usize
    };
    let mut out = String::new();
    for i in 0..n {
        let mut words: Vec<&str> = Vec::new();
        for _ in 0..28 {
            words.push(WORDS[next() % WORDS.len()]);
        }
        out.push_str(&format!("<p>{seed} paragraph {i}: {}.</p>", words.join(" ")));
    }
    out
}

/// A tiny valid PNG of the given size (solid color, uncompressed via stored deflate).
pub fn png(w: u32, h: u32) -> Vec<u8> {
    fn crc32(data: &[u8]) -> u32 {
        let mut c = 0xFFFF_FFFFu32;
        for b in data {
            c ^= *b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }
    fn adler(data: &[u8]) -> u32 {
        let (mut a, mut b) = (1u32, 0u32);
        for x in data {
            a = (a + *x as u32) % 65521;
            b = (b + a) % 65521;
        }
        (b << 16) | a
    }
    fn chunk(out: &mut Vec<u8>, ty: &[u8], data: &[u8]) {
        out.extend((data.len() as u32).to_be_bytes());
        let mut c = ty.to_vec();
        c.extend_from_slice(data);
        out.extend_from_slice(&c);
        out.extend(crc32(&c).to_be_bytes());
    }
    let mut raw = Vec::new();
    for _ in 0..h {
        raw.push(0u8);
        for _ in 0..w {
            raw.extend([200u8, 30, 30]);
        }
    }
    let mut z = vec![0x78, 0x01];
    for (i, block) in raw.chunks(65535).enumerate() {
        let last = (i + 1) * 65535 >= raw.len();
        z.push(if last { 1 } else { 0 });
        z.extend((block.len() as u16).to_le_bytes());
        z.extend((!(block.len() as u16)).to_le_bytes());
        z.extend_from_slice(block);
    }
    z.extend(adler(&raw).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend(w.to_be_bytes());
    ihdr.extend(h.to_be_bytes());
    ihdr.extend([8u8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

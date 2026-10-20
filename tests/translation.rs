//! End-to-end translation through the pipeline against a mock OpenAI-compatible API.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::http::{StatusCode, header};
use axum::routing::post;
use common::*;
use serde_json::{Value, json};

fn tr_settings(mock: &Mock) -> impl FnOnce(&mut rillrss::settings::Settings) + '_ {
    move |s| {
        s.translation.base_url = mock.url("/v1");
        s.translation.model = "mock-model".into();
        s.translation.api_key = "test-key".into();
        s.translation.max_batch_segments = 4;
        s.translation.max_attempts = 3;
    }
}

const ARTICLE: &str = "<p>The prime minister said on <a href=\"https://x.example.com/pm\">Monday</a> that taxes would rise next year.</p>\
<p>Opposition parties criticised the plan as unfair to working families across the country.</p>\
<figure><img src=\"https://img.example.com/a.jpg\"><figcaption>The minister speaking to reporters outside parliament.</figcaption></figure>\
<ul><li>First point about the budget and spending plans for schools.</li><li>Second point about pensions and the cost of living crisis.</li></ul>\
<p>Analysts expect the central bank to respond by keeping interest rates unchanged this winter.</p>";

#[tokio::test]
async fn translates_feed_articles_and_preserves_structure() {
    let mock = start_mock().await;
    let t = start_app(tr_settings(&mock)).await;
    mock.set("/en.xml", "application/rss+xml", rss("English", &[entry("e1", "Taxes to rise next year", "https://news.example.com/e1", ARTICLE)]));
    let fid = t.add_feed(&mock.url("/en.xml"), json!({"translate": true, "target_lang": "zh-CN"})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    let (title, content): (String, String) = t.query("SELECT title, content FROM items", |r| Ok((r.get(0)?, r.get(1)?))).await;
    assert_ne!(title, "Taxes to rise next year");
    assert!(title.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)), "{title}");
    assert!(!content.contains("prime minister"), "{content}");
    assert!(content.contains("<a href=\"https://x.example.com/pm\""), "link kept: {content}");
    assert!(content.contains("<img src=\"https://img.example.com/a.jpg\""), "image kept");
    assert!(content.contains("<li>") && content.contains("<figcaption>"), "structure kept");
    assert!(mock.llm_calls() >= 2);
    let notes: String = t.query("SELECT notes FROM revisions", |r| r.get(0)).await;
    assert_eq!(notes, "[]");
}

#[tokio::test]
async fn bilingual_mode_keeps_original() {
    let mock = start_mock().await;
    let t = start_app(tr_settings(&mock)).await;
    mock.set("/en.xml", "application/rss+xml", rss("English", &[entry("e1", "Taxes to rise", "https://news.example.com/e1", ARTICLE)]));
    let fid = t.add_feed(&mock.url("/en.xml"), json!({"translate": true, "translate_mode": "bilingual"})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    let content: String = t.query("SELECT content FROM items", |r| r.get(0)).await;
    assert!(content.contains("prime minister"), "original kept");
    assert_eq!(content.matches("<p>").count(), 6, "translated paragraphs added: {content}");
    assert_eq!(content.matches("<figcaption>").count(), 1, "one caption per figure");
}

#[tokio::test]
async fn transient_failures_are_retried() {
    let mock = start_mock().await;
    let t = start_app(tr_settings(&mock)).await;
    mock.script_llm(vec![LlmMode::Http(500), LlmMode::Http(429), LlmMode::Garbage, LlmMode::Truncated, LlmMode::Untranslated]);
    mock.set("/en.xml", "application/rss+xml", rss("English", &[entry("e1", "Taxes to rise", "https://news.example.com/e1", ARTICLE)]));
    let fid = t.add_feed(&mock.url("/en.xml"), json!({"translate": true})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 1);
    let content: String = t.query("SELECT content FROM items", |r| r.get(0)).await;
    assert!(!content.contains("prime minister"), "eventually fully translated: {content}");
    assert!(!content.contains("Analysts expect"));
}

#[tokio::test]
async fn persistent_failure_publishes_original_with_note() {
    // Translation enabled but no service configured: after max attempts the original
    // is published with a note.
    let mock = start_mock().await;
    let t = start_app(|s| {
        tr_settings(&mock)(s);
        s.translation.base_url = String::new();
        s.translation.max_attempts = 2;
    })
    .await;
    mock.set("/en.xml", "application/rss+xml", rss("English", &[entry("e1", "Taxes to rise", "https://news.example.com/e1", ARTICLE)]));
    let fid = t.add_feed(&mock.url("/en.xml"), json!({"translate": true})).await;
    t.fetch(fid).await;
    for _ in 0..100 {
        if t.count("SELECT COUNT(*) FROM items").await > 0 {
            break;
        }
        t.app.db.call(|c| Ok(c.execute("UPDATE revisions SET next_attempt_at = 0 WHERE state = 'pending'", [])?)).await.unwrap();
        t.app.wake_pipeline.notify_one();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 1, "published after giving up");
    let content: String = t.query("SELECT content FROM items", |r| r.get(0)).await;
    assert!(content.contains("prime minister"), "original text kept");
    let notes: String = t.query("SELECT notes FROM revisions", |r| r.get(0)).await;
    assert!(notes.contains("翻译失败") && notes.contains("未配置"), "{notes}");
    assert_eq!(mock.llm_calls(), 0);
}

#[tokio::test]
async fn persistently_unusable_output_pauses_translation() {
    // A model that never answers in the requested format: paused with a hint after a few
    // requests, instead of ~3 requests per segment on every attempt.
    let mock = start_mock().await;
    let t = start_app(tr_settings(&mock)).await;
    mock.script_llm(vec![LlmMode::Garbage; 1000]);
    mock.set("/en.xml", "application/rss+xml", rss("English", &[entry("e1", "Taxes to rise", "https://news.example.com/e1", ARTICLE)]));
    let fid = t.add_feed(&mock.url("/en.xml"), json!({"translate": true})).await;
    t.fetch(fid).await;
    wait_for_pause(&t).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(mock.llm_calls() <= 8, "no retry storm: {} calls", mock.llm_calls());
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 0);
    assert!(t.app.translator.stats().pause_hint.is_some_and(|h| h.contains("格式")));
    // A working model (saving settings resets the circuit): published translated.
    mock.script_llm(vec![]);
    t.app.translator.reset_circuit();
    t.app.db.call(|c| Ok(c.execute("UPDATE revisions SET next_attempt_at = 0", [])?)).await.unwrap();
    t.wait_idle().await;
    let content: String = t.query("SELECT content FROM items", |r| r.get(0)).await;
    assert!(!content.contains("prime minister"));
}

#[tokio::test]
async fn partly_unusable_output_publishes_partial_translation_with_note() {
    // The model refuses one paragraph every time: the rest is translated, published with a
    // note after the last attempt, and translation is not paused.
    let mock = start_mock().await;
    let (url, _calls) = llm_mock(|user| if user.contains("Opposition parties") { refusal() } else { completion(user) }).await;
    let t = start_app(|s| {
        llm_settings(&url)(s);
        s.translation.max_attempts = 2;
    })
    .await;
    mock.set("/en.xml", "application/rss+xml", rss("English", &[entry("e1", "Taxes to rise", "https://news.example.com/e1", ARTICLE)]));
    let fid = t.add_feed(&mock.url("/en.xml"), json!({"translate": true})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    let content: String = t.query("SELECT content FROM items", |r| r.get(0)).await;
    assert!(content.contains("Opposition parties") && !content.contains("prime minister"), "{content}");
    let notes: String = t.query("SELECT notes FROM revisions", |r| r.get(0)).await;
    assert!(notes.contains("翻译不完整"), "{notes}");
    assert!(t.app.translator.circuit_open_for().is_none(), "a refused paragraph does not pause the service");
}

#[tokio::test]
async fn moderation_rejection_keeps_the_rest_of_the_article() {
    // DeepSeek rejects one paragraph ("Content Exists Risk"): only that one stays in the
    // source language and nothing is paused.
    let mock = start_mock().await;
    let risk = json!({"error": {"message": "Content Exists Risk", "type": "invalid_request_error", "param": null, "code": "invalid_request_error"}}).to_string();
    let (url, _calls) = llm_mock(move |user| if user.contains("Opposition parties") { (400, risk.clone()) } else { completion(user) }).await;
    let t = start_app(|s| {
        llm_settings(&url)(s);
        s.translation.max_attempts = 2;
    })
    .await;
    mock.set("/en.xml", "application/rss+xml", rss("English", &[entry("e1", "Taxes to rise", "https://news.example.com/e1", ARTICLE)]));
    let fid = t.add_feed(&mock.url("/en.xml"), json!({"translate": true})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    let content: String = t.query("SELECT content FROM items", |r| r.get(0)).await;
    assert!(content.contains("Opposition parties") && !content.contains("prime minister"), "{content}");
    assert!(t.app.translator.circuit_open_for().is_none());
}

#[tokio::test]
async fn moderated_segments_publish_the_mixed_translation_at_once() {
    // Zhipu GLM refuses one paragraph (code 1301). Refusals repeat on every attempt, so the
    // mixed result is published on the first one, even under the "hold" policy.
    let mock = start_mock().await;
    let glm = json!({"contentFilter": [{"level": 1, "role": "assistant"}], "error": {"code": "1301", "message": "系统检测到输入或生成内容可能包含不安全或敏感内容，请您避免输入易产生敏感内容的提示语，感谢您的配合。"}}).to_string();
    let (url, calls) = llm_mock(move |user| if user.contains("Opposition parties") { (400, glm.clone()) } else { completion(user) }).await;
    let t = start_app(|s| {
        llm_settings(&url)(s);
        s.translation.on_failure = "hold".into();
        s.translation.max_attempts = 3;
    })
    .await;
    mock.set("/en.xml", "application/rss+xml", rss("English", &[entry("e1", "Taxes to rise", "https://news.example.com/e1", ARTICLE)]));
    let fid = t.add_feed(&mock.url("/en.xml"), json!({"translate": true})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    let content: String = t.query("SELECT content FROM items", |r| r.get(0)).await;
    assert!(content.contains("Opposition parties") && !content.contains("prime minister"), "{content}");
    let (attempts, notes): (i64, String) = t.query("SELECT attempts, notes FROM revisions", |r| Ok((r.get(0)?, r.get(1)?))).await;
    assert_eq!(attempts, 0, "published without a retry");
    assert!(notes.contains("内容审核") && !notes.contains("翻译不完整"), "{notes}");
    // One batch, two halves, then single segments: a handful of requests, no retry rounds.
    assert!(calls.load(Ordering::SeqCst) <= 6, "{} requests", calls.load(Ordering::SeqCst));
    assert!(t.app.translator.circuit_open_for().is_none());
}

#[tokio::test]
async fn context_errors_caused_by_max_tokens_pause_instead_of_splitting() {
    // max_tokens as large as the model's window (vLLM): every request fails however
    // small. Paused at once with a max_tokens hint instead of bisecting every batch.
    let mock = start_mock().await;
    let error = json!({"object": "error", "message": "This model's maximum context length is 4096 tokens. However, you requested 4610 tokens (514 in the messages, 4096 in the completion). Please reduce the length of the messages or completion.", "type": "BadRequestError", "param": null, "code": 400}).to_string();
    let (url, calls) = llm_mock(move |_| (400, error.clone())).await;
    let t = start_app(|s| {
        llm_settings(&url)(s);
        s.translation.max_tokens = 4096;
    })
    .await;
    let entries: Vec<Entry> = (0..3).map(|i| entry(&format!("c{i}"), &format!("Story {i}"), &format!("https://news.example.com/c{i}"), ARTICLE)).collect();
    mock.set("/cfg.xml", "application/rss+xml", rss("Cfg", &entries));
    let fid = t.add_feed(&mock.url("/cfg.xml"), json!({"translate": true})).await;
    t.fetch(fid).await;
    wait_for_pause(&t).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(calls.load(Ordering::SeqCst) <= 4, "no retry storm: {} calls", calls.load(Ordering::SeqCst));
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 0);
    assert!(t.app.translator.stats().pause_hint.is_some_and(|h| h.contains("max_tokens")));
}

#[tokio::test]
async fn hold_policy_keeps_waiting_and_circuit_breaker_pauses() {
    let mock = start_mock().await;
    let t = start_app(|s| {
        tr_settings(&mock)(s);
        s.translation.on_failure = "hold".into();
        s.translation.max_attempts = 2;
    })
    .await;
    mock.script_llm(vec![LlmMode::Http(503); 500]);
    mock.set("/en.xml", "application/rss+xml", rss("English", &[entry("e1", "Taxes to rise", "https://news.example.com/e1", ARTICLE)]));
    let fid = t.add_feed(&mock.url("/en.xml"), json!({"translate": true})).await;
    t.fetch(fid).await;
    // Let it fail a few rounds.
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        if t.app.translator.circuit_open_for().is_some() {
            break;
        }
    }
    assert!(t.app.translator.circuit_open_for().is_some(), "circuit breaker opened after repeated 503s");
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 0, "held, not published untranslated");
    assert_eq!(t.count("SELECT COUNT(*) FROM revisions WHERE state = 'pending' AND stage = 'translate'").await, 1);
    // Service recovers; operator retries (resets circuit) -> published translated.
    mock.script_llm(vec![]);
    rillrss::pipeline::retry_failed(&t.app, None).await.unwrap();
    t.app.db.call(|c| Ok(c.execute("UPDATE revisions SET next_attempt_at = 0", [])?)).await.unwrap();
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 1);
    let content: String = t.query("SELECT content FROM items", |r| r.get(0)).await;
    assert!(!content.contains("prime minister"));
}

#[tokio::test]
async fn updates_reuse_cached_segments_and_never_publish_stale_translation() {
    let mock = start_mock().await;
    let t = start_app(tr_settings(&mock)).await;
    mock.set("/en.xml", "application/rss+xml", rss("English", &[entry("e1", "Taxes to rise", "https://news.example.com/e1", ARTICLE)]));
    let fid = t.add_feed(&mock.url("/en.xml"), json!({"translate": true})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    let segs_v1 = mock.llm_segments();

    // An update adds two paragraphs: only the new paragraphs (+ title) are translated.
    let updated = format!("{ARTICLE}{}", paragraphs("Late news", 2));
    mock.set("/en.xml", "application/rss+xml", rss("English", &[entry("e1", "Taxes to rise sharply", "https://news.example.com/e1", &updated)]));
    t.fetch(fid).await;
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 2);
    let new_segments = mock.llm_segments() - segs_v1;
    assert!(new_segments <= 4, "only changed segments re-translated, got {new_segments}");

    // Race: a slow translation of revision 3 is overtaken by revision 4.
    mock.script_llm(vec![LlmMode::Slow(800); 4]);
    let v3 = format!("{updated}{}", paragraphs("Third version", 2));
    mock.set("/en.xml", "application/rss+xml", rss("English", &[entry("e1", "Taxes v3", "https://news.example.com/e1", &v3)]));
    t.fetch(fid).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await; // translation of v3 in flight
    let v4 = format!("{v3}{}", paragraphs("Fourth version", 2));
    mock.set("/en.xml", "application/rss+xml", rss("English", &[entry("e1", "Taxes v4", "https://news.example.com/e1", &v4)]));
    t.fetch(fid).await;
    t.wait_idle().await;
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 3, "v3 never published, v4 published");
    assert_eq!(t.count("SELECT COUNT(*) FROM revisions WHERE state = 'obsolete'").await, 1);
    let max_version: i64 = t.count("SELECT MAX(version) FROM items").await;
    assert_eq!(max_version, 3);
    let (latest_title, hidden_old): (String, i64) = t
        .query("SELECT title, (SELECT COUNT(*) FROM items WHERE is_read = 0) FROM items ORDER BY id DESC LIMIT 1", |r| Ok((r.get(0)?, r.get(1)?)))
        .await;
    assert!(latest_title.starts_with("[更新]"));
    assert_eq!(hidden_old, 1, "only the newest version is unread");
}

#[tokio::test]
async fn articles_already_in_target_language_are_skipped() {
    let mock = start_mock().await;
    let t = start_app(tr_settings(&mock)).await;
    let zh = "<p>法国多地上周末爆发骚乱及抢掠，当局其后在巴黎市中心部分地区及其他城市实施夜间宵禁。</p><p>巴黎警方表示，宵禁将于当地时间晚上十一时至次日早上六时期间执行。</p>";
    mock.set("/zh.xml", "application/rss+xml", rss("中文", &[entry("z1", "法国骚乱：巴黎局部宵禁", "https://news.example.com/z1", zh)]));
    let fid = t.add_feed(&mock.url("/zh.xml"), json!({"translate": true, "target_lang": "zh-CN"})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    assert_eq!(mock.llm_calls(), 0);
    let notes: String = t.query("SELECT notes FROM revisions", |r| r.get(0)).await;
    assert!(notes.contains("已是目标语言"));
}

#[tokio::test]
async fn opencc_then_translation_order_and_title() {
    let mock = start_mock().await;
    let t = start_app(tr_settings(&mock)).await;
    let trad = "<p>法國多地上週末爆發騷亂及搶掠，當局其後在巴黎市中心部分地區及其他城市實施夜間宵禁。</p>";
    mock.set("/tw.xml", "application/rss+xml", rss("繁體", &[entry("t1", "法國騷亂：巴黎局部宵禁", "https://news.example.com/t1", trad)]));
    let fid = t.add_feed(&mock.url("/tw.xml"), json!({"opencc": "t2s"})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    let (title, content): (String, String) = t.query("SELECT title, content FROM items", |r| Ok((r.get(0)?, r.get(1)?))).await;
    assert_eq!(title, "法国骚乱：巴黎局部宵禁");
    assert!(content.contains("法国多地上周末爆发骚乱及抢掠"), "{content}");
}

#[tokio::test]
async fn invalid_parameter_errors_pause_instead_of_retry_storm() {
    let mock = start_mock().await;
    let t = start_app(tr_settings(&mock)).await;
    mock.script_llm(vec![LlmMode::ConfigError; 1000]);
    let entries: Vec<Entry> = (0..3).map(|i| entry(&format!("c{i}"), &format!("Story {i}"), &format!("https://news.example.com/c{i}"), ARTICLE)).collect();
    mock.set("/cfg.xml", "application/rss+xml", rss("Cfg", &entries));
    let fid = t.add_feed(&mock.url("/cfg.xml"), json!({"translate": true})).await;
    t.fetch(fid).await;
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        if t.app.translator.circuit_open_for().is_some() {
            break;
        }
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(t.app.translator.circuit_open_for().is_some(), "an invalid-parameter error pauses translation");
    assert!(mock.llm_calls() <= 4, "no retry storm: {} calls", mock.llm_calls());
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 0, "articles wait for a working configuration");
    // Configuration fixed (saving settings resets the circuit): everything gets translated.
    mock.script_llm(vec![]);
    t.app.translator.reset_circuit();
    t.app.db.call(|c| Ok(c.execute("UPDATE revisions SET next_attempt_at = 0", [])?)).await.unwrap();
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 3);
    let content: String = t.query("SELECT content FROM items LIMIT 1", |r| r.get(0)).await;
    assert!(!content.contains("prime minister"));
}

fn llm_settings(url: &str) -> impl FnOnce(&mut rillrss::settings::Settings) + '_ {
    move |s| {
        s.translation.base_url = url.to_string();
        s.translation.model = "mock-model".into();
        s.translation.api_key = "test-key".into();
        s.translation.max_batch_segments = 4;
        s.translation.max_attempts = 3;
    }
}

async fn wait_for_pause(t: &TestApp) {
    for _ in 0..100 {
        if t.app.translator.circuit_open_for().is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("translation was not paused");
}

/// A chat-completions endpoint answering `f(user prompt) -> (status, body)`; returns its
/// base URL and request counter.
async fn llm_mock(f: impl Fn(&str) -> (u16, String) + Send + Sync + 'static) -> (String, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let f = Arc::new(f);
    let counter = calls.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |body: Bytes| {
            let (f, counter) = (f.clone(), counter.clone());
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                let req: Value = serde_json::from_slice(&body).unwrap_or_default();
                let (status, body) = f(req["messages"][1]["content"].as_str().unwrap_or(""));
                (StatusCode::from_u16(status).unwrap(), [(header::CONTENT_TYPE, "application/json")], body)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/v1"), calls)
}

/// A completion translating every `<seg>` of the prompt.
fn completion(user: &str) -> (u16, String) {
    let re = regex::Regex::new(r#"(?s)<seg id="(\d+)">(.*?)</seg>"#).unwrap();
    let content: String = re.captures_iter(user).map(|c| format!("<seg id=\"{}\">{}</seg>\n", &c[1], pseudo_translate(&c[2]))).collect();
    chat_response(&content)
}

fn refusal() -> (u16, String) {
    chat_response("Sorry, I can't help with that.")
}

fn chat_response(content: &str) -> (u16, String) {
    let resp = json!({
        "id": "mock", "object": "chat.completion", "model": "mock-model",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 10}
    });
    (200, resp.to_string())
}

// ---------------------------------------------------------------------------
// Machine-translation engines and the fallback engine
// ---------------------------------------------------------------------------

use rillrss::content::translate::Engine;
use rillrss::content::translate::llm::ApiError;
use rillrss::content::translate::mt::{self, MtBackend};

/// A machine-translation engine translating every text with `pseudo_translate` (tags
/// kept), or failing every request with `fail`.
struct FakeMt {
    calls: AtomicUsize,
    segments: AtomicUsize,
    fail: Option<u16>,
}

impl FakeMt {
    fn new(fail: Option<u16>) -> Arc<FakeMt> {
        Arc::new(FakeMt { calls: AtomicUsize::new(0), segments: AtomicUsize::new(0), fail })
    }
}

impl MtBackend for FakeMt {
    fn translate(&self, texts: Vec<String>, _target: String) -> futures::future::BoxFuture<'_, Result<Vec<String>, ApiError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(status) = self.fail {
                return Err(ApiError::Http { status, body: "down".into(), retry_after: None });
            }
            self.segments.fetch_add(texts.len(), Ordering::SeqCst);
            Ok(texts.iter().map(|t| pseudo_translate(t)).collect())
        })
    }
}

#[tokio::test]
async fn machine_translation_engine_translates_articles() {
    let mock = start_mock().await;
    let t = start_app(|s| s.translation.engine = "google".into()).await;
    let google = FakeMt::new(None);
    t.app.translator.set_mt_backend(Engine::Google, google.clone());
    mock.set("/en.xml", "application/rss+xml", rss("English", &[entry("e1", "Taxes to rise", "https://news.example.com/e1", ARTICLE)]));
    let fid = t.add_feed(&mock.url("/en.xml"), json!({"translate": true})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    let (title, content): (String, String) = t.query("SELECT title, content FROM items", |r| Ok((r.get(0)?, r.get(1)?))).await;
    assert!(!content.contains("prime minister") && content.contains("href=\"https://x.example.com/pm\""), "{content}");
    assert!(!title.contains("Taxes"), "{title}");
    // One request for the whole article (title included), no LLM involved.
    assert_eq!(google.calls.load(Ordering::SeqCst), 1);
    assert_eq!(mock.llm_calls(), 0);
    assert_eq!(t.app.translator.engine_stats(Engine::Google).requests, 1);
}

#[tokio::test]
async fn fallback_engine_fills_in_moderated_segments() {
    // Zhipu refuses one paragraph; Google translates it, so the article is complete.
    let mock = start_mock().await;
    let glm = json!({"contentFilter": [{"level": 1, "role": "assistant"}], "error": {"code": "1301", "message": "系统检测到输入或生成内容可能包含不安全或敏感内容"}}).to_string();
    let (url, _calls) = llm_mock(move |user| if user.contains("Opposition parties") { (400, glm.clone()) } else { completion(user) }).await;
    let t = start_app(|s| {
        llm_settings(&url)(s);
        s.translation.fallback = "google".into();
    })
    .await;
    let google = FakeMt::new(None);
    t.app.translator.set_mt_backend(Engine::Google, google.clone());
    mock.set("/en.xml", "application/rss+xml", rss("English", &[entry("e1", "Taxes to rise", "https://news.example.com/e1", ARTICLE)]));
    let fid = t.add_feed(&mock.url("/en.xml"), json!({"translate": true})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    let content: String = t.query("SELECT content FROM items", |r| r.get(0)).await;
    assert!(!content.contains("Opposition parties") && !content.contains("prime minister"), "{content}");
    assert_eq!(google.segments.load(Ordering::SeqCst), 1, "only the refused paragraph");
    let notes: String = t.query("SELECT notes FROM revisions", |r| r.get(0)).await;
    assert!(notes.contains("由Google 翻译补译") && !notes.contains("内容审核"), "{notes}");
}

#[tokio::test]
async fn fallback_engine_takes_the_article_when_the_main_engine_fails() {
    // The LLM is down. Earlier attempts retry it; the last one goes to Microsoft.
    let mock = start_mock().await;
    let t = start_app(|s| {
        tr_settings(&mock)(s);
        s.translation.fallback = "microsoft".into();
        s.translation.max_attempts = 2;
    })
    .await;
    mock.script_llm(vec![LlmMode::Http(400); 1000]);
    let microsoft = FakeMt::new(None);
    t.app.translator.set_mt_backend(Engine::Microsoft, microsoft.clone());
    mock.set("/en.xml", "application/rss+xml", rss("English", &[entry("e1", "Taxes to rise", "https://news.example.com/e1", ARTICLE)]));
    let fid = t.add_feed(&mock.url("/en.xml"), json!({"translate": true})).await;
    t.fetch(fid).await;
    for _ in 0..100 {
        if t.count("SELECT COUNT(*) FROM items").await > 0 {
            break;
        }
        t.app.db.call(|c| Ok(c.execute("UPDATE revisions SET next_attempt_at = 0 WHERE state = 'pending'", [])?)).await.unwrap();
        t.app.wake_pipeline.notify_one();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let content: String = t.query("SELECT content FROM items", |r| r.get(0)).await;
    assert!(!content.contains("prime minister"), "{content}");
    let notes: String = t.query("SELECT notes FROM revisions", |r| r.get(0)).await;
    assert!(notes.contains("已改用微软翻译"), "{notes}");
    assert!(microsoft.calls.load(Ordering::SeqCst) >= 1);
}

#[tokio::test]
async fn paused_main_engine_hands_new_articles_to_the_fallback() {
    // A bad API key pauses the LLM at once; articles keep flowing through Google.
    let mock = start_mock().await;
    let t = start_app(|s| {
        tr_settings(&mock)(s);
        s.translation.fallback = "google".into();
        s.translation.on_failure = "hold".into();
    })
    .await;
    mock.script_llm(vec![LlmMode::Http(401); 1000]);
    let google = FakeMt::new(None);
    t.app.translator.set_mt_backend(Engine::Google, google.clone());
    let entries: Vec<Entry> = (0..3).map(|i| entry(&format!("p{i}"), &format!("Story {i}"), &format!("https://news.example.com/p{i}"), ARTICLE)).collect();
    mock.set("/en.xml", "application/rss+xml", rss("English", &entries));
    let fid = t.add_feed(&mock.url("/en.xml"), json!({"translate": true})).await;
    t.fetch(fid).await;
    for _ in 0..100 {
        if t.count("SELECT COUNT(*) FROM items").await == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 3);
    assert!(t.app.translator.circuit_open_for().is_some(), "the LLM is paused");
    assert!(mock.llm_calls() <= 3, "{} LLM calls", mock.llm_calls());
    assert_eq!(t.count("SELECT COUNT(*) FROM items WHERE content LIKE '%prime minister%'").await, 0);
}

#[tokio::test]
async fn failing_fallback_leaves_the_main_engine_result() {
    // Both engines fail: the failure policy applies as before (original after the
    // last attempt), and the fallback's error shows in the problems.
    let mock = start_mock().await;
    let (url, _calls) = llm_mock(|user| if user.contains("Opposition parties") { refusal() } else { completion(user) }).await;
    let t = start_app(|s| {
        llm_settings(&url)(s);
        s.translation.fallback = "google".into();
        s.translation.max_attempts = 1;
    })
    .await;
    t.app.translator.set_mt_backend(Engine::Google, FakeMt::new(Some(503)));
    mock.set("/en.xml", "application/rss+xml", rss("English", &[entry("e1", "Taxes to rise", "https://news.example.com/e1", ARTICLE)]));
    let fid = t.add_feed(&mock.url("/en.xml"), json!({"translate": true})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    let content: String = t.query("SELECT content FROM items", |r| r.get(0)).await;
    assert!(content.contains("Opposition parties") && !content.contains("prime minister"), "{content}");
    let notes: String = t.query("SELECT notes FROM revisions", |r| r.get(0)).await;
    assert!(notes.contains("翻译不完整"), "{notes}");
}

type Handler = Arc<dyn Fn(&str) -> (u16, String) + Send + Sync>;

/// A server answering `path` with `f(request body) -> (status, body)`.
async fn http_mock(routes: Vec<(&'static str, Handler)>) -> String {
    let mut app = Router::new();
    for (path, f) in routes {
        app = app.route(
            path,
            post(move |body: Bytes| {
                let f = f.clone();
                async move {
                    let (status, body) = f(&String::from_utf8_lossy(&body));
                    (StatusCode::from_u16(status).unwrap(), [(header::CONTENT_TYPE, "text/plain")], body)
                }
            }),
        );
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn google_protocol_and_markup() {
    // translateHtml: [[texts, "auto", target], "wt_lib"] -> [[translations], [langs]].
    let base = http_mock(vec![(
        "/html",
        Arc::new(|body: &str| {
            let v: Value = serde_json::from_str(body).unwrap();
            assert_eq!(v[0][2], "zh-CN");
            let texts: Vec<String> = v[0][0].as_array().unwrap().iter().map(|t| t.as_str().unwrap().to_string()).collect();
            assert!(texts[0].contains("<span id=\"g1\">Monday</span>") && texts[0].contains("<img id=\"x2\">"), "{texts:?}");
            let out: Vec<String> = texts.iter().map(|t| pseudo_translate(t)).collect();
            (200, json!([out, vec!["en"; texts.len()]]).to_string())
        }),
    )])
    .await;
    let g = mt::Google::with_urls(reqwest::Client::new(), &format!("{base}/html"), &format!("{base}/t"));
    let out = g.translate(vec!["Said on <g1>Monday</g1>.<x2/>".into(), "Second one.".into()], "zh-cn".into()).await.unwrap();
    assert!(out[0].contains("<g1>") && out[0].contains("</g1>") && out[0].contains("<x2/>") && !out[0].contains("Monday"), "{out:?}");
    assert_eq!(out.len(), 2);

    // A refused key falls back to translate_a/t (form fields q=…; [[text, lang], …]).
    let base = http_mock(vec![
        ("/html", Arc::new(|_: &str| (403, "{\"error\":{\"code\":403}}".to_string()))),
        (
            "/t",
            Arc::new(|body: &str| {
                let texts: Vec<String> = url::form_urlencoded::parse(body.as_bytes()).filter(|(k, _)| k == "q").map(|(_, v)| v.into_owned()).collect();
                (200, json!(texts.iter().map(|t| json!([pseudo_translate(t), "en"])).collect::<Vec<_>>()).to_string())
            }),
        ),
    ])
    .await;
    let g = mt::Google::with_urls(reqwest::Client::new(), &format!("{base}/html"), &format!("{base}/t"));
    let out = g.translate(vec!["Said on <g1>Monday</g1>.".into(), "Second one.".into()], "zh-CN".into()).await.unwrap();
    assert!(out[0].contains("<g1>") && !out[0].contains("Monday") && out.len() == 2, "{out:?}");

    // Rate limits are reported (the engine backs off), not retried elsewhere.
    let base = http_mock(vec![("/html", Arc::new(|_: &str| (429, "<html>Sorry...</html>".to_string())))]).await;
    let g = mt::Google::with_urls(reqwest::Client::new(), &format!("{base}/html"), &format!("{base}/t"));
    let err = g.translate(vec!["x".into()], "zh-CN".into()).await.unwrap_err();
    assert!(matches!(err, ApiError::Http { status: 429, .. }) && err.is_transient(), "{err}");
}

#[tokio::test]
async fn microsoft_protocol() {
    // A bare JSON array of strings -> [{"translations": [{"text": …}]}, …].
    let base = http_mock(vec![(
        "/mt",
        Arc::new(|body: &str| {
            let texts: Vec<String> = serde_json::from_str(body).unwrap();
            assert!(texts[0].contains("<g1>Monday</g1>"), "placeholders sent as they are: {texts:?}");
            let out: Vec<Value> = texts.iter().map(|t| json!({"translations": [{"text": pseudo_translate(t), "to": "zh-Hans"}]})).collect();
            (200, json!(out).to_string())
        }),
    )])
    .await;
    let m = mt::Microsoft::with_url(reqwest::Client::new(), &format!("{base}/mt"));
    let out = m.translate(vec!["Said on <g1>Monday</g1>.".into(), "Second one.".into()], "zh-CN".into()).await.unwrap();
    assert!(out[0].contains("<g1>") && !out[0].contains("Monday") && out.len() == 2, "{out:?}");
}

/// Against the real services (network): `cargo test --test translation live_ -- --ignored`.
#[tokio::test]
#[ignore]
async fn live_machine_translation_engines() {
    let tmp = tempfile::tempdir().unwrap();
    let db = rillrss::db::Db::open(&tmp.path().join("t.db")).unwrap();
    let tr = rillrss::content::translate::Translator::new(db, reqwest::Client::new());
    let html = "<p>The prime minister said on <a href=\"https://x.example.com/pm\">Monday</a> that taxes would <b>rise</b> next year.</p>\
<p>Shares of <code>ACME</code> fell 5% after Jane Doe said a &lt; b &amp; c.<br>Second line here.</p><ul><li>First point about the budget.</li></ul>";
    for engine in ["google", "microsoft"] {
        let s = rillrss::settings::TranslationSettings { engine: engine.into(), ..Default::default() };
        let r = tr.translate_article(&s, "zh-CN", Some("Taxes to rise next year"), html, false).await.unwrap();
        println!("{engine}: complete={} failed={} degraded={} requests={}\n  title: {:?}\n  {}\n  problems: {:?}", r.complete, r.failed_segments, r.degraded_segments, r.requests, r.title, r.html, r.problems);
        assert!(r.complete, "{engine}: {r:?}");
        assert!(r.html.contains("href=\"https://x.example.com/pm\"") && r.html.contains("<code>ACME</code>"), "{engine}: {}", r.html);
    }
}

//! End-to-end translation through the pipeline against a mock OpenAI-compatible API.

mod common;

use common::*;
use serde_json::json;

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
    let mock = start_mock().await;
    let t = start_app(|s| {
        tr_settings(&mock)(s);
        s.translation.max_attempts = 2;
    })
    .await;
    mock.script_llm(vec![LlmMode::Http(400); 200]);
    mock.set("/en.xml", "application/rss+xml", rss("English", &[entry("e1", "Taxes to rise", "https://news.example.com/e1", ARTICLE)]));
    let fid = t.add_feed(&mock.url("/en.xml"), json!({"translate": true})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    assert_eq!(t.count("SELECT COUNT(*) FROM items").await, 1, "published after giving up");
    let content: String = t.query("SELECT content FROM items", |r| r.get(0)).await;
    assert!(content.contains("prime minister"), "original text kept");
    let notes: String = t.query("SELECT notes FROM revisions", |r| r.get(0)).await;
    assert!(notes.contains("translation failed"), "{notes}");
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
    assert!(notes.contains("already in target language"));
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

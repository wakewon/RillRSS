//! OpenAI-compatible chat completions backend and tolerant response parsing.

use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub system: String,
    pub user: String,
    pub json_mode: bool,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub content: String,
    pub finish_reason: Option<String>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ApiError {
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String, retry_after: Option<Duration> },
    #[error("网络错误：{0}")]
    Network(String),
    #[error("请求超时")]
    Timeout,
    #[error("接口返回无效：{0}")]
    BadResponse(String),
}

impl ApiError {
    /// Quota / billing exhaustion (often reported as 429) — retrying will not help.
    pub fn is_quota(&self) -> bool {
        match self {
            ApiError::Http { status, body, .. } => {
                *status == 402
                    || (*status == 429 && {
                        let b = body.to_ascii_lowercase();
                        b.contains("insufficient_quota") || b.contains("exceeded_current_quota") || b.contains("billing") || b.contains("balance")
                    })
            }
            _ => false,
        }
    }

    /// Errors worth retrying with the same request.
    pub fn is_transient(&self) -> bool {
        if self.is_quota() {
            return false;
        }
        match self {
            ApiError::Http { status, .. } => matches!(status, 408 | 409 | 425 | 429 | 500..=599),
            ApiError::Network(_) | ApiError::Timeout | ApiError::BadResponse(_) => true,
        }
    }
    pub fn is_auth(&self) -> bool {
        matches!(self, ApiError::Http { status: 401 | 403, .. }) || self.is_quota()
    }

    fn body_lower(&self) -> Option<String> {
        match self {
            ApiError::Http { body, .. } => Some(body.to_lowercase()),
            _ => None,
        }
    }

    /// A parameter-validation message ("temperature参数非法", "invalid parameter"):
    /// a configuration problem even when it mentions lengths or tokens.
    fn is_param_error(&self) -> bool {
        let b = self.body_lower().unwrap_or_default();
        ["参数非法", "参数有误", "参数错误", "invalid parameter", "invalid_parameter", "unknown parameter", "unrecognized request argument", "extra inputs are not permitted"]
            .iter()
            .any(|k| b.contains(k))
    }

    /// The request was too large for the model (split the batch and retry).
    pub fn is_context_length(&self) -> bool {
        match self {
            ApiError::Http { status: 413, .. } => true,
            ApiError::Http { status: 400 | 422, .. } if !self.is_param_error() => {
                let b = self.body_lower().unwrap_or_default();
                [
                    "context_length_exceeded",
                    "context length",
                    "context window",
                    "too long",
                    "too many tokens",
                    "token limit",
                    "prompt is too",
                    "input length",
                    "maximum length",
                    "exceeds the maximum",
                    "超长",
                    "过长",
                    "超过最大",
                    "上下文",
                ]
                .iter()
                .any(|k| b.contains(k))
            }
            _ => false,
        }
    }

    /// The provider refused this content (moderation): isolate the offending segment.
    pub fn is_content_filter(&self) -> bool {
        match self {
            ApiError::Http { status: 400 | 422 | 451, .. } if !self.is_param_error() => {
                let b = self.body_lower().unwrap_or_default();
                [
                    "content_filter",
                    "contentfilter",
                    "content filter",
                    "content_policy",
                    "content policy",
                    "content management policy",
                    "datainspectionfailed",
                    "data_inspection_failed",
                    "inappropriate content",
                    "sensitive content",
                    "sensitive words",
                    "moderation",
                    "safety system",
                    "\"1301\"",
                    "敏感",
                    "审核",
                    "不安全",
                    "违规",
                ]
                .iter()
                .any(|k| b.contains(k))
            }
            _ => false,
        }
    }

    /// What the operator should check, for errors that pause translation.
    pub fn hint(&self) -> Option<&'static str> {
        if self.is_quota() {
            Some("额度不足或账户欠费：请检查服务商账户余额。")
        } else if self.is_auth() {
            Some("API Key 无效或无权使用该模型：请检查 Key 与模型名称。")
        } else if self.is_config() {
            Some("接口拒绝了请求：请检查 API 地址、模型名称、Temperature、max_tokens 与附加请求参数。")
        } else if matches!(self, ApiError::Http { status: 429, .. }) {
            Some("请求过于频繁：可降低最大并发或设置每分钟请求上限。")
        } else {
            None
        }
    }

    /// The request itself is invalid for this provider (bad parameter, unknown model,
    /// wrong endpoint): every request will fail until the configuration is fixed.
    pub fn is_config(&self) -> bool {
        matches!(self, ApiError::Http { status: 400 | 404 | 405 | 415 | 422, .. }) && !self.is_context_length() && !self.is_content_filter()
    }

    /// Errors that indicate the provider is unhealthy (count toward the circuit breaker).
    pub fn is_provider_failure(&self) -> bool {
        match self {
            ApiError::Http { status, .. } => matches!(status, 408 | 429 | 500..=599) || self.is_auth() || self.is_config(),
            ApiError::Network(_) | ApiError::Timeout => true,
            ApiError::BadResponse(_) => false,
        }
    }

    /// Errors that need a configuration change: open the circuit right away.
    pub fn needs_operator(&self) -> bool {
        self.is_auth() || self.is_config()
    }
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            ApiError::Http { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

/// A chat model backend (OpenAI-compatible HTTP API in production, mocks in tests).
pub trait ChatBackend: Send + Sync {
    fn chat(&self, req: ChatRequest) -> BoxFuture<'_, Result<ChatResponse, ApiError>>;
    /// Identifier used in cache keys (provider + model).
    fn model_id(&self) -> String;
}

pub struct OpenAiCompat {
    pub client: reqwest::Client,
    pub endpoint: String,
    pub api_key: String,
    pub model: String,
    pub timeout: Duration,
    pub extra_body: Option<Value>,
}

/// Temperature rounded to two decimals as f64.
pub fn round_temperature(t: f32) -> f64 {
    ((t as f64) * 100.0).round() / 100.0
}

/// Build the chat-completions endpoint from a configured base URL.
pub fn endpoint_for(base: &str) -> String {
    let b = base.trim().trim_end_matches('/');
    if b.ends_with("/chat/completions") {
        b.to_string()
    } else {
        format!("{b}/chat/completions")
    }
}

fn parse_retry_after(v: Option<&reqwest::header::HeaderValue>) -> Option<Duration> {
    let s = v?.to_str().ok()?.trim();
    if let Ok(secs) = s.parse::<f64>() {
        return Some(Duration::from_secs_f64(secs.clamp(0.0, 3600.0)));
    }
    let when = chrono::DateTime::parse_from_rfc2822(s).ok()?;
    let delta = when.timestamp() - chrono::Utc::now().timestamp();
    Some(Duration::from_secs(delta.clamp(0, 3600) as u64))
}

impl ChatBackend for OpenAiCompat {
    fn model_id(&self) -> String {
        self.model.clone()
    }

    fn chat(&self, req: ChatRequest) -> BoxFuture<'_, Result<ChatResponse, ApiError>> {
        Box::pin(async move {
            let mut body = json!({
                "model": self.model,
                "messages": [
                    {"role": "system", "content": req.system},
                    {"role": "user", "content": req.user},
                ],
                "stream": false,
            });
            if let Some(t) = req.temperature {
                // f32 -> JSON would yield 0.20000000298023224, which providers such as
                // Zhipu GLM reject ("限制小数点[2]位"); send a 2-decimal f64.
                body["temperature"] = json!(round_temperature(t));
            }
            if let Some(m) = req.max_tokens {
                body["max_tokens"] = json!(m);
            }
            if req.json_mode {
                body["response_format"] = json!({"type": "json_object"});
            }
            if let Some(Value::Object(extra)) = &self.extra_body {
                for (k, v) in extra {
                    body[k] = v.clone();
                }
            }
            let mut rb = self.client.post(&self.endpoint).timeout(self.timeout).json(&body);
            if !self.api_key.is_empty() {
                rb = rb.bearer_auth(&self.api_key);
            }
            let resp = rb.send().await.map_err(|e| {
                if e.is_timeout() { ApiError::Timeout } else { ApiError::Network(format!("{e}")) }
            })?;
            let status = resp.status().as_u16();
            let retry_after = resp
                .headers()
                .get("retry-after-ms")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(Duration::from_millis)
                .or_else(|| parse_retry_after(resp.headers().get(reqwest::header::RETRY_AFTER)));
            let text = resp.text().await.map_err(|e| {
                if e.is_timeout() { ApiError::Timeout } else { ApiError::Network(format!("{e}")) }
            })?;
            if !(200..300).contains(&status) {
                return Err(ApiError::Http { status, body: crate::util::truncate_chars(text.trim(), 500), retry_after });
            }
            let v: Value = serde_json::from_str(&text)
                .map_err(|e| ApiError::BadResponse(format!("{e}: {}", crate::util::truncate_chars(&text, 200))))?;
            if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
                return Err(ApiError::BadResponse(format!("provider error: {}", crate::util::truncate_chars(&err.to_string(), 300))));
            }
            let choice = v
                .get("choices")
                .and_then(|c| c.get(0))
                .ok_or_else(|| ApiError::BadResponse("no choices in response".into()))?;
            let content = choice
                .pointer("/message/content")
                .and_then(|c| c.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
            let finish_reason = choice.get("finish_reason").and_then(|f| f.as_str()).map(|s| s.to_string());
            let usage = v.get("usage");
            let pt = usage.and_then(|u| u.get("prompt_tokens")).and_then(|x| x.as_u64()).unwrap_or(0);
            let ct = usage.and_then(|u| u.get("completion_tokens")).and_then(|x| x.as_u64()).unwrap_or(0);
            Ok(ChatResponse { content, finish_reason, prompt_tokens: pt, completion_tokens: ct })
        })
    }
}

/// Remove reasoning blocks and markdown fences around a model answer.
pub fn clean_model_output(s: &str) -> String {
    let mut t = s.to_string();
    // <think>…</think> / <thinking>…</thinking> (possibly unterminated at the start)
    for (open, close) in [("<think>", "</think>"), ("<thinking>", "</thinking>"), ("<reasoning>", "</reasoning>")] {
        while let Some(start) = t.find(open) {
            match t[start..].find(close) {
                Some(end) => t.replace_range(start..start + end + close.len(), ""),
                None => {
                    t.truncate(start);
                    break;
                }
            }
        }
        if let Some(end) = t.find(close) {
            // closing tag without opening tag: drop everything before it
            t = t[end + close.len()..].to_string();
        }
    }
    let trimmed = t.trim();
    let trimmed = if let Some(rest) = trimmed.strip_prefix("```") {
        let rest = rest.trim_start_matches(|c: char| c.is_ascii_alphanumeric());
        rest.trim_end().strip_suffix("```").unwrap_or(rest).trim()
    } else {
        trimmed
    };
    trimmed.to_string()
}

/// Parse `<seg id="N">…</seg>` blocks. Unclosed (truncated) trailing blocks are ignored.
pub fn parse_seg_tags(answer: &str) -> Option<Vec<(usize, String)>> {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"(?is)<seg\s+id\s*=\s*["']?(\d+)["']?\s*>(.*?)</seg\s*>"#).unwrap()
    });
    let cleaned = clean_model_output(answer);
    let mut out = Vec::new();
    for c in RE.captures_iter(&cleaned) {
        if let Ok(id) = c[1].parse::<usize>() {
            out.push((id, c[2].trim().to_string()));
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Parse a model answer in either the tag or the JSON format.
pub fn parse_any(answer: &str) -> Option<Vec<(usize, String)>> {
    parse_seg_tags(answer).or_else(|| parse_translations(answer))
}

/// Parse translations from a model answer: accepts {"translations":[{id,text}]},
/// a bare array, {"segments":[...]}, or an {"id": "text"} map. Returns (id, text) pairs.
pub fn parse_translations(answer: &str) -> Option<Vec<(usize, String)>> {
    let cleaned = clean_model_output(answer);
    let v = parse_json_lenient(&cleaned)?;
    let list = match &v {
        Value::Array(a) => Some(a.clone()),
        Value::Object(o) => o
            .get("translations")
            .or_else(|| o.get("segments"))
            .or_else(|| o.get("results"))
            .or_else(|| o.get("data"))
            .and_then(|x| x.as_array().cloned()),
        _ => None,
    };
    let mut out = Vec::new();
    if let Some(list) = list {
        for (i, item) in list.iter().enumerate() {
            match item {
                Value::Object(o) => {
                    let id = o
                        .get("id")
                        .and_then(|x| x.as_u64().or_else(|| x.as_str().and_then(|s| s.trim().parse().ok())))
                        .map(|x| x as usize);
                    let text = ["text", "translation", "translated", "t", "content", "target"]
                        .iter()
                        .find_map(|k| o.get(*k).and_then(|x| x.as_str()))
                        .map(|s| s.to_string());
                    if let (Some(id), Some(text)) = (id, text) {
                        out.push((id, text));
                    }
                }
                Value::String(s) => out.push((usize::MAX - i, s.clone())), // positional, resolved by caller
                _ => {}
            }
        }
        return Some(out);
    }
    if let Value::Object(o) = &v {
        for (k, val) in o {
            if let (Ok(id), Some(text)) = (k.trim().parse::<usize>(), val.as_str()) {
                out.push((id, text.to_string()));
            }
        }
        if !out.is_empty() {
            return Some(out);
        }
    }
    None
}

/// Parse the first JSON value in a string, tolerating leading/trailing chatter.
fn parse_json_lenient(s: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(s) {
        return Some(v);
    }
    let start = s.find(['{', '['])?;
    let slice = &s[start..];
    let mut de = serde_json::Deserializer::from_str(slice).into_iter::<Value>();
    match de.next() {
        Some(Ok(v)) => Some(v),
        _ => {
            // try the last closing bracket
            let end = slice.rfind(['}', ']'])?;
            serde_json::from_str(&slice[..=end]).ok()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_building() {
        assert_eq!(endpoint_for("https://api.openai.com/v1/"), "https://api.openai.com/v1/chat/completions");
        assert_eq!(endpoint_for("http://h:11434/v1/chat/completions"), "http://h:11434/v1/chat/completions");
    }

    #[test]
    fn cleans_reasoning_and_fences() {
        assert_eq!(clean_model_output("<think>hmm\nlet me</think>\n```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(clean_model_output("reasoning...</think>{\"a\":1}"), "{\"a\":1}");
        assert_eq!(clean_model_output("{\"a\":1}"), "{\"a\":1}");
    }

    #[test]
    fn parses_seg_tags() {
        let a = parse_any("<think>x</think>Here:\n<seg id=\"1\">你好 <g1>世界</g1></seg>\n<seg id='2'>再见</seg>\n<seg id=\"3\">trunc").unwrap();
        assert_eq!(a, vec![(1, "你好 <g1>世界</g1>".to_string()), (2, "再见".to_string())]);
        let b = parse_any("<seg id=5>\nmulti\nline\n</seg>").unwrap();
        assert_eq!(b, vec![(5, "multi\nline".to_string())]);
        assert!(parse_any("nothing").is_none());
        assert!(parse_any(r#"{"translations":[{"id":1,"text":"x"}]}"#).is_some());
    }

    #[test]
    fn temperature_has_two_decimals() {
        assert_eq!(serde_json::json!(round_temperature(0.2)).to_string(), "0.2");
        assert_eq!(serde_json::json!(round_temperature(0.2 + 0.1)).to_string(), "0.3");
        assert_eq!(serde_json::json!(round_temperature(0.95)).to_string(), "0.95");
        assert_eq!(serde_json::json!(round_temperature(1.0)).to_string(), "1.0");
    }

    #[test]
    fn classifies_400_errors() {
        let glm = ApiError::Http { status: 400, body: r#"{"error":{"code":"1210","message":"temperature参数非法：限制小数点[2]位"}}"#.into(), retry_after: None };
        assert!(glm.is_config() && glm.needs_operator() && !glm.is_transient() && !glm.is_context_length());
        let ctx = ApiError::Http { status: 400, body: "This model's maximum context length is 8192 tokens".into(), retry_after: None };
        assert!(ctx.is_context_length() && !ctx.is_config());
        let filt = ApiError::Http { status: 400, body: r#"{"error":{"code":"1301","message":"系统检测到输入或生成内容可能包含不安全或敏感内容"}}"#.into(), retry_after: None };
        assert!(filt.is_content_filter() && !filt.is_config());
        let model = ApiError::Http { status: 404, body: "model not found".into(), retry_after: None };
        assert!(model.is_config());
        let busy = ApiError::Http { status: 503, body: "overloaded".into(), retry_after: None };
        assert!(busy.is_transient() && !busy.needs_operator());
        assert!(glm.hint().is_some_and(|h| h.contains("Temperature")) && busy.hint().is_none());
        // Parameter errors that mention tokens/lengths are still configuration errors.
        let mt = ApiError::Http { status: 400, body: r#"{"error":{"code":"1210","message":"max_tokens参数非法"}}"#.into(), retry_after: None };
        assert!(mt.is_config() && !mt.is_context_length());
        let cs = ApiError::Http { status: 400, body: "The model name is case-sensitive: GLM-4-Flash not found".into(), retry_after: None };
        assert!(cs.is_config() && !cs.is_content_filter());
        for body in ["Your request exceeded model token limit: 8192", "Range of input length should be [1, 30720]", r#"{"error":{"code":"context_length_exceeded"}}"#] {
            let e = ApiError::Http { status: 400, body: body.into(), retry_after: None };
            assert!(e.is_context_length() && !e.is_config(), "{body}");
        }
    }

    #[test]
    fn quota_errors_not_transient() {
        let e = ApiError::Http { status: 429, body: "{\"error\":{\"code\":\"insufficient_quota\"}}".into(), retry_after: None };
        assert!(!e.is_transient());
        assert!(e.is_auth());
        let e2 = ApiError::Http { status: 429, body: "rate limited".into(), retry_after: None };
        assert!(e2.is_transient());
    }

    #[test]
    fn parses_variants() {
        let a = parse_translations(r#"{"translations":[{"id":1,"text":"你好"},{"id":"2","text":"世界"}]}"#).unwrap();
        assert_eq!(a, vec![(1, "你好".to_string()), (2, "世界".to_string())]);
        let b = parse_translations(r#"Sure! Here you go: [{"id": 3, "translation": "x"}] hope it helps"#).unwrap();
        assert_eq!(b, vec![(3, "x".to_string())]);
        let c = parse_translations(r#"{"1": "a", "2": "b"}"#).unwrap();
        assert_eq!(c.len(), 2);
        assert!(parse_translations("not json at all").is_none());
        assert!(parse_translations(r#"{"translations":[{"id":1,"text":"trunc"#).is_none());
    }
}

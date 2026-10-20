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
    #[error("network error: {0}")]
    Network(String),
    #[error("request timed out")]
    Timeout,
    #[error("invalid API response: {0}")]
    BadResponse(String),
}

impl ApiError {
    /// Errors worth retrying with the same request.
    pub fn is_transient(&self) -> bool {
        match self {
            ApiError::Http { status, .. } => matches!(status, 408 | 409 | 425 | 429 | 500..=599),
            ApiError::Network(_) | ApiError::Timeout | ApiError::BadResponse(_) => true,
        }
    }
    pub fn is_auth(&self) -> bool {
        matches!(self, ApiError::Http { status: 401 | 403, .. })
    }
    /// Errors that indicate the provider is unhealthy (count toward the circuit breaker).
    pub fn is_provider_failure(&self) -> bool {
        match self {
            ApiError::Http { status, .. } => matches!(status, 401 | 403 | 404 | 408 | 429 | 500..=599),
            ApiError::Network(_) | ApiError::Timeout => true,
            ApiError::BadResponse(_) => false,
        }
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
                body["temperature"] = json!(t);
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
            let retry_after = parse_retry_after(resp.headers().get(reqwest::header::RETRY_AFTER));
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

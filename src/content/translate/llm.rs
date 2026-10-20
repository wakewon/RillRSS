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
    /// An error object inside a 200 response (e.g. an upstream failure mid-generation).
    #[error("服务商返回错误：{0}")]
    Provider(String),
    /// The answer is empty or not in the requested format.
    #[error("模型输出无法使用：{detail}")]
    Unparseable { truncated: bool, detail: String },
    /// The provider cut the answer off by moderation (finish_reason content_filter / sensitive).
    #[error("模型输出被内容审核拦截（finish_reason={0}）")]
    Filtered(String),
    /// A failure every request will hit until the configuration changes, recognized from
    /// the pattern of errors rather than from the error itself.
    #[error("{detail}")]
    Config { detail: String, hint: &'static str },
}

const HINT_QUOTA: &str = "额度不足或账户欠费：请检查服务商账户余额。";
const HINT_AUTH: &str = "API Key 无效或无权使用该模型：请检查 Key 与模型名称。";
pub const HINT_CONFIG: &str = "接口拒绝了请求：请检查 API 地址、模型名称、Temperature、max_tokens 与附加请求参数。";
pub const HINT_TOO_LARGE: &str =
    "很短的请求也超出了模型的上下文长度：请减小或取消 max_tokens、缩短附加提示词，或换用上下文更长的模型。";
pub const HINT_NO_OUTPUT: &str = "模型在输出译文前就达到了长度上限：请提高或取消 max_tokens；推理（思考）模型会先消耗大量 token，可换用非推理模型或在附加请求参数中关闭思考。";
const HINT_TRUNCATED: &str = "模型输出在第一段译文完成前就被截断：请提高或取消 max_tokens。";
const HINT_FORMAT: &str =
    "模型没有按要求的格式输出译文（空内容或无法解析）：请换用更强的模型；推理模型请关闭思考，也可尝试 JSON 输出格式。";
const HINT_BAD_RESPONSE: &str = "接口返回的不是 chat completions 响应：请检查 API 地址（通常以 /v1 结尾）。";
const HINT_RATE: &str = "请求过于频繁：可降低最大并发或设置每分钟请求上限。";

/// Request too large for the model, even inside a parameter error (DashScope reports
/// "Range of input length …" as invalid_parameter_error).
const CONTEXT_STRONG: &[&str] = &[
    "context_length_exceeded",
    "context length",
    "context window",
    "context size",
    "exceed_context_size",
    "input length",
    "input token count",
    "max_seq_len",
    "prompt is too long",
    "too many tokens",
    "token limit",
];
/// Weaker signs of a request too large, ignored in parameter errors
/// ("max_tokens参数非法：超过最大值").
const CONTEXT_WEAK: &[&str] = &["too long", "maximum length", "exceeds the maximum", "超长", "过长", "超过最大", "上下文"];
/// The provider refused the content (moderation).
const FILTER: &[&str] = &[
    "content_filter",
    "contentfilter",
    "content filter",
    "content_policy",
    "content policy",
    "content management policy",
    "content exists risk",
    "high risk",
    "datainspectionfailed",
    "data_inspection_failed",
    "inappropriate content",
    "sensitive content",
    "sensitive words",
    "moderation",
    "flagged",
    "safety system",
    "\"1301\"",
    "敏感",
    "审核",
    "不安全",
    "违规",
];
/// Moderation reported as 403 (OpenRouter: "… requires moderation … flagged").
const FILTER_403: &[&str] = &["requires moderation", "moderation", "flagged"];
/// The request itself is wrong (parameter, model, key): every request fails the same way.
const OPERATOR: &[&str] = &[
    "参数",
    "invalid parameter",
    "invalid_parameter",
    "unknown parameter",
    "unrecognized request argument",
    "extra inputs are not permitted",
    "unsupported",
    "not supported",
    "不支持",
    "invalid value",
    "invalid_value",
    "not found",
    "not_found",
    "does not exist",
    "not exist",
    "不存在",
    "unknown model",
    "invalid model",
    "api key",
    "api_key",
];
/// Exhausted quota or balance. Other 429s are rate limits, even when their text points
/// at billing (Gemini, Groq).
const QUOTA: &[&str] = &[
    "insufficient_quota",
    "exceeded_current_quota",
    "insufficient balance",
    "insufficient_balance",
    "balance is insufficient",
    "arrearage",
    "good standing",
    "余额不足",
    "欠费",
    "充值",
    "调用次数限额",
    "使用上限",
    "套餐已到期",
    "\"1113\"",
    "\"1304\"",
    "\"1308\"",
    "\"1309\"",
    "\"1310\"",
];

impl ApiError {
    fn body_has(&self, keys: &[&str]) -> bool {
        match self {
            ApiError::Http { body, .. } => {
                let b = body.to_lowercase();
                keys.iter().any(|k| b.contains(k))
            }
            _ => false,
        }
    }

    /// Quota / billing exhaustion — retrying will not help.
    pub fn is_quota(&self) -> bool {
        match self {
            ApiError::Http { status: 402, .. } => true,
            ApiError::Http { status: 400 | 403 | 429, .. } => self.body_has(QUOTA),
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
            ApiError::Network(_) | ApiError::Timeout | ApiError::BadResponse(_) | ApiError::Provider(_) => true,
            ApiError::Unparseable { .. } | ApiError::Filtered(_) | ApiError::Config { .. } => false,
        }
    }

    pub fn is_auth(&self) -> bool {
        match self {
            ApiError::Http { status: 401, .. } => true,
            ApiError::Http { status: 403, .. } => !self.is_content_filter(),
            _ => self.is_quota(),
        }
    }

    /// A parameter, model or key problem ("temperature参数非法", "model not found"): a
    /// configuration problem even when it mentions lengths or tokens.
    fn has_operator_marker(&self) -> bool {
        self.body_has(OPERATOR)
    }

    /// The request was too large for the model (split the batch and retry).
    pub fn is_context_length(&self) -> bool {
        match self {
            ApiError::Http { status: 413, .. } => true,
            ApiError::Http { status: 400 | 422, .. } => {
                self.body_has(CONTEXT_STRONG) || (!self.has_operator_marker() && self.body_has(CONTEXT_WEAK))
            }
            _ => false,
        }
    }

    /// The provider refused this content (moderation): isolate the offending segment.
    pub fn is_content_filter(&self) -> bool {
        match self {
            ApiError::Http { status: 400 | 422 | 451, .. } => !self.has_operator_marker() && self.body_has(FILTER),
            ApiError::Http { status: 403, .. } => self.body_has(FILTER_403),
            ApiError::Filtered(_) => true,
            _ => false,
        }
    }

    /// An unrecognized client error: probably about this content (an unknown moderation
    /// message), until an unrelated request fails the same way.
    pub fn is_ambiguous(&self) -> bool {
        matches!(self, ApiError::Http { status: 400 | 422, .. })
            && !self.has_operator_marker()
            && !self.is_context_length()
            && !self.is_content_filter()
            && !self.is_quota()
    }

    /// What the operator should check, for errors that need a configuration change.
    pub fn hint(&self) -> Option<&'static str> {
        Some(match self {
            ApiError::Config { hint, .. } => hint,
            _ if self.is_quota() => HINT_QUOTA,
            _ if self.is_auth() => HINT_AUTH,
            _ if self.is_config() => HINT_CONFIG,
            ApiError::Unparseable { truncated: true, .. } => HINT_TRUNCATED,
            ApiError::Unparseable { .. } => HINT_FORMAT,
            ApiError::BadResponse(_) => HINT_BAD_RESPONSE,
            ApiError::Http { status: 429, .. } => HINT_RATE,
            _ => return None,
        })
    }

    /// The request itself is invalid for this provider (bad parameter, unknown model,
    /// wrong endpoint): every request will fail until the configuration is fixed.
    pub fn is_config(&self) -> bool {
        match self {
            ApiError::Http { status: 404 | 405 | 415, .. } | ApiError::Config { .. } => true,
            ApiError::Http { status: 400 | 422, .. } => {
                self.has_operator_marker() && !self.is_context_length() && !self.is_content_filter() && !self.is_quota()
            }
            _ => false,
        }
    }

    /// Errors that indicate the provider is unhealthy or unusable (count toward the
    /// circuit breaker).
    pub fn is_provider_failure(&self) -> bool {
        match self {
            ApiError::Http { status, .. } => matches!(status, 408 | 429 | 500..=599) || self.is_auth() || self.is_config(),
            ApiError::Network(_)
            | ApiError::Timeout
            | ApiError::BadResponse(_)
            | ApiError::Provider(_)
            | ApiError::Unparseable { .. }
            | ApiError::Config { .. } => true,
            ApiError::Filtered(_) => false,
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

    /// The error message without request-specific details (ids, numbers), to recognize
    /// the same error across requests.
    pub fn fingerprint(&self) -> String {
        let ApiError::Http { status, body, .. } = self else { return self.to_string() };
        let message = serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|v| {
                ["/error/message", "/message", "/0/error/message", "/error"]
                    .iter()
                    .find_map(|p| v.pointer(p).and_then(|m| m.as_str()).map(|m| m.to_string()))
            })
            .unwrap_or_else(|| body.clone());
        let message: String = message.chars().filter(|c| !c.is_ascii_digit()).take(200).collect();
        format!("{status} {}", message.to_lowercase())
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

/// Longest wait honoured from a provider's retry hint.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(3600);

fn parse_retry_after(v: Option<&reqwest::header::HeaderValue>) -> Option<Duration> {
    let s = v?.to_str().ok()?.trim();
    if let Ok(secs) = s.parse::<f64>() {
        // "NaN" and "inf" parse as f64 too: a malformed header must not panic.
        return (secs.is_finite() && secs >= 0.0).then(|| Duration::from_secs_f64(secs.min(MAX_RETRY_AFTER.as_secs_f64())));
    }
    let when = chrono::DateTime::parse_from_rfc2822(s).ok()?;
    let delta = when.timestamp() - chrono::Utc::now().timestamp();
    Some(Duration::from_secs(delta.clamp(0, MAX_RETRY_AFTER.as_secs() as i64) as u64))
}

fn parse_retry_after_ms(v: Option<&reqwest::header::HeaderValue>) -> Option<Duration> {
    let ms = v?.to_str().ok()?.trim().parse::<u64>().ok()?;
    Some(Duration::from_millis(ms).min(MAX_RETRY_AFTER))
}

/// Gemini puts the wait into the 429 body: "retryDelay": "17s".
fn retry_delay_in_body(body: &str) -> Option<Duration> {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#""retryDelay"\s*:\s*"(\d+(?:\.\d+)?)s""#).unwrap());
    let secs: f64 = RE.captures(body)?[1].parse().ok()?;
    Some(Duration::from_secs_f64(secs.min(MAX_RETRY_AFTER.as_secs_f64())))
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
            let retry_after = parse_retry_after_ms(resp.headers().get("retry-after-ms"))
                .or_else(|| parse_retry_after(resp.headers().get(reqwest::header::RETRY_AFTER)));
            let text = resp.text().await.map_err(|e| {
                if e.is_timeout() { ApiError::Timeout } else { ApiError::Network(format!("{e}")) }
            })?;
            if !(200..300).contains(&status) {
                let retry_after = retry_after.or_else(|| if status == 429 { retry_delay_in_body(&text) } else { None });
                return Err(ApiError::Http { status, body: crate::util::truncate_chars(text.trim(), 500), retry_after });
            }
            let v: Value = serde_json::from_str(&text)
                .map_err(|e| ApiError::BadResponse(format!("{e}: {}", crate::util::truncate_chars(&text, 200))))?;
            if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
                return Err(ApiError::Provider(crate::util::truncate_chars(&err.to_string(), 300)));
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
    // ASCII case folding keeps byte offsets valid in the original string.
    let find = |hay: &str, needle: &str| hay.to_ascii_lowercase().find(needle);
    let mut t = s.to_string();
    // <think>…</think> / <thinking>…</thinking>, any case (possibly unterminated at the start)
    for (open, close) in [("<think>", "</think>"), ("<thinking>", "</thinking>"), ("<reasoning>", "</reasoning>")] {
        while let Some(start) = find(&t, open) {
            match find(&t[start..], close) {
                Some(end) => t.replace_range(start..start + end + close.len(), ""),
                None => {
                    t.truncate(start);
                    break;
                }
            }
        }
        if let Some(end) = find(&t, close) {
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
    parse_seg_tags(answer).or_else(|| parse_translations(answer)).filter(|v| !v.is_empty())
}

/// A plain answer without tags, acceptable for a single-segment request: not empty, not
/// structured, not a refusal or a preamble.
pub fn is_plain_answer(s: &str) -> bool {
    const CHATTER: &[&str] =
        &["sorry", "i'm sorry", "i am sorry", "i can't", "i cannot", "as an ai", "here is", "here's", "抱歉", "对不起", "我无法", "我不能"];
    let l = s.trim_start().to_lowercase();
    !l.is_empty() && !l.contains("<seg") && !l.starts_with(['{', '[']) && !CHATTER.iter().any(|p| l.starts_with(p))
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
    fn classifies_real_provider_errors() {
        let e = |status: u16, body: &str| ApiError::Http { status, body: body.into(), retry_after: None };
        // Request too large (split), even inside a parameter error.
        for (status, body) in [
            (400, r#"{"error":{"code":"invalid_parameter_error","param":null,"message":"Range of input length should be [1, 30720]","type":"invalid_request_error"},"id":"chatcmpl-x","request_id":"x"}"#),
            (400, r#"{"error":{"code":400,"message":"request (9000 tokens) exceeds the available context size (8192 tokens), try increasing it","type":"exceed_context_size_error"}}"#),
            (400, r#"{"error":{"code":"1261","message":"Prompt 超长"}}"#),
            (400, r#"[{"error":{"code":400,"message":"The input token count (1234567) exceeds the maximum number of tokens allowed (1048576).","status":"INVALID_ARGUMENT"}}]"#),
        ] {
            let x = e(status, body);
            assert!(x.is_context_length() && !x.is_config() && !x.needs_operator(), "{body}");
        }
        // Moderation (isolate the segment), not a configuration problem.
        for (status, body) in [
            (400, r#"{"error":{"message":"Content Exists Risk","type":"invalid_request_error","param":null,"code":"invalid_request_error"}}"#),
            (403, r#"{"error":{"code":403,"message":"openai/gpt-4o requires moderation on OpenRouter. Your input was flagged for \"violence\". No credits were charged."}}"#),
            (400, r#"{"error":{"code":"data_inspection_failed","param":null,"message":"Input data may contain inappropriate content.","type":"data_inspection_failed"}}"#),
            (400, r#"{"error":{"message":"The request was rejected because it was considered high risk","type":"content_filter"}}"#),
        ] {
            let x = e(status, body);
            assert!(x.is_content_filter() && !x.needs_operator() && !x.is_provider_failure(), "{body}");
        }
        // Rate limits stay transient even when they point at billing.
        let gemini = e(429, r#"[{"error":{"code":429,"message":"You exceeded your current quota, please check your plan and billing details. For more information on this error, head to: https://ai.google.dev/gemini-api/docs/rate-limits.","status":"RESOURCE_EXHAUSTED"}}]"#);
        assert!(gemini.is_transient() && !gemini.is_quota() && !gemini.needs_operator());
        assert!(gemini.hint().is_some_and(|h| h.contains("请求过于频繁")));
        // Exhausted balance in Chinese (Zhipu 1113 / 1304 at 429, DashScope Arrearage at 400).
        for (status, body) in [
            (429, r#"{"error":{"code":"1113","message":"余额不足或无可用资源包,请充值。"}}"#),
            (429, r#"{"error":{"code":"1304","message":"该API已达今日调用次数限额，如有更多需求，请联系客服购买"}}"#),
            (400, r#"{"error":{"code":"Arrearage","message":"Access denied, please make sure your account is in good standing.","type":"Arrearage"}}"#),
        ] {
            let x = e(status, body);
            assert!(x.is_quota() && x.needs_operator() && !x.is_transient(), "{body}");
            assert!(x.hint().is_some_and(|h| h.contains("额度不足")), "{body}");
        }
        // An unrecognized 400 is ambiguous: neither split-worthy nor an immediate pause.
        let unknown = e(400, r#"{"error":{"message":"Bad request","type":"invalid_request_error"}}"#);
        assert!(unknown.is_ambiguous() && !unknown.needs_operator() && !unknown.is_provider_failure());
        assert!(!e(400, r#"{"error":{"code":"1210","message":"temperature参数非法：限制小数点[2]位"}}"#).is_ambiguous());
        // The same error from another request is recognized despite ids and numbers.
        let a = e(400, r#"{"error":{"message":"Rejected request 123","type":"x"},"id":"chatcmpl-1"}"#);
        let b = e(400, r#"{"error":{"message":"Rejected request 456","type":"x"},"id":"chatcmpl-2"}"#);
        assert_eq!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn retry_after_is_finite_and_bounded() {
        use reqwest::header::HeaderValue;
        let ra = |s: &'static str| parse_retry_after(Some(&HeaderValue::from_static(s)));
        assert_eq!(ra("NaN"), None);
        assert_eq!(ra("inf"), None);
        assert_eq!(ra("-5"), None);
        assert_eq!(ra("1e300"), Some(MAX_RETRY_AFTER));
        assert_eq!(ra("2.5"), Some(Duration::from_millis(2500)));
        assert_eq!(parse_retry_after_ms(Some(&HeaderValue::from_static("18446744073709551615"))), Some(MAX_RETRY_AFTER));
        assert_eq!(parse_retry_after_ms(Some(&HeaderValue::from_static("1500"))), Some(Duration::from_millis(1500)));
        assert_eq!(retry_delay_in_body(r#"{"details":[{"retryDelay": "17s"}]}"#), Some(Duration::from_secs(17)));
    }

    #[test]
    fn plain_answers_and_reasoning_blocks() {
        assert!(is_plain_answer("首相表示税收将上涨。"));
        assert!(!is_plain_answer("Sorry, I can't help with that."));
        assert!(!is_plain_answer("  "));
        assert!(!is_plain_answer(r#"{"translations": []}"#));
        // Reasoning blocks in any case are removed, so drafts inside them do not count.
        assert_eq!(parse_any("<THINK>draft <seg id=\"1\">x</seg></THINK><seg id=\"1\">你好</seg>"), Some(vec![(1, "你好".to_string())]));
        // A JSON array without usable items is no answer at all.
        assert_eq!(parse_any("首相表示 [1] 税收将上涨"), None);
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

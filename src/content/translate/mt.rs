//! Free machine translation without an API key: Google (the page translator built into
//! Chrome) and Microsoft (the Edge browser's translator).
//!
//! Both take a batch of segments and return one translation per segment, in order.
//! Segments carry inline markup as placeholders (`<gN>…</gN>`, `<xN/>`):
//! - Google translates HTML and moves real tags with the words they wrap, but treats
//!   unknown tags as noise (sentences fall apart). Placeholders go out as
//!   `<span id="gN">…</span>` and `<img id="xN">` and are mapped back.
//! - Microsoft's free endpoint has no HTML mode: attributes get curly quotes and tags
//!   with attributes are escaped. The bare placeholder tags survive as they are.

use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::{Value, json};

use super::Engine;
use super::lang;
use super::llm::ApiError;

/// A machine-translation service.
pub trait MtBackend: Send + Sync {
    /// Translate `texts` (segment text with placeholders) into `target`.
    fn translate(&self, texts: Vec<String>, target: String) -> BoxFuture<'_, Result<Vec<String>, ApiError>>;
}

/// Characters and segments per request.
pub fn batch_limits(engine: Engine) -> (usize, usize) {
    match engine {
        // Google answers 429 to requests over about 30 KB.
        Engine::Google => (5000, 50),
        // Microsoft's limit is 50 000 characters per request.
        _ => (10_000, 50),
    }
}

const TIMEOUT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";

async fn send(req: reqwest::RequestBuilder) -> Result<String, ApiError> {
    let resp = req.header(reqwest::header::USER_AGENT, USER_AGENT).timeout(TIMEOUT).send().await.map_err(|e| {
        if e.is_timeout() { ApiError::Timeout } else { ApiError::Network(format!("{e}")) }
    })?;
    let status = resp.status();
    let retry_after = super::llm::retry_after(resp.headers());
    let body = resp.text().await.map_err(|e| ApiError::Network(format!("{e}")))?;
    if !status.is_success() {
        // Google's 429 is a whole HTML page.
        let body = if body.trim_start().starts_with('<') { "（HTML 错误页）".to_string() } else { crate::util::truncate_chars(body.trim(), 300) };
        return Err(ApiError::Http { status: status.as_u16(), body, retry_after });
    }
    Ok(body)
}

fn bad(what: &str, body: &str) -> ApiError {
    ApiError::BadResponse(format!("{what}：{}", crate::util::truncate_chars(body, 120)))
}

// ---------------------------------------------------------------------------
// Google
// ---------------------------------------------------------------------------

/// The public key of Chrome's page translator.
const GOOGLE_KEY: &str = "AIzaSyATBXajvzQLTDHEQbcpq0Ihe0vWDHmO520";
const GOOGLE_HTML: &str = "https://translate-pa.googleapis.com/v1/translateHtml";
/// The older endpoint of the same service, used when the one above refuses the
/// request outright (a rotated key, a changed endpoint).
const GOOGLE_T: &str = "https://translate.googleapis.com/translate_a/t";

pub struct Google {
    client: reqwest::Client,
    html_url: String,
    t_url: String,
}

impl Google {
    pub fn new(client: reqwest::Client) -> Google {
        Google::with_urls(client, GOOGLE_HTML, GOOGLE_T)
    }

    pub fn with_urls(client: reqwest::Client, html_url: &str, t_url: &str) -> Google {
        Google { client, html_url: html_url.into(), t_url: t_url.into() }
    }

    async fn translate_html(&self, texts: &[String], target: &str) -> Result<Vec<String>, ApiError> {
        let body = json!([[texts, "auto", target], "wt_lib"]).to_string();
        let req = self
            .client
            .post(&self.html_url)
            .header(reqwest::header::CONTENT_TYPE, "application/json+protobuf")
            .header("X-Goog-API-Key", GOOGLE_KEY)
            .body(body);
        let body = send(req).await?;
        // [[translation, …], [detected language, …]]
        let v: Value = serde_json::from_str(&body).map_err(|_| bad("Google 返回无法解析", &body))?;
        v.get(0)
            .and_then(|a| a.as_array())
            .map(|a| a.iter().map(|t| t.as_str().unwrap_or_default().to_string()).collect())
            .ok_or_else(|| bad("Google 返回格式不符", &body))
    }

    async fn translate_t(&self, texts: &[String], target: &str) -> Result<Vec<String>, ApiError> {
        let form: Vec<(&str, &str)> = texts.iter().map(|t| ("q", t.as_str())).collect();
        let url = format!("{}?client=gtx&sl=auto&tl={target}&format=html", self.t_url);
        let body = send(self.client.post(url).form(&form)).await?;
        // With sl=auto each entry is [translation, language]; a single text may come
        // back as a bare string or a bare pair.
        let v: Value = serde_json::from_str(&body).map_err(|_| bad("Google 返回无法解析", &body))?;
        let text_of = |e: &Value| e.as_str().map(str::to_string).or_else(|| e.get(0).and_then(|t| t.as_str()).map(str::to_string));
        match &v {
            Value::String(s) if texts.len() == 1 => Ok(vec![s.clone()]),
            Value::Array(a) if texts.len() == 1 && a.len() == 2 && a.iter().all(|e| e.is_string()) => Ok(vec![text_of(&v).unwrap_or_default()]),
            Value::Array(a) => Ok(a.iter().map(|e| text_of(e).unwrap_or_default()).collect()),
            _ => Err(bad("Google 返回格式不符", &body)),
        }
    }
}

impl MtBackend for Google {
    fn translate(&self, texts: Vec<String>, target: String) -> BoxFuture<'_, Result<Vec<String>, ApiError>> {
        Box::pin(async move {
            let target = google_lang(&target);
            let sent: Vec<String> = texts.iter().map(|t| to_html(t)).collect();
            let out = match self.translate_html(&sent, &target).await {
                Ok(out) => out,
                // Refused outright (not a rate limit or an outage): try the other endpoint.
                Err(ApiError::Http { status: 400 | 401 | 403 | 404, .. }) | Err(ApiError::BadResponse(_)) => {
                    self.translate_t(&sent, &target).await?
                }
                Err(e) => return Err(e),
            };
            Ok(out.iter().map(|t| from_html(t)).collect())
        })
    }
}

/// Google's language codes ("zh-CN", "zh-TW").
fn google_lang(code: &str) -> String {
    match lang::norm(code).as_str() {
        "zh" | "zh-cn" | "zh-hans" | "zh-sg" | "zh-hans-cn" => "zh-CN".into(),
        "zh-tw" | "zh-hant" | "zh-hk" | "zh-mo" | "zh-hant-tw" | "zh-hant-hk" => "zh-TW".into(),
        "he" => "iw".into(),
        c => c.to_string(),
    }
}

/// Placeholders to the tags Google keeps with their words.
fn to_html(text: &str) -> String {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(r"<(/?)([gx])(\d+)(/?)>").unwrap());
    RE.replace_all(text, |c: &regex::Captures| match (&c[1], &c[2]) {
        ("/", _) => "</span>".to_string(),
        (_, "g") => format!("<span id=\"g{}\">", &c[3]),
        _ => format!("<img id=\"x{}\">", &c[3]),
    })
    .into_owned()
}

/// Google's tags back to placeholders. A closing tag closes the innermost open one.
fn from_html(text: &str) -> String {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#"<span id="g(\d+)">|</span>|<img id="x(\d+)"\s*/?>"#).unwrap());
    let mut open: Vec<String> = Vec::new();
    RE.replace_all(text, |c: &regex::Captures| {
        if let Some(n) = c.get(1) {
            open.push(n.as_str().to_string());
            format!("<g{}>", n.as_str())
        } else if let Some(n) = c.get(2) {
            format!("<x{}/>", n.as_str())
        } else {
            open.pop().map(|n| format!("</g{n}>")).unwrap_or_default()
        }
    })
    .into_owned()
}

// ---------------------------------------------------------------------------
// Microsoft
// ---------------------------------------------------------------------------

const MICROSOFT: &str = "https://edge.microsoft.com/translate/translatetext";

pub struct Microsoft {
    client: reqwest::Client,
    url: String,
}

impl Microsoft {
    pub fn new(client: reqwest::Client) -> Microsoft {
        Microsoft::with_url(client, MICROSOFT)
    }

    pub fn with_url(client: reqwest::Client, url: &str) -> Microsoft {
        Microsoft { client, url: url.into() }
    }
}

impl MtBackend for Microsoft {
    fn translate(&self, texts: Vec<String>, target: String) -> BoxFuture<'_, Result<Vec<String>, ApiError>> {
        Box::pin(async move {
            let url = format!("{}?from=&to={}&isEnterpriseClient=false", self.url, microsoft_lang(&target));
            let req = self.client.post(url).header(reqwest::header::CONTENT_TYPE, "application/json").body(json!(texts).to_string());
            let body = send(req).await?;
            // [{"translations": [{"text": …, "to": …}], "detectedLanguage": …}, …]
            let v: Value = serde_json::from_str(&body).map_err(|_| bad("微软翻译返回无法解析", &body))?;
            let items = v.as_array().ok_or_else(|| bad("微软翻译返回格式不符", &body))?;
            items
                .iter()
                .map(|i| {
                    i.pointer("/translations/0/text")
                        .and_then(|t| t.as_str())
                        .map(str::to_string)
                        .ok_or_else(|| bad("微软翻译返回格式不符", &body))
                })
                .collect()
        })
    }
}

/// Microsoft's language codes ("zh-Hans", "zh-Hant").
fn microsoft_lang(code: &str) -> String {
    match lang::norm(code).as_str() {
        "zh" | "zh-cn" | "zh-hans" | "zh-sg" | "zh-hans-cn" => "zh-Hans".into(),
        "zh-tw" | "zh-hant" | "zh-hant-tw" | "zh-hk" | "zh-mo" | "zh-hant-hk" => "zh-Hant".into(),
        "pt-br" => "pt".into(),
        "pt" | "pt-pt" => "pt-pt".into(),
        "fr-ca" => "fr-ca".into(),
        c => c.split('-').next().unwrap_or(c).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn google_markup_round_trip() {
        let src = "Shares of <x1/> fell after <g2>Jane <g3>Doe</g3></g2> said a &lt; b.";
        let html = to_html(src);
        assert_eq!(html, "Shares of <img id=\"x1\"> fell after <span id=\"g2\">Jane <span id=\"g3\">Doe</span></span> said a &lt; b.");
        assert_eq!(from_html(&html), src);
        // As Google answers: words and tags reordered.
        assert_eq!(
            from_html("股票<img id=\"x1\"><span id=\"g2\">Jane <span id=\"g3\">Doe</span></span>表示 a &lt; b 后下跌。"),
            "股票<x1/><g2>Jane <g3>Doe</g3></g2>表示 a &lt; b 后下跌。"
        );
        // A stray closing tag is dropped (validation reports the missing placeholder).
        assert_eq!(from_html("a</span>b"), "ab");
    }

    #[test]
    fn language_codes() {
        assert_eq!(google_lang("zh-CN"), "zh-CN");
        assert_eq!(google_lang("zh_hant"), "zh-TW");
        assert_eq!(google_lang("ja"), "ja");
        assert_eq!(microsoft_lang("zh-CN"), "zh-Hans");
        assert_eq!(microsoft_lang("zh-TW"), "zh-Hant");
        assert_eq!(microsoft_lang("en-GB"), "en");
        assert_eq!(microsoft_lang("pt-BR"), "pt");
    }
}

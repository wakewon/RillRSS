//! Full-text extraction: built-in Readability port, CSS selectors, external service.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use url::Url;

use super::dom::{self, MAX_PAGE_DEPTH, fragment_html, parse_fragment};
use crate::http::{self, HttpClients};
use crate::util::{html::base_href, html::escape, text};

#[derive(Debug, Clone, Default)]
pub struct Extracted {
    pub html: String,
    pub title: Option<String>,
    pub byline: Option<String>,
    pub lang: Option<String>,
    pub image: Option<String>,
    pub modified_time: Option<String>,
    pub text_chars: usize,
}

fn text_chars(html: &str) -> usize {
    text::html_to_text(html).chars().filter(|c| !c.is_whitespace()).count()
}

/// Parse a fetched page, refusing pages nested deeper than browsers render: html5ever's
/// parse time grows with depth × size and Readability recurses over the tree (a deep
/// enough page overflows the stack and aborts the process). The caller falls back to
/// the feed content.
fn parse_page(page_html: &str) -> Result<dom_query::Document> {
    if dom::nesting_depth(page_html, MAX_PAGE_DEPTH) > MAX_PAGE_DEPTH {
        bail!("page is nested too deeply to extract (over {MAX_PAGE_DEPTH} levels)");
    }
    let doc = dom_query::Document::from(page_html);
    if dom::tree_depth(&doc.root(), MAX_PAGE_DEPTH) > MAX_PAGE_DEPTH {
        bail!("page is nested too deeply to extract (over {MAX_PAGE_DEPTH} levels)");
    }
    Ok(doc)
}

/// The address the page's relative URLs resolve against: its `<base href>` (relative to
/// the page) or else the page's own, final URL.
fn page_base(doc: &dom_query::Document, page_url: &str) -> Option<Url> {
    let page = Url::parse(page_url).ok()?;
    let base = doc.select("base[href]").attr("href").and_then(|h| page.join(h.trim()).ok());
    Some(base.filter(|u| matches!(u.scheme(), "http" | "https")).unwrap_or(page))
}

/// Prefix extracted HTML with its base as `<base href>`, so that the transform stage
/// resolves relative URLs against the page they came from (`sanitize` honours it and
/// drops the element) rather than against the feed's link.
fn with_base(html: String, base: Option<&Url>) -> String {
    match base {
        Some(b) => format!("<base href=\"{}\">{html}", escape(b.as_str())),
        None => html,
    }
}

/// Built-in extractor (port of Mozilla Readability). `url` is the page's final URL.
pub fn readability(page_html: &str, url: &str) -> Result<Extracted> {
    let doc = parse_page(page_html)?;
    let base = page_base(&doc, url);
    let cfg = dom_smoothie::Config { max_elements_to_parse: 0, ..Default::default() };
    let mut r = dom_smoothie::Readability::with_document(doc, Some(url), Some(cfg))
        .map_err(|e| anyhow!("readability init: {e}"))?;
    let a = r.parse().map_err(|e| anyhow!("readability: {e}"))?;
    let html = a.content.to_string();
    // Drop the readability wrapper div.
    let html = unwrap_readability_page(&html);
    Ok(Extracted {
        text_chars: text_chars(&html),
        html: with_base(html, base.as_ref()),
        title: Some(a.title).filter(|t| !t.trim().is_empty()),
        byline: a.byline.filter(|b| !b.trim().is_empty()),
        lang: a.lang,
        image: a.image,
        modified_time: a.modified_time,
    })
}

fn unwrap_readability_page(html: &str) -> String {
    let trimmed = html.trim();
    if trimmed.starts_with("<div id=\"readability-page-1\"") {
        let doc = parse_fragment(trimmed);
        if let Some(n) = doc.select("#readability-page-1").nodes().first() {
            return n.inner_html().to_string();
        }
    }
    html.to_string()
}

/// Extract by CSS selector(s): all matches concatenated in document order. `page_url` is
/// the page's final URL.
pub fn by_selector(page_html: &str, selector: &str, page_url: &str) -> Result<Extracted> {
    let selector = selector.trim();
    if selector.is_empty() {
        bail!("no CSS selector configured");
    }
    let doc = parse_page(page_html)?;
    let sel = doc.try_select(selector).ok_or_else(|| anyhow!("selector matched nothing: {selector}"))?;
    let mut html = String::new();
    for n in sel.nodes() {
        html.push_str(&n.html());
    }
    if html.trim().is_empty() {
        bail!("selector matched nothing: {selector}");
    }
    let title = doc.select("title").text().trim().to_string();
    Ok(Extracted {
        text_chars: text_chars(&html),
        html: with_base(html, page_base(&doc, page_url).as_ref()),
        title: Some(title).filter(|t| !t.is_empty()),
        ..Default::default()
    })
}

/// External extractor: GET `template` with `{url}` replaced by the encoded article URL.
/// JSON responses may carry the HTML in `content`, `html`, `article.content` or
/// `data.content` (and the page's final address in `url`); anything else is treated as
/// the HTML itself. Relative URLs in it resolve against that address or the article URL.
pub async fn external(http: &HttpClients, template: &str, url: &str, timeout: Duration) -> Result<Extracted> {
    if template.trim().is_empty() {
        bail!("external extractor URL is not configured");
    }
    let encoded = percent_encoding::utf8_percent_encode(url, percent_encoding::NON_ALPHANUMERIC).to_string();
    let endpoint = template.replace("{url}", &encoded).replace("{raw_url}", url);
    let resp = http
        .api
        .get(&endpoint)
        .timeout(timeout)
        .send()
        .await
        .context("external extractor request")?;
    let f = http::read_limited(resp, 20 * 1024 * 1024).await?;
    if f.status >= 400 {
        bail!("external extractor returned HTTP {}", f.status);
    }
    let ct = f.content_type().unwrap_or_default();
    let body = String::from_utf8_lossy(&f.body).to_string();
    let (html, title, page_url) = if ct.contains("json") || body.trim_start().starts_with('{') {
        let v: serde_json::Value = serde_json::from_str(&body).context("external extractor JSON")?;
        let pick = |v: &serde_json::Value, paths: &[&[&str]]| -> Option<String> {
            for p in paths {
                let mut cur = v;
                let mut ok = true;
                for k in *p {
                    match cur.get(*k) {
                        Some(x) => cur = x,
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                if ok && let Some(s) = cur.as_str() && !s.trim().is_empty() {
                    return Some(s.to_string());
                }
            }
            None
        };
        let html = pick(&v, &[&["content"], &["html"], &["article", "content"], &["data", "content"], &["result", "content"]])
            .ok_or_else(|| anyhow!("external extractor JSON has no content field"))?;
        let page_url = pick(&v, &[&["url"], &["final_url"], &["article", "url"], &["data", "url"]]);
        (html, pick(&v, &[&["title"], &["article", "title"], &["data", "title"]]), page_url)
    } else {
        (body, None, None)
    };
    let base = page_url
        .and_then(|u| Url::parse(&u).ok())
        .filter(|u| matches!(u.scheme(), "http" | "https"))
        .or_else(|| Url::parse(url).ok());
    let text_chars = text_chars(&html);
    let html = if base_href(&html).is_some() { html } else { with_base(html, base.as_ref()) };
    Ok(Extracted { text_chars, html, title, ..Default::default() })
}

/// Remove elements matching `remove_selector` from extracted HTML.
pub fn apply_remove(html: &str, remove_selector: &str) -> String {
    if remove_selector.trim().is_empty() {
        return html.to_string();
    }
    let doc = parse_fragment(&dom::flatten_deep(html, MAX_PAGE_DEPTH));
    if let Some(s) = doc.try_select(remove_selector.trim()) {
        s.remove();
    }
    fragment_html(&doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> String {
        let para = "<p>The government announced on Monday that it would introduce new rules for private car parks, giving drivers the right to challenge charges they believe are unfair. Officials said the code would be enforced from next year.</p>";
        format!(
            r#"<html><head><title>Parking rules | News</title></head><body>
            <nav><a href="/">Home</a><a href="/news">News</a></nav>
            <div class="sidebar"><a href="/x">Most read 1</a><a href="/y">Most read 2</a></div>
            <article><h1>Parking rules</h1><div class="body">{}</div><figure><img src="/img/park.jpg" alt="car park"><figcaption>A car park</figcaption></figure></article>
            <footer>Copyright</footer></body></html>"#,
            para.repeat(6)
        )
    }

    #[test]
    fn readability_extracts_article() {
        let e = readability(&page(), "https://news.example.com/parking").unwrap();
        assert!(e.text_chars > 500, "{}", e.text_chars);
        assert!(e.html.contains("challenge charges"));
        assert!(!e.html.contains("Most read"));
        assert!(!e.html.contains("readability-page-1"));
    }

    #[test]
    fn selector_extracts() {
        let e = by_selector(&page(), "article .body", "https://news.example.com/parking").unwrap();
        assert!(e.html.contains("challenge charges"));
        assert!(!e.html.contains("car park\""));
        assert_eq!(e.title.as_deref(), Some("Parking rules | News"));
        assert!(by_selector(&page(), ".nope", "https://news.example.com/parking").is_err());
    }

    #[test]
    fn remove_selector_works() {
        let h = "<p>a</p><div class=\"promo\">buy</div>";
        assert_eq!(apply_remove(h, ".promo"), "<p>a</p>");
        let h = "<base href=\"https://a.example/x/\"><p>a</p><div class=\"promo\">buy</div>";
        assert_eq!(apply_remove(h, ".promo"), "<base href=\"https://a.example/x/\"><p>a</p>");
    }

    #[test]
    fn relative_urls_follow_the_page_not_the_feed_link() {
        use crate::content::sanitize::sanitize;
        let feed_link = Url::parse("https://feeds.example.net/r/site/~3/abc123/").unwrap();
        let page = page().replace(
            "<div class=\"body\">",
            "<div class=\"body\"><img src=\"images/photo.jpg\"><img data-src=\"/lazy/12345\"><a href=\"related.html\">rel</a>",
        );
        // Selector mode: the final URL after redirects.
        let e = by_selector(&page, "article .body", "https://www.site.example/2024/10/story.html").unwrap();
        let out = sanitize(&e.html, Some(&feed_link));
        assert!(out.contains("\"https://www.site.example/2024/10/images/photo.jpg\""), "{out}");
        assert!(out.contains("\"https://www.site.example/2024/10/related.html\""), "{out}");
        // ... or the page's own <base href>.
        let with_base = page.replace("<head>", "<head><base href=\"https://cdn.site.example/2024/\">");
        let e = by_selector(&with_base, "article .body", "https://www.site.example/2024/10/story.html").unwrap();
        let out = sanitize(&e.html, Some(&feed_link));
        assert!(out.contains("\"https://cdn.site.example/2024/images/photo.jpg\""), "{out}");
        // Readability: lazy images it leaves alone resolve against the page too.
        let e = readability(&page, "https://www.site.example/2024/10/story.html").unwrap();
        let out = sanitize(&e.html, Some(&feed_link));
        assert!(out.contains("\"https://www.site.example/lazy/12345\""), "{out}");
        assert!(!out.contains("feeds.example.net"), "{out}");
    }

    #[test]
    fn deeply_nested_pages_are_refused() {
        for n in [600, 20_000] {
            let deep = page().replace("<footer>", &format!("{}x{}<footer>", "<div>".repeat(n), "</div>".repeat(n)));
            let e = readability(&deep, "https://news.example.com/parking").unwrap_err();
            assert!(e.to_string().contains("nested too deeply"), "{e}");
            assert!(by_selector(&deep, "article .body", "https://news.example.com/parking").is_err());
        }
        // Ordinary nesting is fine.
        let ok = page().replace("<footer>", &format!("{}x{}<footer>", "<div>".repeat(100), "</div>".repeat(100)));
        assert!(readability(&ok, "https://news.example.com/parking").is_ok());
    }
}

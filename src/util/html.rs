//! Small HTML helpers.

use std::sync::LazyLock;

use regex::Regex;

/// Escape text for safe inclusion in HTML text or attribute values.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Placeholder used in stored HTML for this server's own absolute base URL. It is
/// replaced at response time so cached-image URLs follow the configured/public URL.
pub const BASE_MARKER: &str = "rill-base://local";

/// Replace the base-URL marker with the real base (no trailing slash).
pub fn expand_base(html: &str, base: &str) -> String {
    if html.contains(BASE_MARKER) {
        html.replace(BASE_MARKER, base.trim_end_matches('/'))
    } else {
        html.to_string()
    }
}

static BASE_HREF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)<base(?:\s[^>]*?)?\shref\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'>]+))"#).unwrap());

/// The `href` of the first `<base>` element of an HTML document or fragment.
pub fn base_href(html: &str) -> Option<String> {
    let c = BASE_HREF.captures(html)?;
    let v = c.get(1).or(c.get(2)).or(c.get(3))?.as_str();
    let v = html_escape::decode_html_entities(v).trim().to_string();
    (!v.is_empty()).then_some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes() {
        assert_eq!(escape("<a href=\"x\">'&'</a>"), "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;&lt;/a&gt;");
    }

    #[test]
    fn finds_base_href() {
        assert_eq!(base_href("<head><BASE target=_blank href='https://a.example/x/?a=1&amp;b=2'></head>").as_deref(), Some("https://a.example/x/?a=1&b=2"));
        assert_eq!(base_href("<base href=/blog/><base href=\"/other/\">").as_deref(), Some("/blog/"));
        assert_eq!(base_href("<base data-href=\"/no/\"><p>x</p>"), None);
        assert_eq!(base_href("<p>&lt;base href=\"/no/\"&gt;</p>"), None);
    }

    #[test]
    fn expands_marker() {
        let h = format!("<img src=\"{BASE_MARKER}/img/ab.jpg\">");
        assert_eq!(expand_base(&h, "https://rss.example.com/"), "<img src=\"https://rss.example.com/img/ab.jpg\">");
    }
}

//! Small HTML helpers.

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes() {
        assert_eq!(escape("<a href=\"x\">'&'</a>"), "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;&lt;/a&gt;");
    }

    #[test]
    fn expands_marker() {
        let h = format!("<img src=\"{BASE_MARKER}/img/ab.jpg\">");
        assert_eq!(expand_base(&h, "https://rss.example.com/"), "<img src=\"https://rss.example.com/img/ab.jpg\">");
    }
}

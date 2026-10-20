//! URL / GUID normalization used for story identity.

use std::sync::LazyLock;

use regex::Regex;
use url::Url;

/// Query parameters that only carry tracking information.
fn is_tracking_param(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    if n.starts_with("utm_") || n.starts_with("at_") || n.starts_with("mc_") || n.starts_with("_hs") {
        return true;
    }
    matches!(
        n.as_str(),
        "fbclid"
            | "gclid"
            | "dclid"
            | "gbraid"
            | "wbraid"
            | "msclkid"
            | "yclid"
            | "twclid"
            | "igshid"
            | "ocid"
            | "cmpid"
            | "cmp"
            | "xtor"
            | "ito"
            | "mkt_tok"
            | "_ga"
            | "_gl"
            | "spm"
            | "share"
            | "ref_src"
            | "ref_url"
            | "rss"
            | "from_rss"
            | "via"
            | "s_cid"
            | "ns_mchannel"
            | "ns_source"
            | "ns_campaign"
            | "ns_linkname"
            | "ns_fee"
            | "feed_id"
            | "smid"
            | "smtyp"
    )
}

/// Canonical key for an article link: scheme-less, lowercase host, no default port,
/// no fragment, no tracking parameters, sorted query, no trailing slash.
/// Returns an empty string for unusable input.
pub fn url_key(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return String::new();
    }
    let Ok(mut url) = Url::parse(raw) else {
        return raw.to_string();
    };
    if !matches!(url.scheme(), "http" | "https") {
        return raw.to_string();
    }
    url.set_fragment(None);
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| !is_tracking_param(k))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    pairs.sort();
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
    let port = match url.port() {
        Some(p) => format!(":{p}"),
        None => String::new(),
    };
    let mut path = url.path().to_string();
    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    let mut key = format!("{host}{port}{path}");
    if !pairs.is_empty() {
        let q = serde_urlencoded::to_string(&pairs).unwrap_or_default();
        key.push('?');
        key.push_str(&q);
    }
    key
}

static SHORT_NUMERIC_FRAGMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(https?://[^#\s]+)#\d{1,3}$").unwrap());

/// Normalized guid used for identity. URL-like guids carrying a short numeric fragment
/// (e.g. BBC's `https://www.bbc.co.uk/news/articles/xyz#2`, where the number is a
/// front-page section index that changes over time) are reduced to the bare URL.
pub fn guid_key(raw: &str) -> String {
    let g = raw.trim();
    if let Some(c) = SHORT_NUMERIC_FRAGMENT.captures(g) {
        return c[1].to_string();
    }
    g.to_string()
}

/// Resolve a possibly-relative URL against a base.
pub fn resolve(base: Option<&Url>, href: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty() {
        return None;
    }
    if href.starts_with("data:") || href.starts_with("javascript:") {
        return None;
    }
    if let Ok(u) = Url::parse(href) {
        return Some(u.to_string());
    }
    let base = base?;
    base.join(href).ok().map(|u| u.to_string())
}

/// Extract the host of a URL (lowercase), if any.
pub fn host_of(raw: &str) -> Option<String> {
    Url::parse(raw)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_tracking_and_fragment() {
        assert_eq!(
            url_key("https://www.bbc.co.uk/news/articles/c50me4w4x1vyo?at_medium=RSS&at_campaign=rss"),
            "bbc.co.uk/news/articles/c50me4w4x1vyo"
        );
        assert_eq!(
            url_key("http://Example.com:8080/a/b/?utm_source=x&id=3&b=2#frag"),
            "example.com:8080/a/b?b=2&id=3"
        );
        assert_eq!(url_key("https://example.com/"), "example.com/");
        assert_eq!(
            url_key("https://www.bbc.com/zhongwen/articles/cwp9g811d5ydo/trad?at_medium=RSS&at_campaign=rss"),
            "bbc.com/zhongwen/articles/cwp9g811d5ydo/trad"
        );
        assert_eq!(url_key(""), "");
    }

    #[test]
    fn scheme_insensitive() {
        assert_eq!(url_key("http://a.com/x"), url_key("https://a.com/x"));
    }

    #[test]
    fn guid_fragment_handling() {
        assert_eq!(
            guid_key("https://www.bbc.co.uk/news/articles/cj62y9dnd6pwo#2"),
            "https://www.bbc.co.uk/news/articles/cj62y9dnd6pwo"
        );
        // Long / non-numeric fragments are meaningful (e.g. live-blog posts).
        assert_eq!(
            guid_key("https://example.com/live#post-12"),
            "https://example.com/live#post-12"
        );
        assert_eq!(guid_key("https://example.com/live#123456"), "https://example.com/live#123456");
        assert_eq!(guid_key("383e2acf01ad8ffb1c19cb201311ed07"), "383e2acf01ad8ffb1c19cb201311ed07");
        assert_eq!(guid_key("tag:example.com,2024:1#2"), "tag:example.com,2024:1#2");
    }

    #[test]
    fn resolves_relative() {
        let base = Url::parse("https://example.com/news/a.html").unwrap();
        assert_eq!(resolve(Some(&base), "/img/x.png").unwrap(), "https://example.com/img/x.png");
        assert_eq!(resolve(Some(&base), "b.png").unwrap(), "https://example.com/news/b.png");
        assert_eq!(resolve(Some(&base), "//cdn.example.com/c.png").unwrap(), "https://cdn.example.com/c.png");
        assert!(resolve(Some(&base), "javascript:alert(1)").is_none());
        assert!(resolve(None, "x.png").is_none());
    }
}

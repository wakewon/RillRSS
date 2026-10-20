//! HTML normalization + sanitization for article content.

use std::collections::HashSet;
use std::sync::LazyLock;

use ammonia::{Builder, UrlRelative};
use regex::Regex;
use url::Url;

use super::dom::{fragment_html, parse_fragment, tag_name, unwrap_element};
use crate::util::urlnorm;

const LAZY_SRC_ATTRS: &[&str] = &[
    "data-src",
    "data-original",
    "data-lazy-src",
    "data-actualsrc",
    "data-hi-res-src",
    "data-full-src",
    "data-orig-file",
    "data-original-src",
    "data-lazyload",
    "data-echo",
];
const LAZY_SRCSET_ATTRS: &[&str] = &["data-srcset", "data-lazy-srcset", "data-original-srcset"];

static PLACEHOLDER_SRC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(^data:|placeholder|blank\.(gif|png)|spacer\.gif|transparent\.(gif|png)|1x1\.|pixel\.gif|grey-placeholder|lazy[-_]?load|loading\.gif)").unwrap()
});

static VIDEO_EMBED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(https?:)?//(www\.)?(youtube(-nocookie)?\.com/embed/([\w-]+)|player\.vimeo\.com/video/(\d+)|player\.bilibili\.com/player\.html\?.*?bvid=(\w+))").unwrap()
});

/// A parsed `srcset` candidate.
#[derive(Debug, Clone, PartialEq)]
pub struct SrcsetCandidate {
    pub url: String,
    /// Width descriptor (w) or density * 1000 (x) or 0.
    pub width: u32,
    pub density: f32,
}

pub fn parse_srcset(srcset: &str) -> Vec<SrcsetCandidate> {
    // URLs may contain commas (rare); split on ", " boundaries followed by a URL-ish token.
    let mut out = Vec::new();
    for part in srcset.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let mut it = part.split_whitespace();
        let Some(url) = it.next() else { continue };
        let desc = it.next().unwrap_or("");
        let (mut width, mut density) = (0u32, 0f32);
        if let Some(w) = desc.strip_suffix('w') {
            width = w.parse().unwrap_or(0);
        } else if let Some(x) = desc.strip_suffix('x') {
            density = x.parse().unwrap_or(0.0);
        }
        out.push(SrcsetCandidate { url: url.to_string(), width, density });
    }
    out
}

/// Pick the best image URL from a srcset for mobile reading (close to 1280px wide).
pub fn best_from_srcset(srcset: &str) -> Option<String> {
    let c = parse_srcset(srcset);
    if c.is_empty() {
        return None;
    }
    if c.iter().any(|x| x.width > 0) {
        let mut ws: Vec<&SrcsetCandidate> = c.iter().filter(|x| x.width > 0).collect();
        ws.sort_by_key(|x| x.width);
        // smallest candidate >= 1000px, else the largest
        return ws
            .iter()
            .find(|x| x.width >= 1000)
            .or(ws.last())
            .map(|x| x.url.clone());
    }
    let mut ds: Vec<&SrcsetCandidate> = c.iter().collect();
    ds.sort_by(|a, b| a.density.partial_cmp(&b.density).unwrap_or(std::cmp::Ordering::Equal));
    ds.iter().find(|x| x.density >= 2.0).or(ds.last()).map(|x| x.url.clone())
}

fn resolve_srcset(srcset: &str, base: Option<&Url>) -> String {
    parse_srcset(srcset)
        .into_iter()
        .filter_map(|c| {
            let u = urlnorm::resolve(base, &c.url)?;
            Some(if c.width > 0 {
                format!("{u} {}w", c.width)
            } else if c.density > 0.0 {
                format!("{u} {}x", c.density)
            } else {
                u
            })
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// DOM-level cleanup before sanitizing: lazy images, <picture>, <noscript>, embeds,
/// removal of non-content elements.
pub fn preprocess(html: &str, base: Option<&Url>) -> String {
    let doc = parse_fragment(html);

    // Unwrap <noscript> (often contains the real <img>).
    for n in doc.select("noscript").nodes().to_vec() {
        let inner = n.text().to_string();
        if inner.contains("<img") {
            n.replace_with_html(inner);
        } else {
            unwrap_element(&n);
        }
    }

    // <picture>: make sure the inner <img> has a src, then unwrap.
    for pic in doc.select("picture").nodes().to_vec() {
        let source_srcset = pic
            .descendants()
            .into_iter()
            .filter(|n| tag_name(n).as_deref() == Some("source"))
            .find_map(|s| s.attr("srcset").or_else(|| s.attr("data-srcset")).map(|v| v.to_string()));
        for d in pic.descendants() {
            if tag_name(&d).as_deref() == Some("source") {
                d.remove_from_parent();
            }
        }
        if let Some(img) = pic.descendants().into_iter().find(|n| tag_name(n).as_deref() == Some("img"))
            && img.attr("src").is_none_or(|s| s.trim().is_empty() || PLACEHOLDER_SRC.is_match(&s))
            && let Some(best) = source_srcset.as_deref().and_then(best_from_srcset)
        {
            img.set_attr("src", &best);
        }
        unwrap_element(&pic);
    }

    // Lazy-loaded images.
    for img in doc.select("img").nodes().to_vec() {
        let src = img.attr("src").map(|s| s.trim().to_string()).unwrap_or_default();
        let needs = src.is_empty() || PLACEHOLDER_SRC.is_match(&src);
        if needs {
            if let Some(real) = LAZY_SRC_ATTRS.iter().find_map(|a| img.attr(a).map(|v| v.trim().to_string()).filter(|v| !v.is_empty() && !v.starts_with("data:"))) {
                img.set_attr("src", &real);
            } else if let Some(ss) = LAZY_SRCSET_ATTRS
                .iter()
                .chain(["srcset"].iter())
                .find_map(|a| img.attr(a).map(|v| v.to_string()))
                .and_then(|s| best_from_srcset(&s))
            {
                img.set_attr("src", &ss);
            } else {
                img.remove_from_parent();
                continue;
            }
        }
        if img.attr("srcset").is_none()
            && let Some(ss) = LAZY_SRCSET_ATTRS.iter().find_map(|a| img.attr(a).map(|v| v.to_string()))
        {
            img.set_attr("srcset", &ss);
        }
        if let Some(ss) = img.attr("srcset") {
            let resolved = resolve_srcset(&ss, base);
            if resolved.is_empty() {
                img.remove_attr("srcset");
            } else {
                img.set_attr("srcset", &resolved);
            }
        }
    }

    // De-duplicate identical consecutive images (lazy + noscript copies).
    let mut seen: HashSet<String> = HashSet::new();
    for img in doc.select("img").nodes().to_vec() {
        if let Some(src) = img.attr("src") {
            let key = urlnorm::resolve(base, &src).unwrap_or_else(|| src.to_string());
            if !seen.insert(key) {
                img.remove_from_parent();
            }
        }
    }

    // Video embeds become links; other iframes are dropped.
    for f in doc.select("iframe").nodes().to_vec() {
        let src = f.attr("src").map(|s| s.to_string()).unwrap_or_default();
        if let Some(c) = VIDEO_EMBED.captures(&src) {
            let link = if let Some(id) = c.get(5) {
                format!("https://www.youtube.com/watch?v={}", id.as_str())
            } else if let Some(id) = c.get(6) {
                format!("https://vimeo.com/{}", id.as_str())
            } else if let Some(id) = c.get(7) {
                format!("https://www.bilibili.com/video/{}", id.as_str())
            } else {
                src.clone()
            };
            f.replace_with_html(format!(
                "<p><a href=\"{}\">▶ {}</a></p>",
                crate::util::html::escape(&link),
                crate::util::html::escape(&link)
            ));
        } else {
            f.remove_from_parent();
        }
    }

    if let Some(sel) = doc.try_select("script, style, link, meta, form, input, button, select, textarea, svg, canvas, object, embed, template, nav, dialog") {
        sel.remove();
    }
    fragment_html(&doc)
}

fn builder(base: Option<&Url>) -> Builder<'static> {
    let mut b = Builder::default();
    b.add_tags(&[
        "figure", "figcaption", "video", "audio", "source", "track", "details", "summary", "time", "mark", "section",
        "article", "header", "footer", "main", "aside",
    ]);
    b.add_tag_attributes("img", &["srcset", "sizes", "title"]);
    b.add_tag_attributes("video", &["src", "poster", "controls", "width", "height", "preload"]);
    b.add_tag_attributes("audio", &["src", "controls", "preload"]);
    b.add_tag_attributes("source", &["src", "srcset", "type", "media"]);
    b.add_tag_attributes("track", &["src", "kind", "srclang", "label"]);
    b.add_tag_attributes("time", &["datetime"]);
    b.add_tag_attributes("a", &["title"]);
    b.add_generic_attributes(&["lang", "dir"]);
    b.link_rel(Some("noopener noreferrer"));
    match base {
        Some(u) => {
            b.url_relative(UrlRelative::RewriteWithBase(u.clone()));
        }
        None => {
            b.url_relative(UrlRelative::PassThrough);
        }
    }
    b
}

/// Full cleanup: preprocess + allowlist sanitization + empty-element pruning.
pub fn sanitize(html: &str, base: Option<&Url>) -> String {
    let pre = preprocess(html, base);
    let clean = builder(base).clean(&pre).to_string();
    prune_empty(&clean)
}

/// Remove empty paragraphs/divs/spans that only contain whitespace.
fn prune_empty(html: &str) -> String {
    let doc = parse_fragment(html);
    for _ in 0..3 {
        let mut changed = false;
        for n in doc.select("p, div, span, section, figure, figcaption, li, strong, b, em, i, a, blockquote, header, footer, aside").nodes().to_vec() {
            if n.text().trim().is_empty()
                && !n.descendants().iter().any(|d| matches!(tag_name(d).as_deref(), Some("img" | "video" | "audio" | "br" | "hr" | "table")))
            {
                n.remove_from_parent();
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    fragment_html(&doc)
}

/// All image `src` URLs in an HTML fragment.
pub fn image_urls(html: &str) -> Vec<String> {
    let doc = parse_fragment(html);
    doc.select("img")
        .nodes()
        .iter()
        .filter_map(|n| n.attr("src").map(|s| s.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://news.example.com/a/b.html").unwrap()
    }

    #[test]
    fn strips_dangerous_content() {
        let h = r#"<p onclick="x()">Hi<script>alert(1)</script><a href="javascript:alert(1)">x</a><style>p{}</style></p><form><input></form>"#;
        let out = sanitize(h, Some(&base()));
        assert!(!out.contains("script"));
        assert!(!out.contains("onclick"));
        assert!(!out.contains("javascript"));
        assert!(!out.contains("<form"));
        assert!(out.contains("Hi"));
    }

    #[test]
    fn resolves_relative_and_lazy_images() {
        let h = r#"<p>a</p><img src="data:image/gif;base64,R0lGOD" data-src="/img/1.jpg"><img src="2.png" srcset="2-400.png 400w, 2-1200.png 1200w"><img class="x">"#;
        let out = sanitize(h, Some(&base()));
        assert!(out.contains(r#"src="https://news.example.com/img/1.jpg""#), "{out}");
        assert!(out.contains(r#"src="https://news.example.com/a/2.png""#), "{out}");
        assert!(out.contains("https://news.example.com/a/2-1200.png 1200w"), "{out}");
        assert_eq!(out.matches("<img").count(), 2);
    }

    #[test]
    fn picture_and_noscript() {
        let h = r#"<picture><source srcset="/p-800.webp 800w, /p-1600.webp 1600w"><img alt="pic"></picture><img src="lazy-placeholder.gif"><noscript><img src="/real.jpg"></noscript>"#;
        let out = sanitize(h, Some(&base()));
        assert!(out.contains("https://news.example.com/p-1600.webp"), "{out}");
        assert!(out.contains("https://news.example.com/real.jpg"), "{out}");
        assert!(!out.contains("<source"));
    }

    #[test]
    fn video_iframe_becomes_link() {
        let h = r#"<iframe src="https://www.youtube.com/embed/abc123"></iframe><iframe src="https://ads.example.com/x"></iframe>"#;
        let out = sanitize(h, Some(&base()));
        assert!(out.contains("https://www.youtube.com/watch?v=abc123"), "{out}");
        assert!(!out.contains("ads.example.com"));
    }

    #[test]
    fn srcset_choice() {
        assert_eq!(best_from_srcset("a.jpg 240w, b.jpg 800w, c.jpg 1024w, d.jpg 1600w").as_deref(), Some("c.jpg"));
        assert_eq!(best_from_srcset("a.jpg 240w, b.jpg 640w").as_deref(), Some("b.jpg"));
        assert_eq!(best_from_srcset("a.jpg 1x, b.jpg 2x, c.jpg 3x").as_deref(), Some("b.jpg"));
        assert_eq!(best_from_srcset("only.jpg").as_deref(), Some("only.jpg"));
        assert_eq!(best_from_srcset(""), None);
    }

    #[test]
    fn prunes_empty_elements() {
        let out = sanitize("<p> </p><div><span></span></div><p>text</p><p><img src=\"https://x.com/a.png\"></p>", None);
        assert_eq!(out, "<p>text</p><p><img src=\"https://x.com/a.png\"></p>");
    }
}

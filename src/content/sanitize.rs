//! HTML normalization + sanitization for article content.

use std::collections::HashSet;
use std::sync::LazyLock;

use ammonia::{Builder, UrlRelative};
use regex::Regex;
use url::Url;

use super::dom::{MAX_CONTENT_DEPTH, MAX_PAGE_DEPTH, cap_depth, flatten_deep, fragment_html, parse_fragment, tag_name, unwrap_element};
use crate::util::{html::base_href, urlnorm};

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

/// Parse a `srcset` value like browsers do ("parse a srcset attribute"): a candidate's
/// URL runs up to whitespace, so it may contain commas (image CDNs use `w_400,c_fill`),
/// and its descriptors run up to the next comma. Candidates with invalid descriptors
/// (including non-finite densities) are dropped.
pub fn parse_srcset(srcset: &str) -> Vec<SrcsetCandidate> {
    let mut out = Vec::new();
    let mut rest = srcset;
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_ascii_whitespace() || c == ',');
        if rest.is_empty() {
            break;
        }
        let end = rest.find(|c: char| c.is_ascii_whitespace()).unwrap_or(rest.len());
        let (mut url, after) = rest.split_at(end);
        rest = after;
        let mut descriptors: Vec<String> = Vec::new();
        if url.ends_with(',') {
            url = url.trim_end_matches(',');
        } else {
            let (mut cur, mut in_parens, mut consumed) = (String::new(), false, rest.len());
            for (i, c) in rest.char_indices() {
                if in_parens {
                    cur.push(c);
                    in_parens = c != ')';
                } else if c.is_ascii_whitespace() {
                    if !cur.is_empty() {
                        descriptors.push(std::mem::take(&mut cur));
                    }
                } else if c == ',' {
                    consumed = i + 1;
                    break;
                } else {
                    in_parens = c == '(';
                    cur.push(c);
                }
            }
            if !cur.is_empty() {
                descriptors.push(cur);
            }
            rest = &rest[consumed..];
        }
        if url.is_empty() {
            continue;
        }
        let (mut width, mut density, mut height, mut ok) = (None, None, false, true);
        for d in &descriptors {
            let kind = d.as_bytes()[d.len() - 1];
            if !kind.is_ascii() {
                ok = false;
                break;
            }
            let num = &d[..d.len() - 1];
            match kind {
                b'w' if width.is_none() && density.is_none() && !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit()) => {
                    width = num.parse::<u32>().ok().filter(|w| *w > 0);
                    ok &= width.is_some();
                }
                b'x' if width.is_none() && density.is_none() => {
                    density = num.parse::<f32>().ok().filter(|x| x.is_finite() && *x > 0.0);
                    ok &= density.is_some();
                }
                b'h' if !height && !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit()) => height = true,
                _ => ok = false,
            }
        }
        if ok && !(height && width.is_none()) {
            out.push(SrcsetCandidate { url: url.to_string(), width: width.unwrap_or(0), density: density.unwrap_or(0.0) });
        }
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
    ds.sort_by(|a, b| a.density.total_cmp(&b.density));
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
/// removal of non-content elements, and a cap on nesting depth.
pub fn preprocess(html: &str, base: Option<&Url>) -> String {
    let doc = parse_fragment(&flatten_deep(html, MAX_PAGE_DEPTH));

    // Unwrap <noscript> (often contains the real <img>).
    for n in doc.select("noscript").nodes().to_vec() {
        let inner = n.text().to_string();
        if inner.contains("<img") {
            n.replace_with_html(flatten_deep(&inner, MAX_PAGE_DEPTH).into_owned());
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
    // Later stages (translation, readers) walk the tree recursively.
    if let Some(root) = doc.root().first_child() {
        cap_depth(&root, MAX_CONTENT_DEPTH);
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
/// Relative URLs resolve against a `<base href>` in the fragment (extraction adds one for
/// the page's final address), else against `base`.
pub fn sanitize(html: &str, base: Option<&Url>) -> String {
    let own_base = base_href(html)
        .and_then(|h| match base {
            Some(b) => b.join(&h).ok(),
            None => Url::parse(&h).ok(),
        })
        .filter(|u| matches!(u.scheme(), "http" | "https"));
    let base = own_base.as_ref().or(base);
    let pre = preprocess(html, base);
    let clean = builder(base).clean(&pre).to_string();
    prune_empty(&clean)
}

/// Remove empty paragraphs/divs/spans that only contain whitespace. Inline elements
/// holding only whitespace become a single space: it may be all that separates words.
fn prune_empty(html: &str) -> String {
    let doc = parse_fragment(html);
    for _ in 0..3 {
        let mut changed = false;
        // Innermost first, so nothing is changed inside an already removed element.
        for n in doc.select("p, div, span, section, figure, figcaption, li, strong, b, em, i, a, blockquote, header, footer, aside").nodes().iter().rev() {
            let text = n.text();
            if text.trim().is_empty()
                && !n.descendants().iter().any(|d| matches!(tag_name(d).as_deref(), Some("img" | "video" | "audio" | "br" | "hr" | "table")))
            {
                if !text.is_empty() && matches!(tag_name(n).as_deref(), Some("span" | "strong" | "b" | "em" | "i" | "a")) {
                    n.replace_with_html(" ");
                } else {
                    n.remove_from_parent();
                }
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

    #[test]
    fn whitespace_only_inline_elements_keep_the_space() {
        assert_eq!(sanitize("<p>Hello<span> </span>world</p>", None), "<p>Hello world</p>");
        assert_eq!(sanitize("<p>The<a href=\"https://x.example/\"> </a>quick <b>&nbsp;</b>fox</p>", None), "<p>The quick  fox</p>");
        assert_eq!(sanitize("<p><strong>Breaking:</strong><em> </em>News</p><p><span> <b> </b> </span></p>", None), "<p><strong>Breaking:</strong> News</p>");
    }

    #[test]
    fn srcset_urls_may_contain_commas() {
        let ss = "https://res.cloudinary.com/demo/image/upload/w_400,c_fill/v1/photo.jpg 400w, https://res.cloudinary.com/demo/image/upload/w_1200,c_fill/v1/photo.jpg 1200w";
        let c = parse_srcset(ss);
        assert_eq!(c.len(), 2);
        assert_eq!(c[1].url, "https://res.cloudinary.com/demo/image/upload/w_1200,c_fill/v1/photo.jpg");
        assert_eq!(c[1].width, 1200);
        assert_eq!(best_from_srcset(ss).as_deref(), Some("https://res.cloudinary.com/demo/image/upload/w_1200,c_fill/v1/photo.jpg"));
        // Without descriptors, trailing commas end the URL.
        let urls: Vec<String> = parse_srcset("a.jpg,, b.jpg 2x,, c.jpg").into_iter().map(|c| c.url).collect();
        assert_eq!(urls, ["a.jpg", "b.jpg", "c.jpg"]);
        let out = sanitize(&format!(r#"<img src="data:image/gif;base64,R0lGOD" data-srcset="{ss}">"#), Some(&base()));
        assert!(out.contains(r#"src="https://res.cloudinary.com/demo/image/upload/w_1200,c_fill/v1/photo.jpg""#), "{out}");
        assert!(out.contains("w_400,c_fill/v1/photo.jpg 400w, https://res.cloudinary.com/demo/image/upload/w_1200,c_fill/v1/photo.jpg 1200w"), "{out}");
    }

    #[test]
    fn invalid_srcset_descriptors_are_dropped() {
        let c = parse_srcset("a.jpg NaNx, b.jpg infx, c.jpg -1x, d.jpg 0w, e.jpg 2é, f.jpg 1.5x, g.jpg 100w 2x, h.jpg (x, y) 2x");
        let urls: Vec<&str> = c.iter().map(|c| c.url.as_str()).collect();
        assert_eq!(urls, ["f.jpg"]);
        // Many candidates with NaN densities used to make the sort panic.
        let ss = (0..60).map(|i| format!("i{i}.jpg {}x", if i % 3 == 0 { "NaN".to_string() } else { format!("{}", i % 7 + 1) })).collect::<Vec<_>>().join(", ");
        assert_eq!(best_from_srcset(&ss).as_deref(), Some("i1.jpg"));
        assert!(sanitize(&format!("<img src=\"\" srcset=\"{ss}\">"), None).contains("<img"));
    }

    #[test]
    fn fragment_base_wins() {
        let h = r#"<base href="https://www.site.example/2024/10/story.html"><p>a</p><img src="img/a.jpg"><img data-src="/lazy/1.jpg" src="data:image/gif;base64,R0lGOD"><a href="related.html">r</a>"#;
        let out = sanitize(h, Some(&Url::parse("https://feeds.example.net/r/abc").unwrap()));
        assert!(out.contains("https://www.site.example/2024/10/img/a.jpg"), "{out}");
        assert!(out.contains("https://www.site.example/lazy/1.jpg"), "{out}");
        assert!(out.contains("https://www.site.example/2024/10/related.html"), "{out}");
        assert!(!out.contains("<base"), "{out}");
        // A relative <base> resolves against the given base; other schemes are ignored.
        let out = sanitize(r#"<base href="/blog/"><img src="x.png">"#, Some(&base()));
        assert!(out.contains("https://news.example.com/blog/x.png"), "{out}");
        let out = sanitize(r#"<base href="javascript:alert(1)//"><img src="x.png">"#, Some(&base()));
        assert!(out.contains("https://news.example.com/a/x.png"), "{out}");
    }

    #[test]
    fn caps_nesting_depth() {
        use crate::content::dom::tree_depth;
        let depth = |html: &str| {
            let doc = parse_fragment(html);
            tree_depth(&doc.root().first_child().unwrap(), usize::MAX)
        };
        for n in [300, 8000] {
            let h = format!("<p>intro</p>{}deep text{}", "<div>".repeat(n), "</div>".repeat(n));
            let out = sanitize(&h, None);
            assert!(depth(&out) <= MAX_CONTENT_DEPTH, "{n}: {}", depth(&out));
            assert!(out.contains("deep text") && out.contains("intro"));
        }
        // Markup hidden as text in <noscript> is re-parsed: it is capped as well.
        let h = format!("<noscript>&lt;img src=\"https://x.example/a.png\"&gt;{}x</noscript>", "&lt;div&gt;".repeat(3000));
        let out = sanitize(&h, None);
        assert!(depth(&out) <= MAX_CONTENT_DEPTH, "{}", depth(&out));
        assert!(out.contains("https://x.example/a.png"));
    }
}

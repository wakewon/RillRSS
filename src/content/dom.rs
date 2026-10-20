//! Helpers around `dom_query` for HTML fragments.
//!
//! Notes on dom_query behavior:
//! - `Document::fragment` puts an `<html>` element under the root; serialize its
//!   inner HTML to get the fragment back.
//! - Mutating the tree while iterating `descendants_it()` panics (RefCell), so collect
//!   nodes into a Vec first.
//! - html5ever builds trees of any depth, in time that grows with depth × size, and
//!   recursive consumers (Readability, translation) overflow their stack on deep trees.
//!   Untrusted HTML is checked with [`nesting_depth`] / [`flatten_deep`] before parsing.

use std::borrow::Cow;
use std::collections::HashMap;

use dom_query::{Document, NodeRef};

/// Deepest element nesting accepted in fetched pages and article HTML before parsing.
/// Browsers flatten anything deeper than 512 levels, so real pages stay well below.
pub const MAX_PAGE_DEPTH: usize = 512;

/// Article HTML is capped harder: `sanitize` unwraps elements nested deeper than this.
pub const MAX_CONTENT_DEPTH: usize = 200;

/// Parse an HTML fragment.
pub fn parse_fragment(html: &str) -> Document {
    Document::fragment(html)
}

/// Serialize a fragment document back to HTML (without the synthetic `<html>` wrapper).
pub fn fragment_html(doc: &Document) -> String {
    match doc.root().first_child() {
        Some(html) if html.node_name().as_deref() == Some("html") => html.inner_html().to_string(),
        _ => doc.html().to_string(),
    }
}

/// Lowercase element name of a node (None for non-elements).
pub fn tag_name(n: &NodeRef) -> Option<String> {
    if !n.is_element() {
        return None;
    }
    n.node_name().map(|s| s.to_ascii_lowercase())
}

/// Elements whose text is never translated / converted.
pub const SKIP_TEXT_TAGS: &[&str] = &["script", "style", "code", "pre", "kbd", "samp", "var", "math", "svg", "noscript", "template", "textarea"];

/// Is the node inside an element whose text must be left alone?
pub fn in_skip_context(n: &NodeRef) -> bool {
    let mut cur = n.parent();
    while let Some(p) = cur {
        if let Some(name) = tag_name(&p)
            && SKIP_TEXT_TAGS.contains(&name.as_str())
        {
            return true;
        }
        cur = p.parent();
    }
    false
}

/// Apply `f` to every text node that is not inside code/pre/script and similar.
pub fn map_text_nodes(html: &str, mut f: impl FnMut(&str) -> String) -> String {
    let doc = parse_fragment(html);
    let nodes = doc.root().descendants();
    for n in nodes {
        if n.is_text() && !in_skip_context(&n) {
            let t = n.text().to_string();
            if t.trim().is_empty() {
                continue;
            }
            let out = f(&t);
            if out != t {
                n.set_text(out);
            }
        }
    }
    fragment_html(&doc)
}

/// Replace an element by its children. (dom_query's `unwrap_node` removes the
/// *parent* of the node it is called on, jQuery-style.)
pub fn unwrap_element(n: &NodeRef) {
    match n.first_child() {
        Some(child) => child.unwrap_node(),
        None => n.remove_from_parent(),
    }
}

/// Remove all elements matching a CSS selector. Invalid selectors are ignored.
pub fn remove_selector(html: &str, selector: &str) -> String {
    let selector = selector.trim();
    if selector.is_empty() {
        return html.to_string();
    }
    let doc = parse_fragment(html);
    if let Some(sel) = doc.try_select(selector) {
        sel.remove();
    }
    fragment_html(&doc)
}

/// Depth of the deepest element below `root` (its children are at depth 1), counting
/// no further than `limit + 1`. Iterative, so safe on any tree.
pub fn tree_depth(root: &NodeRef, limit: usize) -> usize {
    let mut max = 0;
    let mut stack = vec![(*root, 0usize)];
    while let Some((n, d)) = stack.pop() {
        max = max.max(d);
        if max > limit {
            break;
        }
        stack.extend(n.children().into_iter().filter(|c| c.is_element()).map(|c| (c, d + 1)));
    }
    max
}

/// Unwrap (replace by their children) the non-void elements nested more than `max`
/// levels below `root`, so the text of deep structures ends up at depth `max`.
pub fn cap_depth(root: &NodeRef, max: usize) {
    let mut deep: Vec<NodeRef> = Vec::new();
    let mut stack = vec![(*root, 0usize)];
    while let Some((n, d)) = stack.pop() {
        for c in n.children() {
            if c.is_element() {
                if d + 1 > max && !tag_name(&c).is_some_and(|t| VOID_TAGS.contains(&t.as_str())) {
                    deep.push(c);
                }
                stack.push((c, d + 1));
            }
        }
    }
    for n in deep {
        unwrap_element(&n);
    }
}

// --- Nesting estimate before parsing ------------------------------------------------
//
// A single pass over the tokens tracking the stack of open elements the way html5ever
// would, but only popping where html5ever surely pops too, so the estimate stays at or
// above the depth of the parsed tree (give or take its implicit html/body; re-opened
// formatting elements can add at most what was counted for them before). Tags are
// tokenized like html5ever does (quoted attribute values may contain '>'), comments end
// where html5ever ends them, and raw text is skipped only in plain HTML context.

const VOID_TAGS: &[&str] = &[
    "area", "base", "basefont", "bgsound", "br", "col", "embed", "frame", "hr", "img", "input", "keygen", "link", "meta", "param",
    "source", "track", "wbr",
];
/// Void elements that stay void inside SVG/MathML (they break out of foreign content).
const BREAKOUT_VOID_TAGS: &[&str] = &["br", "embed", "hr", "img", "meta"];
/// Elements whose content is text up to their end tag (in HTML content).
const RAW_TEXT_TAGS: &[&str] = &["script", "style", "xmp", "iframe", "noembed", "noframes", "textarea", "title"];
/// html5ever's "special" elements (HTML, MathML and SVG names together): an end tag never
/// closes an element across one of these. Listing extra names only overestimates.
const SPECIAL_TAGS: &[&str] = &[
    "address", "applet", "area", "article", "aside", "base", "basefont", "bgsound", "blockquote", "body", "br", "button", "caption",
    "center", "col", "colgroup", "dd", "details", "dir", "div", "dl", "dt", "embed", "fieldset", "figcaption", "figure", "footer",
    "form", "frame", "frameset", "h1", "h2", "h3", "h4", "h5", "h6", "head", "header", "hgroup", "hr", "html", "iframe", "img",
    "input", "keygen", "li", "link", "listing", "main", "marquee", "menu", "meta", "nav", "noembed", "noframes", "noscript",
    "object", "ol", "p", "param", "plaintext", "pre", "script", "search", "section", "select", "source", "style", "summary",
    "table", "tbody", "td", "template", "textarea", "tfoot", "th", "thead", "title", "tr", "track", "ul", "wbr", "xmp", "mi",
    "mo", "mn", "ms", "mtext", "annotation-xml", "foreignobject", "desc",
];
/// Start tags that close a `<p>` that is the current element.
const CLOSES_P_TAGS: &[&str] = &[
    "address", "article", "aside", "blockquote", "center", "dd", "details", "dialog", "dir", "div", "dl", "dt", "fieldset",
    "figcaption", "figure", "footer", "form", "h1", "h2", "h3", "h4", "h5", "h6", "header", "hgroup", "hr", "li", "listing", "main",
    "menu", "nav", "ol", "p", "plaintext", "pre", "search", "section", "summary", "ul", "xmp",
];
const HEADING_TAGS: &[&str] = &["h1", "h2", "h3", "h4", "h5", "h6"];

struct Open {
    name: String,
    special: bool,
    /// Emitted by `flatten_deep` (false for tags dropped beyond the limit).
    kept: bool,
}

struct OpenStack {
    /// Elements opened deeper than this are not kept by `flatten_deep`.
    limit: usize,
    open: Vec<Open>,
    /// Positions of the open elements of each name (for end tags).
    by_name: HashMap<String, Vec<usize>>,
    /// Positions of the open special elements.
    specials: Vec<usize>,
    /// Open `<svg>`/`<math>` and `<select>` elements: raw text and void elements behave
    /// differently there, so those shortcuts are skipped while any is open.
    foreign: usize,
    selects: usize,
    max: usize,
}

impl OpenStack {
    fn len(&self) -> usize {
        self.open.len()
    }

    fn new(limit: usize) -> OpenStack {
        OpenStack { limit, open: Vec::new(), by_name: HashMap::new(), specials: Vec::new(), foreign: 0, selects: 0, max: 0 }
    }

    /// Open an element; returns whether it is kept (within the limit).
    fn push(&mut self, name: String) -> bool {
        let at = self.open.len();
        let kept = at < self.limit;
        let special = SPECIAL_TAGS.contains(&name.as_str());
        if special {
            self.specials.push(at);
        }
        match name.as_str() {
            "svg" | "math" => self.foreign += 1,
            "select" => self.selects += 1,
            _ => {}
        }
        self.by_name.entry(name.clone()).or_default().push(at);
        self.open.push(Open { name, special, kept });
        self.max = self.max.max(self.open.len());
        kept
    }

    fn truncate(&mut self, len: usize) {
        while self.open.len() > len {
            let Some(o) = self.open.pop() else { break };
            if o.special {
                self.specials.pop();
            }
            match o.name.as_str() {
                "svg" | "math" => self.foreign -= 1,
                "select" => self.selects -= 1,
                _ => {}
            }
            if let Some(v) = self.by_name.get_mut(&o.name) {
                v.pop();
            }
        }
    }

    fn top_is(&self, names: &[&str]) -> bool {
        self.open.last().is_some_and(|o| names.contains(&o.name.as_str()))
    }

    /// Position of the element an end tag closes: the nearest open element of that name,
    /// unless a special element lies above it (html5ever then ignores the end tag).
    fn end_target(&self, name: &str) -> Option<usize> {
        let at = *self.by_name.get(name)?.last()?;
        self.specials.last().is_none_or(|&s| s <= at).then_some(at)
    }

    /// What a start tag implies before it is inserted: end tags of the current element
    /// where html5ever closes it, and the tbody/tr/colgroup it adds inside tables.
    fn implied(&mut self, name: &str) {
        let same: &[&str] = match name {
            "li" => &["li"],
            "dd" | "dt" => &["dd", "dt"],
            "option" | "optgroup" => &["option"],
            "td" | "th" => &["td", "th"],
            "tr" => &["tr"],
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => HEADING_TAGS,
            _ => &[],
        };
        if self.top_is(same) {
            self.truncate(self.len() - 1);
        }
        if name == "optgroup" && self.top_is(&["optgroup"]) {
            self.truncate(self.len() - 1);
        }
        if CLOSES_P_TAGS.contains(&name) && self.top_is(&["p"]) {
            self.truncate(self.len() - 1);
        }
        match name {
            "tr" | "td" | "th" if self.top_is(&["table"]) => {
                self.push("tbody".into());
            }
            "col" if self.top_is(&["table"]) => {
                self.push("colgroup".into());
            }
            _ => {}
        }
        if matches!(name, "td" | "th") && self.top_is(&["tbody", "thead", "tfoot"]) {
            self.push("tr".into());
        }
    }
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c')
}

/// Index just past the next '>' at or after `from` (or the end).
fn past_gt(b: &[u8], from: usize) -> usize {
    b[from.min(b.len())..].iter().position(|&c| c == b'>').map_or(b.len(), |p| from + p + 1)
}

/// End of a comment whose text starts at `from` (just after `<!--`).
fn comment_end(b: &[u8], from: usize) -> usize {
    let rest = &b[from.min(b.len())..];
    if rest.starts_with(b">") {
        return from + 1;
    }
    if rest.starts_with(b"->") {
        return from + 2;
    }
    let mut k = from;
    while let Some(p) = b[k.min(b.len())..].iter().position(|&c| c == b'-') {
        let at = k + p;
        if b[at..].starts_with(b"-->") {
            return at + 3;
        }
        if b[at..].starts_with(b"--!>") {
            return at + 4;
        }
        k = at + 1;
    }
    b.len()
}

/// Read a tag name starting at `from`; returns it lowercased and the index after it.
fn read_tag_name(b: &[u8], from: usize) -> (String, usize) {
    let end = b[from..].iter().position(|&c| is_space(c) || c == b'/' || c == b'>').map_or(b.len(), |p| from + p);
    (String::from_utf8_lossy(&b[from..end]).to_ascii_lowercase(), end)
}

/// Skip the attributes of a tag (from just after its name); returns the index after its
/// '>', or None when the input ends inside the tag (html5ever then drops it).
fn tag_end(b: &[u8], mut j: usize) -> Option<usize> {
    let len = b.len();
    loop {
        while j < len && (is_space(b[j]) || b[j] == b'/') {
            j += 1;
        }
        if j >= len {
            return None;
        }
        if b[j] == b'>' {
            return Some(j + 1);
        }
        // Attribute name (a leading '=' belongs to it).
        j += 1;
        while j < len && !is_space(b[j]) && !matches!(b[j], b'/' | b'>' | b'=') {
            j += 1;
        }
        while j < len && is_space(b[j]) {
            j += 1;
        }
        if j < len && b[j] == b'=' {
            j += 1;
            while j < len && is_space(b[j]) {
                j += 1;
            }
            match b.get(j)? {
                q @ (b'"' | b'\'') => j = j + 1 + b[j + 1..].iter().position(|c| c == q)? + 1,
                b'>' => return Some(j + 1),
                _ => {
                    while j < len && !is_space(b[j]) && b[j] != b'>' {
                        j += 1;
                    }
                }
            }
        }
    }
}

/// Index after the end tag closing a raw-text element `name` whose content starts at
/// `from` (or the end of the input).
fn raw_text_end(b: &[u8], from: usize, name: &str) -> usize {
    let n = name.len();
    let mut k = from;
    while let Some(p) = b[k.min(b.len())..].iter().position(|&c| c == b'<') {
        let at = k + p;
        let tail = &b[at..];
        if tail.len() > n + 2
            && tail[1] == b'/'
            && tail[2..2 + n].eq_ignore_ascii_case(name.as_bytes())
            && (is_space(tail[2 + n]) || matches!(tail[2 + n], b'/' | b'>'))
        {
            return tag_end(b, at + 2 + n).unwrap_or(b.len());
        }
        k = at + 1;
    }
    b.len()
}

/// Walk the tokens of `html`; with `out`, copy it there minus the tags of elements that
/// would sit deeper than `limit`. Returns the (estimated) deepest nesting, stopping early
/// once it exceeds `limit` when there is no output to produce.
fn scan(html: &str, limit: usize, mut out: Option<&mut String>) -> usize {
    let b = html.as_bytes();
    let len = b.len();
    let mut st = OpenStack::new(limit);
    let mut copied = 0;
    let mut cut = |out: &mut Option<&mut String>, from: usize, to: usize| {
        if let Some(o) = out.as_deref_mut() {
            o.push_str(&html[copied..from]);
            copied = to;
        }
    };
    let mut i = 0;
    while let Some(p) = b[i..].iter().position(|&c| c == b'<') {
        let lt = i + p;
        let rest = &b[lt..];
        if rest.starts_with(b"<!--") {
            i = comment_end(b, lt + 4);
        } else if rest.starts_with(b"<!") || rest.starts_with(b"<?") {
            i = past_gt(b, lt + 2);
        } else if rest.starts_with(b"</") {
            match rest.get(2) {
                Some(c) if c.is_ascii_alphabetic() => {
                    let (name, after) = read_tag_name(b, lt + 2);
                    let Some(end) = tag_end(b, after) else { break };
                    if let Some(at) = st.end_target(&name) {
                        if !st.open[at].kept {
                            cut(&mut out, lt, end);
                        }
                        st.truncate(at);
                    }
                    i = end;
                }
                Some(b'>') => i = lt + 3,
                _ => i = past_gt(b, lt + 2),
            }
        } else if rest.get(1).is_some_and(|c| c.is_ascii_alphabetic()) {
            let (name, after) = read_tag_name(b, lt + 1);
            let Some(end) = tag_end(b, after) else { break };
            let html_context = st.foreign == 0 && st.selects == 0;
            st.implied(&name);
            i = end;
            if VOID_TAGS.contains(&name.as_str()) && (st.foreign == 0 || BREAKOUT_VOID_TAGS.contains(&name.as_str())) {
                continue;
            }
            if html_context && (name == "plaintext" || RAW_TEXT_TAGS.contains(&name.as_str())) {
                // Pushed and popped around text only: one level, briefly.
                let close = if name == "plaintext" { len } else { raw_text_end(b, end, &name) };
                st.max = st.max.max(st.len() + 1);
                if st.len() >= limit {
                    cut(&mut out, lt, close);
                }
                i = close;
            } else if !st.push(name) {
                cut(&mut out, lt, end);
            }
        } else {
            i = lt + 1;
        }
        if out.is_none() && st.max > limit {
            break;
        }
    }
    if let Some(o) = out {
        o.push_str(&html[copied..]);
    }
    st.max
}

/// Estimated element nesting depth of an HTML document or fragment (see above);
/// counting stops once it exceeds `limit`.
pub fn nesting_depth(html: &str, limit: usize) -> usize {
    scan(html, limit, None)
}

/// The HTML unchanged when it nests no deeper than `limit`, else a copy without the tags
/// of the elements beyond that depth (their text stays).
pub fn flatten_deep(html: &str, limit: usize) -> Cow<'_, str> {
    if nesting_depth(html, limit) <= limit {
        return Cow::Borrowed(html);
    }
    let mut out = String::with_capacity(html.len());
    scan(html, limit, Some(&mut out));
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_fragment() {
        let h = "<p>Hello <b>world</b></p><div><img src=\"a.png\"></div>tail";
        assert_eq!(fragment_html(&parse_fragment(h)), h);
    }

    #[test]
    fn maps_text_but_not_code() {
        let h = "<p>abc <code>abc</code></p><pre>abc</pre>";
        let out = map_text_nodes(h, |t| t.replace("abc", "XYZ"));
        assert_eq!(out, "<p>XYZ <code>abc</code></p><pre>abc</pre>");
    }

    #[test]
    fn removes_by_selector() {
        let h = "<p>keep</p><div class=\"ad\">ad</div><span aria-hidden=\"true\">x</span>";
        assert_eq!(remove_selector(h, ".ad, [aria-hidden=\"true\"]"), "<p>keep</p>");
        assert_eq!(remove_selector(h, "<<invalid"), h);
    }

    fn nested(tag: &str, n: usize) -> String {
        format!("{}x{}", format!("<{tag}>").repeat(n), format!("</{tag}>").repeat(n))
    }

    fn parsed_depth(html: &str) -> usize {
        let doc = parse_fragment(html);
        tree_depth(&doc.root().first_child().unwrap(), usize::MAX)
    }

    #[test]
    fn estimates_nesting() {
        assert_eq!(nesting_depth("<div><div><p>x</p></div></div>", 10), 3);
        assert_eq!(nesting_depth("<p>a<p>b<p>c", 10), 1);
        assert_eq!(nesting_depth("<ul><li>a<li>b<li>c</ul><img><br>", 10), 2);
        assert_eq!(nesting_depth("<svg><path/><path/><rect/></svg><div></div>", 10), 4);
        assert_eq!(nesting_depth("<script>x = '<div><div>';</script><div></div>", 10), 1);
        assert_eq!(nesting_depth(r#"<a href="x>y">z</a>"#, 10), 1);
        assert!(nesting_depth(&nested("div", 5000), MAX_PAGE_DEPTH) > MAX_PAGE_DEPTH);
        assert!(nesting_depth(&nested("b", 5000), MAX_PAGE_DEPTH) > MAX_PAGE_DEPTH);
    }

    #[test]
    fn estimate_is_not_below_the_parsed_depth() {
        // Inputs where a naive tag counter undercounts what html5ever builds.
        let n = 60;
        let cases = [
            "<span><div></span>".repeat(n),
            r#"<div title="></div>">"#.repeat(n),
            format!("<!-->{}", "<div>".repeat(n)),
            format!("<!-- --!>{}", "<div>".repeat(n)),
            format!("<svg><style>{}", "<div>".repeat(n)),
            format!("<div><svg><g><desc></svg></desc>{}", "<x/>".repeat(n)),
            format!("<noscript>{}", "<div>".repeat(n)),
            format!("<table><tr><td>{}", "<div><table><tr><td></div>".repeat(n)),
            "<table><td>".repeat(n),
            "<b><div></b>".repeat(n),
            "<div/>".repeat(n),
        ];
        for h in &cases {
            let (est, real) = (nesting_depth(h, usize::MAX), parsed_depth(h));
            assert!(est >= real, "estimate {est} < parsed depth {real} for {}", &h[..40]);
            assert!(real > n / 2, "case should nest: {real} for {}", &h[..40]);
        }
    }

    #[test]
    fn flattens_and_caps_deep_html() {
        let deep = nested("div", 3000);
        let flat = flatten_deep(&deep, 100);
        assert!(nesting_depth(&flat, usize::MAX) <= 100);
        assert!(parsed_depth(&flat) <= 101, "{}", parsed_depth(&flat));
        assert!(flat.contains('x'));
        let shallow = "<p>a <b>b</b></p>";
        assert!(matches!(flatten_deep(shallow, 100), Cow::Borrowed(_)));

        let doc = parse_fragment(&format!("<p>{}</p>", nested("span", 300).replace('x', "<img src=a.png>text")));
        let root = doc.root().first_child().unwrap();
        cap_depth(&root, 50);
        assert_eq!(tree_depth(&root, usize::MAX), 51, "the image may sit just below the cap");
        let out = fragment_html(&doc);
        assert!(out.contains("<img src=\"a.png\">text"), "{}", &out[..200]);
    }
}

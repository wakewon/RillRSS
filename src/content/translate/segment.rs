//! HTML segmentation for translation.
//!
//! The fragment is split into *blocks*: elements whose content is inline-only (text
//! and inline elements), plus anonymous runs of inline content inside mixed elements.
//! Each block becomes a [`Segment`] whose text contains numbered placeholders for
//! inline markup:
//!   `<gN>…</gN>` paired tags (links, bold, …) whose text is translated,
//!   `<xN/>`      atomic placeholders (br, img, inline code, …) kept verbatim.
//! After translation the placeholders are restored to the original markup.

use std::collections::HashMap;

use dom_query::{Document, NodeRef};

use crate::content::dom::{fragment_html, parse_fragment, tag_name};
use crate::util::html::escape;

/// Inline (phrasing) elements that are kept inside a segment as `<gN>` tags.
const INLINE_PAIRED: &[&str] = &[
    "a", "abbr", "b", "bdi", "bdo", "cite", "data", "del", "dfn", "em", "font", "i", "ins", "label", "mark", "q",
    "ruby", "rb", "rt", "rp", "s", "small", "span", "strike", "strong", "sub", "sup", "time", "u", "big", "tt",
];
/// Inline elements that are kept verbatim as `<xN/>`.
const INLINE_ATOMIC: &[&str] = &["br", "img", "wbr", "code", "kbd", "samp", "var", "math", "svg", "video", "audio", "picture", "input"];
/// Containers whose text must never be translated.
const NO_TRANSLATE: &[&str] = &["pre", "script", "style", "noscript", "template", "textarea", "table-of-contents"];

#[derive(Debug, Clone, PartialEq)]
pub enum Placeholder {
    /// Paired tag: opening HTML and closing HTML.
    Pair { open: String, close: String },
    /// Atomic HTML kept verbatim.
    Atom(String),
}

#[derive(Debug, Clone)]
pub struct Segment {
    pub id: usize,
    /// Source text with placeholders (HTML-escaped text).
    pub text: String,
    /// Placeholder number -> original markup.
    pub placeholders: HashMap<usize, Placeholder>,
}

impl Segment {
    /// Plain text without placeholders (for language checks / length estimates).
    pub fn plain(&self) -> String {
        strip_placeholders(&self.text)
    }
}

/// Where a segment lives in the document.
#[derive(Debug, Clone)]
enum Target {
    /// Replace the inner HTML of this element.
    Element(dom_query::NodeId),
    /// Replace this run of sibling nodes.
    Run(Vec<dom_query::NodeId>),
}

/// A parsed document plus the segments extracted from it.
pub struct Segmented {
    doc: Document,
    targets: Vec<Target>,
    pub segments: Vec<Segment>,
}

fn is_inline(n: &NodeRef) -> bool {
    if n.is_text() {
        return true;
    }
    match tag_name(n) {
        Some(t) => INLINE_PAIRED.contains(&t.as_str()) || INLINE_ATOMIC.contains(&t.as_str()),
        None => true, // comments etc.
    }
}

fn has_letters(s: &str) -> bool {
    s.chars().any(|c| c.is_alphabetic())
}

/// Does the visible text of these nodes deserve translation?
fn worth_translating(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() || !has_letters(t) {
        return false;
    }
    // A bare URL / e-mail
    if !t.contains(' ') && (t.starts_with("http://") || t.starts_with("https://") || t.starts_with("www.") || t.contains('@')) {
        return false;
    }
    true
}

struct Builder {
    counter: usize,
    placeholders: HashMap<usize, Placeholder>,
}

impl Builder {
    fn open_tag_html(n: &NodeRef) -> (String, String) {
        let name = tag_name(n).unwrap_or_else(|| "span".into());
        let mut open = format!("<{name}");
        for a in n.attrs() {
            open.push(' ');
            open.push_str(&a.name.local);
            open.push_str("=\"");
            open.push_str(&escape(&a.value));
            open.push('"');
        }
        open.push('>');
        (open, format!("</{name}>"))
    }

    fn serialize(&mut self, n: &NodeRef, out: &mut String) {
        if n.is_text() {
            out.push_str(&escape_text(&n.text()));
            return;
        }
        if !n.is_element() {
            return;
        }
        let name = tag_name(n).unwrap_or_default();
        if INLINE_ATOMIC.contains(&name.as_str()) || NO_TRANSLATE.contains(&name.as_str()) {
            self.counter += 1;
            let id = self.counter;
            self.placeholders.insert(id, Placeholder::Atom(n.html().to_string()));
            out.push_str(&format!("<x{id}/>"));
            return;
        }
        // Paired inline tag. Empty ones (no text) are atomic.
        if n.text().trim().is_empty() {
            self.counter += 1;
            let id = self.counter;
            self.placeholders.insert(id, Placeholder::Atom(n.html().to_string()));
            out.push_str(&format!("<x{id}/>"));
            return;
        }
        self.counter += 1;
        let id = self.counter;
        let (open, close) = Self::open_tag_html(n);
        self.placeholders.insert(id, Placeholder::Pair { open, close });
        out.push_str(&format!("<g{id}>"));
        for c in n.children() {
            self.serialize(&c, out);
        }
        out.push_str(&format!("</g{id}>"));
    }
}

/// Escape text for the segment format (only markup-significant characters).
fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Collapse whitespace of segment text but keep single spaces.
fn normalize_space(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

fn make_segment(id: usize, nodes: &[NodeRef]) -> Option<Segment> {
    let visible: String = nodes.iter().map(|n| n.text().to_string()).collect();
    if !worth_translating(&visible) {
        return None;
    }
    let mut b = Builder { counter: 0, placeholders: HashMap::new() };
    let mut text = String::new();
    for n in nodes {
        b.serialize(n, &mut text);
    }
    let text = normalize_space(&text);
    if !worth_translating(&strip_placeholders(&text)) {
        return None;
    }
    Some(Segment { id, text, placeholders: b.placeholders })
}

/// Segment an HTML fragment.
pub fn segment_html(html: &str) -> Segmented {
    let doc = parse_fragment(html);
    let mut targets = Vec::new();
    let mut segments = Vec::new();
    let root = match doc.root().first_child() {
        Some(h) if h.node_name().as_deref() == Some("html") => h,
        _ => doc.root(),
    };
    walk(&root, &mut targets, &mut segments, true);
    Segmented { doc, targets, segments }
}

fn walk(node: &NodeRef, targets: &mut Vec<Target>, segments: &mut Vec<Segment>, is_root: bool) {
    if let Some(name) = tag_name(node)
        && (NO_TRANSLATE.contains(&name.as_str()) || INLINE_ATOMIC.contains(&name.as_str()))
    {
        return;
    }
    let children = node.children();
    let all_inline = children.iter().all(is_inline);
    if all_inline && !is_root {
        if let Some(seg) = make_segment(segments.len() + 1, &children) {
            targets.push(Target::Element(node.id));
            segments.push(seg);
        }
        return;
    }
    // Mixed content: inline runs become anonymous segments, blocks are recursed.
    let mut run: Vec<NodeRef> = Vec::new();
    let flush = |run: &mut Vec<NodeRef>, targets: &mut Vec<Target>, segments: &mut Vec<Segment>| {
        if run.is_empty() {
            return;
        }
        if let Some(seg) = make_segment(segments.len() + 1, run) {
            targets.push(Target::Run(run.iter().map(|n| n.id).collect()));
            segments.push(seg);
        }
        run.clear();
    };
    for c in children {
        if is_inline(&c) {
            run.push(c);
        } else {
            flush(&mut run, targets, segments);
            walk(&c, targets, segments, false);
        }
    }
    flush(&mut run, targets, segments);
}

/// Remove placeholder tags, leaving text.
pub fn strip_placeholders(s: &str) -> String {
    let re = placeholder_re();
    let t = re.replace_all(s, "");
    html_escape::decode_html_entities(&t).into_owned()
}

fn placeholder_re() -> &'static regex::Regex {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)<\s*(/?)\s*([gx])\s*(\d+)\s*(/?)\s*>").unwrap());
    &RE
}

/// Problems found when restoring placeholders.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RestoreIssues {
    pub missing: Vec<usize>,
    pub unbalanced: bool,
    pub unknown: Vec<usize>,
}

impl RestoreIssues {
    pub fn is_ok(&self) -> bool {
        self.missing.is_empty() && !self.unbalanced && self.unknown.is_empty()
    }
}

/// Check that a translated segment keeps all placeholders, properly nested.
pub fn check_placeholders(seg: &Segment, translated: &str) -> RestoreIssues {
    let mut issues = RestoreIssues::default();
    let mut stack: Vec<usize> = Vec::new();
    let mut seen_open: HashMap<usize, usize> = HashMap::new();
    let mut seen_close: HashMap<usize, usize> = HashMap::new();
    let mut seen_atom: HashMap<usize, usize> = HashMap::new();
    for c in placeholder_re().captures_iter(translated) {
        let closing = !c[1].is_empty();
        let kind = c[2].to_ascii_lowercase();
        let id: usize = c[3].parse().unwrap_or(0);
        match (kind.as_str(), seg.placeholders.get(&id)) {
            ("g", Some(Placeholder::Pair { .. })) => {
                if closing {
                    *seen_close.entry(id).or_default() += 1;
                    if stack.pop() != Some(id) {
                        issues.unbalanced = true;
                    }
                } else {
                    *seen_open.entry(id).or_default() += 1;
                    stack.push(id);
                }
            }
            ("x", Some(Placeholder::Atom(_))) => {
                *seen_atom.entry(id).or_default() += 1;
            }
            _ => issues.unknown.push(id),
        }
    }
    if !stack.is_empty() {
        issues.unbalanced = true;
    }
    for (id, p) in &seg.placeholders {
        match p {
            Placeholder::Pair { .. } => {
                let o = seen_open.get(id).copied().unwrap_or(0);
                let c = seen_close.get(id).copied().unwrap_or(0);
                if o == 0 || c == 0 {
                    issues.missing.push(*id);
                } else if o != 1 || c != 1 {
                    issues.unbalanced = true;
                }
            }
            Placeholder::Atom(_) => {
                if seen_atom.get(id).copied().unwrap_or(0) == 0 {
                    issues.missing.push(*id);
                }
            }
        }
    }
    issues.missing.sort();
    issues
}

/// Turn a translated segment back into HTML. Unknown/unbalanced placeholder tags are
/// dropped (their text is kept); missing atomic placeholders (images, line breaks…)
/// are appended so no media is lost. Stray `<` from the model are escaped.
pub fn restore(seg: &Segment, translated: &str) -> String {
    let re = placeholder_re();
    let issues = check_placeholders(seg, translated);
    let balanced_ok = !issues.unbalanced;
    let mut out = String::with_capacity(translated.len() + 64);
    let mut last = 0;
    let mut used_atoms: Vec<usize> = Vec::new();
    for m in re.captures_iter(translated) {
        let whole = m.get(0).unwrap();
        out.push_str(&sanitize_text_chunk(&translated[last..whole.start()]));
        last = whole.end();
        let closing = !m[1].is_empty();
        let kind = m[2].to_ascii_lowercase();
        let id: usize = m[3].parse().unwrap_or(0);
        match (kind.as_str(), seg.placeholders.get(&id)) {
            ("g", Some(Placeholder::Pair { open, close })) if balanced_ok => {
                out.push_str(if closing { close } else { open });
            }
            ("x", Some(Placeholder::Atom(html))) if !used_atoms.contains(&id) => {
                out.push_str(html);
                used_atoms.push(id);
            }
            _ => {}
        }
    }
    out.push_str(&sanitize_text_chunk(&translated[last..]));
    // Re-append atoms the model dropped (sorted by id = original order).
    let mut missing_atoms: Vec<usize> = seg
        .placeholders
        .iter()
        .filter(|(id, p)| matches!(p, Placeholder::Atom(_)) && !used_atoms.contains(id))
        .map(|(id, _)| *id)
        .collect();
    missing_atoms.sort();
    for id in missing_atoms {
        if let Some(Placeholder::Atom(html)) = seg.placeholders.get(&id) {
            out.push(' ');
            out.push_str(html);
        }
    }
    out.trim().to_string()
}

/// Text between placeholders: keep entities, escape raw '<' / '>' that are not markup.
fn sanitize_text_chunk(s: &str) -> String {
    // Decode first so that "&lt;" stays one escape, then re-escape.
    let decoded = html_escape::decode_html_entities(s);
    escape_text(&decoded)
}

impl Segmented {
    /// Apply translations (segment id -> translated text with placeholders).
    /// `bilingual` keeps the original and adds the translation after it.
    pub fn apply(self, translations: &HashMap<usize, String>, bilingual: bool) -> String {
        for (seg, target) in self.segments.iter().zip(self.targets.iter()) {
            let Some(tr) = translations.get(&seg.id) else { continue };
            let html = restore(seg, tr);
            match target {
                Target::Element(id) => {
                    let node = NodeRef::new(*id, &self.doc.tree);
                    if bilingual {
                        let name = tag_name(&node).unwrap_or_default();
                        if matches!(name.as_str(), "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "blockquote" | "figcaption" | "div" | "dd" | "dt" | "section" | "header" | "footer" | "article" | "aside") {
                            let (open, close) = Builder::open_tag_html(&node);
                            node.after_html(format!("{open}{html}{close}"));
                        } else {
                            node.append_html(format!("<br>{html}"));
                        }
                    } else {
                        node.set_html(html);
                    }
                }
                Target::Run(ids) => {
                    let Some(first) = ids.first() else { continue };
                    let last = ids.last().unwrap();
                    if bilingual {
                        NodeRef::new(*last, &self.doc.tree).after_html(format!("<br>{html}"));
                    } else {
                        NodeRef::new(*first, &self.doc.tree).before_html(html);
                        for id in ids {
                            NodeRef::new(*id, &self.doc.tree).remove_from_parent();
                        }
                    }
                }
            }
        }
        fragment_html(&self.doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_blocks_and_placeholders() {
        let h = r#"<p>The <a href="https://x.com/a">prime minister</a> said <b>taxes</b> will rise.<br>Second line</p><ul><li>One</li><li>Two <code>x = 1</code></li></ul><pre>code block</pre><p>12:30</p>"#;
        let s = segment_html(h);
        let texts: Vec<&str> = s.segments.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["The <g1>prime minister</g1> said <g2>taxes</g2> will rise.<x3/>Second line", "One", "Two <x1/>"]
        );
        assert!(matches!(s.segments[0].placeholders.get(&1), Some(Placeholder::Pair { open, .. }) if open.contains("href=\"https://x.com/a\"")));
    }

    #[test]
    fn mixed_content_runs() {
        let h = "<div>Intro text <b>bold</b><p>Para</p>tail text</div>";
        let s = segment_html(h);
        let texts: Vec<&str> = s.segments.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(texts, vec!["Intro text <g1>bold</g1>", "Para", "tail text"]);
        let mut tr = HashMap::new();
        tr.insert(1, "Texte d'intro <g1>gras</g1>".to_string());
        tr.insert(2, "Paragraphe".to_string());
        tr.insert(3, "texte de fin".to_string());
        let out = s.apply(&tr, false);
        assert_eq!(out, "<div>Texte d'intro <b>gras</b><p>Paragraphe</p>texte de fin</div>");
    }

    #[test]
    fn restore_and_check() {
        let s = segment_html(r#"<p>See <a href="/x">this <i>page</i></a><img src="a.png"> now</p>"#);
        let seg = &s.segments[0];
        assert_eq!(seg.text, "See <g1>this <g2>page</g2></g1><x3/> now");
        let good = "Voir <g1>cette <g2>page</g2></g1><x3/> maintenant";
        assert!(check_placeholders(seg, good).is_ok());
        assert_eq!(restore(seg, good), r#"Voir <a href="/x">cette <i>page</i></a><img src="a.png"> maintenant"#);
        // Missing image placeholder: re-appended; unbalanced tags dropped.
        let bad = "Voir <g1>cette <g2>page</g1></g2> maintenant";
        let issues = check_placeholders(seg, bad);
        assert!(issues.unbalanced);
        assert_eq!(issues.missing, vec![3]);
        assert_eq!(restore(seg, bad), r#"Voir cette page maintenant <img src="a.png">"#);
        // Stray markup from the model is escaped.
        assert_eq!(restore(seg, "a <script>x</script> <g1>b <g2>c</g2></g1><x3/>"), "a &lt;script&gt;x&lt;/script&gt; <a href=\"/x\">b <i>c</i></a><img src=\"a.png\">");
    }

    #[test]
    fn bilingual_mode() {
        let s = segment_html("<p>Hello</p><ul><li>Item</li></ul>");
        let mut tr = HashMap::new();
        tr.insert(1, "你好".into());
        tr.insert(2, "项目".into());
        assert_eq!(s.apply(&tr, true), "<p>Hello</p><p>你好</p><ul><li>Item<br>项目</li></ul>");
    }

    #[test]
    fn entities_roundtrip() {
        let s = segment_html("<p>AT&amp;T &lt;rocks&gt;</p>");
        assert_eq!(s.segments[0].text, "AT&amp;T &lt;rocks&gt;");
        let mut tr = HashMap::new();
        tr.insert(1, "AT&T <rocks> 很棒".into());
        assert_eq!(s.apply(&tr, false), "<p>AT&amp;T &lt;rocks&gt; 很棒</p>");
    }

    #[test]
    fn skips_non_text() {
        let s = segment_html("<p>2026-10-20</p><p>https://example.com/x</p><figure><img src=\"a.jpg\"><figcaption>A caption</figcaption></figure>");
        let texts: Vec<&str> = s.segments.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(texts, vec!["A caption"]);
    }
}

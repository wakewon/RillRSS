//! Helpers around `dom_query` for HTML fragments.
//!
//! Notes on dom_query behavior:
//! - `Document::fragment` puts an `<html>` element under the root; serialize its
//!   inner HTML to get the fragment back.
//! - Mutating the tree while iterating `descendants_it()` panics (RefCell), so collect
//!   nodes into a Vec first.

use dom_query::{Document, NodeRef};

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
}

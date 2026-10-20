//! Feed document parsing (RSS 0.9x/1.0/2.0, Atom, JSON Feed) into a simple model.

use std::sync::LazyLock;

use anyhow::{Result, anyhow};
use regex::Regex;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::util::urlnorm;

#[derive(Debug, Clone)]
pub struct ParsedFeed {
    pub title: String,
    pub site_url: Option<String>,
    pub description: Option<String>,
    pub icon_url: Option<String>,
    /// Suggested TTL in minutes (RSS <ttl>), if any.
    pub ttl_min: Option<u32>,
    pub entries: Vec<ParsedEntry>,
}

#[derive(Debug, Clone)]
pub struct ParsedEntry {
    /// Raw guid / id as provided (falls back to the link, then a content hash).
    pub guid: String,
    pub title: String,
    pub link: Option<String>,
    pub author: Option<String>,
    /// HTML content (content:encoded / atom content / description).
    pub html: String,
    pub published: Option<i64>,
    pub updated: Option<i64>,
    pub media: Media,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Media {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enclosures: Vec<Enclosure>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Enclosure {
    pub url: String,
    #[serde(default)]
    pub mime: String,
    #[serde(default)]
    pub length: u64,
}

impl Media {
    pub fn is_empty(&self) -> bool {
        self.thumbnail.is_none() && self.enclosures.is_empty()
    }
    pub fn to_json(&self) -> Option<String> {
        if self.is_empty() { None } else { serde_json::to_string(self).ok() }
    }
    pub fn from_json(s: Option<&str>) -> Media {
        s.and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default()
    }
}

/// Parse a feed document. `base` is the feed URL (used to resolve relative links).
pub fn parse_feed(body: &[u8], base: &str) -> Result<ParsedFeed> {
    let base_url = Url::parse(base).ok();
    // Do not let feed-rs synthesize ids (it hashes link+title, which changes with the
    // title); we fall back to the link ourselves.
    let parser = feed_rs::parser::Builder::new()
        .base_uri(Some(base))
        .id_generator(|_, _, _| String::new())
        .build();
    let feed = parser.parse(body).map_err(|e| anyhow!("feed parse error: {e}"))?;

    let site_url = pick_alternate_link(&feed.links).and_then(|h| urlnorm::resolve(base_url.as_ref(), &h));
    let entry_base = site_url
        .as_deref()
        .and_then(|s| Url::parse(s).ok())
        .or_else(|| base_url.clone());

    let mut entries = Vec::with_capacity(feed.entries.len());
    for e in feed.entries {
        let link = pick_alternate_link(&e.links).and_then(|h| urlnorm::resolve(entry_base.as_ref(), &h));
        let title = e.title.as_ref().map(text_title).unwrap_or_default();

        // Prefer full content, then summary.
        let content_body = e.content.as_ref().and_then(|c| c.body.clone()).filter(|b| !b.trim().is_empty());
        let summary = e.summary.as_ref().map(|s| s.content.clone()).filter(|s| !s.trim().is_empty());
        let mut html = match (content_body, summary) {
            (Some(c), Some(s)) if c.len() < s.len() / 2 => s,
            (Some(c), _) => c,
            (None, Some(s)) => s,
            (None, None) => String::new(),
        };
        // Some feeds put plain text in description: keep paragraphs.
        if !html.contains('<') && html.contains('\n') {
            html = html
                .split("\n\n")
                .map(|p| format!("<p>{}</p>", crate::util::html::escape(p.trim())))
                .collect();
        }

        let author = e
            .authors
            .iter()
            .filter_map(|a| a.name.as_deref().map(|n| n.trim().to_string()))
            .find(|n| !n.is_empty() && n != "unknown");

        let mut media = Media::default();
        for m in &e.media {
            for t in &m.thumbnails {
                if media.thumbnail.is_none() {
                    media.thumbnail = urlnorm::resolve(entry_base.as_ref(), t.image.uri.as_str());
                }
            }
            for c in &m.content {
                if let Some(u) = &c.url {
                    let mime = c.content_type.as_ref().map(|m| m.to_string()).unwrap_or_default();
                    if mime.starts_with("image/") && media.thumbnail.is_none() {
                        media.thumbnail = Some(u.to_string());
                        continue;
                    }
                    if !media.enclosures.iter().any(|x| x.url == u.as_str()) {
                        media.enclosures.push(Enclosure { url: u.to_string(), mime, length: c.size.unwrap_or(0) });
                    }
                }
            }
        }
        for l in &e.links {
            if l.rel.as_deref() == Some("enclosure")
                && let Some(u) = urlnorm::resolve(entry_base.as_ref(), &l.href)
                && !media.enclosures.iter().any(|x| x.url == u)
            {
                let mime = l.media_type.clone().unwrap_or_default();
                if mime.starts_with("image/") && media.thumbnail.is_none() {
                    media.thumbnail = Some(u);
                } else {
                    media.enclosures.push(Enclosure { url: u, mime, length: l.length.unwrap_or(0) });
                }
            }
        }

        let published = e.published.map(|d| d.timestamp());
        let updated = e.updated.map(|d| d.timestamp());

        // feed-rs synthesizes an id when the entry has none; prefer the link then.
        let raw_id = e.id.trim().to_string();
        let guid = if raw_id.is_empty() {
            link.clone().unwrap_or_else(|| crate::util::short_hash(&format!("{title}\n{html}")))
        } else {
            raw_id
        };

        entries.push(ParsedEntry { guid, title, link, author, html, published, updated, media });
    }

    // Fill thumbnails feed-rs did not pick up, and resolve relative item links against
    // the site (feed-rs resolves them against the feed URL, often another host).
    let scan = scan_items(body);
    if scan.items.len() == entries.len() {
        for (e, item) in entries.iter_mut().zip(scan.items) {
            if e.media.thumbnail.is_none() {
                e.media.thumbnail = item.image.and_then(|u| urlnorm::resolve(entry_base.as_ref(), &u));
            }
            if !scan.xml_base
                && let Some(raw) = item.link.filter(|l| Url::parse(l).is_err())
                && let Some(u) = urlnorm::resolve(entry_base.as_ref(), &raw)
            {
                e.link = Some(u);
            }
        }
    }

    let icon_url = feed
        .icon
        .as_ref()
        .map(|i| i.uri.clone())
        .or_else(|| feed.logo.as_ref().map(|l| l.uri.clone()))
        .and_then(|u| urlnorm::resolve(base_url.as_ref(), &u));

    Ok(ParsedFeed {
        title: feed.title.as_ref().map(text_title).unwrap_or_default(),
        site_url,
        description: feed.description.map(|d| d.content),
        icon_url,
        ttl_min: feed.ttl,
        entries,
    })
}

fn pick_alternate_link(links: &[feed_rs::model::Link]) -> Option<String> {
    links
        .iter()
        .find(|l| l.rel.as_deref().is_none_or(|r| r == "alternate") && l.media_type.as_deref().is_none_or(|t| t.contains("html")))
        .or_else(|| links.iter().find(|l| l.rel.as_deref() != Some("self") && l.rel.as_deref() != Some("enclosure")))
        .map(|l| l.href.trim().to_string())
        .filter(|h| !h.is_empty())
}

/// What the raw document says about one item (feed-rs drops or rewrites these).
#[derive(Default)]
struct ScannedItem {
    /// Item-level image feed-rs ignores (a bare `<media:thumbnail>` directly under
    /// `<item>`, `<itunes:image>`, `<media:content medium="image">`).
    image: Option<String>,
    /// The item link as written (RSS `<link>` text, or the alternate Atom `<link href>`).
    link: Option<String>,
}

struct Scan {
    /// Indexed by item order.
    items: Vec<ScannedItem>,
    /// The document uses xml:base (feed-rs resolves against it: keep its links).
    xml_base: bool,
}

fn scan_items(body: &[u8]) -> Scan {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_reader(body);
    reader.config_mut().trim_text(true);
    let mut out: Vec<ScannedItem> = Vec::new();
    let mut xml_base = false;
    let mut depth_in_item: Option<usize> = None;
    // Depth and text so far of an RSS `<link>` being read.
    let mut in_link: Option<(usize, String)> = None;
    let mut depth = 0usize;
    let mut buf = Vec::new();
    while let Ok(ev) = reader.read_event_into(&mut buf) {
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let is_empty = matches!(ev, Event::Empty(_));
                let name = e.name();
                let local_name = name.local_name();
                let local: &str = local_name.as_ref();
                let attr = |k: &str| -> Option<String> {
                    e.attributes()
                        .flatten()
                        .find(|a| AsRef::<str>::as_ref(&a.key) == k)
                        .map(|a| html_escape::decode_html_entities(&a.value).into_owned())
                };
                if attr("xml:base").is_some() {
                    xml_base = true;
                }
                if depth_in_item.is_none() && !is_empty && (local == "item" || local == "entry") {
                    depth_in_item = Some(depth);
                    out.push(ScannedItem::default());
                } else if depth_in_item.is_some() {
                    let full_name: &str = name.as_ref();
                    let full = full_name.to_ascii_lowercase();
                    let candidate = match full.as_str() {
                        "media:thumbnail" => attr("url"),
                        "itunes:image" => attr("href"),
                        "media:content" => {
                            let medium = attr("medium").unwrap_or_default();
                            let ty = attr("type").unwrap_or_default();
                            if medium == "image" || ty.starts_with("image/") { attr("url") } else { None }
                        }
                        _ => None,
                    };
                    if let (Some(c), Some(last)) = (candidate, out.last_mut())
                        && last.image.is_none()
                        && !c.trim().is_empty()
                    {
                        last.image = Some(c.trim().to_string());
                    }
                    if full == "link"
                        && let Some(last) = out.last_mut()
                        && last.link.is_none()
                    {
                        match attr("href") {
                            Some(href) => {
                                if attr("rel").is_none_or(|r| r == "alternate") {
                                    last.link = Some(href.trim().to_string()).filter(|h| !h.is_empty());
                                }
                            }
                            None if !is_empty => in_link = Some((depth, String::new())),
                            None => {}
                        }
                    }
                }
                if !is_empty {
                    depth += 1;
                }
            }
            Event::Text(ref t) => {
                if let Some((_, text)) = in_link.as_mut() {
                    text.push_str(t);
                }
            }
            Event::CData(ref t) => {
                if let Some((_, text)) = in_link.as_mut() {
                    text.push_str(t);
                }
            }
            Event::GeneralRef(ref r) => {
                if let Some((_, text)) = in_link.as_mut() {
                    text.push_str(&html_escape::decode_html_entities(&format!("&{};", &**r)));
                }
            }
            Event::End(_) => {
                depth = depth.saturating_sub(1);
                if in_link.as_ref().is_some_and(|(d, _)| *d == depth)
                    && let (Some((_, text)), Some(last)) = (in_link.take(), out.last_mut())
                {
                    last.link = Some(text.trim().to_string()).filter(|t| !t.is_empty());
                }
                if depth_in_item == Some(depth) {
                    depth_in_item = None;
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Scan { items: out, xml_base }
}

/// Inline formatting tags some feeds put into plain-text titles.
static TITLE_TAGS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"</?(?:b|i|em|strong|span|font|u|s|small|big|mark|sup|sub|br|wbr|a|abbr|cite|q)(?:\s[^<>]*)?/?>").unwrap()
});

/// Title of a feed or entry as plain text. Markup is only parsed when the feed declares
/// HTML (Atom `type="html"`/`"xhtml"`): in plain-text titles `<` is literal ("Vec<T>"),
/// apart from a few inline formatting tags and HTML entities feeds commonly leave in.
fn text_title(t: &feed_rs::model::Text) -> String {
    if t.content_type.as_str().contains("html") {
        clean_title(&t.content)
    } else {
        let s = TITLE_TAGS.replace_all(&t.content, "");
        let s = html_escape::decode_html_entities(&s);
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

/// Titles may contain HTML entities or markup; flatten to text.
pub fn clean_title(t: &str) -> String {
    let s = if t.contains('<') || t.contains('&') {
        crate::util::text::html_to_text(t)
    } else {
        t.to_string()
    };
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const RSS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:media="http://search.yahoo.com/mrss/" xmlns:content="http://purl.org/rss/1.0/modules/content/">
<channel>
  <title><![CDATA[Test News]]></title>
  <link>https://news.example.com/</link>
  <ttl>15</ttl>
  <item>
    <title><![CDATA[First &amp; best story]]></title>
    <description><![CDATA[Short summary.]]></description>
    <content:encoded><![CDATA[<p>Full <b>content</b> here with more words than the summary has.</p>]]></content:encoded>
    <link>https://news.example.com/a/1?at_medium=RSS</link>
    <guid isPermaLink="false">https://news.example.com/a/1#1</guid>
    <pubDate>Tue, 20 Oct 2026 01:31:09 GMT</pubDate>
    <media:thumbnail width="240" height="135" url="https://img.example.com/1.jpg"/>
  </item>
  <item>
    <title>No guid</title>
    <link>/a/2</link>
    <description>Plain text</description>
    <enclosure url="https://cdn.example.com/a.mp3" type="audio/mpeg" length="1234"/>
  </item>
</channel></rss>"#;

    #[test]
    fn parses_rss() {
        let f = parse_feed(RSS.as_bytes(), "https://news.example.com/rss.xml").unwrap();
        assert_eq!(f.title, "Test News");
        assert_eq!(f.ttl_min, Some(15));
        assert_eq!(f.entries.len(), 2);
        let e = &f.entries[0];
        assert_eq!(e.title, "First & best story");
        assert_eq!(e.guid, "https://news.example.com/a/1#1");
        assert!(e.html.contains("Full <b>content</b>"));
        assert_eq!(e.media.thumbnail.as_deref(), Some("https://img.example.com/1.jpg"));
        assert_eq!(e.published, Some(1792459869));
        let e2 = &f.entries[1];
        assert_eq!(e2.link.as_deref(), Some("https://news.example.com/a/2"));
        assert_eq!(e2.media.enclosures.len(), 1);
        assert_eq!(e2.media.enclosures[0].mime, "audio/mpeg");
        assert!(!e2.guid.is_empty());
    }

    #[test]
    fn parses_atom() {
        let atom = r#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Atom Blog</title>
  <link href="https://blog.example.org/"/>
  <id>urn:uuid:1</id>
  <updated>2026-10-19T18:30:02Z</updated>
  <entry>
    <title type="html">Hello &lt;em&gt;World&lt;/em&gt;</title>
    <link href="https://blog.example.org/hello"/>
    <id>tag:blog.example.org,2026:1</id>
    <updated>2026-10-19T18:30:02Z</updated>
    <author><name>Jane</name></author>
    <content type="html">&lt;p&gt;Body&lt;/p&gt;</content>
  </entry>
</feed>"#;
        let f = parse_feed(atom.as_bytes(), "https://blog.example.org/feed").unwrap();
        assert_eq!(f.title, "Atom Blog");
        assert_eq!(f.site_url.as_deref(), Some("https://blog.example.org/"));
        let e = &f.entries[0];
        assert_eq!(e.title, "Hello World");
        assert_eq!(e.author.as_deref(), Some("Jane"));
        assert_eq!(e.guid, "tag:blog.example.org,2026:1");
        assert!(e.html.contains("<p>Body</p>"));
    }

    #[test]
    fn plain_text_titles_keep_angle_brackets() {
        let xml = r#"<?xml version="1.0"?><rss version="2.0"><channel><title>Dev &amp;amp; Ops</title><link>https://www.example.com/</link>
<item><title>Rust: Vec&lt;T&gt; vs &amp;[T] explained</title><link>https://www.example.com/1</link><guid>1</guid></item>
<item><title><![CDATA[Prices < $5 while ratings > 4 stars]]></title><link>https://www.example.com/2</link><guid>2</guid></item>
<item><title><![CDATA[It&#8217;s <b>breaking</b> news]]></title><link>https://www.example.com/3</link><guid>3</guid></item>
</channel></rss>"#;
        let f = parse_feed(xml.as_bytes(), "https://www.example.com/rss").unwrap();
        assert_eq!(f.title, "Dev & Ops");
        let titles: Vec<&str> = f.entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, vec!["Rust: Vec<T> vs &[T] explained", "Prices < $5 while ratings > 4 stars", "It\u{2019}s breaking news"]);
    }

    #[test]
    fn relative_item_links_resolve_against_the_site() {
        let xml = r#"<?xml version="1.0"?><rss version="2.0"><channel><title>T</title><link>https://www.example.com/</link>
<item><title>A</title><link>/news/a?x=1&amp;y=2</link><guid>a</guid></item>
<item><title>B</title><link>https://cdn.example.net/b</link><guid>b</guid></item>
</channel></rss>"#;
        let f = parse_feed(xml.as_bytes(), "https://feeds.feedhost.net/example/rss").unwrap();
        assert_eq!(f.entries[0].link.as_deref(), Some("https://www.example.com/news/a?x=1&y=2"));
        assert_eq!(f.entries[1].link.as_deref(), Some("https://cdn.example.net/b"));
        // xml:base wins (feed-rs applies it).
        let atom = r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom" xml:base="https://base.example.org/blog/"><title>X</title>
<link href="https://www.example.com/"/><entry><title>E</title><id>e1</id><link href="post/1"/></entry></feed>"#;
        let f = parse_feed(atom.as_bytes(), "https://feeds.feedhost.net/x").unwrap();
        assert_eq!(f.entries[0].link.as_deref(), Some("https://base.example.org/blog/post/1"));
    }

    #[test]
    fn parses_gbk_feed() {
        let xml = "<?xml version=\"1.0\" encoding=\"GBK\"?><rss version=\"2.0\"><channel><title>中文源</title><link>http://cn.example.com/</link><item><title>标题一</title><link>http://cn.example.com/1</link><description>内容</description></item></channel></rss>";
        let (bytes, _, _) = encoding_rs::GBK.encode(xml);
        let f = parse_feed(&bytes, "http://cn.example.com/rss").unwrap();
        assert_eq!(f.title, "中文源");
        assert_eq!(f.entries[0].title, "标题一");
    }
}

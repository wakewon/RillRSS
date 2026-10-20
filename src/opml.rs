//! OPML import / export. Per-feed processing options are exported in a `rill:options`
//! attribute (JSON) so a backup restores them; other readers ignore it.

use anyhow::{Result, bail};
use quick_xml::events::Event;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::app::{AppRef, EventRef, Level};
use crate::feeds::fetcher::ensure_category;
use crate::settings::FeedOptions;
use crate::util::{self, html::escape};

#[derive(Debug, Clone, PartialEq)]
pub struct OpmlFeed {
    pub url: String,
    pub title: String,
    pub site_url: Option<String>,
    pub category: Option<String>,
    pub options: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct ImportReport {
    pub found: usize,
    pub added: usize,
    pub existing: usize,
    pub categories: usize,
}

/// Parse an OPML document leniently (nested outlines; the nearest parent outline
/// without xmlUrl is the category).
pub fn parse(body: &[u8]) -> Result<Vec<OpmlFeed>> {
    let text = String::from_utf8_lossy(body);
    if !text.to_ascii_lowercase().contains("<opml") {
        bail!("not an OPML document");
    }
    let mut reader = quick_xml::Reader::from_str(&text);
    reader.config_mut().trim_text(true);
    let mut out = Vec::new();
    // Stack of category names for open (non-feed) outlines.
    let mut stack: Vec<Option<String>> = Vec::new();
    loop {
        let ev = match reader.read_event() {
            Ok(e) => e,
            Err(e) => bail!("OPML parse error: {e}"),
        };
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let name = e.name();
                let local_name = name.local_name();
                let local: &str = local_name.as_ref();
                if !local.eq_ignore_ascii_case("outline") {
                    continue;
                }
                let mut attrs: Vec<(String, String)> = Vec::new();
                for a in e.attributes().flatten() {
                    let k: &str = a.key.as_ref();
                    attrs.push((k.to_string(), html_escape::decode_html_entities(&a.value).into_owned()));
                }
                let get = |k: &str| attrs.iter().find(|(ak, _)| ak.eq_ignore_ascii_case(k)).map(|(_, v)| v.trim().to_string()).filter(|v| !v.is_empty());
                let is_empty = matches!(ev, Event::Empty(_));
                if let Some(url) = get("xmlUrl") {
                    let category = stack.iter().rev().find_map(|c| c.clone());
                    out.push(OpmlFeed {
                        title: get("title").or_else(|| get("text")).unwrap_or_else(|| url.clone()),
                        site_url: get("htmlUrl"),
                        category,
                        options: get("rill:options"),
                        url,
                    });
                    if !is_empty {
                        stack.push(None);
                    }
                } else if !is_empty {
                    stack.push(get("title").or_else(|| get("text")));
                }
            }
            Event::End(ref e) => {
                let name = e.name();
                let local_name = name.local_name();
                let local: &str = local_name.as_ref();
                if local.eq_ignore_ascii_case("outline") {
                    stack.pop();
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

/// Import an OPML document: create categories and feeds (existing URLs are skipped).
pub async fn import(app: &AppRef, body: &[u8]) -> Result<ImportReport> {
    let feeds = parse(body)?;
    let now = util::now();
    let report = app
        .db
        .write(move |tx| {
            let mut r = ImportReport { found: feeds.len(), ..Default::default() };
            let cats_before: i64 = tx.query_row("SELECT COUNT(*) FROM categories", [], |x| x.get(0))?;
            for f in feeds {
                if !(f.url.starts_with("http://") || f.url.starts_with("https://")) {
                    continue;
                }
                if tx.query_row("SELECT id FROM feeds WHERE url = ?", [&f.url], |x| x.get::<_, i64>(0)).optional()?.is_some() {
                    r.existing += 1;
                    continue;
                }
                let cid = match &f.category {
                    Some(c) => ensure_category(tx, c)?,
                    None => None,
                };
                let opts = f.options.as_deref().map(FeedOptions::from_json).unwrap_or_default();
                tx.execute(
                    "INSERT INTO feeds (url, title, site_url, category_id, options, next_fetch_at, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                    params![f.url, f.title, f.site_url, cid, opts.to_json(), now + (r.added as i64 % 60), now, now],
                )?;
                r.added += 1;
            }
            let cats_after: i64 = tx.query_row("SELECT COUNT(*) FROM categories", [], |x| x.get(0))?;
            r.categories = (cats_after - cats_before).max(0) as usize;
            Ok(r)
        })
        .await?;
    app.event(
        Level::Info,
        "system",
        EventRef::default(),
        format!("OPML import: {} feeds found, {} added, {} already subscribed, {} new categories", report.found, report.added, report.existing, report.categories),
    )
    .await;
    app.wake_fetcher.notify_one();
    Ok(report)
}

/// Export all subscriptions as OPML 2.0.
pub async fn export(app: &AppRef) -> Result<String> {
    #[allow(clippy::type_complexity)]
    let rows: Vec<(String, String, Option<String>, Option<String>, String)> = app
        .db
        .call(|c| {
            let mut st = c.prepare(
                "SELECT f.title, f.url, f.site_url, c.name, f.options FROM feeds f LEFT JOIN categories c ON c.id = f.category_id
                 ORDER BY c.name COLLATE NOCASE, f.title COLLATE NOCASE",
            )?;
            let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(v)
        })
        .await?;
    let mut xml = String::new();
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str("<opml version=\"2.0\" xmlns:rill=\"https://github.com/rillrss/rillrss\">\n");
    xml.push_str(&format!(
        "  <head>\n    <title>RillRSS subscriptions</title>\n    <dateCreated>{}</dateCreated>\n  </head>\n  <body>\n",
        chrono::Utc::now().to_rfc2822()
    ));
    let outline = |title: &str, url: &str, site: &Option<String>, opts: &str, indent: &str| -> String {
        let default_opts = FeedOptions::default().to_json();
        let opts_attr = if opts == default_opts || opts == "{}" { String::new() } else { format!(" rill:options=\"{}\"", escape(opts)) };
        format!(
            "{indent}<outline type=\"rss\" text=\"{t}\" title=\"{t}\" xmlUrl=\"{u}\"{h}{opts_attr}/>\n",
            t = escape(title),
            u = escape(url),
            h = site.as_ref().map(|s| format!(" htmlUrl=\"{}\"", escape(s))).unwrap_or_default()
        )
    };
    let mut current: Option<String> = None;
    for (title, url, site, cat, opts) in &rows {
        if cat != &current {
            if current.is_some() {
                xml.push_str("    </outline>\n");
            }
            if let Some(c) = cat {
                xml.push_str(&format!("    <outline text=\"{c}\" title=\"{c}\">\n", c = escape(c)));
            }
            current = cat.clone();
        }
        let indent = if current.is_some() { "      " } else { "    " };
        xml.push_str(&outline(title, url, site, opts, indent));
    }
    if current.is_some() {
        xml.push_str("    </outline>\n");
    }
    xml.push_str("  </body>\n</opml>\n");
    Ok(xml)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_and_flat() {
        let doc = r#"<?xml version="1.0"?>
<opml version="1.0"><head><title>x</title></head><body>
  <outline text="News" title="News">
    <outline type="rss" text="BBC" title="BBC News" xmlUrl="https://feeds.bbci.co.uk/news/rss.xml" htmlUrl="https://www.bbc.co.uk/news"/>
    <outline text="Sub"><outline type="rss" text="Deep" xmlUrl="https://deep.example.com/rss"/></outline>
  </outline>
  <outline type="rss" text="Top &amp; Level" xmlUrl="https://top.example.com/feed?a=1&amp;b=2" rill:options="{&quot;translate&quot;:true}"/>
</body></opml>"#;
        let f = parse(doc.as_bytes()).unwrap();
        assert_eq!(f.len(), 3);
        assert_eq!(f[0].title, "BBC News");
        assert_eq!(f[0].category.as_deref(), Some("News"));
        assert_eq!(f[1].category.as_deref(), Some("Sub"));
        assert_eq!(f[2].category, None);
        assert_eq!(f[2].title, "Top & Level");
        assert_eq!(f[2].url, "https://top.example.com/feed?a=1&b=2");
        assert_eq!(f[2].options.as_deref(), Some("{\"translate\":true}"));
        assert!(parse(b"<html></html>").is_err());
    }
}

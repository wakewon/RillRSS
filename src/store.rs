//! Read/write queries on published items, feeds, categories and labels shared by the
//! reader APIs and the web admin.

use std::collections::HashMap;

use anyhow::Result;
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use crate::util;

/// Offset added to item ids in the Google Reader API so that short ids are 16 decimal
/// digits (some clients, e.g. Readrops, assume FreshRSS-style 16-digit ids) while the
/// TT-RSS API keeps small ids that fit in 32 bits.
pub const GREADER_ID_OFFSET: i64 = 1_000_000_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Order {
    #[default]
    IdDesc,
    IdAsc,
    PublishedDesc,
    PublishedAsc,
    TitleAsc,
}

#[derive(Debug, Clone, Default)]
pub struct ItemQuery {
    pub feed_ids: Option<Vec<i64>>,
    /// Some(Some(id)) = category id, Some(None) = uncategorized.
    pub category: Option<Option<i64>>,
    pub label_id: Option<i64>,
    pub read: Option<bool>,
    pub starred: Option<bool>,
    pub published_flag: Option<bool>,
    pub has_note: bool,
    pub include_hidden: bool,
    /// Insertion (crawl) time window in microseconds, inclusive.
    pub min_created_us: Option<i64>,
    pub max_created_us: Option<i64>,
    /// With `min_created_us`: also match items whose read state changed since (secs).
    pub or_read_since: Option<i64>,
    pub read_since: Option<i64>,
    /// Keyset / id bounds (exclusive).
    pub id_gt: Option<i64>,
    pub id_lt: Option<i64>,
    pub id_in: Option<Vec<i64>>,
    pub search: Option<String>,
    pub order: Order,
    pub limit: Option<usize>,
    pub offset: usize,
}

impl ItemQuery {
    fn where_clause(&self) -> (String, Vec<Value>) {
        let mut w: Vec<String> = Vec::new();
        let mut p: Vec<Value> = Vec::new();
        if !self.include_hidden {
            w.push("i.hidden = 0".into());
        }
        if let Some(f) = &self.feed_ids {
            if f.is_empty() {
                w.push("0".into());
            } else {
                w.push(format!("i.feed_id IN ({})", crate::db::placeholders(f.len())));
                p.extend(f.iter().map(|x| Value::Integer(*x)));
            }
        }
        match self.category {
            Some(Some(c)) => {
                w.push("i.feed_id IN (SELECT id FROM feeds WHERE category_id = ?)".into());
                p.push(Value::Integer(c));
            }
            Some(None) => w.push("i.feed_id IN (SELECT id FROM feeds WHERE category_id IS NULL)".into()),
            None => {}
        }
        if let Some(l) = self.label_id {
            w.push("i.id IN (SELECT item_id FROM item_labels WHERE label_id = ?)".into());
            p.push(Value::Integer(l));
        }
        if let Some(r) = self.read {
            w.push(format!("i.is_read = {}", r as i64));
        }
        if let Some(s) = self.starred {
            w.push(format!("i.is_starred = {}", s as i64));
        }
        if let Some(s) = self.published_flag {
            w.push(format!("i.is_published = {}", s as i64));
        }
        if self.has_note {
            w.push("i.note IS NOT NULL AND i.note <> ''".into());
        }
        match (self.min_created_us, self.or_read_since) {
            (Some(min), Some(rs)) => {
                w.push("(i.created_at >= ? OR (i.is_read = 1 AND i.read_at >= ?))".into());
                p.push(Value::Integer(min));
                p.push(Value::Integer(rs));
            }
            (Some(min), None) => {
                w.push("i.created_at >= ?".into());
                p.push(Value::Integer(min));
            }
            _ => {}
        }
        if let Some(max) = self.max_created_us {
            w.push("i.created_at <= ?".into());
            p.push(Value::Integer(max));
        }
        if let Some(rs) = self.read_since {
            w.push("i.read_at >= ?".into());
            p.push(Value::Integer(rs));
        }
        if let Some(x) = self.id_gt {
            w.push("i.id > ?".into());
            p.push(Value::Integer(x));
        }
        if let Some(x) = self.id_lt {
            w.push("i.id < ?".into());
            p.push(Value::Integer(x));
        }
        if let Some(ids) = &self.id_in {
            if ids.is_empty() {
                w.push("0".into());
            } else {
                w.push(format!("i.id IN ({})", crate::db::placeholders(ids.len())));
                p.extend(ids.iter().map(|x| Value::Integer(*x)));
            }
        }
        if let Some(s) = self.search.as_ref().filter(|s| !s.trim().is_empty()) {
            for term in s.split_whitespace().take(8) {
                w.push("(i.title LIKE ? ESCAPE '\\' OR i.excerpt LIKE ? ESCAPE '\\')".into());
                let pat = format!("%{}%", term.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
                p.push(Value::Text(pat.clone()));
                p.push(Value::Text(pat));
            }
        }
        let sql = if w.is_empty() { "1".to_string() } else { w.join(" AND ") };
        (sql, p)
    }

    fn order_sql(&self) -> &'static str {
        match self.order {
            Order::IdDesc => "i.id DESC",
            Order::IdAsc => "i.id ASC",
            Order::PublishedDesc => "i.published_at DESC, i.id DESC",
            Order::PublishedAsc => "i.published_at ASC, i.id ASC",
            Order::TitleAsc => "i.title COLLATE NOCASE ASC, i.id ASC",
        }
    }

    pub fn ids(&self, conn: &Connection) -> Result<Vec<i64>> {
        let (w, mut p) = self.where_clause();
        let mut sql = format!("SELECT i.id FROM items i WHERE {w} ORDER BY {}", self.order_sql());
        match self.limit {
            Some(l) => {
                sql.push_str(" LIMIT ? OFFSET ?");
                p.push(Value::Integer(l as i64));
                p.push(Value::Integer(self.offset as i64));
            }
            None if self.offset > 0 => {
                sql.push_str(" LIMIT -1 OFFSET ?");
                p.push(Value::Integer(self.offset as i64));
            }
            None => {}
        }
        let mut st = conn.prepare(&sql)?;
        let v = st.query_map(params_from_iter(p), |r| r.get(0))?.collect::<Result<Vec<i64>, _>>()?;
        Ok(v)
    }

    pub fn count(&self, conn: &Connection) -> Result<i64> {
        let (w, p) = self.where_clause();
        Ok(conn.query_row(&format!("SELECT COUNT(*) FROM items i WHERE {w}"), params_from_iter(p), |r| r.get(0))?)
    }

    /// Mark all matching items read. Returns the number of changed rows.
    pub fn mark_read(&self, conn: &Connection) -> Result<usize> {
        let (w, p) = self.where_clause();
        let sql = format!("UPDATE items SET is_read = 1, read_at = {} WHERE id IN (SELECT i.id FROM items i WHERE {w} AND i.is_read = 0)", util::now());
        Ok(conn.execute(&sql, params_from_iter(p))?)
    }
}

#[derive(Debug, Clone)]
pub struct ItemRow {
    pub id: i64,
    pub feed_id: i64,
    pub feed_title: String,
    pub feed_url: String,
    pub site_url: String,
    pub category_id: Option<i64>,
    pub category: Option<String>,
    pub guid: String,
    pub version: i64,
    pub title: String,
    pub link: String,
    pub author: String,
    pub content: String,
    pub excerpt: String,
    pub media: Option<String>,
    pub lang: String,
    pub published_at: i64,
    pub updated_at: i64,
    pub created_at: i64,
    pub is_read: bool,
    pub is_starred: bool,
    pub is_published: bool,
    pub note: Option<String>,
    pub superseded_by: Option<i64>,
    pub labels: Vec<(i64, String)>,
}

/// Load items by id (any order; unknown ids are skipped). `with_content` = false avoids
/// loading the HTML body.
pub fn load_items(conn: &Connection, ids: &[i64], with_content: bool) -> Result<Vec<ItemRow>> {
    let mut out = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(500) {
        let sql = format!(
            "SELECT i.id, i.feed_id, f.title, f.url, COALESCE(f.site_url, ''), f.category_id, c.name, i.guid, i.version, i.title,
                    COALESCE(i.link, ''), COALESCE(i.author, ''), {}, i.excerpt, i.media, COALESCE(i.lang, ''), i.published_at,
                    i.updated_at, i.created_at, i.is_read, i.is_starred, i.is_published, i.note, i.superseded_by
             FROM items i JOIN feeds f ON f.id = i.feed_id LEFT JOIN categories c ON c.id = f.category_id
             WHERE i.id IN ({})",
            if with_content { "i.content" } else { "''" },
            crate::db::placeholders(chunk.len())
        );
        let mut st = conn.prepare(&sql)?;
        let rows = st.query_map(params_from_iter(chunk.iter()), |r| {
            Ok(ItemRow {
                id: r.get(0)?,
                feed_id: r.get(1)?,
                feed_title: r.get(2)?,
                feed_url: r.get(3)?,
                site_url: r.get(4)?,
                category_id: r.get(5)?,
                category: r.get(6)?,
                guid: r.get(7)?,
                version: r.get(8)?,
                title: r.get(9)?,
                link: r.get(10)?,
                author: r.get(11)?,
                content: r.get(12)?,
                excerpt: r.get(13)?,
                media: r.get(14)?,
                lang: r.get(15)?,
                published_at: r.get(16)?,
                updated_at: r.get(17)?,
                created_at: r.get(18)?,
                is_read: r.get::<_, i64>(19)? != 0,
                is_starred: r.get::<_, i64>(20)? != 0,
                is_published: r.get::<_, i64>(21)? != 0,
                note: r.get(22)?,
                superseded_by: r.get(23)?,
                labels: Vec::new(),
            })
        })?;
        for r in rows {
            out.push(r?);
        }
    }
    // Labels
    if !out.is_empty() {
        let mut map: HashMap<i64, Vec<(i64, String)>> = HashMap::new();
        for chunk in ids.chunks(500) {
            let sql = format!(
                "SELECT il.item_id, l.id, l.name FROM item_labels il JOIN labels l ON l.id = il.label_id WHERE il.item_id IN ({})",
                crate::db::placeholders(chunk.len())
            );
            let mut st = conn.prepare(&sql)?;
            let rows = st.query_map(params_from_iter(chunk.iter()), |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)))?;
            for r in rows {
                let (iid, lid, name) = r?;
                map.entry(iid).or_default().push((lid, name));
            }
        }
        for it in &mut out {
            if let Some(l) = map.remove(&it.id) {
                it.labels = l;
            }
        }
    }
    Ok(out)
}

/// Load items preserving the order of `ids`.
pub fn load_items_ordered(conn: &Connection, ids: &[i64], with_content: bool) -> Result<Vec<ItemRow>> {
    let mut items = load_items(conn, ids, with_content)?;
    let pos: HashMap<i64, usize> = ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    items.sort_by_key(|it| pos.get(&it.id).copied().unwrap_or(usize::MAX));
    Ok(items)
}

// ---------------------------------------------------------------------------
// State changes
// ---------------------------------------------------------------------------

pub fn set_read(conn: &Connection, ids: &[i64], read: bool) -> Result<usize> {
    let now = util::now();
    let mut n = 0;
    for chunk in ids.chunks(500) {
        let sql = if read {
            format!("UPDATE items SET is_read = 1, read_at = ? WHERE is_read = 0 AND id IN ({})", crate::db::placeholders(chunk.len()))
        } else {
            format!("UPDATE items SET is_read = 0, read_at = ? WHERE is_read = 1 AND id IN ({})", crate::db::placeholders(chunk.len()))
        };
        let mut p: Vec<Value> = vec![Value::Integer(now)];
        p.extend(chunk.iter().map(|x| Value::Integer(*x)));
        n += conn.execute(&sql, params_from_iter(p))?;
    }
    Ok(n)
}

pub fn set_starred(conn: &Connection, ids: &[i64], starred: bool) -> Result<usize> {
    let now = util::now();
    let mut n = 0;
    for chunk in ids.chunks(500) {
        let sql = format!(
            "UPDATE items SET is_starred = ?, starred_at = ? WHERE is_starred <> ? AND id IN ({})",
            crate::db::placeholders(chunk.len())
        );
        let mut p: Vec<Value> = vec![Value::Integer(starred as i64), Value::Integer(now), Value::Integer(starred as i64)];
        p.extend(chunk.iter().map(|x| Value::Integer(*x)));
        n += conn.execute(&sql, params_from_iter(p))?;
    }
    Ok(n)
}

pub fn set_published_flag(conn: &Connection, ids: &[i64], v: bool) -> Result<usize> {
    let mut n = 0;
    for chunk in ids.chunks(500) {
        let sql = format!("UPDATE items SET is_published = ? WHERE is_published <> ? AND id IN ({})", crate::db::placeholders(chunk.len()));
        let mut p: Vec<Value> = vec![Value::Integer(v as i64), Value::Integer(v as i64)];
        p.extend(chunk.iter().map(|x| Value::Integer(*x)));
        n += conn.execute(&sql, params_from_iter(p))?;
    }
    Ok(n)
}

pub fn set_note(conn: &Connection, ids: &[i64], note: &str) -> Result<usize> {
    let mut n = 0;
    let note = if note.is_empty() { None } else { Some(note.to_string()) };
    for id in ids {
        n += conn.execute("UPDATE items SET note = ? WHERE id = ?", params![note, id])?;
    }
    Ok(n)
}

pub fn ensure_label(conn: &Connection, name: &str) -> Result<i64> {
    let name = name.trim();
    if let Some(id) = conn.query_row("SELECT id FROM labels WHERE name = ? COLLATE NOCASE", [name], |r| r.get(0)).optional()? {
        return Ok(id);
    }
    conn.execute("INSERT INTO labels (name, created_at) VALUES (?, ?)", params![name, util::now()])?;
    Ok(conn.last_insert_rowid())
}

pub fn find_label(conn: &Connection, name: &str) -> Result<Option<i64>> {
    Ok(conn.query_row("SELECT id FROM labels WHERE name = ? COLLATE NOCASE", [name.trim()], |r| r.get(0)).optional()?)
}

pub fn set_label(conn: &Connection, ids: &[i64], label_id: i64, assign: bool) -> Result<usize> {
    let mut n = 0;
    for id in ids {
        n += if assign {
            conn.execute("INSERT OR IGNORE INTO item_labels (item_id, label_id) SELECT id, ? FROM items WHERE id = ?", params![label_id, id])?
        } else {
            conn.execute("DELETE FROM item_labels WHERE item_id = ? AND label_id = ?", params![id, label_id])?
        };
    }
    Ok(n)
}

// ---------------------------------------------------------------------------
// Feeds / categories / labels listings
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct FeedInfo {
    pub id: i64,
    pub title: String,
    pub url: String,
    pub site_url: String,
    pub category_id: Option<i64>,
    pub category: Option<String>,
    pub unread: i64,
    pub last_success_at: Option<i64>,
    pub last_error: Option<String>,
    pub has_icon: bool,
    pub icon_hash: Option<String>,
    pub newest_created_us: i64,
    pub sort_order: i64,
}

pub fn list_feeds(conn: &Connection) -> Result<Vec<FeedInfo>> {
    let unread = unread_by_feed(conn)?;
    let newest: HashMap<i64, i64> = {
        let mut st = conn.prepare("SELECT feed_id, MAX(created_at) FROM items GROUP BY feed_id")?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?
    };
    let mut st = conn.prepare(
        "SELECT f.id, f.title, f.url, COALESCE(f.site_url, ''), f.category_id, c.name, f.last_success_at, f.last_error,
                (SELECT length(data) > 0 FROM feed_icons fi WHERE fi.feed_id = f.id), (SELECT hash FROM feed_icons fi WHERE fi.feed_id = f.id),
                f.sort_order
         FROM feeds f LEFT JOIN categories c ON c.id = f.category_id
         ORDER BY c.name COLLATE NOCASE, f.title COLLATE NOCASE",
    )?;
    let rows = st.query_map([], |r| {
        Ok(FeedInfo {
            id: r.get(0)?,
            title: r.get(1)?,
            url: r.get(2)?,
            site_url: r.get(3)?,
            category_id: r.get(4)?,
            category: r.get(5)?,
            unread: 0,
            last_success_at: r.get(6)?,
            last_error: r.get(7)?,
            has_icon: r.get::<_, Option<bool>>(8)?.unwrap_or(false),
            icon_hash: r.get(9)?,
            newest_created_us: 0,
            sort_order: r.get(10)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        let mut f = r?;
        f.unread = unread.get(&f.id).copied().unwrap_or(0);
        f.newest_created_us = newest.get(&f.id).copied().unwrap_or(0);
        out.push(f);
    }
    Ok(out)
}

pub fn unread_by_feed(conn: &Connection) -> Result<HashMap<i64, i64>> {
    let mut st = conn.prepare("SELECT feed_id, COUNT(*) FROM items WHERE hidden = 0 AND is_read = 0 GROUP BY feed_id")?;
    let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
    Ok(v)
}

#[derive(Debug, Clone)]
pub struct CategoryInfo {
    pub id: i64,
    pub name: String,
    pub sort_order: i64,
}

pub fn list_categories(conn: &Connection) -> Result<Vec<CategoryInfo>> {
    let mut st = conn.prepare("SELECT id, name, sort_order FROM categories ORDER BY sort_order, name COLLATE NOCASE")?;
    let v = st
        .query_map([], |r| Ok(CategoryInfo { id: r.get(0)?, name: r.get(1)?, sort_order: r.get(2)? }))?
        .collect::<Result<_, _>>()?;
    Ok(v)
}

pub fn find_category(conn: &Connection, name: &str) -> Result<Option<i64>> {
    Ok(conn.query_row("SELECT id FROM categories WHERE name = ? COLLATE NOCASE", [name.trim()], |r| r.get(0)).optional()?)
}

#[derive(Debug, Clone)]
pub struct LabelInfo {
    pub id: i64,
    pub name: String,
    pub unread: i64,
    pub total: i64,
}

pub fn list_labels(conn: &Connection) -> Result<Vec<LabelInfo>> {
    let mut st = conn.prepare(
        "SELECT l.id, l.name,
                (SELECT COUNT(*) FROM item_labels il JOIN items i ON i.id = il.item_id WHERE il.label_id = l.id AND i.is_read = 0 AND i.hidden = 0),
                (SELECT COUNT(*) FROM item_labels il WHERE il.label_id = l.id)
         FROM labels l ORDER BY l.name COLLATE NOCASE",
    )?;
    let v = st
        .query_map([], |r| Ok(LabelInfo { id: r.get(0)?, name: r.get(1)?, unread: r.get(2)?, total: r.get(3)? }))?
        .collect::<Result<_, _>>()?;
    Ok(v)
}

/// Move a feed to a category (None = uncategorized).
pub fn set_feed_category(conn: &Connection, feed_id: i64, category_id: Option<i64>) -> Result<()> {
    conn.execute("UPDATE feeds SET category_id = ?, updated_at = ? WHERE id = ?", params![category_id, util::now(), feed_id])?;
    Ok(())
}

pub fn rename_feed(conn: &Connection, feed_id: i64, title: &str) -> Result<()> {
    conn.execute("UPDATE feeds SET title = ?, title_locked = 1, updated_at = ? WHERE id = ?", params![title.trim(), util::now(), feed_id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn setup() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        {
            let c = db.conn().unwrap();
            c.execute("INSERT INTO categories (name, created_at) VALUES ('News', 0)", []).unwrap();
            c.execute("INSERT INTO feeds (url, title, category_id, created_at, updated_at) VALUES ('u1', 'F1', 1, 0, 0)", []).unwrap();
            c.execute("INSERT INTO feeds (url, title, created_at, updated_at) VALUES ('u2', 'F2', 0, 0)", []).unwrap();
            for i in 0..10 {
                let feed = if i % 2 == 0 { 1 } else { 2 };
                c.execute(
                    "INSERT INTO items (feed_id, guid, title, content, published_at, updated_at, created_at, is_read, is_starred)
                     VALUES (?, ?, ?, '<p>x</p>', ?, ?, ?, ?, ?)",
                    params![feed, format!("g{i}"), format!("Title {i}"), 1000 + i, 1000 + i, (1000 + i) * 1_000_000, (i % 3 == 0) as i64, (i == 4) as i64],
                )
                .unwrap();
            }
        }
        (dir, db)
    }

    #[test]
    fn query_filters() {
        let (_d, db) = setup();
        let c = db.conn().unwrap();
        let all = ItemQuery::default().ids(&c).unwrap();
        assert_eq!(all.len(), 10);
        assert_eq!(all[0], 10); // newest first
        let unread = ItemQuery { read: Some(false), ..Default::default() }.ids(&c).unwrap();
        assert_eq!(unread.len(), 6);
        let cat = ItemQuery { category: Some(Some(1)), ..Default::default() }.count(&c).unwrap();
        assert_eq!(cat, 5);
        let uncat = ItemQuery { category: Some(None), ..Default::default() }.count(&c).unwrap();
        assert_eq!(uncat, 5);
        let page = ItemQuery { limit: Some(3), id_lt: Some(8), ..Default::default() }.ids(&c).unwrap();
        assert_eq!(page, vec![7, 6, 5]);
        let window = ItemQuery { min_created_us: Some(1005 * 1_000_000), max_created_us: Some(1007 * 1_000_000), ..Default::default() }.ids(&c).unwrap();
        assert_eq!(window, vec![8, 7, 6]);
        let star = ItemQuery { starred: Some(true), ..Default::default() }.ids(&c).unwrap();
        assert_eq!(star, vec![5]);
        let search = ItemQuery { search: Some("title 3".into()), ..Default::default() }.ids(&c).unwrap();
        assert_eq!(search, vec![4]);
    }

    #[test]
    fn state_changes_and_labels() {
        let (_d, db) = setup();
        let c = db.conn().unwrap();
        assert_eq!(set_read(&c, &[2, 3], true).unwrap(), 2);
        assert_eq!(set_starred(&c, &[2], true).unwrap(), 1);
        let l = ensure_label(&c, "Later").unwrap();
        assert_eq!(ensure_label(&c, "later").unwrap(), l);
        set_label(&c, &[2, 3], l, true).unwrap();
        let items = load_items_ordered(&c, &[3, 2, 99], true).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, 3);
        assert!(items[1].is_starred);
        assert_eq!(items[0].labels, vec![(l, "Later".to_string())]);
        assert_eq!(items[1].feed_title, "F2");
        let labels = list_labels(&c).unwrap();
        assert_eq!(labels[0].total, 2);
        let n = ItemQuery { feed_ids: Some(vec![1]), ..Default::default() }.mark_read(&c).unwrap();
        assert!(n > 0);
        assert_eq!(ItemQuery { feed_ids: Some(vec![1]), read: Some(false), ..Default::default() }.count(&c).unwrap(), 0);
        let feeds = list_feeds(&c).unwrap();
        assert_eq!(feeds.len(), 2);
    }
}

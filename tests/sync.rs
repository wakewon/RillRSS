//! Simulated reader clients syncing through the APIs, as real clients do (see the client
//! research): they cache content per article id forever, so updates must arrive as new
//! ids, and the old version must leave their unread list.

mod common;

use std::collections::{HashMap, HashSet};

use common::*;
use serde_json::{Value, json};

/// NetNewsWire / Capy style Google Reader client.
#[derive(Default)]
struct GrClient {
    /// short id -> (title, content, read, starred)
    items: HashMap<String, (String, String, bool, bool)>,
    last_sync: i64,
}

fn long_to_short(long: &str) -> String {
    let hex = long.rsplit('/').next().unwrap();
    (u64::from_str_radix(hex, 16).unwrap() as i64).to_string()
}

impl GrClient {
    async fn ids(&self, t: &TestApp, auth: &str, query: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut c: Option<String> = None;
        loop {
            let q = match &c {
                Some(c) => format!("stream/items/ids?output=json&n=2&{query}&c={c}"),
                None => format!("stream/items/ids?output=json&n=2&{query}"),
            };
            let v = t.gr_get(auth, &q).await;
            for r in v["itemRefs"].as_array().unwrap() {
                out.push(r["id"].as_str().unwrap().to_string());
            }
            match v.get("continuation").and_then(|x| x.as_str()) {
                Some(next) => c = Some(next.to_string()),
                None => break,
            }
        }
        out
    }

    async fn sync(&mut self, t: &TestApp, auth: &str) {
        let unread: HashSet<String> = self.ids(t, auth, "s=user/-/state/com.google/reading-list&xt=user/-/state/com.google/read").await.into_iter().collect();
        let starred: HashSet<String> = self.ids(t, auth, "s=user/-/state/com.google/starred").await.into_iter().collect();
        let recent = self.ids(t, auth, &format!("s=user/-/state/com.google/reading-list&ot={}", self.last_sync)).await;
        // Fetch contents only for ids we do not have (NetNewsWire never re-downloads).
        let missing: Vec<String> = unread.iter().chain(starred.iter()).chain(recent.iter()).filter(|id| !self.items.contains_key(*id)).cloned().collect::<HashSet<_>>().into_iter().collect();
        if !missing.is_empty() {
            let mut form: Vec<(&str, String)> = Vec::new();
            for id in &missing {
                // send the long hex form like NetNewsWire
                form.push(("i", format!("tag:google.com,2005:reader/item/{:016x}", id.parse::<i64>().unwrap())));
            }
            let form_ref: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
            let r = t
                .client
                .post(t.url("/api/greader.php/reader/api/0/stream/items/contents"))
                .header("Authorization", format!("GoogleLogin auth={auth}"))
                .form(&form_ref)
                .send()
                .await
                .unwrap();
            let v: Value = r.json().await.unwrap();
            assert!(v["id"].is_string() && v["updated"].is_i64(), "NetNewsWire needs id + updated");
            let items = v["items"].as_array().unwrap();
            assert_eq!(items.len(), missing.len(), "contents returns exactly the requested ids");
            for it in items {
                let short = long_to_short(it["id"].as_str().unwrap());
                assert!(it["crawlTimeMsec"].is_string() && it["timestampUsec"].is_string() && it["published"].is_i64());
                assert!(it["origin"]["streamId"].is_string() && it["canonical"][0]["href"].is_string());
                self.items.insert(
                    short,
                    (it["title"].as_str().unwrap().to_string(), it["summary"]["content"].as_str().unwrap().to_string(), false, false),
                );
            }
        }
        for (id, v) in self.items.iter_mut() {
            v.2 = !unread.contains(id);
            v.3 = starred.contains(id);
        }
        self.last_sync = chrono::Utc::now().timestamp();
    }

    fn unread_titles(&self) -> Vec<String> {
        let mut v: Vec<String> = self.items.values().filter(|x| !x.2).map(|x| x.0.clone()).collect();
        v.sort();
        v
    }
}

/// TTRSS-Reader / News+ style client: since_id for new articles + getCompactHeadlines
/// for unread reconciliation.
#[derive(Default)]
struct TtClient {
    items: HashMap<i64, (String, String, bool, bool)>,
    max_id: i64,
}

impl TtClient {
    async fn sync(&mut self, t: &TestApp, sid: &str) {
        let mut skip = 0;
        loop {
            let page = t
                .tt_op(sid, json!({"op": "getHeadlines", "feed_id": "-4", "since_id": self.max_id.to_string(), "limit": "2", "skip": skip.to_string(), "show_content": "1", "view_mode": "all_articles"}))
                .await;
            let arr = page.as_array().unwrap().clone();
            for h in &arr {
                assert!(h["unread"].is_boolean() && h["marked"].is_boolean(), "booleans must be JSON booleans");
                assert!(h["id"].as_i64().unwrap() < i32::MAX as i64, "TT-RSS ids must fit in 32 bits");
                let id = h["id"].as_i64().unwrap();
                self.items.entry(id).or_insert((h["title"].as_str().unwrap().into(), h["content"].as_str().unwrap().into(), false, false));
            }
            if arr.len() < 2 {
                break;
            }
            skip += arr.len();
        }
        self.max_id = self.items.keys().copied().max().unwrap_or(0);
        let unread: HashSet<i64> = t
            .tt_op(sid, json!({"op": "getCompactHeadlines", "feed_id": -4, "view_mode": "unread"}))
            .await
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["id"].as_i64().unwrap())
            .collect();
        let starred: HashSet<i64> = t
            .tt_op(sid, json!({"op": "getCompactHeadlines", "feed_id": -1}))
            .await
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["id"].as_i64().unwrap())
            .collect();
        for (id, v) in self.items.iter_mut() {
            v.2 = !unread.contains(id);
            v.3 = starred.contains(id);
        }
    }
    fn unread_titles(&self) -> Vec<String> {
        let mut v: Vec<String> = self.items.values().filter(|x| !x.2).map(|x| x.0.clone()).collect();
        v.sort();
        v
    }
}

async fn setup_two_stories() -> (Mock, TestApp, i64) {
    let mock = start_mock().await;
    let t = start_app(|_| {}).await;
    mock.set(
        "/f.xml",
        "application/rss+xml",
        rss(
            "Feed",
            &[entry("a", "Story A", "https://n.example.com/a", &paragraphs("Alpha", 3)), entry("b", "Story B", "https://n.example.com/b", &paragraphs("Bravo", 3))],
        ),
    );
    let fid = t.add_feed(&mock.url("/f.xml"), json!({})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    (mock, t, fid)
}

#[tokio::test]
async fn greader_client_sees_updates_as_new_items() {
    let (mock, t, fid) = setup_two_stories().await;
    let auth = t.greader_login().await;
    let mut client = GrClient::default();
    client.sync(&t, &auth).await;
    assert_eq!(client.unread_titles(), vec!["Story A", "Story B"]);

    // Read A on the device; star B.
    let a_id = client.items.iter().find(|(_, v)| v.0 == "Story A").map(|(k, _)| k.clone()).unwrap();
    let b_id = client.items.iter().find(|(_, v)| v.0 == "Story B").map(|(k, _)| k.clone()).unwrap();
    assert_eq!(t.gr_post(&auth, "edit-tag", &[("i", a_id.as_str()), ("a", "user/-/state/com.google/read"), ("T", "x")]).await, (200, "OK".into()));
    assert_eq!(t.gr_post(&auth, "edit-tag", &[("i", b_id.as_str()), ("a", "user/-/state/com.google/starred")]).await, (200, "OK".into()));

    // Story B is substantively updated.
    mock.set(
        "/f.xml",
        "application/rss+xml",
        rss(
            "Feed",
            &[
                entry("a", "Story A", "https://n.example.com/a", &paragraphs("Alpha", 3)),
                entry("b", "Story B escalates", "https://n.example.com/b", &format!("{}{}", paragraphs("Bravo", 3), paragraphs("Charlie", 3))),
            ],
        ),
    );
    t.fetch(fid).await;
    t.wait_idle().await;
    client.sync(&t, &auth).await;
    assert_eq!(client.items.len(), 3, "old version kept locally, new version added");
    assert_eq!(client.unread_titles(), vec!["[更新] Story B escalates"]);
    let new = client.items.values().find(|v| v.0.contains("escalates")).unwrap();
    assert!(new.1.contains("Charlie"), "client got the new content");
    assert!(new.3, "star carried to the new version");
    assert!(client.items.get(&b_id).unwrap().3, "old version keeps its star");

    // Story A (already read) is updated too: the update is unread again.
    mock.set(
        "/f.xml",
        "application/rss+xml",
        rss(
            "Feed",
            &[
                entry("a", "Story A revised", "https://n.example.com/a", &format!("{}{}", paragraphs("Alpha", 3), paragraphs("Delta", 3))),
                entry("b", "Story B escalates", "https://n.example.com/b", &format!("{}{}", paragraphs("Bravo", 3), paragraphs("Charlie", 3))),
            ],
        ),
    );
    t.fetch(fid).await;
    t.wait_idle().await;
    client.sync(&t, &auth).await;
    assert_eq!(client.items.len(), 4);
    assert_eq!(client.unread_titles(), vec!["[更新] Story A revised", "[更新] Story B escalates"]);
}

#[tokio::test]
async fn ttrss_client_sees_updates_as_new_items() {
    let (mock, t, fid) = setup_two_stories().await;
    let sid = t.tt_login().await;
    let mut client = TtClient::default();
    client.sync(&t, &sid).await;
    assert_eq!(client.unread_titles(), vec!["Story A", "Story B"]);
    let b = *client.items.iter().find(|(_, v)| v.0 == "Story B").unwrap().0;
    t.tt_op(&sid, json!({"op": "updateArticle", "article_ids": b.to_string(), "mode": 1, "field": 0})).await;

    mock.set(
        "/f.xml",
        "application/rss+xml",
        rss(
            "Feed",
            &[
                entry("a", "Story A", "https://n.example.com/a", &paragraphs("Alpha", 3)),
                entry("b", "Story B escalates", "https://n.example.com/b", &format!("{}{}", paragraphs("Bravo", 3), paragraphs("Echo", 3))),
            ],
        ),
    );
    t.fetch(fid).await;
    t.wait_idle().await;
    client.sync(&t, &sid).await;
    assert_eq!(client.items.len(), 3);
    assert_eq!(client.unread_titles(), vec!["Story A", "[更新] Story B escalates"]);
    let (new_id, new) = client.items.iter().find(|(_, v)| v.0.contains("escalates")).unwrap();
    assert!(*new_id > b);
    assert!(new.1.contains("Echo"));
    assert!(new.3, "star carried over");
}

#[tokio::test]
async fn fluent_reader_style_stream_contents_sync() {
    let (mock, t, fid) = setup_two_stories().await;
    let auth = t.greader_login().await;
    // stream/contents without a stream id = reading list; ot = crawl time of newest item.
    let v = t.gr_get(&auth, "stream/contents?output=json&n=1000").await;
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    let newest_crawl: i64 = items[0]["crawlTimeMsec"].as_str().unwrap().parse::<i64>().unwrap() / 1000;
    let seen: HashSet<String> = items.iter().map(|i| i["id"].as_str().unwrap().to_string()).collect();

    mock.set(
        "/f.xml",
        "application/rss+xml",
        rss(
            "Feed",
            &[
                entry("a", "Story A", "https://n.example.com/a", &paragraphs("Alpha", 3)),
                entry("b", "Story B", "https://n.example.com/b", &format!("{}{}", paragraphs("Bravo", 3), paragraphs("Foxtrot", 3))),
                entry("c", "Story C", "https://n.example.com/c", &paragraphs("Golf", 3)),
            ],
        ),
    );
    t.fetch(fid).await;
    t.wait_idle().await;
    let v = t.gr_get(&auth, &format!("stream/contents?output=json&n=1000&ot={newest_crawl}")).await;
    let new_ids: Vec<String> = v["items"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap().to_string()).filter(|id| !seen.contains(id)).collect();
    assert_eq!(new_ids.len(), 2, "new story + updated version both appear after ot");
    // Newest first by crawl order.
    let ids: Vec<String> = v["items"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap().to_string()).collect();
    let mut sorted = ids.clone();
    sorted.sort();
    sorted.reverse();
    assert_eq!(ids, sorted);
}

#[tokio::test]
async fn read_since_query_reports_superseded_versions() {
    // Read You asks for read ids with ot = one month ago; superseded versions of older
    // stories must show up there once they become read.
    let (mock, t, fid) = setup_two_stories().await;
    let auth = t.greader_login().await;
    // Pretend the stories were crawled long ago.
    t.app.db.call(|c| Ok(c.execute("UPDATE items SET created_at = created_at - 90 * 86400 * 1000000", [])?)).await.unwrap();
    mock.set(
        "/f.xml",
        "application/rss+xml",
        rss(
            "Feed",
            &[
                entry("a", "Story A", "https://n.example.com/a", &format!("{}{}", paragraphs("Alpha", 3), paragraphs("Hotel", 3))),
                entry("b", "Story B", "https://n.example.com/b", &paragraphs("Bravo", 3)),
            ],
        ),
    );
    t.fetch(fid).await;
    t.wait_idle().await;
    let month_ago = chrono::Utc::now().timestamp() - 30 * 86400;
    let v = t.gr_get(&auth, &format!("stream/items/ids?output=json&n=1000&s=user/-/state/com.google/reading-list&it=user/-/state/com.google/read&ot={month_ago}")).await;
    let refs = v["itemRefs"].as_array().unwrap();
    assert_eq!(refs.len(), 1, "the superseded old version is reported as read: {v}");
}

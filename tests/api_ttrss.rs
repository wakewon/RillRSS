//! TT-RSS API conformance.

mod common;

use common::*;
use serde_json::{Value, json};

async fn setup() -> (Mock, TestApp, String) {
    let mock = start_mock().await;
    let t = start_app(|_| {}).await;
    let entries: Vec<Entry> = (0..4).map(|i| entry(&format!("g{i}"), &format!("Item {i}"), &format!("https://n.example.com/{i}"), &paragraphs(&format!("Body{i}"), 1))).collect();
    mock.set("/f.xml", "application/rss+xml", rss("Feed One", &entries));
    let fid = t.add_feed(&mock.url("/f.xml"), json!({})).await;
    t.app
        .db
        .write(move |tx| {
            let cid = rillrss::feeds::fetcher::ensure_category(tx, "Tech")?;
            rillrss::store::set_feed_category(tx, fid, cid)?;
            Ok(())
        })
        .await
        .unwrap();
    t.fetch(fid).await;
    t.wait_idle().await;
    let sid = t.tt_login().await;
    (mock, t, sid)
}

#[tokio::test]
async fn envelope_and_auth() {
    let (_m, t, sid) = setup().await;
    // Raw body (key order seq, status, content).
    let raw = t.client.post(t.url("/api/")).body(r#"{"op":"getApiLevel","sid":"nope","seq":7}"#).send().await.unwrap().text().await.unwrap();
    assert_eq!(raw, r#"{"seq":null,"status":1,"content":{"error":"NOT_LOGGED_IN"}}"#);
    let bad = t.tt(json!({"op": "login", "user": "admin", "password": "wrong"})).await;
    assert_eq!(bad["content"]["error"], "LOGIN_ERROR");
    let lvl = t.tt(json!({"op": "getApiLevel", "sid": sid, "seq": 3})).await;
    assert_eq!(lvl, json!({"seq": 3, "status": 0, "content": {"level": 25}}));
    // All paths, form-encoded content type ignored.
    for p in ["/api", "/api/index.php", "/tt-rss/api/"] {
        let r: Value = t
            .client
            .post(t.url(p))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(json!({"op": "isLoggedIn", "sid": sid}).to_string())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(r["content"]["status"], true, "{p}");
    }
    let unknown = t.tt(json!({"op": "doesNotExist", "sid": sid})).await;
    assert_eq!(unknown["content"], json!({"error": "UNKNOWN_METHOD", "method": "doesnotexist"}));
    // base64 password leniency for very old clients
    let b64 = t.tt(json!({"op": "login", "user": "admin", "password": "cGFzc3dvcmQxMjM="})).await;
    assert_eq!(b64["status"], 0);
    assert_eq!(t.tt_op(&sid, json!({"op": "logout"})).await["status"], "OK");
    assert_eq!(t.tt(json!({"op": "isLoggedIn", "sid": sid})).await["content"]["status"], false);
}

#[tokio::test]
async fn feeds_categories_counters_tree() {
    let (_m, t, sid) = setup().await;
    let cats = t.tt_op(&sid, json!({"op": "getCategories"})).await;
    let tech = cats.as_array().unwrap().iter().find(|c| c["title"] == "Tech").unwrap();
    assert_eq!(tech["unread"], 4);
    assert!(cats.as_array().unwrap().iter().any(|c| c["id"] == -1));
    let feeds = t.tt_op(&sid, json!({"op": "getFeeds", "cat_id": "-3"})).await;
    let f = &feeds[0];
    assert_eq!(f["id"], 1);
    assert_eq!(f["unread"], 4);
    assert!(f["has_icon"].is_boolean());
    assert!(f["cat_id"].is_i64());
    let specials = t.tt_op(&sid, json!({"op": "getFeeds", "cat_id": -4})).await;
    assert!(specials.as_array().unwrap().iter().any(|f| f["id"] == -4 && f["unread"] == 4));
    let counters = t.tt_op(&sid, json!({"op": "getCounters", "output_mode": "flc"})).await;
    let arr = counters.as_array().unwrap();
    assert_eq!(arr[0], json!({"id": "global-unread", "counter": 4}));
    assert!(arr.iter().any(|c| c["id"] == 1 && c["counter"] == 4 && c.get("kind").is_none()));
    assert!(arr.iter().any(|c| c["kind"] == "cat" && c["counter"] == 4));
    let tree = t.tt_op(&sid, json!({"op": "getFeedTree"})).await;
    let items = tree["categories"]["items"].as_array().unwrap();
    assert_eq!(items[0]["id"], "CAT:-1");
    let techcat = items.iter().find(|c| c["name"] == "Tech").unwrap();
    assert!(techcat["bare_id"].is_i64());
    assert_eq!(techcat["items"][0]["bare_id"], 1);
    assert_eq!(techcat["items"][0]["unread"], -1);
    assert_eq!(t.tt_op(&sid, json!({"op": "getUnread"})).await["unread"], 4);
    assert_eq!(t.tt_op(&sid, json!({"op": "getUnread", "feed_id": 1})).await["unread"], 4);
    assert_eq!(t.tt_op(&sid, json!({"op": "getPref", "pref_name": "FRESH_ARTICLE_MAX_AGE"})).await["value"], 24);
    let cfg = t.tt_op(&sid, json!({"op": "getConfig"})).await;
    assert_eq!(cfg["icons_url"], "feed-icons");
    assert_eq!(cfg["num_feeds"], 1);
}

#[tokio::test]
async fn headlines_paging_and_updates() {
    let (_m, t, sid) = setup().await;
    let h = t
        .tt_op(&sid, json!({"op": "getHeadlines", "feed_id": 1, "limit": 2, "skip": 0, "show_content": true, "show_excerpt": true, "excerpt_length": 20, "include_attachments": true, "include_header": true}))
        .await;
    let header = &h[0];
    assert_eq!(header["id"], 1);
    assert!(header["is_cat"] == false && header["first_id"].is_i64());
    let list = h[1].as_array().unwrap();
    assert_eq!(list.len(), 2);
    let first = &list[0];
    let keys: Vec<&String> = first.as_object().unwrap().keys().collect();
    assert_eq!(&keys[..8], &["id", "guid", "unread", "marked", "published", "updated", "is_updated", "title"]);
    assert_eq!(first["tags"], json!([""]));
    assert_eq!(first["labels"], json!([]));
    assert!(first["excerpt"].as_str().unwrap().ends_with("&hellip;"));
    assert!(first["content"].as_str().unwrap().contains("Body"));
    assert!(first["unread"] == true && first["is_updated"] == false);
    assert!(first["note"].is_null());
    // since_id
    let max = first["id"].as_i64().unwrap();
    let newer = t.tt_op(&sid, json!({"op": "getHeadlines", "feed_id": -4, "since_id": max})).await;
    assert_eq!(newer.as_array().unwrap().len(), 0);
    let older = t.tt_op(&sid, json!({"op": "getHeadlines", "feed_id": -4, "since_id": max - 2})).await;
    assert_eq!(older.as_array().unwrap().len(), 2);
    // order_by date_reverse + skip
    let rev = t.tt_op(&sid, json!({"op": "getHeadlines", "feed_id": 1, "order_by": "date_reverse", "limit": 1, "skip": 1})).await;
    assert_eq!(rev.as_array().unwrap().len(), 1);
    // category view (is_cat) and feed 0 (archived) -> []
    let cat = t.tt_op(&sid, json!({"op": "getHeadlines", "feed_id": 1, "is_cat": true})).await;
    assert_eq!(cat.as_array().unwrap().len(), 4);
    assert_eq!(t.tt_op(&sid, json!({"op": "getHeadlines", "feed_id": 0})).await, json!([]));
    // missing feed_id -> INCORRECT_USAGE
    let bad = t.tt(json!({"op": "getHeadlines", "sid": sid})).await;
    assert_eq!(bad["content"]["error"], "INCORRECT_USAGE");

    // updateArticle: read two, star one (toggle), note
    let ids: Vec<i64> = list.iter().map(|x| x["id"].as_i64().unwrap()).collect();
    let r = t.tt_op(&sid, json!({"op": "updateArticle", "article_ids": format!("{},{}", ids[0], ids[1]), "mode": 0, "field": 2})).await;
    assert_eq!(r, json!({"status": "OK", "updated": 2}));
    t.tt_op(&sid, json!({"op": "updateArticle", "article_ids": [ids[0]], "mode": 2, "field": 0})).await;
    t.tt_op(&sid, json!({"op": "updateArticle", "article_ids": ids[0].to_string(), "mode": 1, "field": 3, "data": "remember"})).await;
    let art = t.tt_op(&sid, json!({"op": "getArticle", "article_id": ids[0].to_string()})).await;
    assert_eq!(art[0]["unread"], false);
    assert_eq!(art[0]["marked"], true);
    assert_eq!(art[0]["note"], "remember");
    assert!(art[0]["content"].is_string() && art[0].get("comments").is_some());
    assert_eq!(t.tt_op(&sid, json!({"op": "getArticle", "article_id": ""})).await, json!([]));
    // view modes and News+ compact headlines
    let unread = t.tt_op(&sid, json!({"op": "getCompactHeadlines", "feed_id": -4, "view_mode": "unread"})).await;
    assert_eq!(unread.as_array().unwrap().len(), 2);
    assert!(unread[0].as_object().unwrap().len() == 1 && unread[0]["id"].is_i64());
    let marked = t.tt_op(&sid, json!({"op": "getHeadlines", "feed_id": -4, "view_mode": "marked"})).await;
    assert_eq!(marked.as_array().unwrap().len(), 1);
    let starred_feed = t.tt_op(&sid, json!({"op": "getHeadlines", "feed_id": -1})).await;
    assert_eq!(starred_feed.as_array().unwrap().len(), 1);
    let adaptive = t.tt_op(&sid, json!({"op": "getHeadlines", "feed_id": 1, "view_mode": "adaptive"})).await;
    assert_eq!(adaptive.as_array().unwrap().len(), 2);
    let unread_first = t.tt_op(&sid, json!({"op": "getCompactHeadlines", "feed_id": -4, "view_mode": "unread_first"})).await;
    assert_eq!(unread_first.as_array().unwrap().len(), 4);
    // labels
    t.app.db.call(|c| rillrss::store::ensure_label(c, "Later")).await.unwrap();
    let labels = t.tt_op(&sid, json!({"op": "getLabels", "article_id": ids[0]})).await;
    assert_eq!(labels[0]["id"], -1026);
    assert_eq!(labels[0]["checked"], false);
    t.tt_op(&sid, json!({"op": "setArticleLabel", "article_ids": ids[0].to_string(), "label_id": -1026, "assign": true})).await;
    let in_label = t.tt_op(&sid, json!({"op": "getHeadlines", "feed_id": -1026})).await;
    assert_eq!(in_label[0]["labels"][0], json!([-1026, "Later", "", ""]));
    // catchup
    t.tt_op(&sid, json!({"op": "catchupFeed", "feed_id": 1, "is_cat": false})).await;
    assert_eq!(t.tt_op(&sid, json!({"op": "getUnread"})).await["unread"], 0);
}

#[tokio::test]
async fn subscribe_and_unsubscribe() {
    let (mock, t, sid) = setup().await;
    mock.set("/g.xml", "application/rss+xml", rss("Two", &[entry("x", "X", "https://n.example.com/x", "<p>x</p>")]));
    let r = t.tt_op(&sid, json!({"op": "subscribeToFeed", "feed_url": mock.url("/g.xml"), "category_id": 1})).await;
    assert_eq!(r["status"]["code"], 1);
    let id = r["status"]["feed_id"].as_i64().unwrap();
    let again = t.tt_op(&sid, json!({"op": "subscribeToFeed", "feed_url": mock.url("/g.xml")})).await;
    assert_eq!(again["status"]["code"], 0);
    assert_eq!(t.tt_op(&sid, json!({"op": "unsubscribeFeed", "feed_id": id})).await["status"], "OK");
    let gone = t.tt(json!({"op": "unsubscribeFeed", "feed_id": id, "sid": sid})).await;
    assert_eq!(gone["content"]["error"], "E_OPERATION_FAILED");
    let invalid = t.tt_op(&sid, json!({"op": "subscribeToFeed", "feed_url": "not a url"})).await;
    assert_eq!(invalid["status"]["code"], 2);
}

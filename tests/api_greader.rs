//! Google Reader API conformance (formats clients depend on).

mod common;

use common::*;
use serde_json::{Value, json};

async fn setup() -> (Mock, TestApp, String) {
    let mock = start_mock().await;
    let t = start_app(|_| {}).await;
    let entries: Vec<Entry> = (0..5).map(|i| entry(&format!("g{i}"), &format!("Item {i}"), &format!("https://n.example.com/{i}"), &paragraphs(&format!("Body{i}"), 1))).collect();
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
    let auth = t.greader_login().await;
    (mock, t, auth)
}

#[tokio::test]
async fn login_and_auth_errors() {
    let (_m, t, auth) = setup().await;
    assert!(!auth.contains('=') && !auth.contains(' '));
    // Wrong password -> 401 with both bad-token headers.
    let r = t.client.post(t.url("/accounts/ClientLogin")).form(&[("Email", "admin"), ("Passwd", "nope")]).send().await.unwrap();
    assert_eq!(r.status(), 401);
    // Missing auth on API -> 401 + headers.
    let r = t.client.get(t.url("/reader/api/0/subscription/list?output=json")).send().await.unwrap();
    assert_eq!(r.status(), 401);
    assert_eq!(r.headers()["X-Reader-Google-Bad-Token"], "true");
    assert_eq!(r.headers()["Google-Bad-Token"], "true");
    // Same token works on every root, and is stable across logins.
    assert_eq!(t.greader_login().await, auth);
    for root in ["/api/greader.php", "", "/plugins.local/freshapi/api/greader.php", "/tt-rss/plugins.local/freshapi/api/greader.php"] {
        let r = t.client.get(t.url(&format!("{root}/reader/api/0/user-info"))).header("Authorization", format!("GoogleLogin auth={auth}")).send().await.unwrap();
        assert_eq!(r.status(), 200, "{root}");
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["userName"], "admin");
    }
    // Bare root answers OK; token is 57 chars + newline.
    assert_eq!(t.client.get(t.url("/api/greader.php")).send().await.unwrap().text().await.unwrap(), "OK");
    let tok = t.client.get(t.url("/api/greader.php/reader/api/0/token")).header("Authorization", format!("GoogleLogin auth={auth}")).send().await.unwrap().text().await.unwrap();
    assert_eq!(tok.len(), 58);
    assert!(tok.ends_with('\n'));
    // Miniflux style: POST authenticated only by T = auth token.
    let r = t.client.post(t.url("/reader/api/0/edit-tag")).form(&[("T", auth.as_str()), ("i", "1"), ("a", "user/-/state/com.google/read")]).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "OK");
}

#[tokio::test]
async fn subscription_and_tag_lists() {
    let (_m, t, auth) = setup().await;
    let subs = t.gr_get(&auth, "subscription/list?output=json").await;
    let s = &subs["subscriptions"][0];
    assert_eq!(s["id"], "feed/1");
    assert_eq!(s["title"], "Feed One");
    assert_eq!(s["categories"][0]["id"], "user/-/label/Tech");
    assert_eq!(s["categories"][0]["label"], "Tech");
    assert!(s["htmlUrl"].is_string() && s["iconUrl"].is_string() && s["url"].is_string());
    let tags = t.gr_get(&auth, "tag/list?output=json").await;
    let ids: Vec<&str> = tags["tags"].as_array().unwrap().iter().map(|x| x["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"user/-/state/com.google/starred"));
    assert!(ids.contains(&"user/-/label/Tech"));
    for tag in tags["tags"].as_array().unwrap() {
        if let Some(ty) = tag.get("type") {
            assert!(ty == "folder" || ty == "tag");
        }
    }
    let uc = t.gr_get(&auth, "unread-count?output=json").await;
    assert_eq!(uc["max"], 5);
    let rl = uc["unreadcounts"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(rl["id"], "user/-/state/com.google/reading-list");
    assert_eq!(rl["count"], 5);
    assert!(rl["newestItemTimestampUsec"].is_string());
}

#[tokio::test]
async fn item_ids_contents_and_pagination() {
    let (_m, t, auth) = setup().await;
    // Pagination with continuation (strings; omitted at the end).
    let p1 = t.gr_get(&auth, "stream/items/ids?output=json&s=user/-/state/com.google/reading-list&n=3").await;
    assert_eq!(p1["itemRefs"].as_array().unwrap().len(), 3);
    let c = p1["continuation"].as_str().unwrap();
    for r in p1["itemRefs"].as_array().unwrap() {
        let id = r["id"].as_str().unwrap();
        assert_eq!(id.len(), 16, "16-digit decimal ids");
        assert_eq!(r.as_object().unwrap().len(), 1, "itemRefs carry only id");
    }
    let p2 = t.gr_get(&auth, &format!("stream/items/ids?output=json&s=user/-/state/com.google/reading-list&n=3&c={c}")).await;
    assert_eq!(p2["itemRefs"].as_array().unwrap().len(), 2);
    assert!(p2.get("continuation").is_none());
    // Empty result still has itemRefs.
    let empty = t.gr_get(&auth, "stream/items/ids?output=json&s=user/-/state/com.google/starred").await;
    assert_eq!(empty["itemRefs"], json!([]));
    // Contents by any id form; long ids are 16 lowercase hex digits.
    let short = p1["itemRefs"][0]["id"].as_str().unwrap().to_string();
    let n: i64 = short.parse().unwrap();
    let long = format!("tag:google.com,2005:reader/item/{n:016x}");
    let bare = format!("{n:016x}");
    let r = t
        .client
        .post(t.url("/reader/api/0/stream/items/contents"))
        .header("Authorization", format!("GoogleLogin auth={auth}"))
        .form(&[("i", short.as_str()), ("i", long.as_str()), ("i", bare.as_str()), ("i", "garbage")])
        .send()
        .await
        .unwrap();
    let v: Value = r.json().await.unwrap();
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "deduplicated");
    let it = &items[0];
    assert_eq!(it["id"], long);
    for k in ["crawlTimeMsec", "timestampUsec"] {
        assert!(it[k].is_string(), "{k} must be a string");
    }
    assert!(it["published"].is_i64());
    assert!(it["summary"]["content"].as_str().unwrap().contains("Body"));
    assert!(it["origin"]["streamId"] == "feed/1" && it["origin"]["title"] == "Feed One");
    assert!(it["categories"].as_array().unwrap().iter().any(|c| c == "user/-/label/Tech"));
    // stream/contents with escaped stream id in path, and with ?s=
    let a = t.gr_get(&auth, "stream/contents/feed%2F1?output=json&n=2").await;
    let b = t.gr_get(&auth, "stream/contents?s=feed/1&n=2").await;
    let c2 = t.gr_get(&auth, "stream/contents/user/-/label/Tech?n=2").await;
    assert_eq!(a["items"].as_array().unwrap().len(), 2);
    assert_eq!(a["items"], b["items"]);
    assert_eq!(a["items"], c2["items"]);
    assert!(a["continuation"].is_string());
    // r=o: oldest (smallest id / earliest insertion) first
    let all = t.gr_get(&auth, "stream/items/ids?output=json&s=user/-/state/com.google/reading-list&n=100").await;
    let min_id: i64 = all["itemRefs"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap().parse::<i64>().unwrap()).min().unwrap();
    let o = t.gr_get(&auth, "stream/contents/user/-/state/com.google/reading-list?r=o&n=1").await;
    assert_eq!(o["items"][0]["id"], format!("tag:google.com,2005:reader/item/{min_id:016x}"));
}

#[tokio::test]
async fn edit_tag_filters_and_mark_all_read() {
    let (_m, t, auth) = setup().await;
    let ids = t.gr_get(&auth, "stream/items/ids?output=json&s=user/-/state/com.google/reading-list&n=10").await;
    let all: Vec<String> = ids["itemRefs"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap().to_string()).collect();
    // mark two read (decimal + long form), star one, label one
    let long1 = format!("tag:google.com,2005:reader/item/{:016x}", all[1].parse::<i64>().unwrap());
    assert_eq!(t.gr_post(&auth, "edit-tag", &[("i", &all[0]), ("i", &long1), ("a", "user/-/state/com.google/read"), ("T", "x\n")]).await.0, 200);
    t.gr_post(&auth, "edit-tag", &[("i", &all[2]), ("a", "user/-/state/com.google/starred")]).await;
    t.gr_post(&auth, "edit-tag", &[("i", &all[2]), ("a", "user/-/label/Later")]).await;
    let unread = t.gr_get(&auth, "stream/items/ids?output=json&s=user/-/state/com.google/reading-list&xt=user/-/state/com.google/read").await;
    assert_eq!(unread["itemRefs"].as_array().unwrap().len(), 3);
    let read = t.gr_get(&auth, "stream/items/ids?output=json&s=user/-/state/com.google/read").await;
    assert_eq!(read["itemRefs"].as_array().unwrap().len(), 2);
    let starred_unread = t.gr_get(&auth, "stream/items/ids?output=json&s=user/-/state/com.google/reading-list&it=user/-/state/com.google/starred&xt=user/-/state/com.google/read").await;
    assert_eq!(starred_unread["itemRefs"].as_array().unwrap().len(), 1);
    let label = t.gr_get(&auth, "stream/items/ids?output=json&s=user/-/label/Later").await;
    assert_eq!(label["itemRefs"][0]["id"], all[2].as_str());
    let tags = t.gr_get(&auth, "tag/list").await;
    assert!(tags["tags"].as_array().unwrap().iter().any(|x| x["id"] == "user/-/label/Later" && x["type"] == "tag"));
    // unknown stream -> empty, not everything
    let unk = t.gr_get(&auth, "stream/items/ids?output=json&s=user/-/state/com.google/like").await;
    assert_eq!(unk["itemRefs"], json!([]));
    // kept-unread marks unread again
    t.gr_post(&auth, "edit-tag", &[("i", &all[0]), ("r", "user/-/state/com.google/read"), ("a", "user/-/state/com.google/kept-unread")]).await;
    let unread = t.gr_get(&auth, "stream/items/ids?output=json&s=user/-/state/com.google/reading-list&xt=user/-/state/com.google/read").await;
    assert_eq!(unread["itemRefs"].as_array().unwrap().len(), 4);
    // mark-all-as-read with ts in the past affects nothing; without ts everything.
    assert_eq!(t.gr_post(&auth, "mark-all-as-read", &[("s", "feed/1"), ("ts", "1000000000000000")]).await, (200, "OK".into()));
    let unread = t.gr_get(&auth, "unread-count?output=json").await;
    assert_eq!(unread["max"], 4);
    assert_eq!(t.gr_post(&auth, "mark-all-as-read", &[("s", "user/-/label/Tech")]).await, (200, "OK".into()));
    let unread = t.gr_get(&auth, "unread-count?output=json").await;
    assert_eq!(unread["max"], 0);
}

#[tokio::test]
async fn subscription_management() {
    let (mock, t, auth) = setup().await;
    // rename + move (NetNewsWire sends r=old&a=new)
    assert_eq!(
        t.gr_post(&auth, "subscription/edit", &[("ac", "edit"), ("s", "feed/1"), ("r", "user/-/label/Tech"), ("a", "user/-/label/World"), ("t", "Renamed")]).await,
        (200, "OK".into())
    );
    let subs = t.gr_get(&auth, "subscription/list").await;
    assert_eq!(subs["subscriptions"][0]["title"], "Renamed");
    assert_eq!(subs["subscriptions"][0]["categories"][0]["label"], "World");
    // rename-tag / disable-tag
    assert_eq!(t.gr_post(&auth, "rename-tag", &[("s", "user/-/label/World"), ("dest", "user/-/label/Planet")]).await, (200, "OK".into()));
    let subs = t.gr_get(&auth, "subscription/list").await;
    assert_eq!(subs["subscriptions"][0]["categories"][0]["label"], "Planet");
    assert_eq!(t.gr_post(&auth, "disable-tag", &[("s", "user/-/label/Planet")]).await, (200, "OK".into()));
    let subs = t.gr_get(&auth, "subscription/list").await;
    assert_eq!(subs["subscriptions"][0]["categories"][0]["label"], "Uncategorized");
    // quickadd (new + already subscribed)
    mock.set("/g.xml", "application/rss+xml", rss("Feed Two", &[entry("x", "X", "https://n.example.com/x", "<p>x</p>")]));
    let (_, body) = t.gr_post(&auth, "subscription/quickadd", &[("quickadd", &mock.url("/g.xml"))]).await;
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["numResults"], 1);
    assert_eq!(v["streamId"], "feed/2");
    let (_, body) = t.gr_post(&auth, "subscription/quickadd", &[("quickadd", &mock.url("/g.xml"))]).await;
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(v["error"].as_str().unwrap().starts_with("Already subscribed! "));
    // OPML export and unsubscribe
    let opml = t.client.get(t.url("/reader/api/0/subscription/export")).header("Authorization", format!("GoogleLogin auth={auth}")).send().await.unwrap().text().await.unwrap();
    assert!(opml.contains("Feed Two") && opml.contains("<opml"));
    assert_eq!(t.gr_post(&auth, "subscription/edit", &[("ac", "unsubscribe"), ("s", "feed/2")]).await, (200, "OK".into()));
    let subs = t.gr_get(&auth, "subscription/list").await;
    assert_eq!(subs["subscriptions"].as_array().unwrap().len(), 1);
    // import raw OPML body
    let r = t
        .client
        .post(t.url("/reader/api/0/subscription/import"))
        .header("Authorization", format!("GoogleLogin auth={auth}"))
        .header("Content-Type", "text/xml")
        .body(opml)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let subs = t.gr_get(&auth, "subscription/list").await;
    assert_eq!(subs["subscriptions"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn cached_image_urls_use_request_host() {
    let mock = start_mock().await;
    let t = start_app(|_| {}).await;
    mock.set("/pic.png", "image/png", png(40, 30));
    let html = format!("<p>Look at this picture of the scene.</p><p><img src=\"{}\"></p>", mock.url("/pic.png"));
    mock.set("/f.xml", "application/rss+xml", rss("Pics", &[entry("p1", "Pic", "https://n.example.com/p1", &html)]));
    let fid = t.add_feed(&mock.url("/f.xml"), json!({"cache_images": true})).await;
    t.fetch(fid).await;
    t.wait_idle().await;
    let auth = t.greader_login().await;
    let r = t
        .client
        .get(t.url("/reader/api/0/stream/contents/feed/1"))
        .header("Authorization", format!("GoogleLogin auth={auth}"))
        .header("X-Forwarded-Proto", "https")
        .header("X-Forwarded-Host", "rss.example.org")
        .send()
        .await
        .unwrap();
    let v: Value = r.json().await.unwrap();
    let content = v["items"][0]["summary"]["content"].as_str().unwrap();
    assert!(content.contains("src=\"https://rss.example.org/img/"), "{content}");
    let path = content.split("https://rss.example.org").nth(1).unwrap().split('"').next().unwrap();
    let img = t.client.get(t.url(path)).send().await.unwrap();
    assert_eq!(img.status(), 200);
    assert_eq!(img.headers()["content-type"], "image/png");
}

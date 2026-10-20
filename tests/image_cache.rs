//! Image cache size cap: the images of the oldest articles make room for new ones,
//! and those articles point at the original images again.

mod common;

use common::*;
use serde_json::json;

/// Publish article `n` with one `w`×400 image and wait until it is processed.
async fn publish(t: &TestApp, mock: &Mock, fid: i64, n: u32, w: u32, image: &str) {
    if mock.hits(image) == 0 {
        mock.set(image, "image/png", png(w, 400));
    }
    let html = format!("<p>Article {n} with a photo and a few words of text.</p><img src=\"{}\">", mock.url(image));
    mock.set("/f.xml", "application/rss+xml", rss("Pics", &[entry(&format!("g{n}"), &format!("Article {n}"), &format!("https://n.example.com/{n}"), &html)]));
    t.fetch(fid).await;
    t.wait_idle().await;
}

async fn content(t: &TestApp, n: u32) -> String {
    let title = format!("Article {n}");
    t.app.db.call(move |c| Ok(c.query_row("SELECT content FROM items WHERE title = ?", [title], |r| r.get(0))?)).await.unwrap()
}

fn cached(html: &str) -> bool {
    html.contains("rill-base://local/img/")
}

#[tokio::test]
async fn oldest_articles_lose_cached_images_first() {
    let mock = start_mock().await;
    let t = start_app(|s| s.images.max_total_mb = 0).await;
    let fid = t.add_feed(&mock.url("/f.xml"), json!({"cache_images": true})).await;
    for (n, w) in [(1, 300), (2, 310), (3, 320)] {
        publish(&t, &mock, fid, n, w, &format!("/img{n}.png")).await;
    }
    assert_eq!(t.count("SELECT COUNT(*) FROM images").await, 3);
    let size = |n: u32| png(300 + 10 * (n - 1), 400).len() as i64;
    let hash2 = rillrss::pipeline::image_hashes(&content(&t, 2).await).remove(0);
    t.app.db.call(|c| Ok(c.execute("UPDATE items SET is_starred = 1 WHERE title = 'Article 1'", [])?)).await.unwrap();

    // Just over the cap: one image goes, the oldest one that is not starred.
    let (n, freed) = t.app.images.evict(t.app.images.total_bytes() - 1).await.unwrap();
    assert_eq!((n, freed), (1, size(2)));
    let c2 = content(&t, 2).await;
    assert!(!cached(&c2) && c2.contains(&mock.url("/img2.png")), "{c2}");
    assert!(cached(&content(&t, 1).await) && cached(&content(&t, 3).await));
    assert_eq!(t.count("SELECT COUNT(*) FROM images").await, 2);
    assert_eq!(t.count("SELECT COUNT(*) FROM item_images").await, 2);
    assert!(!t.app.images.file_path(&hash2, "png").exists());

    // Copies that clients already have are redirected to the original image.
    let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let r = client.get(t.url(&format!("/img/{}/{hash2}.png", &hash2[..2]))).send().await.unwrap();
    assert_eq!(r.status(), 302);
    assert_eq!(r.headers()["location"], mock.url("/img2.png").as_str());

    // Starred articles keep theirs longest: the newer article 3 goes before article 1.
    let (n, _) = t.app.images.evict(size(1) + size(3) - 1).await.unwrap();
    assert_eq!(n, 1);
    assert!(!cached(&content(&t, 3).await));
    assert!(cached(&content(&t, 1).await));
}

#[tokio::test]
async fn new_images_are_cached_past_the_cap() {
    let mock = start_mock().await;
    let t = start_app(|s| s.images.max_total_mb = 1).await;
    let fid = t.add_feed(&mock.url("/f.xml"), json!({"cache_images": true})).await;
    // About 480 KB, 492 KB and 504 KB: the third one passes 1 MB.
    for (n, w) in [(1, 400), (2, 410), (3, 420)] {
        publish(&t, &mock, fid, n, w, &format!("/img{n}.png")).await;
    }
    // The cap is enforced in the background right after caching.
    for _ in 0..100 {
        if t.count("SELECT COUNT(*) FROM images").await == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(cached(&content(&t, 3).await), "the new image is cached");
    for n in [1, 2] {
        let c = content(&t, n).await;
        assert!(!cached(&c) && c.contains(&mock.url(&format!("/img{n}.png"))), "{c}");
    }
    assert!(t.app.images.total_bytes() <= 1024 * 1024 * 9 / 10);

    // A later article with a dropped picture caches it again.
    publish(&t, &mock, fid, 4, 400, "/img1.png").await;
    assert!(cached(&content(&t, 4).await));
    assert_eq!(t.count("SELECT COUNT(*) FROM images").await, 2);
}

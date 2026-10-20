//! Image cache behaviour: dedupe, limits, tracking pixels, GC.

mod common;

use common::*;
use serde_json::json;

#[tokio::test]
async fn caches_dedupes_and_filters_images() {
    let mock = start_mock().await;
    let t = start_app(|s| s.images.max_mb = 1).await;
    mock.set("/a.png", "image/png", png(64, 48));
    mock.set("/a-copy.png", "image/png", png(64, 48)); // same bytes, other URL
    mock.set("/pixel.png", "image/png", png(1, 1));
    mock.set("/huge.png", "image/png", png(800, 600)); // ~1.4 MB uncompressed > 1 MB
    mock.set("/vector.svg", "image/svg+xml", "<svg xmlns='http://www.w3.org/2000/svg'><script>alert(1)</script></svg>");
    mock.set("/fake.png", "image/png", "<html>not an image</html>");
    let html1 = format!(
        "<p>Photo story with several pictures and some text.</p><img src=\"{}\"><img src=\"{}\"><img src=\"{}\"><img src=\"{}\"><img src=\"{}\">",
        mock.url("/a.png"),
        mock.url("/pixel.png"),
        mock.url("/huge.png"),
        mock.url("/vector.svg"),
        mock.url("/fake.png")
    );
    let html2 = format!("<p>Another article reusing the same picture under a different URL.</p><img src=\"{}\">", mock.url("/a-copy.png"));
    mock.set(
        "/f.xml",
        "application/rss+xml",
        rss("Pics", &[entry("1", "One", "https://n.example.com/1", &html1), entry("2", "Two", "https://n.example.com/2", &html2)]),
    );
    let fid = t.add_feed(&mock.url("/f.xml"), json!({"cache_images": true})).await;
    t.fetch(fid).await;
    t.wait_idle().await;

    assert_eq!(t.count("SELECT COUNT(*) FROM images").await, 2, "a.png (deduped with a-copy) + pixel");
    let c1: String = t.query("SELECT content FROM items WHERE title = 'One'", |r| r.get(0)).await;
    assert!(c1.contains("rill-base://local/img/"), "{c1}");
    assert!(!c1.contains("pixel.png"), "tracking pixel removed: {c1}");
    assert!(c1.contains("/huge.png"), "oversized image keeps its original URL");
    assert!(c1.contains("/vector.svg"), "SVG is never cached");
    assert!(c1.contains("/fake.png"), "non-images are not cached");
    let c2: String = t.query("SELECT content FROM items WHERE title = 'Two'", |r| r.get(0)).await;
    let h1 = rillrss::pipeline::image_hashes(&c1);
    let h2 = rillrss::pipeline::image_hashes(&c2);
    assert_eq!(h1, h2, "identical bytes stored once");
    assert_eq!(t.count("SELECT COUNT(*) FROM item_images").await, 2);
    let notes: String = t.query("SELECT notes FROM revisions WHERE title = 'One'", |r| r.get(0)).await;
    assert!(notes.contains("original URL"), "{notes}");

    // Already-downloaded URLs are not fetched again for later articles.
    let hits_before = mock.hits("/a.png");
    mock.set(
        "/f.xml",
        "application/rss+xml",
        rss("Pics", &[entry("3", "Three", "https://n.example.com/3", &format!("<p>Third article with the first picture again.</p><img src=\"{}\">", mock.url("/a.png")))]),
    );
    t.fetch(fid).await;
    t.wait_idle().await;
    assert_eq!(mock.hits("/a.png"), hits_before);

    // GC: once no item references an image it is deleted (after the grace period).
    t.app.db.call(|c| Ok(c.execute("DELETE FROM items", [])?)).await.unwrap();
    let (n, freed) = t.app.images.gc(-10).await.unwrap();
    assert_eq!(n, 2);
    assert!(freed > 0);
    assert_eq!(t.count("SELECT COUNT(*) FROM images").await, 0);
    let dir = t.app.images.dir.clone();
    assert_eq!(rillrss::content::images::ImageStore::disk_usage(&dir), 0);
}

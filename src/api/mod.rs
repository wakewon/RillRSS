//! Reader-facing APIs and shared HTTP helpers.

pub mod greader;
pub mod ttrss;

use std::net::{IpAddr, SocketAddr};

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use bytes::BytesMut;
use futures::StreamExt;

use crate::app::App;

/// Public base URL for absolute links in API output (cached images, icons).
pub async fn request_base(app: &App, headers: &HeaderMap) -> String {
    if let Some(b) = app.configured_base_url().await {
        return b;
    }
    let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(|s| s.split(',').next().unwrap_or("").trim().to_string());
    let (proto, host) = if app.cfg.trust_proxy {
        (
            get("x-forwarded-proto").filter(|s| !s.is_empty()).unwrap_or_else(|| "http".into()),
            get("x-forwarded-host").filter(|s| !s.is_empty()).or_else(|| get("host")).unwrap_or_else(|| "localhost".into()),
        )
    } else {
        ("http".into(), get("host").unwrap_or_else(|| "localhost".into()))
    };
    let prefix = if app.cfg.trust_proxy { get("x-forwarded-prefix").unwrap_or_default() } else { String::new() };
    format!("{proto}://{host}{}", prefix.trim_end_matches('/'))
}

/// Client IP for login throttling and logs. Forwarding headers count only when
/// trusting a proxy and the connection comes from a private address (the proxy).
/// X-Forwarded-For is read from the right: a proxy appends the address it saw, and
/// anything to its left was sent by the client and can be forged. The rightmost
/// public entry is the client; with only private entries (LAN) the leftmost is.
pub fn client_ip(app: &App, headers: &HeaderMap, peer: Option<SocketAddr>) -> Option<IpAddr> {
    forwarded_client_ip(app.cfg.trust_proxy, headers, peer)
}

fn forwarded_client_ip(trust_proxy: bool, headers: &HeaderMap, peer: Option<SocketAddr>) -> Option<IpAddr> {
    let peer_ip = peer.map(|p| p.ip());
    if !trust_proxy || peer_ip.is_some_and(crate::http::is_public_ip) {
        return peer_ip;
    }
    let forwarded: Vec<IpAddr> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(','))
        .filter_map(|s| s.trim().parse::<IpAddr>().ok())
        .collect();
    if let Some(ip) = forwarded.iter().rev().find(|ip| crate::http::is_public_ip(**ip)).or(forwarded.first()) {
        return Some(*ip);
    }
    headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<IpAddr>().ok())
        .or(peer_ip)
}

/// Multimap of request parameters (query string + form body), preserving order.
#[derive(Debug, Default, Clone)]
pub struct Params(pub Vec<(String, String)>);

impl Params {
    pub fn parse(query: Option<&str>, body: Option<&[u8]>) -> Params {
        let mut v: Vec<(String, String)> = Vec::new();
        if let Some(q) = query {
            v.extend(form_urlencoded_pairs(q.as_bytes()));
        }
        if let Some(b) = body {
            v.extend(form_urlencoded_pairs(b));
        }
        Params(v)
    }
    pub fn get(&self, k: &str) -> Option<&str> {
        self.0.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str())
    }
    /// Last value of a key (query values come first, so the body wins).
    pub fn last(&self, k: &str) -> Option<&str> {
        self.0.iter().rev().find(|(key, _)| key == k).map(|(_, v)| v.as_str())
    }
    pub fn all(&self, k: &str) -> Vec<&str> {
        self.0.iter().filter(|(key, _)| key == k).map(|(_, v)| v.as_str()).collect()
    }
}

/// Most parameters accepted from one source. Clients batch item ids well below this
/// (FreshRSS's PHP default allows 1000 inputs); it bounds the memory a request costs.
const MAX_PARAMS: usize = 20_000;

fn form_urlencoded_pairs(input: &[u8]) -> Vec<(String, String)> {
    url::form_urlencoded::parse(input).take(MAX_PARAMS).map(|(k, v)| (k.into_owned(), v.into_owned())).collect()
}

/// Read a request body of at most `limit` bytes (the API handlers choose the limit per
/// request instead of the router-wide one). Err: 413 when it is larger (refused up
/// front when Content-Length says so), 400 when it could not be read.
pub async fn read_body(body: Body, headers: &HeaderMap, limit: usize) -> Result<Bytes, StatusCode> {
    let declared = headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok());
    if declared.is_some_and(|n| n > limit as u64) {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }
    let mut stream = body.into_data_stream();
    let mut buf = BytesMut::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| StatusCode::BAD_REQUEST)?;
        if buf.len() + chunk.len() > limit {
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf.freeze())
}

/// Headers sent on every API response.
pub fn api_headers(h: &mut HeaderMap) {
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("Authorization, Content-Type"));
    h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET, POST, OPTIONS"));
    h.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
    h.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, HeaderValue::from_static("X-Reader-Google-Bad-Token"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
}

/// Cap HTML content size for clients that choke on huge articles (cut on a char boundary).
pub fn cap_content(s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(headers: &[(&'static str, &str)], peer: &str) -> Option<IpAddr> {
        let mut h = HeaderMap::new();
        for (k, v) in headers {
            h.append(*k, HeaderValue::from_str(v).unwrap());
        }
        forwarded_client_ip(true, &h, Some(format!("{peer}:1234").parse().unwrap()))
    }

    #[test]
    fn client_ip_reads_forwarded_for_from_the_right() {
        let some = |s: &str| Some(s.parse::<IpAddr>().unwrap());
        // nginx appends the real address; forged entries on the left are ignored.
        assert_eq!(ip(&[("x-forwarded-for", "9.9.9.9, 8.8.4.4")], "172.18.0.1"), some("8.8.4.4"));
        // Further private hops (another proxy on the LAN) are skipped.
        assert_eq!(ip(&[("x-forwarded-for", "1.1.1.1, 8.8.4.4, 10.0.0.2")], "127.0.0.1"), some("8.8.4.4"));
        // Only private entries: a LAN client, the leftmost is it.
        assert_eq!(ip(&[("x-forwarded-for", "192.168.1.20, 10.0.0.2")], "127.0.0.1"), some("192.168.1.20"));
        // Repeated headers count as one list.
        assert_eq!(ip(&[("x-forwarded-for", "9.9.9.9"), ("x-forwarded-for", "8.8.4.4")], "127.0.0.1"), some("8.8.4.4"));
        assert_eq!(ip(&[("x-real-ip", "8.8.4.4")], "127.0.0.1"), some("8.8.4.4"));
        assert_eq!(ip(&[("x-forwarded-for", "junk")], "127.0.0.1"), some("127.0.0.1"));
        // A public peer is not a proxy we run: its headers are not trusted.
        assert_eq!(ip(&[("x-forwarded-for", "8.8.4.4")], "1.1.1.1"), some("1.1.1.1"));
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", HeaderValue::from_static("8.8.4.4"));
        assert_eq!(forwarded_client_ip(false, &h, Some("127.0.0.1:1".parse().unwrap())), some("127.0.0.1"));
    }

    #[tokio::test]
    async fn read_body_limits() {
        let streamed = || Body::from_stream(futures::stream::iter((0..4).map(|_| Ok::<_, std::io::Error>(Bytes::from(vec![b'a'; 1000])))));
        let none = HeaderMap::new();
        assert_eq!(read_body(streamed(), &none, 4000).await.unwrap().len(), 4000);
        // Without Content-Length the limit applies while reading.
        assert_eq!(read_body(streamed(), &none, 3999).await, Err(StatusCode::PAYLOAD_TOO_LARGE));
        // A declared length over the limit is refused before reading.
        let mut declared = HeaderMap::new();
        declared.insert(header::CONTENT_LENGTH, HeaderValue::from_static("5000"));
        assert_eq!(read_body(streamed(), &declared, 4000).await, Err(StatusCode::PAYLOAD_TOO_LARGE));
    }
}

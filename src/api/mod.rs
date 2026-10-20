//! Reader-facing APIs and shared HTTP helpers.

pub mod greader;
pub mod ttrss;

use std::net::{IpAddr, SocketAddr};

use axum::http::{HeaderMap, HeaderValue, header};

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

/// Client IP (honouring X-Forwarded-For / X-Real-IP when trusting a proxy).
pub fn client_ip(app: &App, headers: &HeaderMap, peer: Option<SocketAddr>) -> Option<IpAddr> {
    if app.cfg.trust_proxy {
        for name in ["x-forwarded-for", "x-real-ip"] {
            if let Some(ip) = headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.split(',').next())
                .and_then(|s| s.trim().parse::<IpAddr>().ok())
            {
                return Some(ip);
            }
        }
    }
    peer.map(|p| p.ip())
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

fn form_urlencoded_pairs(input: &[u8]) -> Vec<(String, String)> {
    url::form_urlencoded::parse(input).map(|(k, v)| (k.into_owned(), v.into_owned())).collect()
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

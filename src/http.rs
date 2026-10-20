//! Outgoing HTTP: clients with SSRF protection, size-limited downloads,
//! per-host politeness and HTML charset decoding.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{HeaderMap, HeaderValue};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use url::Url;

/// Is this address publicly routable? (blocks loopback, private, link-local, CGNAT,
/// multicast, documentation and other special ranges)
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || o[0] == 0
        || (o[0] == 100 && (o[1] & 0xC0) == 64) // 100.64.0.0/10 CGNAT
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // 192.0.0.0/24
        || (o[0] == 198 && (o[1] & 0xFE) == 18) // 198.18.0.0/15 benchmarking
        || o[0] >= 240)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let s = ip.segments();
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return false;
    }
    if (s[0] & 0xfe00) == 0xfc00 {
        return false; // unique local fc00::/7
    }
    if (s[0] & 0xffc0) == 0xfe80 {
        return false; // link local fe80::/10
    }
    if s[0] == 0x2001 && s[1] == 0x0db8 {
        return false; // documentation
    }
    if s[0] == 0x64 && s[1] == 0xff9b {
        // NAT64: check the embedded IPv4 address
        let v4 = Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8);
        return is_public_v4(v4);
    }
    if s[0] == 0x2002 {
        // 6to4
        let v4 = Ipv4Addr::new((s[1] >> 8) as u8, s[1] as u8, (s[2] >> 8) as u8, s[2] as u8);
        return is_public_v4(v4);
    }
    true
}

/// DNS resolver that refuses names resolving only to non-public addresses.
struct PublicOnlyResolver;

impl Resolve for PublicOnlyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let host = name.as_str().to_string();
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|a| is_public_ip(a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(format!("blocked: {host} resolves to a non-public address").into());
            }
            let it: Addrs = Box::new(addrs.into_iter());
            Ok(it)
        })
    }
}

/// Check that a URL is http(s) and, if `public_only`, not an IP literal in a private range.
pub fn check_url(url: &Url, public_only: bool) -> Result<()> {
    if !matches!(url.scheme(), "http" | "https") {
        bail!("unsupported URL scheme: {}", url.scheme());
    }
    if public_only {
        match url.host() {
            Some(url::Host::Ipv4(ip)) if !is_public_v4(ip) => bail!("blocked private address {ip}"),
            Some(url::Host::Ipv6(ip)) if !is_public_v6(ip) => bail!("blocked private address {ip}"),
            Some(url::Host::Domain(d)) if d.eq_ignore_ascii_case("localhost") || d.ends_with(".localhost") => {
                bail!("blocked local host {d}")
            }
            None => bail!("URL has no host"),
            _ => {}
        }
    }
    Ok(())
}

#[derive(Clone)]
pub struct HttpClients {
    /// For feeds (admin-provided URLs; private networks allowed).
    pub feed: reqwest::Client,
    /// For article pages, images, icons (URLs coming from feed content).
    pub content: reqwest::Client,
    /// For the translation API / external extractor (admin-provided).
    pub api: reqwest::Client,
    pub public_only: bool,
    pub hosts: HostLimiter,
}

fn redirect_policy(public_only: bool) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.error("too many redirects");
        }
        if let Err(e) = check_url(attempt.url(), public_only) {
            return attempt.error(e.to_string());
        }
        attempt.follow()
    })
}

impl HttpClients {
    pub fn new(allow_private_networks: bool) -> Result<HttpClients> {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::ACCEPT_LANGUAGE,
            HeaderValue::from_static("en-US,en;q=0.9,zh-CN;q=0.8,zh;q=0.7"),
        );
        let base = || {
            reqwest::Client::builder()
                .default_headers(headers.clone())
                .connect_timeout(Duration::from_secs(15))
                .pool_idle_timeout(Duration::from_secs(60))
                .pool_max_idle_per_host(4)
                .tcp_keepalive(Duration::from_secs(60))
        };
        let feed = base().redirect(redirect_policy(false)).build()?;
        let public_only = !allow_private_networks;
        let mut content_b = base().redirect(redirect_policy(public_only));
        if public_only {
            content_b = content_b.dns_resolver(Arc::new(PublicOnlyResolver));
        }
        let content = content_b.build()?;
        let api = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()?;
        Ok(HttpClients { feed, content, api, public_only, hosts: HostLimiter::default() })
    }
}

/// Response body read with a size limit.
pub struct Fetched {
    pub status: u16,
    pub final_url: Url,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Fetched {
    pub fn content_type(&self) -> Option<String> {
        self.headers
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_ascii_lowercase())
    }
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers.get(name).and_then(|v| v.to_str().ok()).map(|s| s.to_string())
    }
}

/// Read the body of a response, failing when it exceeds `max_bytes`.
pub async fn read_limited(mut resp: reqwest::Response, max_bytes: usize) -> Result<Fetched> {
    let status = resp.status().as_u16();
    let final_url = resp.url().clone();
    let headers = resp.headers().clone();
    if let Some(len) = resp.content_length()
        && len as usize > max_bytes
    {
        bail!("response too large ({len} bytes > {max_bytes})");
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if body.len() + chunk.len() > max_bytes {
            bail!("response too large (> {max_bytes} bytes)");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Fetched { status, final_url, headers, body })
}

/// Simple GET with size limit and timeout, returning the body for any status.
pub async fn get(
    client: &reqwest::Client,
    url: &str,
    user_agent: &str,
    timeout: Duration,
    max_bytes: usize,
    public_only: bool,
    accept: &str,
) -> Result<Fetched> {
    let parsed = Url::parse(url).map_err(|e| anyhow!("invalid URL {url}: {e}"))?;
    check_url(&parsed, public_only)?;
    let resp = client
        .get(parsed)
        .header(reqwest::header::USER_AGENT, user_agent)
        .header(reqwest::header::ACCEPT, accept)
        .timeout(timeout)
        .send()
        .await?;
    read_limited(resp, max_bytes).await
}

/// Per-host concurrency + politeness delay.
#[derive(Clone, Default)]
pub struct HostLimiter {
    inner: Arc<Mutex<HashMap<String, HostSlot>>>,
}

struct HostSlot {
    sem: Arc<Semaphore>,
    next_allowed: Instant,
}

pub struct HostPermit {
    _permit: OwnedSemaphorePermit,
}

impl HostLimiter {
    /// Wait for a slot for `host` honoring `max_concurrent` and a minimum `delay`
    /// between request starts.
    pub async fn acquire(&self, host: &str, max_concurrent: u32, delay: Duration) -> HostPermit {
        let sem = {
            let mut map = self.inner.lock().unwrap();
            if map.len() > 4096 {
                map.retain(|_, s| Arc::strong_count(&s.sem) > 1);
            }
            map.entry(host.to_string())
                .or_insert_with(|| HostSlot {
                    sem: Arc::new(Semaphore::new(max_concurrent.max(1) as usize)),
                    next_allowed: Instant::now(),
                })
                .sem
                .clone()
        };
        let permit = sem.acquire_owned().await.expect("semaphore closed");
        loop {
            let wait = {
                let mut map = self.inner.lock().unwrap();
                let now = Instant::now();
                match map.get_mut(host) {
                    Some(slot) if slot.next_allowed > now => slot.next_allowed - now,
                    Some(slot) => {
                        slot.next_allowed = now + delay;
                        Duration::ZERO
                    }
                    None => Duration::ZERO,
                }
            };
            if wait.is_zero() {
                break;
            }
            tokio::time::sleep(wait).await;
        }
        HostPermit { _permit: permit }
    }
}

/// Decode an HTML document to a String using BOM, HTTP charset, <meta> charset and
/// finally statistical detection.
pub fn decode_html(body: &[u8], content_type: Option<&str>) -> String {
    use encoding_rs::Encoding;
    if let Some((enc, bom_len)) = Encoding::for_bom(body) {
        let (s, _) = enc.decode_without_bom_handling(&body[bom_len..]);
        return s.into_owned();
    }
    if let Some(ct) = content_type
        && let Some(cs) = charset_from_content_type(ct)
        && let Some(enc) = Encoding::for_label(cs.as_bytes())
    {
        let (s, _) = enc.decode_without_bom_handling(body);
        return s.into_owned();
    }
    let head = &body[..body.len().min(4096)];
    if let Some(cs) = charset_from_meta(head)
        && let Some(enc) = Encoding::for_label(cs.as_bytes())
    {
        // A page declaring utf-16 in <meta> is really ASCII-compatible.
        let enc = if enc == encoding_rs::UTF_16LE || enc == encoding_rs::UTF_16BE {
            encoding_rs::UTF_8
        } else {
            enc
        };
        let (s, _) = enc.decode_without_bom_handling(body);
        return s.into_owned();
    }
    if std::str::from_utf8(body).is_ok() {
        return String::from_utf8_lossy(body).into_owned();
    }
    let mut det = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Allow);
    det.feed(body, true);
    let enc = det.guess(None, chardetng::Utf8Detection::Allow);
    let (s, _) = enc.decode_without_bom_handling(body);
    s.into_owned()
}

fn charset_from_content_type(ct: &str) -> Option<String> {
    ct.split(';').skip(1).find_map(|p| {
        let p = p.trim();
        let (k, v) = p.split_once('=')?;
        if k.trim().eq_ignore_ascii_case("charset") {
            Some(v.trim().trim_matches('"').trim_matches('\'').to_string())
        } else {
            None
        }
    })
}

fn charset_from_meta(head: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(head).to_ascii_lowercase();
    let mut search = 0;
    while let Some(pos) = s[search..].find("<meta") {
        let start = search + pos;
        let end = s[start..].find('>').map(|e| start + e).unwrap_or(s.len());
        let tag = &s[start..end];
        if let Some(i) = tag.find("charset") {
            let rest = &tag[i + 7..];
            let rest = rest.trim_start().strip_prefix('=').unwrap_or("").trim_start();
            let rest = rest.trim_start_matches(['"', '\'']);
            let cs: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == ':' || *c == '.')
                .collect();
            if !cs.is_empty() {
                return Some(cs);
            }
        }
        search = end;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_ranges_blocked() {
        for ip in ["127.0.0.1", "10.1.2.3", "192.168.1.1", "172.16.0.1", "169.254.169.254", "100.64.0.1", "0.0.0.0", "::1", "fd00::1", "fe80::1", "::ffff:127.0.0.1", "64:ff9b::a00:1"] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip} should be blocked");
        }
        for ip in ["8.8.8.8", "151.101.1.1", "2606:4700::1111", "64:ff9b::808:808"] {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip} should be allowed");
        }
    }

    #[test]
    fn url_checks() {
        assert!(check_url(&Url::parse("http://127.0.0.1/x").unwrap(), true).is_err());
        assert!(check_url(&Url::parse("http://localhost:8080/").unwrap(), true).is_err());
        assert!(check_url(&Url::parse("file:///etc/passwd").unwrap(), false).is_err());
        assert!(check_url(&Url::parse("https://example.com/").unwrap(), true).is_ok());
        assert!(check_url(&Url::parse("http://127.0.0.1/x").unwrap(), false).is_ok());
    }

    #[test]
    fn decodes_meta_charset() {
        let (gbk, _, _) = encoding_rs::GBK.encode("<html><head><meta charset=\"gbk\"></head><body>中文内容</body></html>");
        let s = decode_html(&gbk, Some("text/html"));
        assert!(s.contains("中文内容"));
        let s2 = decode_html(&gbk, Some("text/html; charset=GBK"));
        assert!(s2.contains("中文内容"));
        let utf = "<p>héllo</p>".as_bytes();
        assert_eq!(decode_html(utf, None), "<p>héllo</p>");
    }

    #[tokio::test]
    async fn host_limiter_delays() {
        let l = HostLimiter::default();
        let start = Instant::now();
        {
            let _a = l.acquire("a.com", 2, Duration::from_millis(100)).await;
        }
        {
            let _b = l.acquire("a.com", 2, Duration::from_millis(100)).await;
        }
        assert!(start.elapsed() >= Duration::from_millis(95));
        let start = Instant::now();
        let _c = l.acquire("b.com", 2, Duration::from_millis(100)).await;
        assert!(start.elapsed() < Duration::from_millis(50));
    }
}

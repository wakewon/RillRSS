//! Outgoing HTTP: clients with SSRF protection, size-limited downloads,
//! per-host politeness and HTML charset decoding.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{HeaderMap, HeaderValue, IntoHeaderName};
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
        || o[0] >= 240)
    // Note: 198.18.0.0/15 (benchmarking) is deliberately allowed: "fake-IP" DNS of
    // common proxy tools (Clash, Surge, sing-box TUN mode) maps every domain into it.
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let s = ip.segments();
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return false;
    }
    let embedded_v4 = || Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8);
    if s[..6] == [0; 6] || (s[..4] == [0; 4] && s[4] == 0xffff && s[5] == 0) {
        // IPv4-compatible ::a.b.c.d (deprecated) and IPv4-translated ::ffff:0:a.b.c.d
        return is_public_v4(embedded_v4());
    }
    if (s[0] & 0xfe00) == 0xfc00 {
        return false; // unique local fc00::/7
    }
    if (s[0] & 0xffc0) == 0xfe80 {
        return false; // link local fe80::/10
    }
    if (s[0] & 0xffc0) == 0xfec0 {
        return false; // site local fec0::/10 (deprecated)
    }
    if s[0] == 0x2001 && s[1] == 0x0db8 {
        return false; // documentation
    }
    if s[0] == 0x64 && s[1] == 0xff9b {
        // NAT64: check the embedded IPv4 address
        return is_public_v4(embedded_v4());
    }
    if s[0] == 0x2002 {
        // 6to4
        let v4 = Ipv4Addr::new((s[1] >> 8) as u8, s[1] as u8, (s[2] >> 8) as u8, s[2] as u8);
        return is_public_v4(v4);
    }
    true
}

/// Lowercase host name without a trailing dot.
fn normalize_host(host: &str) -> String {
    host.trim_end_matches('.').to_ascii_lowercase()
}

/// `localhost` and `*.localhost`, with or without the trailing dot.
fn is_local_name(host: &str) -> bool {
    let h = normalize_host(host);
    h == "localhost" || h.ends_with(".localhost")
}

/// DNS resolver that refuses names resolving only to non-public addresses (and drops
/// the non-public ones of mixed answers, so a connection never goes to them). Proxy
/// hosts are exempt: a proxy on the local network is fine, the targets sent to it are
/// checked by [`GuardedRequest`] before sending.
struct PublicOnlyResolver {
    proxy_hosts: Arc<HashSet<String>>,
}

impl Resolve for PublicOnlyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let exempt = self.proxy_hosts.contains(&normalize_host(name.as_str()));
        Box::pin(async move {
            let host = name.as_str().to_string();
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|a| exempt || is_public_ip(a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(format!("blocked: {host} resolves to a non-public address").into());
            }
            let it: Addrs = Box::new(addrs.into_iter());
            Ok(it)
        })
    }
}

/// Host names of proxies given as `HTTP_PROXY`-style values (`host:port` or a URL).
fn proxy_host_names<'a>(values: impl IntoIterator<Item = &'a str>) -> HashSet<String> {
    values
        .into_iter()
        .filter_map(|v| {
            let v = v.trim();
            let url = if v.contains("://") { Url::parse(v) } else { Url::parse(&format!("http://{v}")) };
            match url.ok()?.host()? {
                url::Host::Domain(d) if d.parse::<IpAddr>().is_err() => Some(normalize_host(d)),
                _ => None,
            }
        })
        .collect()
}

/// Check that a URL is http(s) and, if `public_only`, not an IP literal in a private range
/// nor a localhost name. (Other host names are checked when resolved.)
pub fn check_url(url: &Url, public_only: bool) -> Result<()> {
    if !matches!(url.scheme(), "http" | "https") {
        bail!("unsupported URL scheme: {}", url.scheme());
    }
    if public_only {
        match url.host() {
            Some(url::Host::Ipv4(ip)) if !is_public_v4(ip) => bail!("blocked private address {ip}"),
            Some(url::Host::Ipv6(ip)) if !is_public_v6(ip) => bail!("blocked private address {ip}"),
            Some(url::Host::Domain(d)) if is_local_name(d) => bail!("blocked local host {d}"),
            None => bail!("URL has no host"),
            _ => {}
        }
    }
    Ok(())
}

/// Resolve a URL's host name, within `limit`.
async fn lookup(url: &Url, host: &str, limit: Option<Duration>) -> Result<Vec<SocketAddr>> {
    let port = url.port_or_known_default().unwrap_or(80);
    let res = match limit {
        Some(t) => tokio::time::timeout(t, tokio::net::lookup_host((host, port))).await.map_err(|_| anyhow!("timed out resolving {host}"))?,
        None => tokio::net::lookup_host((host, port)).await,
    };
    let addrs: Vec<SocketAddr> = res.map_err(|e| anyhow!("cannot resolve {host}: {e}"))?.collect();
    if addrs.is_empty() {
        bail!("cannot resolve {host}");
    }
    Ok(addrs)
}

/// Fail unless every address the URL's host name resolves to is public. Run before each
/// request that must stay public: with a proxy the target is resolved by the proxy, not
/// by [`PublicOnlyResolver`]. (The proxy resolves it again; a name that changes its
/// answer in between can still slip through, which only the resolver prevents.)
async fn ensure_public_host(url: &Url, limit: Option<Duration>) -> Result<()> {
    let Some(url::Host::Domain(host)) = url.host() else { return Ok(()) };
    if let Some(a) = lookup(url, host, limit).await?.iter().find(|a| !is_public_ip(a.ip())) {
        bail!("blocked: {host} resolves to a non-public address ({})", a.ip());
    }
    Ok(())
}

/// Is the URL on a private network: a private address, a localhost name or a name with a
/// private address among its answers (a LAN host may have a public IPv6 address too)?
async fn is_private_target(url: &Url, limit: Option<Duration>) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => !is_public_v4(ip),
        Some(url::Host::Ipv6(ip)) => !is_public_v6(ip),
        Some(url::Host::Domain(d)) => {
            is_local_name(d) || lookup(url, d, limit).await.is_ok_and(|addrs| addrs.iter().any(|a| !is_public_ip(a.ip())))
        }
        None => false,
    }
}

/// Which addresses the requests of a [`GuardedClient`] may reach.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    /// Anything (private networks allowed by configuration).
    Any,
    /// Public addresses only, on every redirect hop.
    Public,
    /// Private addresses only when the requested URL itself is on a private network (an
    /// admin-entered localhost or LAN feed); otherwise like `Public`, so that a public
    /// feed cannot redirect into the local network.
    Origin,
}

/// Most redirects followed for one request.
const MAX_REDIRECTS: usize = 10;

/// HTTP client for URLs from feeds and pages. Redirects are followed here instead of by
/// reqwest, so that each hop is checked against the client's [`Reach`] (host names are
/// resolved and checked before sending: a configured proxy resolves the real target).
#[derive(Clone)]
pub struct GuardedClient {
    /// Connections that may go to private addresses.
    open: reqwest::Client,
    /// Connections only to public addresses (filtering DNS resolver).
    public: reqwest::Client,
    reach: Reach,
}

impl GuardedClient {
    /// Start a GET request.
    pub fn get(&self, url: Url) -> GuardedRequest {
        GuardedRequest { client: self.clone(), url, headers: HeaderMap::new(), timeout: None, reach: self.reach, error: None }
    }
}

/// A GET request of a [`GuardedClient`] (the subset of reqwest's builder in use here).
pub struct GuardedRequest {
    client: GuardedClient,
    url: Url,
    headers: HeaderMap,
    timeout: Option<Duration>,
    reach: Reach,
    error: Option<String>,
}

impl GuardedRequest {
    pub fn header<K, V>(mut self, key: K, value: V) -> GuardedRequest
    where
        K: IntoHeaderName,
        HeaderValue: TryFrom<V>,
        <HeaderValue as TryFrom<V>>::Error: std::fmt::Display,
    {
        match HeaderValue::try_from(value) {
            Ok(v) => {
                self.headers.insert(key, v);
            }
            Err(e) => {
                self.error.get_or_insert_with(|| format!("invalid header value: {e}"));
            }
        }
        self
    }

    /// Total time for the request, its redirects and reading the response body.
    pub fn timeout(mut self, timeout: Duration) -> GuardedRequest {
        self.timeout = Some(timeout);
        self
    }

    pub async fn send(self) -> Result<reqwest::Response> {
        Ok(self.send_checked().await?.0)
    }

    /// Send, following redirects; also returns whether private addresses were allowed.
    async fn send_checked(self) -> Result<(reqwest::Response, bool)> {
        if let Some(e) = self.error {
            bail!(e);
        }
        let deadline = self.timeout.map(|t| Instant::now() + t);
        let left = || deadline.map(|d| d.saturating_duration_since(Instant::now()));
        let mut url = self.url;
        let private_ok = match self.reach {
            Reach::Any => true,
            Reach::Public => false,
            Reach::Origin => is_private_target(&url, left()).await,
        };
        let client = if private_ok { &self.client.open } else { &self.client.public };
        let mut headers = self.headers;
        let mut hops = 0;
        loop {
            check_url(&url, !private_ok)?;
            if !private_ok {
                ensure_public_host(&url, left()).await?;
            }
            let mut req = client.get(url.clone()).headers(headers.clone());
            if let Some(t) = left() {
                if t.is_zero() {
                    bail!("timed out requesting {url}");
                }
                req = req.timeout(t);
            }
            let resp = req.send().await?;
            if !matches!(resp.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
                return Ok((resp, private_ok));
            }
            let location = resp.headers().get(reqwest::header::LOCATION).and_then(|l| std::str::from_utf8(l.as_bytes()).ok());
            let Some(mut next) = location.and_then(|l| url.join(l).ok()) else {
                return Ok((resp, private_ok));
            };
            hops += 1;
            if hops > MAX_REDIRECTS {
                bail!("too many redirects ({url})");
            }
            // As reqwest does: same-origin redirects keep credentials given in the URL,
            // and the previous URL is the referer (unless leaving https for http).
            if next.origin() == url.origin() && next.username().is_empty() && !url.username().is_empty() {
                let _ = next.set_username(url.username());
                let _ = next.set_password(url.password());
            }
            let mut referer = url.clone();
            let _ = referer.set_username("");
            let _ = referer.set_password(None);
            referer.set_fragment(None);
            match HeaderValue::from_str(referer.as_str()) {
                Ok(r) if !(url.scheme() == "https" && next.scheme() == "http") => headers.insert(reqwest::header::REFERER, r),
                _ => headers.remove(reqwest::header::REFERER),
            };
            url = next;
        }
    }
}

#[derive(Clone)]
pub struct HttpClients {
    /// For feeds (admin-provided URLs): private networks only for localhost/LAN feeds.
    pub feed: GuardedClient,
    /// For article pages, images, icons (URLs coming from feed content): public only.
    pub content: GuardedClient,
    /// For the translation API / external extractor (admin-provided).
    pub api: reqwest::Client,
    pub public_only: bool,
    pub hosts: HostLimiter,
}

impl HttpClients {
    pub fn new(allow_private_networks: bool) -> Result<HttpClients> {
        HttpClients::with_proxy(allow_private_networks, None)
    }

    /// Clients that also send feed and content requests through `proxy` (a URL). Proxies
    /// configured in the environment / system are used as well, as by reqwest itself.
    pub fn with_proxy(allow_private_networks: bool, proxy: Option<&str>) -> Result<HttpClients> {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::ACCEPT_LANGUAGE,
            HeaderValue::from_static("en-US,en;q=0.9,zh-CN;q=0.8,zh;q=0.7"),
        );
        let explicit = proxy;
        let proxy = explicit.map(reqwest::Proxy::all).transpose()?;
        // Proxy hosts given by name must resolve even to private addresses. (Names from
        // the macOS/Windows system settings are not known here; those proxies are
        // normally given as 127.0.0.1.)
        let env: Vec<String> = ["ALL_PROXY", "all_proxy", "HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"]
            .iter()
            .filter_map(|k| std::env::var(k).ok())
            .collect();
        let proxy_hosts = Arc::new(proxy_host_names(env.iter().map(String::as_str).chain(explicit)));
        let base = || {
            let b = reqwest::Client::builder()
                .default_headers(headers.clone())
                .connect_timeout(Duration::from_secs(15))
                .pool_idle_timeout(Duration::from_secs(60))
                .pool_max_idle_per_host(4)
                .tcp_keepalive(Duration::from_secs(60))
                .redirect(reqwest::redirect::Policy::none());
            match &proxy {
                Some(p) => b.proxy(p.clone()),
                None => b,
            }
        };
        let open = base().build()?;
        let public = base().dns_resolver(Arc::new(PublicOnlyResolver { proxy_hosts })).build()?;
        let public_only = !allow_private_networks;
        let client = |reach| GuardedClient { open: open.clone(), public: public.clone(), reach };
        let api = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()?;
        Ok(HttpClients {
            feed: client(if public_only { Reach::Origin } else { Reach::Any }),
            content: client(if public_only { Reach::Public } else { Reach::Any }),
            api,
            public_only,
            hosts: HostLimiter::default(),
        })
    }
}

/// Response body read with a size limit.
pub struct Fetched {
    pub status: u16,
    pub final_url: Url,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
    /// Set by [`get`]: the request was allowed to reach private addresses (private
    /// networks enabled, or a localhost/LAN feed). Links found in a page fetched without
    /// it must be fetched public-only too.
    pub private_ok: bool,
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
    Ok(Fetched { status, final_url, headers, body, private_ok: false })
}

/// Simple GET with size limit and timeout, returning the body for any status.
/// `public_only` restricts the request to public addresses whatever the client allows.
pub async fn get(
    client: &GuardedClient,
    url: &str,
    user_agent: &str,
    timeout: Duration,
    max_bytes: usize,
    public_only: bool,
    accept: &str,
) -> Result<Fetched> {
    let parsed = Url::parse(url).map_err(|e| anyhow!("invalid URL {url}: {e}"))?;
    let mut req = client
        .get(parsed)
        .header(reqwest::header::USER_AGENT, user_agent)
        .header(reqwest::header::ACCEPT, accept)
        .timeout(timeout);
    if public_only {
        req.reach = Reach::Public;
    }
    let (resp, private_ok) = req.send_checked().await?;
    let mut f = read_limited(resp, max_bytes).await?;
    f.private_ok = private_ok;
    Ok(f)
}

/// Per-host concurrency + politeness delay.
#[derive(Clone, Default)]
pub struct HostLimiter {
    inner: Arc<Mutex<HashMap<String, HostSlot>>>,
}

struct HostSlot {
    sem: Arc<Semaphore>,
    /// Permits `sem` was created with.
    limit: usize,
    next_allowed: Instant,
}

pub struct HostPermit {
    _permit: OwnedSemaphorePermit,
}

impl HostLimiter {
    /// Wait for a slot for `host` honoring `max_concurrent` and a minimum `delay`
    /// between request starts.
    pub async fn acquire(&self, host: &str, max_concurrent: u32, delay: Duration) -> HostPermit {
        let limit = max_concurrent.max(1) as usize;
        let sem = {
            let mut map = self.inner.lock().unwrap();
            if map.len() > 4096 {
                map.retain(|_, s| Arc::strong_count(&s.sem) > 1);
            }
            let slot = map
                .entry(host.to_string())
                .or_insert_with(|| HostSlot { sem: Arc::new(Semaphore::new(limit)), limit, next_allowed: Instant::now() });
            // A changed limit (settings) applies once the host is idle.
            if slot.limit != limit && slot.sem.available_permits() == slot.limit {
                slot.sem = Arc::new(Semaphore::new(limit));
                slot.limit = limit;
            }
            slot.sem.clone()
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

    #[tokio::test]
    async fn host_limit_changes_apply_when_idle() {
        let hosts = HostLimiter::default();
        let a = hosts.acquire("h", 1, Duration::ZERO).await;
        // Busy: a new limit does not apply yet, the second request waits.
        assert!(tokio::time::timeout(Duration::from_millis(50), hosts.acquire("h", 2, Duration::ZERO)).await.is_err());
        drop(a);
        // Idle: the new limit of 2 applies.
        let _b = hosts.acquire("h", 2, Duration::ZERO).await;
        let _c = tokio::time::timeout(Duration::from_millis(200), hosts.acquire("h", 2, Duration::ZERO)).await.expect("second slot");
    }

    #[test]
    fn private_ranges_blocked() {
        for ip in [
            "127.0.0.1", "10.1.2.3", "192.168.1.1", "172.16.0.1", "169.254.169.254", "100.64.0.1", "0.0.0.0", "::1", "fd00::1", "fe80::1",
            "::ffff:127.0.0.1", "64:ff9b::a00:1", "::127.0.0.1", "::a9fe:a9fe", "::ffff:0:7f00:1", "fec0::1", "::",
        ] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip} should be blocked");
        }
        for ip in ["8.8.8.8", "151.101.1.1", "2606:4700::1111", "64:ff9b::808:808", "198.18.17.182"] {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip} should be allowed");
        }
    }

    #[test]
    fn url_checks() {
        assert!(check_url(&Url::parse("http://127.0.0.1/x").unwrap(), true).is_err());
        assert!(check_url(&Url::parse("http://localhost:8080/").unwrap(), true).is_err());
        for u in ["http://localhost./", "http://LocalHost.:80/", "http://a.localhost./", "http://[::127.0.0.1]/", "http://0x7f.1/", "http://[fec0::1]/"] {
            assert!(check_url(&Url::parse(u).unwrap(), true).is_err(), "{u}");
        }
        assert!(check_url(&Url::parse("file:///etc/passwd").unwrap(), false).is_err());
        assert!(check_url(&Url::parse("https://example.com/").unwrap(), true).is_ok());
        assert!(check_url(&Url::parse("http://127.0.0.1/x").unwrap(), false).is_ok());
    }

    #[test]
    fn proxy_hosts_from_env_values() {
        let hosts = proxy_host_names(["http://Clash.LAN.:7890", "proxy.internal:3128", "socks5://127.0.0.1:1080", "", "http://[::1]:8080"]);
        assert_eq!(hosts, HashSet::from(["clash.lan".to_string(), "proxy.internal".to_string()]));
    }

    #[tokio::test]
    async fn host_names_are_resolved_and_checked() {
        let local = Url::parse("http://localhost:8080/x").unwrap();
        let e = ensure_public_host(&local, Some(Duration::from_secs(5))).await.unwrap_err();
        assert!(e.to_string().contains("blocked"), "{e}");
        assert!(ensure_public_host(&Url::parse("http://198.18.0.1/").unwrap(), None).await.is_ok(), "literals are checked by check_url");
        assert!(is_private_target(&local, None).await);
        assert!(is_private_target(&Url::parse("http://192.168.1.5/feed").unwrap(), None).await);
        assert!(!is_private_target(&Url::parse("https://8.8.8.8/feed").unwrap(), None).await);
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

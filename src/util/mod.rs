pub mod html;
pub mod text;
pub mod urlnorm;

use std::time::{SystemTime, UNIX_EPOCH};

/// Current unix time in seconds.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Current unix time in microseconds.
pub fn now_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// Random URL-safe token (hex, 256 bits).
pub fn random_token() -> String {
    let bytes: [u8; 32] = rand::random();
    hex::encode(bytes)
}

/// Hash a secret token for storage.
pub fn token_hash(token: &str) -> String {
    blake3::hash(token.as_bytes()).to_hex().to_string()
}

/// Short stable hash (hex) of arbitrary text.
pub fn short_hash(data: &str) -> String {
    let h = blake3::hash(data.as_bytes());
    h.to_hex()[..32].to_string()
}

/// Truncate a string to at most `max` characters (not bytes), appending an ellipsis.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Human readable byte size.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// Format a unix timestamp (secs) as RFC 3339 UTC, or "-" when absent.
pub fn fmt_ts(ts: Option<i64>) -> String {
    match ts {
        Some(t) if t > 0 => chrono::DateTime::from_timestamp(t, 0)
            .map(|d| d.format("%Y-%m-%d %H:%M:%S UTC").to_string())
            .unwrap_or_else(|| "-".into()),
        _ => "-".into(),
    }
}

/// Relative description of a timestamp ("5 min ago", "in 3 h").
pub fn fmt_relative(ts: Option<i64>) -> String {
    let Some(t) = ts.filter(|t| *t > 0) else {
        return "-".into();
    };
    let d = t - now();
    let (abs, future) = if d >= 0 { (d, true) } else { (-d, false) };
    let s = if abs < 60 {
        format!("{abs} s")
    } else if abs < 3600 {
        format!("{} min", abs / 60)
    } else if abs < 86400 {
        format!("{} h", abs / 3600)
    } else {
        format!("{} d", abs / 86400)
    };
    if future { format!("in {s}") } else { format!("{s} ago") }
}

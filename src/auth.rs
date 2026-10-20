//! Single-user authentication: password hashes, session/API tokens, login throttling.

use std::net::IpAddr;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use rusqlite::{OptionalExtension, params};

use crate::app::App;
use crate::settings::{get_raw, set_raw};
use crate::util;

const KEY_PASSWORD: &str = "auth.password_hash";
const KEY_API_PASSWORD: &str = "auth.api_password_hash";
const KEY_SECRET: &str = "auth.secret";

pub fn hash_password(password: &str) -> Result<String> {
    Ok(Argon2::default()
        .hash_password(password.as_bytes())
        .map_err(|e| anyhow!("hash password: {e}"))?
        .to_string())
}

pub fn verify_hash(password: &str, hash: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok(),
        Err(_) => false,
    }
}

/// Make sure a server secret and a password exist (password from RILL_PASSWORD or
/// randomly generated).
pub async fn bootstrap(app: &App) -> Result<()> {
    app.db
        .call(|c| {
            if get_raw(c, KEY_SECRET)?.is_none() {
                set_raw(c, KEY_SECRET, &util::random_token())?;
            }
            Ok(())
        })
        .await?;
    let existing = app.db.call(|c| get_raw(c, KEY_PASSWORD)).await?;
    if existing.is_some() {
        return Ok(());
    }
    let (password, generated) = match app.cfg.password.as_deref().filter(|p| !p.is_empty()) {
        Some(p) => (p.to_string(), false),
        None => {
            let bytes: [u8; 9] = rand::random();
            (base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes), true)
        }
    };
    let hash = hash_password(&password)?;
    app.db.call(move |c| set_raw(c, KEY_PASSWORD, &hash)).await?;
    if generated {
        tracing::warn!(
            "No RILL_PASSWORD set: generated initial password for user '{}': {}  (change it in the web admin)",
            app.cfg.username,
            password
        );
    }
    Ok(())
}

pub fn username_matches(app: &App, user: &str) -> bool {
    user.trim().eq_ignore_ascii_case(app.cfg.username.trim())
}

/// Verify the admin password.
pub async fn verify_password(app: &App, password: &str) -> Result<bool> {
    let hash = app.db.call(|c| get_raw(c, KEY_PASSWORD)).await?;
    let password = password.to_string();
    Ok(tokio::task::spawn_blocking(move || hash.is_some_and(|h| verify_hash(&password, &h))).await?)
}

/// Verify a password presented by an API client: the API password when one is set,
/// otherwise the admin password.
pub async fn verify_api_password(app: &App, password: &str) -> Result<bool> {
    let api = app.db.call(|c| get_raw(c, KEY_API_PASSWORD)).await?;
    match api {
        Some(h) => {
            let password = password.to_string();
            Ok(tokio::task::spawn_blocking(move || verify_hash(&password, &h)).await?)
        }
        None => verify_password(app, password).await,
    }
}

/// Long-lived Google Reader auth token. Derived from the server secret and the current
/// API credential hash, so it stays valid across logins (clients like Capy store the
/// token instead of the password) and changes automatically when the password changes.
pub async fn greader_token(app: &App) -> Result<String> {
    let (secret, cred) = app
        .db
        .call(|c| {
            let secret = get_raw(c, KEY_SECRET)?.unwrap_or_default();
            let cred = match get_raw(c, KEY_API_PASSWORD)? {
                Some(h) => h,
                None => get_raw(c, KEY_PASSWORD)?.unwrap_or_default(),
            };
            Ok((secret, cred))
        })
        .await?;
    let key = blake3::derive_key("rillrss greader token v1", secret.as_bytes());
    let h = blake3::keyed_hash(&key, cred.as_bytes());
    Ok(h.to_hex()[..48].to_string())
}

/// Action token (`T`) for the Google Reader API: 57 characters like FreshRSS.
pub fn greader_action_token(auth_token: &str) -> String {
    let h = blake3::hash(format!("T:{auth_token}").as_bytes()).to_hex().to_string();
    let mut t = h[..57.min(h.len())].to_string();
    while t.len() < 57 {
        t.push('Z');
    }
    t
}

/// Constant-time string comparison.
pub fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub async fn has_api_password(app: &App) -> Result<bool> {
    Ok(app.db.call(|c| get_raw(c, KEY_API_PASSWORD)).await?.is_some())
}

pub async fn set_password(app: &App, password: &str) -> Result<()> {
    let hash = hash_password(password)?;
    app.db
        .call(move |c| {
            set_raw(c, KEY_PASSWORD, &hash)?;
            c.execute("DELETE FROM sessions WHERE kind = 'web'", [])?;
            Ok(())
        })
        .await?;
    // API tokens were issued for the old password when no separate API password is set.
    if !has_api_password(app).await? {
        revoke_sessions(app, &["greader", "ttrss"]).await?;
    }
    Ok(())
}

/// Set (Some) or clear (None) the separate API password. Revokes API tokens.
pub async fn set_api_password(app: &App, password: Option<&str>) -> Result<()> {
    let hash = match password.filter(|p| !p.is_empty()) {
        Some(p) => Some(hash_password(p)?),
        None => None,
    };
    app.db
        .call(move |c| {
            match hash {
                Some(h) => set_raw(c, KEY_API_PASSWORD, &h)?,
                None => {
                    c.execute("DELETE FROM settings WHERE key = ?", [KEY_API_PASSWORD])?;
                }
            }
            Ok(())
        })
        .await?;
    revoke_sessions(app, &["greader", "ttrss"]).await
}

pub async fn revoke_sessions(app: &App, kinds: &[&str]) -> Result<()> {
    let kinds: Vec<String> = kinds.iter().map(|s| s.to_string()).collect();
    app.db
        .call(move |c| {
            for k in kinds {
                c.execute("DELETE FROM sessions WHERE kind = ?", [k])?;
            }
            Ok(())
        })
        .await
}

/// Create a session token of the given kind.
pub async fn create_session(app: &App, kind: &str, ttl: Option<Duration>, user_agent: Option<String>, ip: Option<IpAddr>) -> Result<String> {
    let token = util::random_token();
    let hash = util::token_hash(&token);
    let now = util::now();
    let expires = ttl.map(|t| now + t.as_secs() as i64);
    let kind = kind.to_string();
    app.db
        .call(move |c| {
            c.execute(
                "INSERT INTO sessions (token_hash, kind, created_at, last_used_at, expires_at, user_agent, remote_addr) VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![hash, kind, now, now, expires, user_agent.map(|u| util::truncate_chars(&u, 200)), ip.map(|i| i.to_string())],
            )?;
            Ok(())
        })
        .await?;
    Ok(token)
}

/// Validate a token of one of the given kinds.
pub async fn check_session(app: &App, kinds: &'static [&'static str], token: &str) -> Result<bool> {
    let token = token.trim();
    if token.is_empty() || token.len() > 200 {
        return Ok(false);
    }
    let hash = util::token_hash(token);
    let now = util::now();
    app.db
        .call(move |c| {
            let row: Option<(String, i64, Option<i64>)> = c
                .query_row("SELECT kind, last_used_at, expires_at FROM sessions WHERE token_hash = ?", [&hash], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .optional()?;
            let Some((kind, last_used, expires)) = row else { return Ok(false) };
            if !kinds.contains(&kind.as_str()) {
                return Ok(false);
            }
            if expires.is_some_and(|e| e < now) {
                c.execute("DELETE FROM sessions WHERE token_hash = ?", [&hash])?;
                return Ok(false);
            }
            if now - last_used > 300 {
                c.execute("UPDATE sessions SET last_used_at = ? WHERE token_hash = ?", params![now, hash])?;
            }
            Ok(true)
        })
        .await
}

pub async fn delete_session(app: &App, token: &str) -> Result<()> {
    let hash = util::token_hash(token);
    app.db
        .call(move |c| {
            c.execute("DELETE FROM sessions WHERE token_hash = ?", [hash])?;
            Ok(())
        })
        .await
}

const MAX_FAILURES: u32 = 10;
const FAILURE_WINDOW: Duration = Duration::from_secs(15 * 60);

/// Is this client currently locked out after too many failed logins?
pub fn login_blocked(app: &App, ip: Option<IpAddr>) -> bool {
    let Some(ip) = ip else { return false };
    let map = app.login_failures.lock().unwrap();
    matches!(map.get(&ip), Some((n, t)) if *n >= MAX_FAILURES && t.elapsed() < FAILURE_WINDOW)
}

pub fn record_login(app: &App, ip: Option<IpAddr>, success: bool) {
    let Some(ip) = ip else { return };
    let mut map = app.login_failures.lock().unwrap();
    if success {
        map.remove(&ip);
        return;
    }
    if map.len() > 10_000 {
        map.retain(|_, (_, t)| t.elapsed() < FAILURE_WINDOW);
    }
    let e = map.entry(ip).or_insert((0, Instant::now()));
    if e.1.elapsed() >= FAILURE_WINDOW {
        *e = (0, Instant::now());
    }
    e.0 += 1;
    e.1 = Instant::now();
}

/// Full API login check (username + password + throttling).
pub async fn api_login(app: &App, user: &str, password: &str, ip: Option<IpAddr>) -> Result<bool> {
    if login_blocked(app, ip) {
        return Ok(false);
    }
    let ok = username_matches(app, user) && verify_api_password(app, password).await?;
    record_login(app, ip, ok);
    if !ok {
        app.event(crate::app::Level::Warn, "api", Default::default(), format!("API 登录失败：用户 '{}'，来自 {:?}", util::truncate_chars(user, 64), ip))
            .await;
    }
    Ok(ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_roundtrip() {
        let h = hash_password("s3cret").unwrap();
        assert!(h.starts_with("$argon2"));
        assert!(verify_hash("s3cret", &h));
        assert!(!verify_hash("wrong", &h));
        assert!(!verify_hash("s3cret", "garbage"));
    }
}

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

/// Argon2 needs about 19 MiB per check and runs on the blocking pool; a few slots
/// keep a flood of login attempts from exhausting memory.
static PASSWORD_CHECKS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// A stored credential a password can be checked against.
#[derive(Debug, Clone, Copy)]
pub enum Cred {
    /// The admin password (web admin).
    Admin,
    /// What API clients log in with: the API password when set, else the admin password.
    Api,
}

impl Cred {
    /// SQL expression yielding the stored hash.
    fn sql(self) -> &'static str {
        match self {
            Cred::Admin => "(SELECT value FROM settings WHERE key = 'auth.password_hash')",
            Cred::Api => {
                "COALESCE((SELECT value FROM settings WHERE key = 'auth.api_password_hash'), \
                 (SELECT value FROM settings WHERE key = 'auth.password_hash'))"
            }
        }
    }
}

async fn stored_hash(app: &App, cred: Cred) -> Result<Option<String>> {
    let sql = format!("SELECT {}", cred.sql());
    app.db.call(move |c| Ok(c.query_row(&sql, [], |r| r.get(0))?)).await
}

/// Check a password; returns the hash it matched, so that a token or session can be
/// bound to exactly that credential (a concurrent password change then voids it).
pub async fn check_password(app: &App, cred: Cred, password: &str) -> Result<Option<String>> {
    let Some(hash) = stored_hash(app, cred).await? else { return Ok(None) };
    let _slot = PASSWORD_CHECKS.acquire().await?;
    let (password, h) = (password.to_string(), hash.clone());
    let ok = tokio::task::spawn_blocking(move || verify_hash(&password, &h)).await?;
    Ok(ok.then_some(hash))
}

/// Verify the admin password.
pub async fn verify_password(app: &App, password: &str) -> Result<bool> {
    Ok(check_password(app, Cred::Admin, password).await?.is_some())
}

fn derive_greader_token(secret: &str, cred: &str) -> String {
    let key = blake3::derive_key("rillrss greader token v1", secret.as_bytes());
    blake3::keyed_hash(&key, cred.as_bytes()).to_hex()[..48].to_string()
}

/// Long-lived Google Reader auth token. Derived from the server secret and the current
/// API credential hash, so it stays valid across logins (clients like Capy store the
/// token instead of the password) and changes automatically when the password changes.
pub async fn greader_token(app: &App) -> Result<String> {
    let cred = stored_hash(app, Cred::Api).await?.unwrap_or_default();
    greader_token_for(app, &cred).await
}

/// The token for a given credential hash (the one a login just matched).
pub async fn greader_token_for(app: &App, cred: &str) -> Result<String> {
    let secret = app.db.call(|c| get_raw(c, KEY_SECRET)).await?.unwrap_or_default();
    Ok(derive_greader_token(&secret, cred))
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

/// Create a session token of the given kind for a login that matched `cred_hash`.
/// Returns None when that credential has changed since (password changed meanwhile).
#[allow(clippy::too_many_arguments)]
pub async fn create_session(
    app: &App,
    kind: &str,
    ttl: Option<Duration>,
    user_agent: Option<String>,
    ip: Option<IpAddr>,
    cred: Cred,
    cred_hash: String,
) -> Result<Option<String>> {
    let token = util::random_token();
    let hash = util::token_hash(&token);
    let now = util::now();
    let expires = ttl.map(|t| now + t.as_secs() as i64);
    let kind = kind.to_string();
    let sql = format!(
        "INSERT INTO sessions (token_hash, kind, created_at, last_used_at, expires_at, user_agent, remote_addr)
         SELECT ?, ?, ?, ?, ?, ?, ? WHERE {} = ?",
        cred.sql()
    );
    let inserted = app
        .db
        .call(move |c| {
            Ok(c.execute(
                &sql,
                params![hash, kind, now, now, expires, user_agent.map(|u| util::truncate_chars(&u, 200)), ip.map(|i| i.to_string()), cred_hash],
            )?)
        })
        .await?;
    Ok((inserted == 1).then_some(token))
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

/// Start a login attempt; false while the client is locked out after too many
/// failures. The attempt counts as a failure until `finish_login` reports success,
/// so parallel guesses are limited too.
pub fn begin_login(app: &App, ip: Option<IpAddr>) -> bool {
    let Some(ip) = ip else { return true };
    let mut map = app.login_failures.lock().unwrap();
    if map.len() > 10_000 {
        map.retain(|_, (_, t)| t.elapsed() < FAILURE_WINDOW);
    }
    let e = map.entry(ip).or_insert((0, Instant::now()));
    if e.1.elapsed() >= FAILURE_WINDOW {
        *e = (0, Instant::now());
    }
    if e.0 >= MAX_FAILURES {
        return false;
    }
    e.0 += 1;
    e.1 = Instant::now();
    true
}

/// End a login attempt started with `begin_login`; success clears the failures.
pub fn finish_login(app: &App, ip: Option<IpAddr>, success: bool) {
    if let (Some(ip), true) = (ip, success) {
        app.login_failures.lock().unwrap().remove(&ip);
    }
}

/// Full API login check (username + password + throttling). `passwords` are
/// alternative spellings of one attempt (e.g. base64 from old TT-RSS clients).
/// Returns the matched credential hash.
pub async fn api_login(app: &App, user: &str, passwords: &[&str], ip: Option<IpAddr>) -> Result<Option<String>> {
    if !begin_login(app, ip) {
        return Ok(None);
    }
    let mut matched = None;
    if username_matches(app, user) {
        for p in passwords {
            matched = check_password(app, Cred::Api, p).await?;
            if matched.is_some() {
                break;
            }
        }
    }
    finish_login(app, ip, matched.is_some());
    if matched.is_none() {
        app.event(crate::app::Level::Warn, "api", Default::default(), format!("API 登录失败：用户 '{}'，来自 {:?}", util::truncate_chars(user, 64), ip))
            .await;
    }
    Ok(matched)
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

//! SQLite access: connection pool, pragmas, migrations and a small async bridge.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{Connection, OpenFlags};

pub type Pool = r2d2::Pool<SqliteConnectionManager>;

const MIGRATIONS: &[&str] = &[include_str!("../../migrations/0001_init.sql")];

#[derive(Clone)]
pub struct Db {
    pool: Pool,
}

fn init_conn(conn: &mut Connection) -> rusqlite::Result<()> {
    conn.busy_timeout(Duration::from_secs(15))?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;
         PRAGMA cache_size = -8000;",
    )?;
    Ok(())
}

impl Db {
    /// Open (and create/migrate) the database at `path`.
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let manager = SqliteConnectionManager::file(path)
            .with_flags(
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_CREATE
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX
                    | OpenFlags::SQLITE_OPEN_URI,
            )
            .with_init(init_conn);
        let pool = r2d2::Pool::builder()
            .max_size(8)
            .min_idle(Some(1))
            .connection_timeout(Duration::from_secs(30))
            .build(manager)
            .context("open sqlite pool")?;
        let db = Db { pool };
        {
            let mut conn = db.pool.get()?;
            migrate(&mut conn)?;
        }
        Ok(db)
    }

    /// Get a pooled connection synchronously (only call from blocking contexts).
    pub fn conn(&self) -> Result<r2d2::PooledConnection<SqliteConnectionManager>> {
        Ok(self.pool.get()?)
    }

    /// Run a closure with a connection on the blocking thread pool.
    pub async fn call<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get()?;
            f(&mut conn)
        })
        .await
        .context("db task panicked")?
    }

    /// Run a closure inside an IMMEDIATE transaction (single writer, no upgrade deadlocks).
    pub async fn write<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&rusqlite::Transaction<'_>) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        self.call(move |conn| {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let out = f(&tx)?;
            tx.commit()?;
            Ok(out)
        })
        .await
    }

    /// Consistent online backup of the database to `dest` (SQLite backup API).
    pub fn backup_to(&self, dest: &Path) -> Result<()> {
        let conn = self.conn()?;
        let mut out = Connection::open(dest).with_context(|| format!("open {}", dest.display()))?;
        let backup = rusqlite::backup::Backup::new(&conn, &mut out)?;
        backup.run_to_completion(256, Duration::from_millis(10), None)?;
        Ok(())
    }

    /// `PRAGMA integrity_check` result ("ok" when healthy).
    pub fn integrity_check(&self) -> Result<String> {
        let conn = self.conn()?;
        let mut st = conn.prepare("PRAGMA integrity_check")?;
        let rows: Vec<String> = st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
        Ok(rows.join("\n"))
    }

    /// Size in bytes of the main database file plus WAL.
    pub fn file_size(path: &Path) -> u64 {
        let mut total = 0;
        for suffix in ["", "-wal", "-shm"] {
            let p = format!("{}{}", path.display(), suffix);
            if let Ok(m) = std::fs::metadata(&p) {
                total += m.len();
            }
        }
        total
    }
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if current == 0 {
        // Must be set before the first table is created; enables incremental_vacuum.
        conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL;")?;
    }
    for (idx, sql) in MIGRATIONS.iter().enumerate() {
        let version = (idx + 1) as i64;
        if version <= current {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(sql)
            .with_context(|| format!("apply migration {version}"))?;
        tx.execute_batch(&format!("PRAGMA user_version = {version}"))?;
        tx.commit()?;
        tracing::info!(version, "applied database migration");
    }
    Ok(())
}

/// Helper: build a `?, ?, ?` placeholder list.
pub fn placeholders(n: usize) -> String {
    let mut s = String::with_capacity(n * 3);
    for i in 0..n {
        if i > 0 {
            s.push_str(", ");
        }
        s.push('?');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_apply_and_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let db = Db::open(&path).unwrap();
        drop(db);
        let db = Db::open(&path).unwrap();
        let conn = db.conn().unwrap();
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, MIGRATIONS.len() as i64);
        let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
        drop(conn);
        assert_eq!(db.integrity_check().unwrap(), "ok");
        let dest = dir.path().join("backup.db");
        db.backup_to(&dest).unwrap();
        let copy = Db::open(&dest).unwrap();
        assert_eq!(copy.integrity_check().unwrap(), "ok");
    }
}

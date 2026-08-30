use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::Connection;

use super::migrations;
use crate::error::Result;

pub struct Db {
    path: PathBuf,
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            crate::paths::ensure_dir(parent)?;
        }

        let mut conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;
             PRAGMA journal_mode = WAL;",
        )?;
        migrations::apply(&mut conn)?;

        Ok(Self {
            path: path.to_path_buf(),
            conn: Mutex::new(conn),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn with_conn<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T>,
    {
        self.with_conn_mut(|conn| f(conn))
    }

    pub fn with_conn_mut<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T>,
    {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| crate::error::AppError::other("database connection mutex was poisoned"))?;
        f(&mut conn)
    }
}

#[cfg(test)]
mod tests {
    use super::Db;
    use tempfile::tempdir;

    #[test]
    fn migrates() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("voxtype-meeting-service.db");
        let db = Db::open(&path).expect("open");

        db.with_conn(|conn| {
            let tables: Vec<String> = conn
                .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")?
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            assert!(tables.contains(&"sessions".to_string()));
            assert!(tables.contains(&"audio_chunks".to_string()));
            assert!(tables.contains(&"transcription_jobs".to_string()));
            assert!(tables.contains(&"transcript_events".to_string()));
            assert!(tables.contains(&"transcription_runs".to_string()));
            assert!(tables.contains(&"schema_migrations".to_string()));

            let version: i64 =
                conn.query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                    row.get(0)
                })?;
            assert_eq!(version, 2);
            Ok(())
        })
        .expect("query");

        drop(db);
        let db2 = Db::open(&path).expect("reopen");
        db2.with_conn(|conn| {
            let count: i64 =
                conn.query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
                    row.get(0)
                })?;
            assert_eq!(count, 2);
            Ok(())
        })
        .expect("second open is idempotent");
    }
}

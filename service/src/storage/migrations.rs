use rusqlite::Connection;

use crate::error::Result;

const MIGRATION_001: &str = include_str!("migrations/001_initial.sql");
const MIGRATION_002: &str = include_str!("migrations/002_session_title.sql");

pub fn apply(conn: &mut Connection) -> Result<()> {
    let table_exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations')",
        [],
        |row| row.get(0),
    )?;

    if !table_exists {
        let tx = conn.transaction()?;
        tx.execute_batch(MIGRATION_001)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (1, datetime('now'))",
            [],
        )?;
        tx.commit()?;
    } else {
        let has_v1: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE version = 1)",
            [],
            |row| row.get(0),
        )?;
        if !has_v1 {
            let tx = conn.transaction()?;
            tx.execute_batch(MIGRATION_001)?;
            tx.execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (1, datetime('now'))",
                [],
            )?;
            tx.commit()?;
        }
    }

    let has_v2: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE version = 2)",
        [],
        |row| row.get(0),
    )?;
    if !has_v2 {
        let tx = conn.transaction()?;
        tx.execute_batch(MIGRATION_002)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (2, datetime('now'))",
            [],
        )?;
        tx.commit()?;
    }
    Ok(())
}

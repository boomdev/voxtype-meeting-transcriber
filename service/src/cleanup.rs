use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::error::Result;
use crate::paths::PathResolver;
use crate::storage::Db;

struct EligibleSession {
    id: String,
    files: Vec<PathBuf>,
    bytes: u64,
}

pub fn cmd_cleanup(apply: bool) -> Result<()> {
    let paths = PathResolver::from_env()?;
    let db = Db::open(paths.db_path())?;
    let eligible = db.with_conn(|conn| list_eligible(conn, &paths))?;
    if eligible.is_empty() {
        println!("No fully transcribed completed sessions are eligible for cleanup.");
        return Ok(());
    }
    let mut files = 0usize;
    let mut bytes = 0u64;
    for session in &eligible {
        println!(
            "session {} — {} files, {}",
            session.id,
            session.files.len(),
            crate::disk::format_bytes(session.bytes)
        );
        files += session.files.len();
        bytes = bytes.saturating_add(session.bytes);
    }
    if !apply {
        println!(
            "Would delete {files} files ({}) across {} sessions. Re-run with --apply to delete.",
            crate::disk::format_bytes(bytes),
            eligible.len()
        );
        return Ok(());
    }
    println!(
        "Deleting {files} files ({}) across {} sessions.",
        crate::disk::format_bytes(bytes),
        eligible.len()
    );
    db.with_conn_mut(|conn| {
        for session in &eligible {
            apply_session(conn, &paths, session)?;
        }
        Ok(())
    })?;
    Ok(())
}

pub fn preview_stats(paths: &PathResolver) -> Result<(usize, u64, usize)> {
    let db = Db::open(paths.db_path())?;
    db.with_conn(|conn| {
        let eligible = list_eligible(conn, paths)?;
        let files = eligible.iter().map(|session| session.files.len()).sum();
        let bytes = eligible
            .iter()
            .fold(0u64, |acc, session| acc.saturating_add(session.bytes));
        Ok((files, bytes, eligible.len()))
    })
}

pub fn apply_all(paths: &PathResolver) -> Result<(usize, u64, usize)> {
    let db = Db::open(paths.db_path())?;
    let eligible = db.with_conn(|conn| list_eligible(conn, paths))?;
    let files = eligible.iter().map(|session| session.files.len()).sum();
    let bytes = eligible
        .iter()
        .fold(0u64, |acc, session| acc.saturating_add(session.bytes));
    let sessions = eligible.len();
    db.with_conn_mut(|conn| {
        for session in &eligible {
            apply_session(conn, paths, session)?;
        }
        Ok(())
    })?;
    Ok((files, bytes, sessions))
}

fn list_eligible(conn: &Connection, paths: &PathResolver) -> Result<Vec<EligibleSession>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM sessions
         WHERE state = 'completed'
           AND NOT EXISTS (
             SELECT 1 FROM audio_chunks c
             WHERE c.session_id = sessions.id
               AND NOT EXISTS (
                 SELECT 1 FROM transcript_events e WHERE e.audio_chunk_id = c.id
               )
           )",
    )?;
    let ids = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut eligible = Vec::new();
    for id in ids {
        let dir = paths.session_dir(&id);
        let (files, bytes) = session_tree_stats(&dir)?;
        eligible.push(EligibleSession { id, files, bytes });
    }
    Ok(eligible)
}

fn session_tree_stats(dir: &Path) -> Result<(Vec<PathBuf>, u64)> {
    let mut files = Vec::new();
    let mut bytes = 0u64;
    if !dir.exists() {
        return Ok((files, bytes));
    }
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in fs::read_dir(&current)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                bytes = bytes.saturating_add(entry.metadata()?.len());
                files.push(path);
            }
        }
    }
    Ok((files, bytes))
}

fn apply_session(
    conn: &mut Connection,
    paths: &PathResolver,
    session: &EligibleSession,
) -> Result<()> {
    let dir = paths.session_dir(&session.id);
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    let tx = conn.transaction()?;
    tx.execute(
        "DELETE FROM transcript_events WHERE session_id = ?1",
        rusqlite::params![session.id],
    )?;
    tx.execute(
        "DELETE FROM transcription_jobs WHERE audio_chunk_id IN (
            SELECT id FROM audio_chunks WHERE session_id = ?1
         )",
        rusqlite::params![session.id],
    )?;
    tx.execute(
        "DELETE FROM audio_chunks WHERE session_id = ?1",
        rusqlite::params![session.id],
    )?;
    tx.execute(
        "DELETE FROM transcription_runs WHERE session_id = ?1",
        rusqlite::params![session.id],
    )?;
    tx.execute(
        "DELETE FROM sessions WHERE id = ?1",
        rusqlite::params![session.id],
    )?;
    tx.commit()?;
    tracing::info!(session_id = %session.id, "cleaned up completed session");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::list_eligible;
    use crate::audio::AudioSource;
    use crate::config::ProviderKind;
    use crate::paths::PathResolver;
    use crate::storage::sessions::insert_running_session;
    use crate::storage::types::SessionState;
    use crate::storage::Db;
    use chrono::Utc;
    use tempfile::tempdir;
    use uuid::Uuid;

    fn test_paths(root: &std::path::Path) -> PathResolver {
        PathResolver::from_parts(
            root.to_path_buf(),
            Some(root.join("config")),
            Some(root.join("data")),
            Some(root.join("runtime")),
            1000,
        )
    }

    fn seed_session(
        db: &Db,
        paths: &PathResolver,
        state: SessionState,
        with_event: bool,
    ) -> String {
        let session_id = Uuid::new_v4().to_string();
        db.with_conn(|conn| insert_running_session(conn, &session_id, None, None, None))
            .unwrap();
        let run = db
            .with_conn(|conn| {
                crate::storage::sessions::insert_run(conn, &session_id, ProviderKind::Openai, "m")
            })
            .unwrap();
        let mic_dir = paths.session_dir(&session_id).join("audio/mic");
        crate::paths::ensure_dir(&mic_dir).unwrap();
        let started = Utc::now();
        crate::storage::persist_recorded_chunk(
            db,
            crate::storage::PersistChunk {
                session_id: &session_id,
                run_id: &run.id,
                provider: ProviderKind::Openai,
                model: "m",
                dir: &mic_dir,
                source: AudioSource::Mic,
                started_at: started,
                ended_at: started + chrono::Duration::seconds(1),
                samples: &[0; 1600],
            },
        )
        .unwrap();
        if with_event {
            let job_id: String = db
                .with_conn(|conn| {
                    Ok(conn.query_row(
                        "SELECT j.id FROM transcription_jobs j
                         JOIN audio_chunks c ON c.id = j.audio_chunk_id
                         WHERE c.session_id = ?1",
                        rusqlite::params![session_id],
                        |row| row.get(0),
                    )?)
                })
                .unwrap();
            let result = crate::transcription::TranscriptionResult {
                text: "hi".into(),
                provider: ProviderKind::Openai,
                model: "m".into(),
                provider_metadata: None,
            };
            db.with_conn_mut(|conn| {
                crate::storage::events::record_transcription_success(conn, &job_id, &result)
            })
            .unwrap();
        }
        db.with_conn(|conn| {
            crate::storage::sessions::set_session_state(conn, &session_id, state, Some("now"))
        })
        .unwrap();
        session_id
    }

    #[test]
    fn dry_run_lists_only_completed_with_events() {
        let root = tempdir().unwrap();
        let paths = test_paths(root.path());
        crate::paths::ensure_dir(&paths.sessions_dir()).unwrap();
        let db = Db::open(paths.db_path()).unwrap();
        let eligible_id = seed_session(&db, &paths, SessionState::Completed, true);
        let _interrupted = seed_session(&db, &paths, SessionState::Interrupted, true);
        let _missing_event = seed_session(&db, &paths, SessionState::Completed, false);
        let listed = db.with_conn(|conn| list_eligible(conn, &paths)).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, eligible_id);
    }

    #[test]
    fn apply_deletes_eligible_keeps_ineligible() {
        let root = tempdir().unwrap();
        let paths = test_paths(root.path());
        crate::paths::ensure_dir(&paths.sessions_dir()).unwrap();
        let db = Db::open(paths.db_path()).unwrap();
        let eligible_id = seed_session(&db, &paths, SessionState::Completed, true);
        let keep_id = seed_session(&db, &paths, SessionState::Completed, false);
        let eligible_dir = paths.session_dir(&eligible_id);
        let keep_dir = paths.session_dir(&keep_id);
        assert!(eligible_dir.exists());
        db.with_conn_mut(|conn| {
            let eligible = list_eligible(conn, &paths)?;
            for session in eligible {
                super::apply_session(conn, &paths, &session)?;
            }
            Ok(())
        })
        .unwrap();
        assert!(!eligible_dir.exists());
        assert!(keep_dir.exists());
        let remaining: i64 = db
            .with_conn(|conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM sessions WHERE id = ?1",
                    rusqlite::params![keep_id],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(remaining, 1);
    }
}

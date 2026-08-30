pub mod chunks;
mod db;
pub mod events;
pub mod jobs;
mod migrations;
pub mod recovery;
pub mod sessions;
pub mod types;

pub use db::Db;
pub use types::{JobState, SessionState};

use crate::audio::AudioSource;
use crate::config::ProviderKind;
use crate::encode;
use crate::error::Result;
use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};

pub struct PersistChunk<'a> {
    pub session_id: &'a str,
    pub run_id: &'a str,
    pub provider: ProviderKind,
    pub model: &'a str,
    pub dir: &'a Path,
    pub source: AudioSource,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub samples: &'a [i16],
}

pub fn persist_recorded_chunk(db: &Db, chunk: PersistChunk<'_>) -> Result<PathBuf> {
    let sequence =
        db.with_conn(|conn| chunks::next_sequence(conn, chunk.session_id, chunk.source))?;
    let path = encode::persist_chunk(chunk.dir, sequence, chunk.started_at, chunk.samples)?;
    let duration_ms = (chunk.samples.len() as i64 * 1000) / 16_000;
    let file_path = path.to_string_lossy().into_owned();
    db.with_conn_mut(|conn| {
        chunks::insert_chunk_and_job(
            conn,
            chunks::NewAudioChunk {
                session_id: chunk.session_id,
                run_id: chunk.run_id,
                source: chunk.source,
                sequence,
                started_at: chunk.started_at,
                ended_at: chunk.ended_at,
                file_path: &file_path,
                duration_ms,
                provider: chunk.provider,
                model: chunk.model,
            },
        )
    })?;
    Ok(path)
}

pub fn source_from_str(value: &str) -> Result<AudioSource> {
    match value {
        "mic" => Ok(AudioSource::Mic),
        "system" => Ok(AudioSource::System),
        other => Err(crate::error::AppError::other(format!(
            "invalid audio source '{other}'"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::AudioSource;
    use crate::config::Config;
    use crate::encode;
    use crate::paths::PathResolver;
    use crate::storage::sessions::{get_session, insert_running_session, set_session_state};
    use crate::storage::types::{JobState, SessionState};
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

    #[test]
    fn persist_creates_file_before_job_and_survives_recovery() {
        let root = tempdir().unwrap();
        let paths = test_paths(root.path());
        crate::paths::ensure_dir(&paths.sessions_dir()).unwrap();
        let db = Db::open(paths.db_path()).unwrap();
        let config = Config::default();
        let session_id = Uuid::new_v4().to_string();
        let session_dir = paths.session_dir(&session_id);
        let mic_dir = session_dir.join("audio/mic");
        crate::paths::ensure_dir(&mic_dir).unwrap();

        db.with_conn(|conn| insert_running_session(conn, &session_id, None, None, None))
            .unwrap();
        let run = db
            .with_conn(|conn| {
                crate::storage::sessions::insert_run(
                    conn,
                    &session_id,
                    config.transcription.provider,
                    &config.transcription.model,
                )
            })
            .unwrap();

        let samples = vec![0i16; 16_000];
        let started = Utc::now();
        let ended = started + chrono::Duration::seconds(1);
        let path = persist_recorded_chunk(
            &db,
            PersistChunk {
                session_id: &session_id,
                run_id: &run.id,
                provider: config.transcription.provider,
                model: &config.transcription.model,
                dir: &mic_dir,
                source: AudioSource::Mic,
                started_at: started,
                ended_at: ended,
                samples: &samples,
            },
        )
        .unwrap();
        assert!(path.exists());
        let chunks = db
            .with_conn(|conn| chunks::list_chunks_for_session(conn, &session_id))
            .unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].sequence, 1);
        assert!((chunks[0].duration_ms - 1000).abs() <= 1);
        let job_state: String = db
            .with_conn(|conn| {
                Ok(conn.query_row(
                    "SELECT state FROM transcription_jobs WHERE audio_chunk_id = ?1",
                    rusqlite::params![chunks[0].id],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(job_state, "pending");

        // Two mic chunks increment sequence; system has its own sequence.
        let path2 = persist_recorded_chunk(
            &db,
            PersistChunk {
                session_id: &session_id,
                run_id: &run.id,
                provider: config.transcription.provider,
                model: &config.transcription.model,
                dir: &mic_dir,
                source: AudioSource::Mic,
                started_at: started,
                ended_at: ended,
                samples: &samples,
            },
        )
        .unwrap();
        assert_ne!(path, path2);
        let sys_dir = session_dir.join("audio/system");
        crate::paths::ensure_dir(&sys_dir).unwrap();
        persist_recorded_chunk(
            &db,
            PersistChunk {
                session_id: &session_id,
                run_id: &run.id,
                provider: config.transcription.provider,
                model: &config.transcription.model,
                dir: &sys_dir,
                source: AudioSource::System,
                started_at: started,
                ended_at: ended,
                samples: &samples,
            },
        )
        .unwrap();
        let chunks = db
            .with_conn(|conn| chunks::list_chunks_for_session(conn, &session_id))
            .unwrap();
        let mic_seq: Vec<u64> = chunks
            .iter()
            .filter(|c| c.source == AudioSource::Mic)
            .map(|c| c.sequence)
            .collect();
        let sys_seq: Vec<u64> = chunks
            .iter()
            .filter(|c| c.source == AudioSource::System)
            .map(|c| c.sequence)
            .collect();
        assert_eq!(mic_seq, vec![1, 2]);
        assert_eq!(sys_seq, vec![1]);
    }

    #[test]
    fn recovery_imports_orphan_flac_and_interrupts_running() {
        let root = tempdir().unwrap();
        let paths = test_paths(root.path());
        crate::paths::ensure_dir(&paths.sessions_dir()).unwrap();
        let db = Db::open(paths.db_path()).unwrap();
        let config = Config::default();
        let session_id = Uuid::new_v4().to_string();
        db.with_conn(|conn| insert_running_session(conn, &session_id, None, None, None))
            .unwrap();

        let orphan_session = Uuid::new_v4().to_string();
        let mic_dir = paths.session_dir(&orphan_session).join("audio/mic");
        crate::paths::ensure_dir(&mic_dir).unwrap();
        let samples = vec![0i16; 1600];
        let started = chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 8, 17, 12, 32, 4).unwrap()
            + chrono::Duration::milliseconds(120);
        encode::persist_chunk(&mic_dir, 12, started, &samples).unwrap();

        db.with_conn_mut(|conn| recovery::recover_on_startup(conn, &paths, &config))
            .unwrap();

        let recovered = db
            .with_conn(|conn| crate::storage::sessions::get_session(conn, &session_id))
            .unwrap()
            .unwrap();
        assert_eq!(recovered.state, SessionState::Interrupted);

        let orphan = db
            .with_conn(|conn| crate::storage::sessions::get_session(conn, &orphan_session))
            .unwrap()
            .unwrap();
        assert_eq!(orphan.state, SessionState::Interrupted);
        let chunks = db
            .with_conn(|conn| chunks::list_chunks_for_session(conn, &orphan_session))
            .unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].sequence, 12);
        let pending: i64 = db
            .with_conn(|conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM transcription_jobs WHERE state = 'pending'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert!(pending >= 1);
    }

    #[test]
    fn processing_job_without_event_becomes_pending() {
        let root = tempdir().unwrap();
        let paths = test_paths(root.path());
        let db = Db::open(paths.db_path()).unwrap();
        let config = Config::default();
        let session_id = Uuid::new_v4().to_string();
        db.with_conn(|conn| insert_running_session(conn, &session_id, None, None, None))
            .unwrap();
        let run = db
            .with_conn(|conn| {
                crate::storage::sessions::insert_run(
                    conn,
                    &session_id,
                    config.transcription.provider,
                    "m",
                )
            })
            .unwrap();
        let mic_dir = paths.session_dir(&session_id).join("audio/mic");
        crate::paths::ensure_dir(&mic_dir).unwrap();
        let started = Utc::now();
        persist_recorded_chunk(
            &db,
            PersistChunk {
                session_id: &session_id,
                run_id: &run.id,
                provider: config.transcription.provider,
                model: "m",
                dir: &mic_dir,
                source: AudioSource::Mic,
                started_at: started,
                ended_at: started + chrono::Duration::seconds(1),
                samples: &[0; 1600],
            },
        )
        .unwrap();
        db.with_conn(|conn| {
            conn.execute("UPDATE transcription_jobs SET state = 'processing'", [])?;
            Ok(())
        })
        .unwrap();
        db.with_conn_mut(|conn| recovery::recover_on_startup(conn, &paths, &config))
            .unwrap();
        let pending: i64 = db
            .with_conn(|conn| jobs::count_by_state(conn, JobState::Pending))
            .unwrap();
        assert_eq!(pending, 1);
    }

    #[test]
    fn session_state_transitions() {
        let root = tempdir().unwrap();
        let paths = test_paths(root.path());
        let db = Db::open(paths.db_path()).unwrap();
        let session_id = Uuid::new_v4().to_string();
        db.with_conn(|conn| insert_running_session(conn, &session_id, None, None, None))
            .unwrap();
        let running = db
            .with_conn(|conn| get_session(conn, &session_id))
            .unwrap()
            .unwrap();
        assert_eq!(running.state, SessionState::Running);
        db.with_conn(|conn| {
            set_session_state(conn, &session_id, SessionState::Interrupted, Some("now"))
        })
        .unwrap();
        let interrupted = db
            .with_conn(|conn| get_session(conn, &session_id))
            .unwrap()
            .unwrap();
        assert_eq!(interrupted.state, SessionState::Interrupted);
        db.with_conn(|conn| {
            set_session_state(conn, &session_id, SessionState::Completed, Some("later"))
        })
        .unwrap();
        let completed = db
            .with_conn(|conn| get_session(conn, &session_id))
            .unwrap()
            .unwrap();
        assert_eq!(completed.state, SessionState::Completed);
    }
}

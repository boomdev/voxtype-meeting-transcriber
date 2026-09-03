//! Integration tests for the durable capture → job → fake transcription pipeline.

use chrono::{TimeZone, Utc};
use tempfile::tempdir;
use uuid::Uuid;
use voxtype_meeting_service::audio::AudioSource;
use voxtype_meeting_service::config::Config;
use voxtype_meeting_service::paths::PathResolver;
use voxtype_meeting_service::storage::chunks::list_chunks_for_session;
use voxtype_meeting_service::storage::events::record_transcription_success;
use voxtype_meeting_service::storage::jobs::{get_job, reset_processing_without_event};
use voxtype_meeting_service::storage::sessions::insert_running_session;
use voxtype_meeting_service::storage::types::JobState;
use voxtype_meeting_service::storage::Db;
use voxtype_meeting_service::storage::PersistChunk;
use voxtype_meeting_service::timeutil::parse_rfc3339;
use voxtype_meeting_service::transcript::regenerate_session_transcripts;
use voxtype_meeting_service::transcription::fake::FakeTranscriptionProvider;
use voxtype_meeting_service::transcription::worker::process_job_with_provider;
use voxtype_meeting_service::transcription::TranscriptionResult;

fn test_paths(root: &std::path::Path) -> PathResolver {
    PathResolver::from_parts(
        root.to_path_buf(),
        Some(root.join("config")),
        Some(root.join("data")),
        Some(root.join("runtime")),
        1000,
    )
}

#[tokio::test]
async fn fake_pipeline_persists_then_transcribes() {
    let root = tempdir().unwrap();
    let paths = test_paths(root.path());
    voxtype_meeting_service::paths::ensure_dir(&paths.sessions_dir()).unwrap();
    let db = Db::open(paths.db_path()).unwrap();
    let config = Config::default();
    let session_id = Uuid::new_v4().to_string();
    let mic_dir = paths.session_dir(&session_id).join("audio/mic");
    voxtype_meeting_service::paths::ensure_dir(&mic_dir).unwrap();
    db.with_conn(|conn| insert_running_session(conn, &session_id, None, None, None))
        .unwrap();
    let run = db
        .with_conn(|conn| {
            voxtype_meeting_service::storage::sessions::insert_run(
                conn,
                &session_id,
                config.transcription.provider,
                &config.transcription.model,
            )
        })
        .unwrap();
    let started = Utc::now();
    voxtype_meeting_service::storage::persist_recorded_chunk(
        &db,
        PersistChunk {
            session_id: &session_id,
            run_id: &run.id,
            provider: config.transcription.provider,
            model: &config.transcription.model,
            dir: &mic_dir,
            source: AudioSource::Mic,
            started_at: started,
            ended_at: started + chrono::Duration::seconds(1),
            samples: &vec![0i16; 16_000],
        },
    )
    .unwrap();
    let chunks = db
        .with_conn(|conn| list_chunks_for_session(conn, &session_id))
        .unwrap();
    assert_eq!(chunks.len(), 1);
    assert!(std::path::Path::new(&chunks[0].file_path).exists());
    let job_id: String = db
        .with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT id FROM transcription_jobs WHERE audio_chunk_id = ?1",
                rusqlite::params![chunks[0].id],
                |row| row.get(0),
            )?)
        })
        .unwrap();

    let provider = FakeTranscriptionProvider::always("hello from fake");
    process_job_with_provider(&db, &paths, &job_id, provider.as_ref())
        .await
        .unwrap();

    let md = std::fs::read_to_string(paths.session_dir(&session_id).join("transcript.md")).unwrap();
    assert!(md.contains("hello from fake"));
    let jsonl =
        std::fs::read_to_string(paths.session_dir(&session_id).join("transcript.jsonl")).unwrap();
    assert!(jsonl.contains("hello from fake"));
    assert!(jsonl.contains("\"source\":\"mic\""));
}

#[tokio::test]
async fn failed_transcription_keeps_audio_and_pending_job() {
    let root = tempdir().unwrap();
    let paths = test_paths(root.path());
    voxtype_meeting_service::paths::ensure_dir(&paths.sessions_dir()).unwrap();
    let db = Db::open(paths.db_path()).unwrap();
    let config = Config::default();
    let session_id = Uuid::new_v4().to_string();
    let mic_dir = paths.session_dir(&session_id).join("audio/mic");
    voxtype_meeting_service::paths::ensure_dir(&mic_dir).unwrap();
    db.with_conn(|conn| insert_running_session(conn, &session_id, None, None, None))
        .unwrap();
    let run = db
        .with_conn(|conn| {
            voxtype_meeting_service::storage::sessions::insert_run(
                conn,
                &session_id,
                config.transcription.provider,
                "m",
            )
        })
        .unwrap();
    let started = Utc::now();
    voxtype_meeting_service::storage::persist_recorded_chunk(
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
    let (job_id, file_path): (String, String) = db
        .with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT j.id, c.file_path FROM transcription_jobs j JOIN audio_chunks c ON c.id = j.audio_chunk_id",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?)
        })
        .unwrap();
    let provider = FakeTranscriptionProvider::fail_then_succeed(1, "nope");
    let err = process_job_with_provider(&db, &paths, &job_id, provider.as_ref())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("fake provider failure"));
    assert!(std::path::Path::new(&file_path).exists());
    let job = db
        .with_conn(|conn| get_job(conn, &job_id))
        .unwrap()
        .unwrap();
    assert_eq!(job.state, JobState::Pending);
    assert_eq!(job.attempt_count, 1);
    assert!(job.last_error.is_some());
}

#[test]
fn processing_job_recovers_to_pending() {
    let root = tempdir().unwrap();
    let paths = test_paths(root.path());
    let db = Db::open(paths.db_path()).unwrap();
    let session_id = Uuid::new_v4().to_string();
    db.with_conn(|conn| insert_running_session(conn, &session_id, None, None, None))
        .unwrap();
    let run = db
        .with_conn(|conn| {
            voxtype_meeting_service::storage::sessions::insert_run(
                conn,
                &session_id,
                voxtype_meeting_service::config::ProviderKind::Voxtype,
                "m",
            )
        })
        .unwrap();
    let mic_dir = paths.session_dir(&session_id).join("audio/mic");
    voxtype_meeting_service::paths::ensure_dir(&mic_dir).unwrap();
    let started = Utc::now();
    voxtype_meeting_service::storage::persist_recorded_chunk(
        &db,
        PersistChunk {
            session_id: &session_id,
            run_id: &run.id,
            provider: voxtype_meeting_service::config::ProviderKind::Voxtype,
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
    db.with_conn(reset_processing_without_event).unwrap();
    let pending: i64 = db
        .with_conn(|conn| {
            voxtype_meeting_service::storage::jobs::count_by_state(conn, JobState::Pending)
        })
        .unwrap();
    assert_eq!(pending, 1);
}

#[test]
fn duplicate_success_inserts_one_event() {
    let root = tempdir().unwrap();
    let paths = test_paths(root.path());
    let db = Db::open(paths.db_path()).unwrap();
    let session_id = Uuid::new_v4().to_string();
    db.with_conn(|conn| insert_running_session(conn, &session_id, None, None, None))
        .unwrap();
    let run = db
        .with_conn(|conn| {
            voxtype_meeting_service::storage::sessions::insert_run(
                conn,
                &session_id,
                voxtype_meeting_service::config::ProviderKind::Voxtype,
                "m",
            )
        })
        .unwrap();
    let mic_dir = paths.session_dir(&session_id).join("audio/mic");
    voxtype_meeting_service::paths::ensure_dir(&mic_dir).unwrap();
    let started = Utc::now();
    voxtype_meeting_service::storage::persist_recorded_chunk(
        &db,
        PersistChunk {
            session_id: &session_id,
            run_id: &run.id,
            provider: voxtype_meeting_service::config::ProviderKind::Voxtype,
            model: "m",
            dir: &mic_dir,
            source: AudioSource::Mic,
            started_at: started,
            ended_at: started + chrono::Duration::seconds(1),
            samples: &[0; 1600],
        },
    )
    .unwrap();
    let job_id: String = db
        .with_conn(|conn| {
            Ok(conn.query_row("SELECT id FROM transcription_jobs", [], |row| row.get(0))?)
        })
        .unwrap();
    let result = TranscriptionResult {
        text: "once".into(),
        provider: voxtype_meeting_service::config::ProviderKind::Voxtype,
        model: "m".into(),
        provider_metadata: None,
    };
    db.with_conn_mut(|conn| record_transcription_success(conn, &job_id, &result))
        .unwrap();
    db.with_conn_mut(|conn| record_transcription_success(conn, &job_id, &result))
        .unwrap();
    let count: i64 = db
        .with_conn(|conn| {
            Ok(
                conn.query_row("SELECT COUNT(*) FROM transcript_events", [], |row| {
                    row.get(0)
                })?,
            )
        })
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn delayed_speech_shifts_event_off_chunk_start() {
    let root = tempdir().unwrap();
    let paths = test_paths(root.path());
    voxtype_meeting_service::paths::ensure_dir(&paths.sessions_dir()).unwrap();
    let db = Db::open(paths.db_path()).unwrap();
    let session_id = Uuid::new_v4().to_string();
    db.with_conn(|conn| insert_running_session(conn, &session_id, None, None, None))
        .unwrap();
    let run = db
        .with_conn(|conn| {
            voxtype_meeting_service::storage::sessions::insert_run(
                conn,
                &session_id,
                voxtype_meeting_service::config::ProviderKind::Voxtype,
                "m",
            )
        })
        .unwrap();
    let mic_dir = paths.session_dir(&session_id).join("audio/mic");
    voxtype_meeting_service::paths::ensure_dir(&mic_dir).unwrap();
    let started = Utc.with_ymd_and_hms(2026, 8, 17, 18, 45, 39).unwrap();
    let mut samples = vec![0i16; 16_000];
    samples.extend((0..3_200).map(|i| {
        let t = i as f32 / 16_000.0;
        (8_000.0 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()) as i16
    }));
    voxtype_meeting_service::storage::persist_recorded_chunk(
        &db,
        PersistChunk {
            session_id: &session_id,
            run_id: &run.id,
            provider: voxtype_meeting_service::config::ProviderKind::Voxtype,
            model: "m",
            dir: &mic_dir,
            source: AudioSource::Mic,
            started_at: started,
            ended_at: started + chrono::Duration::milliseconds(1_200),
            samples: &samples,
        },
    )
    .unwrap();
    let job_id: String = db
        .with_conn(|conn| {
            Ok(conn.query_row("SELECT id FROM transcription_jobs", [], |row| row.get(0))?)
        })
        .unwrap();
    let result = TranscriptionResult {
        text: "Voilà, ça veut pas".into(),
        provider: voxtype_meeting_service::config::ProviderKind::Voxtype,
        model: "m".into(),
        provider_metadata: None,
    };
    db.with_conn_mut(|conn| record_transcription_success(conn, &job_id, &result))
        .unwrap();

    let event_started: String = db
        .with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT started_at FROM transcript_events WHERE is_canonical = 1",
                [],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    let offset = (parse_rfc3339(&event_started).unwrap() - started).num_milliseconds();
    assert!(
        (900..=1_050).contains(&offset),
        "speech at 1s should move the event off the chunk start, got {offset}ms ({event_started})"
    );

    db.with_conn(|conn| {
        conn.execute(
            "UPDATE transcript_events SET
                started_at = (SELECT started_at FROM audio_chunks WHERE id = audio_chunk_id),
                ended_at = (SELECT ended_at FROM audio_chunks WHERE id = audio_chunk_id)",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    regenerate_session_transcripts(&db, &session_id, &paths.session_dir(&session_id), false)
        .unwrap();
    let rebuilt: String = db
        .with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT started_at FROM transcript_events WHERE is_canonical = 1",
                [],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    let rebuilt_offset = (parse_rfc3339(&rebuilt).unwrap() - started).num_milliseconds();
    assert!(
        (900..=1_050).contains(&rebuilt_offset),
        "rebuild should realign from the FLAC using chunk times, got {rebuilt_offset}ms"
    );
}

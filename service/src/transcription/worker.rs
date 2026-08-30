use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::error::Result;
use crate::paths::PathResolver;
use crate::runtime::{emit, EventBus, UiEvent};
use crate::storage::chunks::get_chunk;
use crate::storage::events::{claim_due_jobs, mark_job_retry, record_transcription_success};
use crate::storage::Db;
use crate::transcription::retry::retry_delay;
use crate::transcription::{AudioChunkRef, TranscriptionProvider};

pub async fn run_workers(
    db_path: PathBuf,
    paths: PathResolver,
    config: Arc<RwLock<Config>>,
    mut wake: mpsc::Receiver<()>,
    shutdown: CancellationToken,
    events: EventBus,
) -> Result<()> {
    let db = Db::open(db_path)?;
    let initial = config
        .read()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    let limit = initial.transcription.max_concurrent_jobs.max(1);
    let semaphore = Arc::new(Semaphore::new(limit as usize));
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = interval.tick() => {}
            _ = wake.recv() => {}
        }
        if shutdown.is_cancelled() {
            break;
        }
        let ids = match db.with_conn_mut(|conn| claim_due_jobs(conn, limit)) {
            Ok(ids) => ids,
            Err(error) => {
                tracing::error!(error = %error, "failed to claim transcription jobs");
                continue;
            }
        };
        for job_id in ids {
            let permit = match semaphore.clone().acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => break,
            };
            let db_path = db.path().to_path_buf();
            let config = config.clone();
            let paths = paths.clone();
            let shutdown = shutdown.clone();
            let events = events.clone();
            tokio::spawn(async move {
                let _permit = permit;
                if shutdown.is_cancelled() {
                    return;
                }
                match process_job(db_path, paths, config, job_id.clone()).await {
                    Ok(session_id) => {
                        emit(
                            &events,
                            UiEvent::new("transcription_job_updated").session(session_id),
                        );
                    }
                    Err(error) => {
                        tracing::error!(job_id, error = %error, "transcription worker error");
                    }
                }
            });
        }
    }
    Ok(())
}

async fn process_job(
    db_path: PathBuf,
    paths: PathResolver,
    config: Arc<RwLock<Config>>,
    job_id: String,
) -> Result<String> {
    let config = config
        .read()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    let db = Db::open(db_path)?;
    let (provider_kind, model, chunk_id) =
        db.with_conn(|conn| crate::storage::events::get_job_provider(conn, &job_id))?;
    let chunk = db
        .with_conn(|conn| get_chunk(conn, &chunk_id))?
        .ok_or_else(|| {
            crate::error::AppError::transcription(format!(
                "audio chunk {chunk_id} missing for job {job_id}"
            ))
        })?;
    let session_id = chunk.session_id.clone();
    tracing::info!(
        job_id,
        chunk_id = %chunk.id,
        provider = %provider_kind,
        "transcription job start"
    );
    let provider: Box<dyn TranscriptionProvider> =
        match crate::transcription::provider_for_job(provider_kind, model.as_deref(), &config) {
            Ok(provider) => provider,
            Err(error) => {
                schedule_retry(&db, &job_id, &error.to_string())?;
                return Ok(session_id);
            }
        };
    let chunk_ref = AudioChunkRef::from_record(&chunk)?;
    match provider.transcribe(&chunk_ref).await {
        Ok(result) => {
            let inserted =
                db.with_conn_mut(|conn| record_transcription_success(conn, &job_id, &result))?;
            tracing::info!(job_id, inserted, "transcription job success");
            let session_dir = paths.session_dir(&session_id);
            if let Err(error) = crate::transcript::regenerate_session_transcripts(
                &db,
                &session_id,
                &session_dir,
                config.transcript.omit_single_source_headers,
            ) {
                tracing::error!(session_id, error = %error, "transcript regeneration failed");
            }
            if !session_dir.join("retain-audio").exists() {
                if let Err(error) = std::fs::remove_file(&chunk.file_path) {
                    if error.kind() != std::io::ErrorKind::NotFound {
                        tracing::warn!(path = %chunk.file_path, error = %error, "could not delete transcribed audio");
                    }
                }
            }
        }
        Err(error) => {
            tracing::warn!(job_id, error = %error, "transcription job failure");
            schedule_retry(&db, &job_id, &error.to_string())?;
        }
    }
    Ok(session_id)
}

fn schedule_retry(db: &Db, job_id: &str, error: &str) -> Result<()> {
    db.with_conn(|conn| {
        let attempt: i64 = conn.query_row(
            "SELECT attempt_count FROM transcription_jobs WHERE id = ?1",
            rusqlite::params![job_id],
            |row| row.get(0),
        )?;
        let next_attempt = (attempt as u32).saturating_add(1);
        let delay = retry_delay(next_attempt);
        tracing::info!(
            job_id,
            next_attempt,
            delay_secs = delay.as_secs(),
            "retry scheduled"
        );
        mark_job_retry(conn, job_id, error, delay)
    })
}

pub async fn process_job_with_provider(
    db: &Db,
    paths: &PathResolver,
    job_id: &str,
    provider: &dyn TranscriptionProvider,
) -> Result<()> {
    let chunk_id = db.with_conn(|conn| {
        let (_, _, chunk_id) = crate::storage::events::get_job_provider(conn, job_id)?;
        Ok(chunk_id)
    })?;
    let chunk = db
        .with_conn(|conn| get_chunk(conn, &chunk_id))?
        .ok_or_else(|| {
            crate::error::AppError::transcription(format!("audio chunk {chunk_id} missing"))
        })?;
    let chunk_ref = AudioChunkRef::from_record(&chunk)?;
    match provider.transcribe(&chunk_ref).await {
        Ok(result) => {
            db.with_conn_mut(|conn| record_transcription_success(conn, job_id, &result))?;
            crate::transcript::regenerate_session_transcripts(
                db,
                &chunk.session_id,
                &paths.session_dir(&chunk.session_id),
                true,
            )?;
        }
        Err(error) => {
            schedule_retry(db, job_id, &error.to_string())?;
            return Err(error);
        }
    }
    Ok(())
}

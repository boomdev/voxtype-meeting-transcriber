use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::error::{AppError, Result};
use crate::paths::{self, PathResolver};
use crate::runtime::{
    emit, event_bus, lock_status, AudioServerHealth, EventBus, SourceUiState, UiEvent,
    WORKER_SHUTDOWN_GRACE_SECS,
};
use crate::storage::Db;
use crate::timeutil::parse_rfc3339;

pub struct StopOutcome {
    pub session_id: String,
    pub duration_secs: u64,
    pub transcription_pending: bool,
}

pub struct StartRequest {
    pub language: Option<String>,
    pub reply: oneshot::Sender<Result<String>>,
}

#[derive(Clone)]
pub struct RecordingChannels {
    pub start: mpsc::Sender<StartRequest>,
    pub stop: mpsc::Sender<oneshot::Sender<Result<StopOutcome>>>,
}

pub async fn cmd_run() -> Result<()> {
    let paths = PathResolver::from_env()?;
    let config = Config::load_or_write_default(&paths)?;
    paths::ensure_dir(&paths.data_dir())?;
    paths::ensure_dir(&paths.sessions_dir())?;
    crate::disk::assert_enough_space(&paths.data_dir(), config.general.minimum_free_space_mb)?;

    let db = Db::open(paths.db_path())?;
    db.with_conn_mut(|conn| crate::storage::recovery::recover_on_startup(conn, &paths, &config))?;

    let process_shutdown = CancellationToken::new();
    crate::capture::spawn_shutdown_signals(process_shutdown.clone());

    let socket = crate::control::bind_or_fail(&paths).await?;
    let backend = crate::capture::connect_backend(&process_shutdown).await?;
    let status = Arc::new(std::sync::Mutex::new(crate::runtime::RuntimeStatus::new(
        config.transcription.provider,
    )));
    {
        let mut status = lock_status(&status);
        status.audio_server = AudioServerHealth::Available;
    }

    let shared_config = Arc::new(RwLock::new(config.clone()));
    let events = event_bus();
    let (wake_tx, wake_rx) = mpsc::channel::<()>(32);
    let (start_tx, mut start_rx) = mpsc::channel::<StartRequest>(4);
    let (stop_tx, mut stop_rx) = mpsc::channel::<oneshot::Sender<Result<StopOutcome>>>(4);

    let worker = {
        let db_path = db.path().to_path_buf();
        let paths = paths.clone();
        let live_config = shared_config.clone();
        let shutdown = process_shutdown.clone();
        let events = events.clone();
        tokio::spawn(async move {
            if let Err(error) = crate::transcription::worker::run_workers(
                db_path,
                paths,
                live_config,
                wake_rx,
                shutdown,
                events,
            )
            .await
            {
                tracing::error!(error = %error, "transcription worker stopped");
            }
        })
    };

    let extras = crate::control::ControlExtras {
        config: shared_config.clone(),
        events: events.clone(),
        recording: RecordingChannels {
            start: start_tx,
            stop: stop_tx,
        },
        backend: backend.clone(),
    };
    let control_task = {
        let paths = paths.clone();
        let db_path = db.path().to_path_buf();
        let status = status.clone();
        let process_shutdown = process_shutdown.clone();
        tokio::spawn(async move {
            if let Err(error) = crate::control::serve(
                socket,
                paths,
                db_path,
                status,
                process_shutdown,
                Some(extras),
            )
            .await
            {
                tracing::error!(error = %error, "control socket stopped");
            }
        })
    };

    tracing::info!("daemon idle; waiting for start_recording");
    println!("Voxtype Meeting Service daemon is running (idle).");

    loop {
        tokio::select! {
            _ = process_shutdown.cancelled() => break,
            Some(reply) = stop_rx.recv() => {
                let _ = reply.send(Err(AppError::control("no recording is active")));
            }
            Some(start) = start_rx.recv() => {
                if lock_status(&status).capture_active {
                    let _ = start.reply.send(Err(AppError::control(
                        "a recording is already active",
                    )));
                    continue;
                }
                let capture_stop = CancellationToken::new();
                let (started_tx, started_rx) = oneshot::channel();
                let session_result = crate::capture::run_recording_session(
                    backend.clone(),
                    &db,
                    &paths,
                    shared_config.clone(),
                    process_shutdown.clone(),
                    capture_stop.clone(),
                    status.clone(),
                    wake_tx.clone(),
                    events.clone(),
                    Some(started_tx),
                    start.language.clone(),
                );
                tokio::pin!(session_result);
                let session_id = tokio::select! {
                    started = started_rx => {
                        match started {
                            Ok(id) => {
                                emit(&events, UiEvent::new("recording_started").session(&id));
                                emit(&events, UiEvent::new("session_created").session(&id));
                                let _ = start.reply.send(Ok(id.clone()));
                                id
                            }
                            Err(_) => {
                                let _ = start.reply.send(Err(AppError::control(
                                    "recording failed to start",
                                )));
                                let _ = session_result.await;
                                continue;
                            }
                        }
                    }
                    result = &mut session_result => {
                        match result {
                            Ok(id) => {
                                let _ = start.reply.send(Ok(id.to_string()));
                                emit_stopped(&events, &db, &id.to_string(), false);
                                continue;
                            }
                            Err(error) => {
                                let _ = start.reply.send(Err(error));
                                continue;
                            }
                        }
                    }
                    _ = process_shutdown.cancelled() => {
                        capture_stop.cancel();
                        let _ = start.reply.send(Err(AppError::control("daemon is shutting down")));
                        let _ = session_result.await;
                        break;
                    }
                };

                let interrupted = loop {
                    tokio::select! {
                        _ = process_shutdown.cancelled() => {
                            capture_stop.cancel();
                            let _ = session_result.await;
                            break true;
                        }
                        result = &mut session_result => {
                            let _ = result;
                            break lock_status(&status).capture_stop_reason.is_some();
                        }
                        Some(stop_reply) = stop_rx.recv() => {
                            capture_stop.cancel();
                            let _ = session_result.await;
                            let outcome = stop_outcome(&db, &session_id);
                            let _ = stop_reply.send(outcome);
                            break false;
                        }
                        Some(start_reply) = start_rx.recv() => {
                            let _ = start_reply.reply.send(Err(AppError::control(
                                "a recording is already active",
                            )));
                        }
                    }
                };
                emit_stopped(&events, &db, &session_id, interrupted);
                if process_shutdown.is_cancelled() {
                    break;
                }
                tracing::info!("daemon idle");
            }
        }
    }

    control_task.abort();
    let _ = db.with_conn(crate::storage::jobs::reset_processing_without_event)?;
    let _ = tokio::time::timeout(Duration::from_secs(WORKER_SHUTDOWN_GRACE_SECS), worker).await;
    let _ = db.with_conn(crate::storage::jobs::reset_processing_without_event)?;
    {
        let mut status = lock_status(&status);
        status.capture_active = false;
        status.session_id = None;
        status.microphone_state = SourceUiState::Idle;
        status.system_state = SourceUiState::Idle;
    }
    Ok(())
}

fn emit_stopped(events: &EventBus, db: &Db, session_id: &str, interrupted: bool) {
    let event = if interrupted {
        "recording_interrupted"
    } else {
        "recording_stopped"
    };
    emit(events, UiEvent::new(event).session(session_id));
    let _ = db;
}

fn stop_outcome(db: &Db, session_id: &str) -> Result<StopOutcome> {
    let session = db
        .with_conn(|conn| crate::storage::sessions::get_session(conn, session_id))?
        .ok_or_else(|| AppError::control("session was not found after stop"))?;
    let started = parse_rfc3339(&session.started_at).ok();
    let ended = session
        .ended_at
        .as_deref()
        .and_then(|value| parse_rfc3339(value).ok());
    let duration_secs = match (started, ended) {
        (Some(start), Some(end)) => (end - start).num_seconds().max(0) as u64,
        (Some(start), None) => (chrono::Utc::now() - start).num_seconds().max(0) as u64,
        _ => 0,
    };
    let pending = db.with_conn(|conn| {
        let jobs = crate::storage::jobs::list_jobs_for_session(conn, session_id)?;
        Ok(jobs
            .iter()
            .any(|job| job.state != crate::storage::types::JobState::Completed))
    })?;
    Ok(StopOutcome {
        session_id: session_id.to_string(),
        duration_secs,
        transcription_pending: pending,
    })
}

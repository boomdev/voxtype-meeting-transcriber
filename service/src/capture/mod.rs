use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::audio::{print_devices, AudioBackend, AudioSource, PulseAudioBackend};
use crate::config::Config;
use crate::error::Result;
use crate::paths::{self, PathResolver};
use crate::runtime::{
    event_bus, lock_status, AudioServerHealth, EventBus, SharedStatus, AUDIO_RECONNECT_DELAY_SECS,
    DISK_CHECK_INTERVAL_SECS, WORKER_SHUTDOWN_GRACE_SECS,
};
use crate::storage::sessions::{
    insert_run, insert_running_session, set_session_state, snapshot_from_record, write_session_json,
};
use crate::storage::types::SessionState;
use crate::storage::Db;
use crate::timeutil::now_rfc3339;

pub mod chunker;
pub mod source_task;

pub(crate) const FRAME_CHANNEL_BOUND: usize = 256;

pub async fn cmd_devices() -> Result<()> {
    let backend = PulseAudioBackend::connect()?;
    print_devices(&backend)
}

pub async fn cmd_run() -> Result<()> {
    crate::daemon::cmd_run().await
}

struct RunSession<'a> {
    backend: Arc<dyn AudioBackend>,
    db: &'a Db,
    paths: &'a PathResolver,
    live_config: Arc<RwLock<Config>>,
    process_shutdown: CancellationToken,
    capture_stop: CancellationToken,
    socket: Option<crate::control::BoundSocket>,
    status: SharedStatus,
    print_startup_devices: bool,
    spawn_workers: bool,
    wake: mpsc::Sender<()>,
    wake_rx: Option<mpsc::Receiver<()>>,
    on_started: Option<tokio::sync::oneshot::Sender<String>>,
    events: EventBus,
    meeting_language: Option<String>,
}

pub async fn run_session_with_backend(
    backend: Arc<dyn AudioBackend>,
    db: &Db,
    paths: &PathResolver,
    config: &Config,
    process_shutdown: CancellationToken,
    status: SharedStatus,
) -> Result<Uuid> {
    let capture_stop = CancellationToken::new();
    let (wake, wake_rx) = mpsc::channel::<()>(32);
    run_session(RunSession {
        backend,
        db,
        paths,
        live_config: Arc::new(RwLock::new(config.clone())),
        process_shutdown,
        capture_stop,
        socket: None,
        status,
        print_startup_devices: false,
        spawn_workers: true,
        wake,
        wake_rx: Some(wake_rx),
        on_started: None,
        events: event_bus(),
        meeting_language: None,
    })
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_recording_session(
    backend: Arc<dyn AudioBackend>,
    db: &Db,
    paths: &PathResolver,
    live_config: Arc<RwLock<Config>>,
    process_shutdown: CancellationToken,
    capture_stop: CancellationToken,
    status: SharedStatus,
    wake: mpsc::Sender<()>,
    events: EventBus,
    on_started: Option<tokio::sync::oneshot::Sender<String>>,
    meeting_language: Option<String>,
) -> Result<Uuid> {
    run_session(RunSession {
        backend,
        db,
        paths,
        live_config,
        process_shutdown,
        capture_stop,
        socket: None,
        status,
        print_startup_devices: false,
        spawn_workers: false,
        wake,
        wake_rx: None,
        on_started,
        events,
        meeting_language,
    })
    .await
}

pub(crate) async fn connect_backend(shutdown: &CancellationToken) -> Result<Arc<dyn AudioBackend>> {
    loop {
        match PulseAudioBackend::connect() {
            Ok(backend) => return Ok(Arc::new(backend)),
            Err(error) => {
                tracing::error!(
                    error = %error,
                    "audio server unavailable; retrying in {AUDIO_RECONNECT_DELAY_SECS}s"
                );
                tokio::select! {
                    _ = shutdown.cancelled() => {
                        return Err(crate::error::AppError::audio(
                            "stopped before PulseAudio/pipewire-pulse became available",
                        ));
                    }
                    _ = tokio::time::sleep(Duration::from_secs(AUDIO_RECONNECT_DELAY_SECS)) => {}
                }
            }
        }
    }
}

fn warn_if_transcription_unready(config: &Config) {
    match config.transcription.provider {
        crate::config::ProviderKind::Voxtype => {
            if std::process::Command::new("voxtype")
                .arg("--version")
                .output()
                .is_err()
            {
                tracing::warn!("voxtype is unavailable; captured turns will remain queued");
            }
        }
        crate::config::ProviderKind::Openai => {}
        crate::config::ProviderKind::WhisperCpp => {
            let executable = &config.transcription.whisper_cpp.executable;
            let model = &config.transcription.whisper_cpp.model;
            if !executable.exists() {
                tracing::warn!(
                    path = %executable.display(),
                    "whisper-cli was not found; capture will continue and whisper.cpp jobs will stay pending"
                );
            }
            if !model.is_file() {
                tracing::warn!(
                    path = %model.display(),
                    "whisper.cpp model was not found; capture will continue and whisper.cpp jobs will stay pending"
                );
            }
        }
    }
}

pub(crate) fn spawn_shutdown_signals(shutdown: CancellationToken) {
    tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                tracing::info!("received Ctrl+C, stopping");
                shutdown.cancel();
            }
        }
    });
    tokio::spawn(async move {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut term) => {
                term.recv().await;
                tracing::info!("received SIGTERM, stopping");
                shutdown.cancel();
            }
            Err(error) => {
                tracing::warn!(error = %error, "could not install SIGTERM handler");
            }
        }
    });
}

async fn run_session(params: RunSession<'_>) -> Result<Uuid> {
    let RunSession {
        backend,
        db,
        paths,
        live_config,
        process_shutdown,
        capture_stop,
        socket,
        status,
        print_startup_devices,
        spawn_workers,
        wake: wake_tx,
        wake_rx,
        on_started,
        events,
        meeting_language,
    } = params;
    let config = live_config
        .read()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();

    tracing::info!(
        microphone = %config.audio.microphone,
        system_output = %config.audio.system_output,
        provider = %config.transcription.provider,
        "loaded configuration"
    );
    warn_if_transcription_unready(&config);

    if print_startup_devices {
        if let Err(error) = crate::audio::print_devices(backend.as_ref()) {
            tracing::warn!(error = %error, "could not print device list");
        }
    }

    let microphone = backend.current_microphone().ok();
    let output = backend.current_output_sink().ok();
    let monitor = backend.current_output_monitor().ok();

    let session_id = Uuid::new_v4();
    let session_dir = paths.session_dir(&session_id.to_string());
    let mic_dir = session_dir.join("audio").join("mic");
    let system_dir = session_dir.join("audio").join("system");
    paths::ensure_dir(&mic_dir)?;
    paths::ensure_dir(&system_dir)?;
    if config.audio.retain_audio {
        std::fs::write(session_dir.join("retain-audio"), b"enabled\n")?;
    }

    let session = db.with_conn(|conn| {
        insert_running_session(
            conn,
            &session_id.to_string(),
            microphone.as_ref(),
            output.as_ref(),
            monitor.as_ref(),
        )
    })?;
    let model = if config.transcription.provider == crate::config::ProviderKind::Voxtype {
        crate::transcription::voxtype::snapshot_config(
            paths.home(),
            &session_dir,
            meeting_language.as_deref(),
        )?
    } else {
        config.model_for_provider(config.transcription.provider)
    };
    let run =
        db.with_conn(|conn| insert_run(conn, &session.id, config.transcription.provider, &model))?;
    write_session_json(&session_dir, &snapshot_from_record(&session))?;

    {
        let mut status = lock_status(&status);
        status.session_id = Some(session.id.clone());
        status.session_started_at = Some(session.started_at.clone());
        status.microphone = microphone;
        status.output = output;
        status.monitor = monitor;
        status.capture_active = true;
        status.capture_paused = false;
        status.provider = config.transcription.provider;
        status.audio_server = AudioServerHealth::Available;
        status.microphone_state = crate::runtime::SourceUiState::Capturing;
        status.system_state = crate::runtime::SourceUiState::Capturing;
        status.capture_stop_reason = None;
    }
    if let Some(tx) = on_started {
        let _ = tx.send(session.id.clone());
    }

    println!("Session: {session_id}");
    println!("Directory: {}", session_dir.display());
    tracing::info!(
        session_id = %session_id,
        path = %session_dir.display(),
        "session started"
    );

    let capture_stop = capture_stop;
    let worker = if spawn_workers {
        let wake_rx = wake_rx.expect("spawn_workers requires wake_rx");
        let db_path = db.path().to_path_buf();
        let paths = paths.clone();
        let live_config = live_config.clone();
        let shutdown = process_shutdown.clone();
        let events = events.clone();
        Some(tokio::spawn(async move {
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
        }))
    } else {
        drop(wake_rx);
        None
    };

    let mic_task = tokio::spawn(source_task::run_source_task(
        source_task::SourceTaskParams {
            source: AudioSource::Mic,
            backend: backend.clone(),
            dir: mic_dir,
            args: db_clone_args(
                db,
                session.id.clone(),
                run.id.clone(),
                &config,
                wake_tx.clone(),
            ),
            process_shutdown: process_shutdown.clone(),
            capture_stop: capture_stop.clone(),
            status: status.clone(),
            live_config: live_config.clone(),
            events: events.clone(),
        },
    ));
    let sys_task = tokio::spawn(source_task::run_source_task(
        source_task::SourceTaskParams {
            source: AudioSource::System,
            backend: backend.clone(),
            dir: system_dir,
            args: db_clone_args(db, session.id.clone(), run.id.clone(), &config, wake_tx),
            process_shutdown: process_shutdown.clone(),
            capture_stop: capture_stop.clone(),
            status: status.clone(),
            live_config,
            events,
        },
    ));

    let disk_watch = tokio::spawn(watch_disk(DiskWatchParams {
        data_dir: paths.data_dir(),
        minimum_mb: config.general.minimum_free_space_mb,
        db_path: db.path().to_path_buf(),
        session_id: session.id.clone(),
        session_dir: session_dir.clone(),
        process_shutdown: process_shutdown.clone(),
        capture_stop: capture_stop.clone(),
        status: status.clone(),
    }));

    let control_task = if let Some(socket) = socket {
        let paths = paths.clone();
        let db_path = db.path().to_path_buf();
        let status = status.clone();
        let process_shutdown = process_shutdown.clone();
        Some(tokio::spawn(async move {
            if let Err(error) =
                crate::control::serve(socket, paths, db_path, status, process_shutdown, None).await
            {
                tracing::error!(error = %error, "control socket stopped");
            }
        }))
    } else {
        None
    };

    tokio::select! {
        _ = process_shutdown.cancelled() => {
            capture_stop.cancel();
        }
        _ = capture_stop.cancelled() => {}
    }
    let _ = mic_task.await;
    let _ = sys_task.await;
    disk_watch.abort();
    if let Some(task) = control_task {
        task.abort();
    }

    finalize_session(db, &session.id, &session_dir, &status)?;
    {
        let mut status = lock_status(&status);
        status.capture_active = false;
        status.capture_paused = false;
        status.session_id = None;
        status.session_started_at = None;
        status.microphone_state = crate::runtime::SourceUiState::Idle;
        status.system_state = crate::runtime::SourceUiState::Idle;
    }

    if let Some(worker) = worker {
        let _ = db.with_conn(crate::storage::jobs::reset_processing_without_event)?;
        let _ = tokio::time::timeout(Duration::from_secs(WORKER_SHUTDOWN_GRACE_SECS), worker).await;
        let _ = db.with_conn(crate::storage::jobs::reset_processing_without_event)?;
    }
    Ok(session_id)
}

fn finalize_session(
    db: &Db,
    session_id: &str,
    session_dir: &std::path::Path,
    status: &SharedStatus,
) -> Result<()> {
    let record = db.with_conn(|conn| crate::storage::sessions::get_session(conn, session_id))?;
    let Some(mut record) = record else {
        return Ok(());
    };
    if record.state != SessionState::Running {
        write_session_json(session_dir, &snapshot_from_record(&record))?;
        tracing::info!(session_id, state = %record.state.as_str(), "session already ended");
        return Ok(());
    }
    let ended = now_rfc3339();
    let disk_stop = lock_status(status).capture_stop_reason.is_some();
    let state = if disk_stop {
        SessionState::Interrupted
    } else {
        SessionState::Completed
    };
    db.with_conn(|conn| set_session_state(conn, session_id, state, Some(&ended)))?;
    record.state = state;
    record.ended_at = Some(ended);
    write_session_json(session_dir, &snapshot_from_record(&record))?;
    tracing::info!(session_id, state = %state.as_str(), "session ended");
    Ok(())
}

struct DiskWatchParams {
    data_dir: PathBuf,
    minimum_mb: u64,
    db_path: PathBuf,
    session_id: String,
    session_dir: PathBuf,
    process_shutdown: CancellationToken,
    capture_stop: CancellationToken,
    status: SharedStatus,
}

async fn watch_disk(params: DiskWatchParams) {
    let DiskWatchParams {
        data_dir,
        minimum_mb,
        db_path,
        session_id,
        session_dir,
        process_shutdown,
        capture_stop,
        status,
    } = params;
    let mut interval = tokio::time::interval(Duration::from_secs(DISK_CHECK_INTERVAL_SECS));
    loop {
        tokio::select! {
            _ = process_shutdown.cancelled() => break,
            _ = capture_stop.cancelled() => break,
            _ = interval.tick() => {
                match crate::disk::free_bytes(&data_dir) {
                    Ok(free) if !crate::disk::capture_allowed(free, minimum_mb) => {
                        let error = crate::disk::disk_stop_error(free, &data_dir, minimum_mb);
                        tracing::error!("{error}");
                        {
                            let mut status = lock_status(&status);
                            status.capture_stop_reason = Some(error.to_string());
                            status.capture_active = false;
                        }
                        if let Ok(db) = Db::open(&db_path) {
                            let ended = now_rfc3339();
                            let _ = db.with_conn(|conn| {
                                set_session_state(
                                    conn,
                                    &session_id,
                                    SessionState::Interrupted,
                                    Some(&ended),
                                )
                            });
                            if let Ok(Some(mut record)) =
                                db.with_conn(|conn| crate::storage::sessions::get_session(conn, &session_id))
                            {
                                record.state = SessionState::Interrupted;
                                record.ended_at = Some(ended);
                                let _ = write_session_json(&session_dir, &snapshot_from_record(&record));
                            }
                        }
                        capture_stop.cancel();
                        break;
                    }
                    Ok(_) => {}
                    Err(error) => tracing::warn!(error = %error, "could not check free disk space"),
                }
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct SpoolArgs {
    pub db_path: PathBuf,
    pub session_id: String,
    pub run_id: String,
    pub provider: crate::config::ProviderKind,
    pub model: String,
    pub wake: mpsc::Sender<()>,
}

fn db_clone_args(
    db: &Db,
    session_id: String,
    run_id: String,
    config: &Config,
    wake: mpsc::Sender<()>,
) -> SpoolArgs {
    SpoolArgs {
        db_path: db.path().to_path_buf(),
        session_id,
        run_id,
        provider: config.transcription.provider,
        model: config.model_for_provider(config.transcription.provider),
        wake,
    }
}

pub(crate) fn persist_completed(
    db: &Db,
    args: &SpoolArgs,
    dir: &std::path::Path,
    chunks: Vec<chunker::CompletedChunk>,
) -> Result<()> {
    for chunk in chunks {
        let path = crate::storage::persist_recorded_chunk(
            db,
            crate::storage::PersistChunk {
                session_id: &args.session_id,
                run_id: &args.run_id,
                provider: args.provider,
                model: &args.model,
                dir,
                source: chunk.source,
                started_at: chunk.started_at,
                ended_at: chunk.ended_at,
                samples: &chunk.samples,
            },
        )?;
        tracing::info!(
            source = %chunk.source,
            path = %path.display(),
            duration_ms = chunk.duration_ms(),
            "chunk persisted"
        );
        let _ = args.wake.try_send(());
    }
    Ok(())
}

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::audio::{AudioBackend, AudioEvent, AudioSource};
use crate::capture::chunker::Chunker;
use crate::config::Config;
use crate::error::Result;
use crate::runtime::{
    emit, lock_status, AudioServerHealth, EventBus, SharedStatus, SourceUiState, UiEvent,
    AUDIO_RECONNECT_DELAY_SECS,
};
use crate::storage::chunks::next_sequence;
use crate::storage::Db;

use super::{persist_completed, SpoolArgs};

pub(crate) struct SourceTaskParams {
    pub source: AudioSource,
    pub backend: Arc<dyn AudioBackend>,
    pub dir: PathBuf,
    pub args: SpoolArgs,
    pub process_shutdown: CancellationToken,
    pub capture_stop: CancellationToken,
    pub status: SharedStatus,
    pub live_config: Arc<RwLock<Config>>,
    pub events: EventBus,
}

fn configured_device(live_config: &Arc<RwLock<Config>>, source: AudioSource) -> String {
    let config = live_config
        .read()
        .unwrap_or_else(|poison| poison.into_inner());
    match source {
        AudioSource::Mic => config.audio.microphone.clone(),
        AudioSource::System => config.audio.system_output.clone(),
    }
}

pub(crate) async fn run_source_task(params: SourceTaskParams) -> Result<()> {
    let SourceTaskParams {
        source,
        backend,
        dir,
        args,
        process_shutdown,
        capture_stop,
        status,
        live_config,
        events,
    } = params;

    let enabled = {
        let config = live_config.read().unwrap_or_else(|p| p.into_inner());
        match source {
            AudioSource::Mic => config.audio.source.includes_mic(),
            AudioSource::System => config.audio.source.includes_system(),
        }
    };
    if !enabled {
        let mut current = lock_status(&status);
        match source {
            AudioSource::Mic => current.microphone_state = SourceUiState::Idle,
            AudioSource::System => current.system_state = SourceUiState::Idle,
        }
        return Ok(());
    }

    loop {
        if process_shutdown.is_cancelled() || capture_stop.is_cancelled() {
            break;
        }

        let configured = configured_device(&live_config, source);
        let follow_default = configured.is_empty() || configured == "default";

        let resolved = match source {
            AudioSource::Mic => backend.resolve_microphone(&configured).map(|mic| {
                let mut status = lock_status(&status);
                status.microphone = Some(mic.clone());
                status.microphone_state = SourceUiState::Capturing;
                status.audio_server = AudioServerHealth::Available;
                mic.id
            }),
            AudioSource::System => backend.resolve_output(&configured).map(|(sink, monitor)| {
                let id = monitor.id.clone();
                let mut status = lock_status(&status);
                status.output = Some(sink);
                status.monitor = Some(monitor);
                status.system_state = SourceUiState::Capturing;
                status.audio_server = AudioServerHealth::Available;
                id
            }),
        };

        let device_id = match resolved {
            Ok(id) => {
                emit(
                    &events,
                    UiEvent::new("device_state_changed").message(source.as_str()),
                );
                id
            }
            Err(error) => {
                tracing::warn!(source = %source, error = %error, "capture device unresolved");
                mark_unavailable(&status, source, &error.to_string());
                emit(
                    &events,
                    UiEvent::new("device_state_changed").message(source.as_str()),
                );
                {
                    let mut status = lock_status(&status);
                    match source {
                        AudioSource::Mic => status.microphone_state = SourceUiState::Reconnecting,
                        AudioSource::System => status.system_state = SourceUiState::Reconnecting,
                    }
                }
                wait_before_retry(&backend, source, &process_shutdown, &capture_stop).await;
                continue;
            }
        };

        let (tx, rx) = mpsc::channel(super::FRAME_CHANNEL_BOUND);
        let spool = {
            let dir = dir.clone();
            let args = args.clone();
            let process_shutdown = process_shutdown.clone();
            let capture_stop = capture_stop.clone();
            let spool_status = status.clone();
            tokio::spawn(async move {
                spool_until_end(
                    source,
                    rx,
                    dir,
                    process_shutdown,
                    capture_stop,
                    args,
                    spool_status,
                )
                .await
            })
        };

        let capture_token = process_shutdown.child_token();
        let capture_stop_child = capture_stop.clone();
        let stop_capture = capture_token.clone();
        let cancel_on_disk = tokio::spawn(async move {
            capture_stop_child.cancelled().await;
            stop_capture.cancel();
        });
        let switch_token = capture_token.clone();
        let watch_config = live_config.clone();
        let watch_shutdown = process_shutdown.clone();
        let watch_stop = capture_stop.clone();
        let initial_device = configured.clone();
        let cancel_on_switch = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = watch_shutdown.cancelled() => break,
                    _ = watch_stop.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_millis(400)) => {
                        if configured_device(&watch_config, source) != initial_device {
                            switch_token.cancel();
                            break;
                        }
                    }
                }
            }
        });

        let capture_result = backend
            .capture_from(source, &device_id, follow_default, tx, capture_token)
            .await;
        cancel_on_disk.abort();
        cancel_on_switch.abort();
        match spool.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::error!(source = %source, error = %error, "spool task failed")
            }
            Err(error) => tracing::error!(source = %source, error = %error, "spool task panicked"),
        }

        if process_shutdown.is_cancelled() || capture_stop.is_cancelled() {
            break;
        }

        if let Err(error) = capture_result {
            tracing::warn!(source = %source, device_id, error = %error, "capture stream ended with error");
            if error.to_string().to_ascii_lowercase().contains("server") {
                let mut status = lock_status(&status);
                status.audio_server = AudioServerHealth::Unavailable;
                emit(&events, UiEvent::new("audio_server_state_changed"));
            }
            emit(
                &events,
                UiEvent::new("device_state_changed").message(source.as_str()),
            );
            wait_before_retry(&backend, source, &process_shutdown, &capture_stop).await;
        }
    }
    Ok(())
}

async fn spool_until_end(
    source: AudioSource,
    mut rx: mpsc::Receiver<crate::audio::PcmFrame>,
    dir: PathBuf,
    process_shutdown: CancellationToken,
    capture_stop: CancellationToken,
    args: SpoolArgs,
    status: SharedStatus,
) -> Result<()> {
    let db = Db::open(&args.db_path)?;
    let next = db.with_conn(|conn| next_sequence(conn, &args.session_id, source))?;
    let mut chunker = Chunker::starting_at(source, next);
    let mut was_paused = false;
    loop {
        tokio::select! {
            _ = process_shutdown.cancelled() => break,
            _ = capture_stop.cancelled() => break,
            frame = rx.recv() => {
                match frame {
                    Some(frame) => {
                        let paused = lock_status(&status).capture_paused;
                        if paused {
                            if !was_paused {
                                if let Some(partial) = chunker.flush() { persist_completed(&db, &args, &dir, vec![partial])?; }
                            }
                            was_paused = true;
                        } else {
                            was_paused = false;
                            persist_completed(&db, &args, &dir, chunker.push(&frame))?;
                        }
                    },
                    None => break,
                }
            }
        }
    }
    while let Ok(frame) = rx.try_recv() {
        if !lock_status(&status).capture_paused {
            persist_completed(&db, &args, &dir, chunker.push(&frame))?;
        }
    }
    if let Some(partial) = chunker.flush() {
        persist_completed(&db, &args, &dir, vec![partial])?;
    }
    Ok(())
}

fn mark_unavailable(status: &SharedStatus, source: AudioSource, reason: &str) {
    let mut status = lock_status(status);
    match source {
        AudioSource::Mic => {
            status.microphone = None;
            status.microphone_state = SourceUiState::Unavailable;
        }
        AudioSource::System => {
            status.output = None;
            status.monitor = None;
            status.system_state = SourceUiState::Unavailable;
        }
    }
    tracing::warn!(source = %source, reason, "source unavailable");
}

async fn wait_before_retry(
    backend: &Arc<dyn AudioBackend>,
    source: AudioSource,
    process_shutdown: &CancellationToken,
    capture_stop: &CancellationToken,
) {
    let mut events = backend.subscribe();
    tokio::select! {
        _ = process_shutdown.cancelled() => {}
        _ = capture_stop.cancelled() => {}
        _ = tokio::time::sleep(Duration::from_secs(AUDIO_RECONNECT_DELAY_SECS)) => {}
        item = events.recv() => {
            if let Ok(event) = item {
                match (source, event) {
                    (_, AudioEvent::AudioServerAvailable)
                    | (AudioSource::Mic, AudioEvent::MicrophoneAvailable { .. })
                    | (AudioSource::Mic, AudioEvent::MicrophoneChanged { .. })
                    | (AudioSource::System, AudioEvent::OutputAvailable { .. })
                    | (AudioSource::System, AudioEvent::OutputChanged { .. }) => {}
                    _ => {}
                }
            }
        }
    }
}

use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::audio::AudioDevice;
use crate::config::{apply_patch, write_atomic, Config, ConfigPatch, ProviderKind};
use crate::control::protocol::{encode_ok, encode_response, Request, Response};
use crate::control::ControlExtras;
use crate::daemon::StopOutcome;
use crate::error::{AppError, Result};
use crate::paths::PathResolver;
use crate::runtime::{emit, lock_status, SharedStatus, SourceUiState, UiEvent};
use crate::session_status::{
    derive_ui_status, display_title, format_duration_secs, status_context, UiSessionStatus,
};
use crate::storage::types::SessionState;
use crate::storage::Db;
use crate::timeutil::parse_rfc3339;

pub enum HandlerOut {
    Line(String),
    Subscribe,
}

pub async fn handle_request(
    request: Request,
    paths: &PathResolver,
    db_path: &std::path::Path,
    status: &SharedStatus,
    extras: Option<&ControlExtras>,
    shutdown: &tokio_util::sync::CancellationToken,
) -> HandlerOut {
    match request.op.as_str() {
        "status" => HandlerOut::Line(
            match crate::control::collect_status(paths, db_path, status) {
                Ok(payload) => encode_response(&Response::status(payload)),
                Err(error) => encode_response(&Response::error(error.to_string())),
            },
        ),
        "stop" => {
            shutdown.cancel();
            HandlerOut::Line(encode_response(&Response::stop()))
        }
        "subscribe" => HandlerOut::Subscribe,
        "start_recording" => HandlerOut::Line(await_start(extras, request.language.clone()).await),
        "stop_recording" => HandlerOut::Line(await_stop(extras).await),
        "pause_recording" => HandlerOut::Line(set_paused(status, true)),
        "resume_recording" => HandlerOut::Line(set_paused(status, false)),
        other => HandlerOut::Line(
            match dispatch_sync(other, &request, paths, db_path, status, extras) {
                Ok(line) => line,
                Err(error) => encode_response(&Response::error(human_error(&error))),
            },
        ),
    }
}

fn set_paused(status: &SharedStatus, paused: bool) -> String {
    let mut state = lock_status(status);
    if !state.capture_active {
        return encode_response(&Response::error("no meeting is active"));
    }
    if state.capture_paused == paused {
        return encode_ok(json!({ "paused": paused }));
    }
    state.capture_paused = paused;
    encode_ok(json!({ "paused": paused }))
}

fn dispatch_sync(
    op: &str,
    request: &Request,
    paths: &PathResolver,
    db_path: &std::path::Path,
    status: &SharedStatus,
    extras: Option<&ControlExtras>,
) -> Result<String> {
    match op {
        "get_state" => get_state(paths, db_path, status, extras),
        "list_sessions" => list_sessions(paths, db_path, extras),
        "get_session" => get_session(paths, db_path, request.session_id.as_deref(), extras),
        "rename_session" => rename_session(
            paths,
            db_path,
            request.session_id.as_deref(),
            request.title.as_deref(),
            extras,
        ),
        "retry_session" => retry_session(db_path, request.session_id.as_deref(), extras),
        "retranscribe_session" => retranscribe_session(
            db_path,
            extras,
            request.session_id.as_deref(),
            request.provider.as_deref(),
        ),
        "delete_session_audio" => {
            delete_session_audio(paths, db_path, request.session_id.as_deref(), extras)
        }
        "delete_session" => delete_session(
            paths,
            db_path,
            status,
            request.session_id.as_deref(),
            extras,
        ),
        "get_config" => get_config(paths, extras),
        "update_config" => update_config(paths, extras, request.config.clone()),
        "get_diagnostics" => get_diagnostics(paths, extras),
        "list_devices" => list_devices(extras),
        "cleanup_preview" => cleanup_preview(paths),
        "cleanup_apply" => cleanup_apply(paths, extras),
        other => Ok(encode_response(&Response::error(format!(
            "unknown op '{other}'"
        )))),
    }
}

async fn await_start(extras: Option<&ControlExtras>, language: Option<String>) -> String {
    let Some(extras) = extras else {
        return encode_response(&Response::error_code(
            "unavailable",
            "recording control is not available",
        ));
    };
    let (tx, rx) = oneshot::channel();
    if extras
        .recording
        .start
        .send(crate::daemon::StartRequest {
            language,
            reply: tx,
        })
        .await
        .is_err()
    {
        return encode_response(&Response::error("daemon is shutting down"));
    }
    match rx.await {
        Ok(Ok(session_id)) => encode_ok(json!({ "session_id": session_id })),
        Ok(Err(error)) => encode_response(&Response::error(human_error(&error))),
        Err(_) => encode_response(&Response::error("recording start was cancelled")),
    }
}

async fn await_stop(extras: Option<&ControlExtras>) -> String {
    let Some(extras) = extras else {
        return encode_response(&Response::error_code(
            "unavailable",
            "recording control is not available",
        ));
    };
    let (tx, rx) = oneshot::channel();
    if extras.recording.stop.send(tx).await.is_err() {
        return encode_response(&Response::error("daemon is shutting down"));
    }
    match rx.await {
        Ok(Ok(StopOutcome {
            session_id,
            duration_secs,
            transcription_pending,
        })) => encode_ok(json!({
            "session_id": session_id,
            "duration_secs": duration_secs,
            "transcription_pending": transcription_pending
        })),
        Ok(Err(error)) => encode_response(&Response::error(human_error(&error))),
        Err(_) => encode_response(&Response::error("recording stop was cancelled")),
    }
}

fn get_state(
    paths: &PathResolver,
    db_path: &std::path::Path,
    status: &SharedStatus,
    extras: Option<&ControlExtras>,
) -> Result<String> {
    let snapshot = lock_status(status).clone();
    let db = Db::open(db_path)?;
    let config = current_config(paths, extras);
    let ctx = status_context(&config);
    let (pending, processing, completed) = db.with_conn(crate::storage::jobs::counts)?;
    let sessions = db.with_conn(crate::storage::sessions::list_sessions)?;
    let recent: Vec<Value> = sessions
        .iter()
        .filter(|session| session.state != SessionState::Running)
        .take(5)
        .map(|session| session_json(paths, &db, session, &ctx))
        .collect::<Result<_>>()?;
    let attention = panel_attention(&config, pending, &db)?;
    let panel = panel_state(snapshot.capture_active, processing, attention.is_some());
    let microphone = source_status_json(
        extras,
        &config.audio.microphone,
        true,
        snapshot.microphone.as_ref(),
        snapshot.microphone_state,
        snapshot.capture_active,
    );
    let system_audio = source_status_json(
        extras,
        &config.audio.system_output,
        false,
        snapshot.output.as_ref(),
        snapshot.system_state,
        snapshot.capture_active,
    );
    let recording = json!({
        "active": snapshot.capture_active,
        "paused": snapshot.capture_paused,
        "session_id": snapshot.session_id,
        "started_at": snapshot.session_started_at,
        "microphone": microphone,
        "system_audio": system_audio,
    });
    Ok(encode_ok(json!({
        "daemon_version": env!("CARGO_PKG_VERSION"),
        "recording": recording,
        "panel": {
            "state": panel,
            "pending_jobs": pending,
            "processing_jobs": processing,
            "completed_jobs": completed,
        },
        "attention": {
            "required": attention.is_some(),
            "message": attention,
        },
        "recent_sessions": recent,
    })))
}

fn panel_state(recording: bool, processing: i64, attention: bool) -> &'static str {
    if recording {
        "recording"
    } else if attention {
        "attention"
    } else if processing > 0 {
        "transcribing"
    } else {
        "idle"
    }
}

fn panel_attention(config: &Config, pending: i64, db: &Db) -> Result<Option<String>> {
    if config.transcription.provider == ProviderKind::WhisperCpp {
        let executable = &config.transcription.whisper_cpp.executable;
        if !executable.exists() && pending > 0 {
            return Ok(Some(format!(
                "whisper-cli was not found at {}",
                executable.display()
            )));
        }
        let model = &config.transcription.whisper_cpp.model;
        if !model.is_file() && pending > 0 {
            return Ok(Some(format!(
                "whisper.cpp model was not found at {}",
                model.display()
            )));
        }
    }
    let _ = db;
    Ok(None)
}

fn source_json(device: Option<&AudioDevice>, state: SourceUiState) -> Value {
    json!({
        "label": device.map(|d| d.description.clone()).filter(|s| !s.is_empty())
            .or_else(|| device.map(|d| d.id.clone()))
            .unwrap_or_else(|| "Unavailable".into()),
        "state": match state {
            SourceUiState::Idle => "idle",
            SourceUiState::Capturing => "capturing",
            SourceUiState::Unavailable => "unavailable",
            SourceUiState::Reconnecting => "reconnecting",
        }
    })
}

fn source_status_json(
    extras: Option<&ControlExtras>,
    configured: &str,
    microphone: bool,
    recorded: Option<&AudioDevice>,
    recorded_state: SourceUiState,
    capture_active: bool,
) -> Value {
    if capture_active {
        return source_json(recorded, recorded_state);
    }
    let Some(extras) = extras else {
        return json!({
            "label": "Unavailable",
            "state": "unavailable",
        });
    };
    let resolved = if microphone {
        extras.backend.resolve_microphone(configured)
    } else {
        extras
            .backend
            .resolve_output(configured)
            .map(|(sink, _)| sink)
    };
    match resolved {
        Ok(device) => source_json(Some(&device), SourceUiState::Idle),
        Err(_) => json!({
            "label": "Unavailable",
            "state": "unavailable",
        }),
    }
}

fn list_sessions(
    paths: &PathResolver,
    db_path: &std::path::Path,
    extras: Option<&ControlExtras>,
) -> Result<String> {
    let db = Db::open(db_path)?;
    let config = current_config(paths, extras);
    let ctx = status_context(&config);
    let sessions = db.with_conn(crate::storage::sessions::list_sessions)?;
    let items: Vec<Value> = sessions
        .iter()
        .map(|session| session_json(paths, &db, session, &ctx))
        .collect::<Result<_>>()?;
    Ok(encode_ok(json!({ "sessions": items })))
}

fn get_session(
    paths: &PathResolver,
    db_path: &std::path::Path,
    session_id: Option<&str>,
    extras: Option<&ControlExtras>,
) -> Result<String> {
    let session_id = require_session_id(session_id)?;
    let db = Db::open(db_path)?;
    let config = current_config(paths, extras);
    let ctx = status_context(&config);
    let session = db
        .with_conn(|conn| crate::storage::sessions::get_session(conn, session_id))?
        .ok_or_else(|| AppError::control(format!("Session {session_id} not found")))?;
    Ok(encode_ok(json!({
        "session": session_json(paths, &db, &session, &ctx)?
    })))
}

fn session_json(
    paths: &PathResolver,
    db: &Db,
    session: &crate::storage::types::SessionRecord,
    ctx: &crate::session_status::StatusContext<'_>,
) -> Result<Value> {
    let chunks =
        db.with_conn(|conn| crate::storage::chunks::list_chunks_for_session(conn, &session.id))?;
    let jobs =
        db.with_conn(|conn| crate::storage::jobs::list_jobs_for_session(conn, &session.id))?;
    let transcribed =
        db.with_conn(|conn| crate::storage::events::transcribed_chunk_ids(conn, &session.id))?;
    let ui_status = derive_ui_status(session, &chunks, &jobs, &transcribed, ctx);
    let duration_secs = session_duration_secs(session);
    let session_dir = paths.session_dir(&session.id);
    let transcript_md = session_dir.join("transcript.md");
    let can_delete_audio = !chunks.is_empty()
        && session.state != SessionState::Running
        && chunks
            .iter()
            .all(|chunk| transcribed.iter().any(|id| id == &chunk.id));
    Ok(json!({
        "id": session.id,
        "title": display_title(session),
        "stored_title": session.title,
        "state": session.state.as_str(),
        "ui_status": ui_status.as_str(),
        "ui_status_label": ui_status.display_label(),
        "started_at": session.started_at,
        "ended_at": session.ended_at,
        "duration_secs": duration_secs,
        "duration_label": format_duration_secs(duration_secs),
        "chunk_count": chunks.len(),
        "completed_jobs": jobs.iter().filter(|job| job.state == crate::storage::types::JobState::Completed).count(),
        "session_dir": session_dir.display().to_string(),
        "transcript_md": transcript_md.display().to_string(),
        "transcript_exists": transcript_md.is_file(),
        "can_delete_audio": can_delete_audio,
        "can_delete_session": session.state != SessionState::Running,
        "can_retry": matches!(ui_status, UiSessionStatus::WaitingRetry | UiSessionStatus::Attention | UiSessionStatus::Interrupted)
            && jobs.iter().any(|job| job.state != crate::storage::types::JobState::Completed),
    }))
}

fn session_duration_secs(session: &crate::storage::types::SessionRecord) -> i64 {
    let Ok(start) = parse_rfc3339(&session.started_at) else {
        return 0;
    };
    let end = session
        .ended_at
        .as_deref()
        .and_then(|value| parse_rfc3339(value).ok())
        .unwrap_or_else(chrono::Utc::now);
    (end - start).num_seconds().max(0)
}

fn rename_session(
    paths: &PathResolver,
    db_path: &std::path::Path,
    session_id: Option<&str>,
    title: Option<&str>,
    extras: Option<&ControlExtras>,
) -> Result<String> {
    let session_id = require_session_id(session_id)?;
    let title = title.unwrap_or("").trim();
    if title.is_empty() {
        return Err(AppError::control("title must not be empty"));
    }
    if title.chars().count() > 120 {
        return Err(AppError::control("title must be 120 characters or fewer"));
    }
    let db = Db::open(db_path)?;
    db.with_conn(|conn| crate::storage::sessions::set_session_title(conn, session_id, title))?;
    let session = db
        .with_conn(|conn| crate::storage::sessions::get_session(conn, session_id))?
        .ok_or_else(|| AppError::control("Session not found"))?;
    crate::storage::sessions::write_session_json(
        &paths.session_dir(session_id),
        &crate::storage::sessions::snapshot_from_record(&session),
    )?;
    let omit = current_config(paths, extras)
        .transcript
        .omit_single_source_headers;
    let _ = crate::transcript::regenerate_session_transcripts(
        &db,
        session_id,
        &paths.session_dir(session_id),
        omit,
    );
    if let Some(extras) = extras {
        emit(
            &extras.events,
            UiEvent::new("session_updated").session(session_id),
        );
    }
    Ok(encode_ok(json!({ "title": title })))
}

fn retry_session(
    db_path: &std::path::Path,
    session_id: Option<&str>,
    extras: Option<&ControlExtras>,
) -> Result<String> {
    let session_id = require_session_id(session_id)?;
    let db = Db::open(db_path)?;
    let updated =
        db.with_conn(|conn| crate::storage::jobs::retry_session_pending(conn, session_id))?;
    if let Some(extras) = extras {
        emit(
            &extras.events,
            UiEvent::new("transcription_job_updated").session(session_id),
        );
    }
    Ok(encode_ok(json!({ "updated": updated })))
}

fn retranscribe_session(
    db_path: &std::path::Path,
    extras: Option<&ControlExtras>,
    session_id: Option<&str>,
    provider: Option<&str>,
) -> Result<String> {
    let session_id = require_session_id(session_id)?;
    let provider = ProviderKind::parse(provider.unwrap_or(""))?;
    let config = extras
        .map(|extra| {
            extra
                .config
                .read()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
        })
        .unwrap_or_default();
    crate::config::assert_languages_compatible(
        provider,
        &config.transcription.model,
        &config.transcription.languages,
    )?;
    let model = config.model_for_provider(provider);
    let db = Db::open(db_path)?;
    let (run_id, count) = db.with_conn_mut(|conn| {
        crate::storage::jobs::queue_retranscribe(conn, session_id, provider, &model)
    })?;
    if let Some(extras) = extras {
        emit(
            &extras.events,
            UiEvent::new("transcription_job_updated").session(session_id),
        );
    }
    Ok(encode_ok(json!({ "run_id": run_id, "count": count })))
}

fn delete_session_audio(
    paths: &PathResolver,
    db_path: &std::path::Path,
    session_id: Option<&str>,
    extras: Option<&ControlExtras>,
) -> Result<String> {
    let session_id = require_session_id(session_id)?;
    let db = Db::open(db_path)?;
    let session = db
        .with_conn(|conn| crate::storage::sessions::get_session(conn, session_id))?
        .ok_or_else(|| AppError::control(format!("Session {session_id} not found")))?;
    if session.state == SessionState::Running {
        return Err(AppError::control(
            "cannot delete audio for an active recording",
        ));
    }
    let chunks =
        db.with_conn(|conn| crate::storage::chunks::list_chunks_for_session(conn, session_id))?;
    let transcribed =
        db.with_conn(|conn| crate::storage::events::transcribed_chunk_ids(conn, session_id))?;
    if chunks.is_empty()
        || !chunks
            .iter()
            .all(|chunk| transcribed.iter().any(|id| id == &chunk.id))
    {
        return Err(AppError::control(
            "audio can be deleted only after every chunk has a transcript",
        ));
    }
    let session_dir = paths.session_dir(session_id);
    for chunk in &chunks {
        let path = std::path::Path::new(&chunk.file_path);
        if !path.starts_with(&session_dir) {
            return Err(AppError::control(
                "refusing to delete a file outside the session directory",
            ));
        }
        if path.is_file() {
            std::fs::remove_file(path)?;
        }
    }
    if let Some(extras) = extras {
        emit(
            &extras.events,
            UiEvent::new("session_updated").session(session_id),
        );
    }
    Ok(encode_ok(json!({ "deleted": true })))
}

fn delete_session(
    paths: &PathResolver,
    db_path: &std::path::Path,
    status: &SharedStatus,
    session_id: Option<&str>,
    extras: Option<&ControlExtras>,
) -> Result<String> {
    let session_id = require_session_id(session_id)?;
    let snapshot = lock_status(status).clone();
    if snapshot.capture_active && snapshot.session_id.as_deref() == Some(session_id) {
        return Err(AppError::control("cannot delete an active recording"));
    }
    let db = Db::open(db_path)?;
    let session = db
        .with_conn(|conn| crate::storage::sessions::get_session(conn, session_id))?
        .ok_or_else(|| AppError::control(format!("Session {session_id} not found")))?;
    if session.state == SessionState::Running {
        return Err(AppError::control("cannot delete an active recording"));
    }
    let dir = paths.session_dir(session_id);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    db.with_conn_mut(|conn| {
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM transcript_events WHERE session_id = ?1",
            rusqlite::params![session_id],
        )?;
        tx.execute(
            "DELETE FROM transcription_jobs WHERE audio_chunk_id IN (
                SELECT id FROM audio_chunks WHERE session_id = ?1
             )",
            rusqlite::params![session_id],
        )?;
        tx.execute(
            "DELETE FROM audio_chunks WHERE session_id = ?1",
            rusqlite::params![session_id],
        )?;
        tx.execute(
            "DELETE FROM transcription_runs WHERE session_id = ?1",
            rusqlite::params![session_id],
        )?;
        tx.execute(
            "DELETE FROM sessions WHERE id = ?1",
            rusqlite::params![session_id],
        )?;
        tx.commit()?;
        Ok(())
    })?;
    if let Some(extras) = extras {
        emit(
            &extras.events,
            UiEvent::new("session_deleted").session(session_id),
        );
    }
    Ok(encode_ok(json!({ "deleted": true })))
}

fn get_config(paths: &PathResolver, extras: Option<&ControlExtras>) -> Result<String> {
    let config = current_config(paths, extras);
    let stored = crate::disk::stored_audio_bytes(&paths.sessions_dir()).unwrap_or(0);
    let free = crate::disk::free_bytes(&paths.data_dir()).unwrap_or(0);
    let distro = crate::distro::DistroInfo::detect()
        .unwrap_or_else(|_| crate::distro::DistroInfo::from_os_release(""));
    Ok(encode_ok(json!({
        "daemon_version": env!("CARGO_PKG_VERSION"),
        "desktop_environment": std::env::var("XDG_CURRENT_DESKTOP").ok(),
        "distribution": distro.pretty_name(),
        "storage": {
            "stored_audio_bytes": stored,
            "stored_audio_label": crate::disk::format_bytes(stored),
            "free_bytes": free,
            "free_label": crate::disk::format_bytes(free),
        },
        "paths": {
            "config_file": paths.config_file().display().to_string(),
            "config_dir": paths.config_dir().display().to_string(),
            "data_dir": paths.data_dir().display().to_string(),
            "database": paths.db_path().display().to_string(),
            "control_socket": paths.control_socket().ok().map(|p| p.display().to_string()),
        },
        "config": {
            "general": { "minimum_free_space_mb": config.general.minimum_free_space_mb },
            "audio": {
                "microphone": config.audio.microphone,
                "system_output": config.audio.system_output,
                "source": config.audio.source.as_str(),
                "retain_audio": config.audio.retain_audio,
            },
            "transcription": {
                "provider": config.transcription.provider.as_str(),
                "model": config.transcription.model,
                "max_concurrent_jobs": config.transcription.max_concurrent_jobs,
                "languages": config.transcription.languages,
                "whisper_cpp": {
                    "executable": config.transcription.whisper_cpp.executable,
                    "model": config.transcription.whisper_cpp.model,
                }
            },
            "transcript": {
                "omit_single_source_headers": config.transcript.omit_single_source_headers,
            }
        }
    })))
}

fn get_diagnostics(paths: &PathResolver, extras: Option<&ControlExtras>) -> Result<String> {
    let mut checks = crate::doctor::diagnostics_payload(paths);
    checks.insert(
        0,
        json!({
            "id": "daemon",
            "name": "Daemon",
            "status": "PASS",
            "detail": format!("running {}", env!("CARGO_PKG_VERSION")),
        }),
    );
    let socket_ok = extras.is_some() && paths.control_socket().ok().is_some_and(|p| p.exists());
    checks.push(json!({
        "id": "control_socket",
        "name": "Control socket",
        "status": if socket_ok { "PASS" } else { "FAIL" },
        "detail": paths
            .control_socket()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "unavailable".into()),
    }));
    Ok(encode_ok(json!({
        "daemon_version": env!("CARGO_PKG_VERSION"),
        "desktop_environment": std::env::var("XDG_CURRENT_DESKTOP").ok(),
        "checks": checks,
    })))
}

fn update_config(
    paths: &PathResolver,
    extras: Option<&ControlExtras>,
    patch: Option<Value>,
) -> Result<String> {
    let Some(extras) = extras else {
        return Err(AppError::control(
            "configuration updates require the daemon",
        ));
    };
    let patch: ConfigPatch = serde_json::from_value(patch.unwrap_or(json!({})))?;
    let current = extras
        .config
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    let next = match apply_patch(&current, patch, paths.home()) {
        Ok(next) => next,
        Err(error) => {
            let message = human_error(&error);
            let mut fields = std::collections::BTreeMap::new();
            if let Some(field) = crate::config::validation_field(&message) {
                fields.insert(field.to_string(), message.clone());
            }
            return Ok(encode_response(&Response::error_fields(message, fields)));
        }
    };
    write_atomic(paths, &next)?;
    {
        let mut guard = extras.config.write().unwrap_or_else(|p| p.into_inner());
        *guard = next;
    }
    emit(&extras.events, UiEvent::new("config_updated"));
    get_config(paths, Some(extras))
}

fn list_devices(extras: Option<&ControlExtras>) -> Result<String> {
    let Some(extras) = extras else {
        return Err(AppError::control("device listing requires the daemon"));
    };
    let inputs = extras.backend.list_input_devices()?;
    let outputs = extras.backend.list_output_devices()?;
    Ok(encode_ok(json!({
        "microphones": inputs.iter().map(|d| json!({"id": d.id, "description": d.description})).collect::<Vec<_>>(),
        "outputs": outputs.iter().map(|d| json!({"id": d.id, "description": d.description})).collect::<Vec<_>>(),
    })))
}

fn cleanup_preview(paths: &PathResolver) -> Result<String> {
    let (files, bytes, sessions) = crate::cleanup::preview_stats(paths)?;
    Ok(encode_ok(json!({
        "sessions": sessions,
        "files": files,
        "bytes": bytes,
        "bytes_label": crate::disk::format_bytes(bytes),
    })))
}

fn cleanup_apply(paths: &PathResolver, extras: Option<&ControlExtras>) -> Result<String> {
    let (files, bytes, sessions) = crate::cleanup::apply_all(paths)?;
    if let Some(extras) = extras {
        emit(&extras.events, UiEvent::new("session_deleted"));
    }
    Ok(encode_ok(json!({
        "sessions": sessions,
        "files": files,
        "bytes": bytes,
        "bytes_label": crate::disk::format_bytes(bytes),
    })))
}

fn current_config(paths: &PathResolver, extras: Option<&ControlExtras>) -> Config {
    if let Some(extras) = extras {
        extras
            .config
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    } else {
        Config::load_or_default(paths).unwrap_or_else(|_| Config::default())
    }
}

fn require_session_id(session_id: Option<&str>) -> Result<&str> {
    session_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppError::control("session_id is required"))
}

fn human_error(error: &AppError) -> String {
    error.to_string()
}

use crate::cli::RecordCommand;
use crate::config::{Config, ProviderKind};
use crate::error::{AppError, Result};
use crate::paths::{self, PathResolver};
use crate::storage::jobs::{queue_retranscribe, retry_pending};
use crate::storage::Db;

pub fn cmd_retry(provider: Option<ProviderKind>) -> Result<()> {
    let paths = PathResolver::from_env()?;
    let config = Config::load_or_default(&paths)?;
    if let Some(kind) = provider {
        crate::config::assert_languages_compatible(
            kind,
            &config.transcription.model,
            &config.transcription.languages,
        )?;
    }
    let db = Db::open(paths.db_path())?;
    db.with_conn(crate::storage::jobs::reset_processing_without_event)?;
    let updated = db.with_conn(|conn| match provider {
        Some(kind) => {
            let model = config.model_for_provider(kind);
            retry_pending(conn, Some((kind, model)))
        }
        None => retry_pending(conn, None),
    })?;
    if updated == 0 {
        println!("No eligible jobs.");
    } else {
        println!("Queued {updated} transcription job(s) for retry.");
    }
    Ok(())
}

pub fn cmd_retranscribe(session_id: &str, provider: ProviderKind) -> Result<()> {
    let paths = PathResolver::from_env()?;
    let config = Config::load_or_default(&paths)?;
    crate::config::assert_languages_compatible(
        provider,
        &config.transcription.model,
        &config.transcription.languages,
    )?;
    let db = Db::open(paths.db_path())?;
    let model = config.model_for_provider(provider);
    let (run_id, count) =
        db.with_conn_mut(|conn| queue_retranscribe(conn, session_id, provider, &model))?;
    println!("Queued {count} chunks for retranscription with {provider} (run {run_id})");
    Ok(())
}

pub fn cmd_rebuild(session_id: &str) -> Result<()> {
    let paths = PathResolver::from_env()?;
    let config = Config::load_or_default(&paths)?;
    paths::ensure_dir(&paths.session_dir(session_id))?;
    let db = Db::open(paths.db_path())?;
    crate::transcript::regenerate_session_transcripts(
        &db,
        session_id,
        &paths.session_dir(session_id),
        config.transcript.omit_single_source_headers,
    )?;
    println!("Rebuilt transcript.md and transcript.jsonl for session {session_id}");
    Ok(())
}

pub async fn cmd_record(action: RecordCommand) -> Result<()> {
    let paths = PathResolver::from_env()?;
    let socket = paths.control_socket()?;
    if !socket.exists() {
        return Err(AppError::control(
            "voxtype-meeting-service daemon is not running (no control socket); start it with `voxtype-meeting-service start` or `voxtype-meeting-service run`",
        ));
    }
    match action {
        RecordCommand::Start => {
            let line =
                crate::control::send_json(&socket, &serde_json::json!({"op":"start_recording"}))
                    .await?;
            print_json_line(&line)
        }
        RecordCommand::Stop => {
            let line =
                crate::control::send_json(&socket, &serde_json::json!({"op":"stop_recording"}))
                    .await?;
            print_json_line(&line)
        }
        RecordCommand::Status => {
            let line =
                crate::control::send_json(&socket, &serde_json::json!({"op":"get_state"})).await?;
            print_json_line(&line)
        }
    }
}

fn print_json_line(line: &str) -> Result<()> {
    let value: serde_json::Value = serde_json::from_str(line)?;
    if value.get("ok").and_then(|ok| ok.as_bool()) == Some(false) {
        let message = value
            .get("error")
            .and_then(|error| error.as_str())
            .unwrap_or("request failed");
        return Err(AppError::control(message.to_string()));
    }
    println!("{line}");
    Ok(())
}

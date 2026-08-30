use std::fs;
use std::path::Path;

use rusqlite::Connection;
use uuid::Uuid;

use crate::audio::AudioSource;
use crate::config::Config;
use crate::error::Result;
use crate::paths::PathResolver;
use crate::storage::chunks::{chunk_exists_for_path, insert_chunk_and_job, sequence_exists};
use crate::storage::jobs::{
    complete_jobs_that_already_have_events, reset_processing_without_event,
};
use crate::storage::sessions::{
    insert_run, interrupt_running_sessions, latest_run, write_session_json,
};
use crate::storage::types::{SessionSnapshot, SessionState};
use crate::timeutil::{now_rfc3339, parse_rfc3339};

pub fn recover_on_startup(
    conn: &mut Connection,
    paths: &PathResolver,
    config: &Config,
) -> Result<()> {
    let interrupted = interrupt_running_sessions(conn)?;
    if interrupted > 0 {
        tracing::info!(
            count = interrupted,
            "marked leftover running sessions as interrupted"
        );
    }
    let completed = complete_jobs_that_already_have_events(conn)?;
    if completed > 0 {
        tracing::info!(
            count = completed,
            "completed processing jobs that already had transcript events"
        );
    }
    let reset = reset_processing_without_event(conn)?;
    if reset > 0 {
        tracing::info!(
            count = reset,
            "reset interrupted processing jobs to pending"
        );
    }
    delete_tmp_files(&paths.sessions_dir())?;
    reconcile_orphan_flac(conn, paths, config)?;
    Ok(())
}

fn delete_tmp_files(sessions_dir: &Path) -> Result<()> {
    if !sessions_dir.exists() {
        return Ok(());
    }
    for entry in walk_files(sessions_dir)? {
        if entry.extension().is_some_and(|ext| ext == "tmp")
            && entry
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(".flac"))
        {
            tracing::info!(path = %entry.display(), "removing leftover temporary audio file");
            fs::remove_file(&entry)?;
        }
    }
    Ok(())
}

fn reconcile_orphan_flac(
    conn: &mut Connection,
    paths: &PathResolver,
    config: &Config,
) -> Result<()> {
    let sessions_dir = paths.sessions_dir();
    if !sessions_dir.exists() {
        return Ok(());
    }
    for session_dir in fs::read_dir(&sessions_dir)? {
        let session_dir = session_dir?.path();
        if !session_dir.is_dir() {
            continue;
        }
        let Some(session_id) = session_dir.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if Uuid::parse_str(session_id).is_err() {
            continue;
        }
        ensure_session_and_run(conn, session_id, &session_dir, config)?;
        let run = latest_run(conn, session_id)?.ok_or_else(|| {
            crate::error::AppError::other(format!(
                "session {session_id} has no transcription run after recovery"
            ))
        })?;
        for source in [AudioSource::Mic, AudioSource::System] {
            let audio_dir = session_dir.join("audio").join(source.audio_subdir());
            if !audio_dir.exists() {
                continue;
            }
            let mut files: Vec<_> = fs::read_dir(&audio_dir)?
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|path| {
                    path.extension().is_some_and(|ext| ext == "flac")
                        && path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| !name.ends_with(".tmp"))
                })
                .collect();
            files.sort();
            for file in files {
                let file_path = file.to_string_lossy().into_owned();
                if chunk_exists_for_path(conn, &file_path)? {
                    continue;
                }
                let (sequence, started_at) = parse_chunk_filename(&file)?;
                if sequence_exists(conn, session_id, source, sequence)? {
                    tracing::warn!(
                        session_id,
                        source = %source,
                        sequence,
                        path = %file.display(),
                        "skipping orphan FLAC because this sequence is already in SQLite"
                    );
                    continue;
                }
                let duration_ms = 30_000;
                let ended_at = started_at + chrono::Duration::milliseconds(duration_ms);
                insert_chunk_and_job(
                    conn,
                    crate::storage::chunks::NewAudioChunk {
                        session_id,
                        run_id: &run.id,
                        source,
                        sequence,
                        started_at,
                        ended_at,
                        file_path: &file_path,
                        duration_ms,
                        provider: run.provider,
                        model: run.model.as_deref().unwrap_or(""),
                    },
                )?;
                tracing::info!(
                    session_id,
                    source = %source,
                    sequence,
                    path = %file.display(),
                    "recovered orphan FLAC into SQLite"
                );
            }
        }
    }
    Ok(())
}

fn ensure_session_and_run(
    conn: &Connection,
    session_id: &str,
    session_dir: &Path,
    config: &Config,
) -> Result<()> {
    let existing = crate::storage::sessions::get_session(conn, session_id)?;
    if existing.is_none() {
        let now = now_rfc3339();
        conn.execute(
            "INSERT INTO sessions (
                id, state, started_at, ended_at,
                microphone_id, microphone_description,
                output_id, output_description,
                monitor_id, monitor_description, created_at
            ) VALUES (?1, 'interrupted', ?2, ?2, NULL, NULL, NULL, NULL, NULL, NULL, ?2)",
            rusqlite::params![session_id, now],
        )?;
        let snapshot = SessionSnapshot {
            id: session_id.to_string(),
            state: SessionState::Interrupted,
            started_at: now.clone(),
            ended_at: Some(now),
            microphone: None,
            output: None,
            monitor: None,
            title: None,
        };
        write_session_json(session_dir, &snapshot)?;
        tracing::info!(
            session_id,
            "created interrupted session for recovered audio"
        );
    }
    if latest_run(conn, session_id)?.is_none() {
        let model = config.model_for_provider(config.transcription.provider);
        insert_run(conn, session_id, config.transcription.provider, &model)?;
    }
    Ok(())
}

pub fn parse_chunk_filename(path: &Path) -> Result<(u64, chrono::DateTime<chrono::Utc>)> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            crate::error::AppError::other(format!(
                "audio filename is not valid UTF-8: {}",
                path.display()
            ))
        })?;
    let Some(stem) = name.strip_suffix(".flac") else {
        return Err(crate::error::AppError::other(format!(
            "audio file is not a .flac chunk: {name}"
        )));
    };
    let Some((seq, stamp)) = stem.split_once('_') else {
        return Err(crate::error::AppError::other(format!(
            "audio filename '{name}' does not match <sequence>_<timestamp>.flac"
        )));
    };
    let sequence: u64 = seq.parse().map_err(|_| {
        crate::error::AppError::other(format!(
            "invalid sequence number in audio filename '{name}'"
        ))
    })?;
    // Filename uses hyphens in the time portion: 2026-08-17T12-32-04.120Z
    let restored = if let Some((date, time)) = stamp.split_once('T') {
        format!("{date}T{}", time.replace('-', ":"))
    } else {
        stamp.to_string()
    };
    let started_at = parse_rfc3339(&restored)?;
    Ok((sequence, started_at))
}

fn walk_files(root: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?.path();
            if entry.is_dir() {
                stack.push(entry);
            } else {
                files.push(entry);
            }
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::parse_chunk_filename;
    use chrono::{TimeZone, Utc};
    use std::path::Path;

    #[test]
    fn parse_spec_filename() {
        let path = Path::new("00000012_2026-08-17T12-32-04.120Z.flac");
        let (seq, started) = parse_chunk_filename(path).unwrap();
        assert_eq!(seq, 12);
        assert_eq!(
            started,
            Utc.with_ymd_and_hms(2026, 8, 17, 12, 32, 4).unwrap()
                + chrono::Duration::milliseconds(120)
        );
    }
}

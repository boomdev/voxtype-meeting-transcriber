use rusqlite::{params, Connection, OptionalExtension};
use uuid::Uuid;

use crate::audio::AudioSource;
use crate::config::ProviderKind;
use crate::error::Result;
use crate::timeutil::now_rfc3339;
use crate::transcription::TranscriptionResult;

#[derive(Clone, Debug)]
pub struct TranscriptEventRecord {
    pub id: String,
    pub session_id: String,
    pub audio_chunk_id: String,
    pub job_id: String,
    pub source: AudioSource,
    pub sequence: u64,
    pub started_at: String,
    pub ended_at: String,
    pub text: String,
    pub provider: ProviderKind,
    pub model: String,
    pub is_canonical: bool,
    pub created_at: String,
}

pub fn record_transcription_success(
    conn: &mut Connection,
    job_id: &str,
    result: &TranscriptionResult,
) -> Result<bool> {
    let tx = conn.transaction()?;
    let existing: Option<String> = tx
        .query_row(
            "SELECT id FROM transcript_events WHERE job_id = ?1",
            params![job_id],
            |row| row.get(0),
        )
        .optional()?;
    if existing.is_some() {
        tx.execute(
            "UPDATE transcription_jobs SET state = 'completed', completed_at = COALESCE(completed_at, ?1)
             WHERE id = ?2",
            params![now_rfc3339(), job_id],
        )?;
        tx.commit()?;
        return Ok(false);
    }

    let job = tx.query_row(
        "SELECT audio_chunk_id, session_id FROM transcription_jobs j
         JOIN audio_chunks c ON c.id = j.audio_chunk_id
         WHERE j.id = ?1",
        params![job_id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )?;
    let (audio_chunk_id, session_id) = job;
    let chunk = tx.query_row(
        "SELECT source, sequence, started_at, ended_at, file_path FROM audio_chunks WHERE id = ?1",
        params![audio_chunk_id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        },
    )?;
    let (source, sequence, chunk_started_at, chunk_ended_at, file_path) = chunk;
    let (started_at, ended_at) =
        aligned_times_for_event(&result.text, &file_path, &chunk_started_at, &chunk_ended_at);
    let event_id = Uuid::new_v4().to_string();
    let now = now_rfc3339();

    tx.execute(
        "UPDATE transcript_events SET is_canonical = 0
         WHERE audio_chunk_id = ?1 AND is_canonical = 1",
        params![audio_chunk_id],
    )?;
    match tx.execute(
        "INSERT INTO transcript_events (
            id, session_id, audio_chunk_id, job_id, source, sequence,
            started_at, ended_at, text, provider, model, is_canonical, created_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 1, ?12)",
        params![
            event_id,
            session_id,
            audio_chunk_id,
            job_id,
            source,
            sequence,
            started_at,
            ended_at,
            result.text,
            result.provider.as_str(),
            result.model,
            now
        ],
    ) {
        Ok(_) => {}
        Err(error) if is_unique_constraint(&error) => {
            tx.rollback()?;
            conn.execute(
                "UPDATE transcription_jobs SET state = 'completed', completed_at = COALESCE(completed_at, ?1)
                 WHERE id = ?2 AND id IN (SELECT job_id FROM transcript_events)",
                params![now_rfc3339(), job_id],
            )?;
            return Ok(false);
        }
        Err(error) => return Err(error.into()),
    }
    tx.execute(
        "UPDATE transcription_jobs
         SET state = 'completed', completed_at = ?1, last_error = NULL
         WHERE id = ?2",
        params![now, job_id],
    )?;
    tx.commit()?;
    Ok(true)
}

fn is_unique_constraint(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(info, _)
            if info.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

/// Move canonical event times from the capture-chunk window onto speech inside
/// that chunk's FLAC. Chunk file timestamps are left unchanged. Empty text keeps
/// the window so silence still occupies its original slot.
pub fn restamp_canonical_events_from_audio(conn: &Connection, session_id: &str) -> Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT e.id, e.text, c.file_path, c.started_at, c.ended_at
         FROM transcript_events e
         JOIN audio_chunks c ON c.id = e.audio_chunk_id
         WHERE e.session_id = ?1 AND e.is_canonical = 1",
    )?;
    let rows: Vec<(String, String, String, String, String)> = stmt
        .query_map(params![session_id], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);
    let mut updated = 0usize;
    for (id, text, file_path, chunk_started_at, chunk_ended_at) in rows {
        let (started_at, ended_at) =
            aligned_times_for_event(&text, &file_path, &chunk_started_at, &chunk_ended_at);
        let changed = conn.execute(
            "UPDATE transcript_events SET started_at = ?1, ended_at = ?2 WHERE id = ?3
             AND (started_at != ?1 OR ended_at != ?2)",
            params![started_at, ended_at, id],
        )?;
        updated = updated.saturating_add(changed);
    }
    Ok(updated)
}

fn aligned_times_for_event(
    text: &str,
    file_path: &str,
    chunk_started_at: &str,
    chunk_ended_at: &str,
) -> (String, String) {
    if text.trim().is_empty() {
        return (chunk_started_at.to_string(), chunk_ended_at.to_string());
    }
    let path = std::path::Path::new(file_path);
    if !path.is_file() {
        return (chunk_started_at.to_string(), chunk_ended_at.to_string());
    }
    let Ok(chunk_start) = crate::timeutil::parse_rfc3339(chunk_started_at) else {
        return (chunk_started_at.to_string(), chunk_ended_at.to_string());
    };
    let Ok(chunk_end) = crate::timeutil::parse_rfc3339(chunk_ended_at) else {
        return (chunk_started_at.to_string(), chunk_ended_at.to_string());
    };
    match crate::encode::decode_flac_i16_mono_16k(path) {
        Ok(samples) => {
            let (start, end) =
                crate::audio::speech::align_chunk_times_16k(chunk_start, chunk_end, &samples);
            (
                crate::timeutil::datetime_rfc3339(start),
                crate::timeutil::datetime_rfc3339(end),
            )
        }
        Err(error) => {
            tracing::warn!(
                error = %error,
                file_path,
                "could not decode chunk audio for transcript timing; using chunk window"
            );
            (chunk_started_at.to_string(), chunk_ended_at.to_string())
        }
    }
}

pub fn list_canonical_events(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<TranscriptEventRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, session_id, audio_chunk_id, job_id, source, sequence,
                started_at, ended_at, text, provider, model, is_canonical, created_at
         FROM transcript_events
         WHERE session_id = ?1 AND is_canonical = 1",
    )?;
    let mut rows = stmt.query(params![session_id])?;
    let mut events = Vec::new();
    while let Some(row) = rows.next()? {
        events.push(event_from_row(row)?);
    }
    Ok(events)
}

pub fn transcribed_chunk_ids(conn: &Connection, session_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT DISTINCT audio_chunk_id FROM transcript_events WHERE session_id = ?1")?;
    let mut rows = stmt.query(params![session_id])?;
    let mut ids = Vec::new();
    while let Some(row) = rows.next()? {
        ids.push(row.get(0)?);
    }
    Ok(ids)
}

fn event_from_row(row: &rusqlite::Row<'_>) -> Result<TranscriptEventRecord> {
    Ok(TranscriptEventRecord {
        id: row.get(0)?,
        session_id: row.get(1)?,
        audio_chunk_id: row.get(2)?,
        job_id: row.get(3)?,
        source: crate::storage::source_from_str(&row.get::<_, String>(4)?)?,
        sequence: row.get::<_, i64>(5)? as u64,
        started_at: row.get(6)?,
        ended_at: row.get(7)?,
        text: row.get(8)?,
        provider: ProviderKind::parse(&row.get::<_, String>(9)?)?,
        model: row.get(10)?,
        is_canonical: row.get::<_, i64>(11)? == 1,
        created_at: row.get(12)?,
    })
}

pub fn mark_job_retry(
    conn: &Connection,
    job_id: &str,
    error: &str,
    delay: std::time::Duration,
) -> Result<()> {
    let next = chrono::Utc::now()
        + chrono::Duration::from_std(delay).unwrap_or(chrono::Duration::seconds(5));
    conn.execute(
        "UPDATE transcription_jobs
         SET state = 'pending',
             started_at = NULL,
             attempt_count = attempt_count + 1,
             last_error = ?1,
             next_retry_at = ?2
         WHERE id = ?3",
        params![error, crate::timeutil::datetime_rfc3339(next), job_id],
    )?;
    Ok(())
}

pub fn claim_due_jobs(conn: &mut Connection, limit: u32) -> Result<Vec<String>> {
    let tx = conn.transaction()?;
    let now = now_rfc3339();
    let processing: i64 = tx.query_row(
        "SELECT COUNT(*) FROM transcription_jobs WHERE state = 'processing'",
        [],
        |row| row.get(0),
    )?;
    let available = limit.saturating_sub(processing as u32);
    if available == 0 {
        tx.commit()?;
        return Ok(Vec::new());
    }
    let mut stmt = tx.prepare(
        "SELECT id FROM transcription_jobs
         WHERE state = 'pending'
           AND (next_retry_at IS NULL OR next_retry_at <= ?1)
         ORDER BY next_retry_at, created_at
         LIMIT ?2",
    )?;
    let ids: Vec<String> = stmt
        .query_map(params![now, available as i64], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);
    for id in &ids {
        tx.execute(
            "UPDATE transcription_jobs SET state = 'processing', started_at = ?1 WHERE id = ?2",
            params![now, id],
        )?;
    }
    tx.commit()?;
    Ok(ids)
}

pub fn get_job_provider(
    conn: &Connection,
    job_id: &str,
) -> Result<(ProviderKind, Option<String>, String)> {
    let (provider, model, chunk_id): (String, Option<String>, String) = conn.query_row(
        "SELECT provider, model, audio_chunk_id FROM transcription_jobs WHERE id = ?1",
        params![job_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    Ok((ProviderKind::parse(&provider)?, model, chunk_id))
}

use rusqlite::{params, Connection};
use uuid::Uuid;

use crate::audio::AudioSource;
use crate::config::ProviderKind;
use crate::error::Result;
use crate::storage::types::{AudioChunkRecord, JobState, TranscriptionJobRecord};
use crate::timeutil::{datetime_rfc3339, now_rfc3339};

pub fn next_sequence(conn: &Connection, session_id: &str, source: AudioSource) -> Result<u64> {
    let max: Option<i64> = conn.query_row(
        "SELECT MAX(sequence) FROM audio_chunks WHERE session_id = ?1 AND source = ?2",
        params![session_id, source.as_str()],
        |row| row.get(0),
    )?;
    Ok(max.unwrap_or(0) as u64 + 1)
}

pub fn sequence_exists(
    conn: &Connection,
    session_id: &str,
    source: AudioSource,
    sequence: u64,
) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM audio_chunks
         WHERE session_id = ?1 AND source = ?2 AND sequence = ?3",
        params![session_id, source.as_str(), sequence as i64],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

pub fn chunk_exists_for_path(conn: &Connection, file_path: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM audio_chunks WHERE file_path = ?1",
        params![file_path],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

pub struct NewAudioChunk<'a> {
    pub session_id: &'a str,
    pub run_id: &'a str,
    pub source: AudioSource,
    pub sequence: u64,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub ended_at: chrono::DateTime<chrono::Utc>,
    pub file_path: &'a str,
    pub duration_ms: i64,
    pub provider: ProviderKind,
    pub model: &'a str,
}

pub fn insert_chunk_and_job(
    conn: &mut Connection,
    spec: NewAudioChunk<'_>,
) -> Result<(AudioChunkRecord, TranscriptionJobRecord)> {
    let tx = conn.transaction()?;
    let chunk_id = Uuid::new_v4().to_string();
    let job_id = Uuid::new_v4().to_string();
    let now = now_rfc3339();
    let started = datetime_rfc3339(spec.started_at);
    let ended = datetime_rfc3339(spec.ended_at);

    tx.execute(
        "INSERT INTO audio_chunks (
            id, session_id, source, sequence, started_at, ended_at, file_path, duration_ms, created_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            chunk_id,
            spec.session_id,
            spec.source.as_str(),
            spec.sequence as i64,
            started,
            ended,
            spec.file_path,
            spec.duration_ms,
            now
        ],
    )?;
    tx.execute(
        "INSERT INTO transcription_jobs (
            id, audio_chunk_id, run_id, provider, model, state, attempt_count,
            last_error, next_retry_at, created_at, started_at, completed_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, NULL, ?6, ?6, NULL, NULL)",
        params![
            job_id,
            chunk_id,
            spec.run_id,
            spec.provider.as_str(),
            spec.model,
            now
        ],
    )?;
    tx.commit()?;

    Ok((
        AudioChunkRecord {
            id: chunk_id.clone(),
            session_id: spec.session_id.to_string(),
            source: spec.source,
            sequence: spec.sequence,
            started_at: started,
            ended_at: ended,
            file_path: spec.file_path.to_string(),
            duration_ms: spec.duration_ms,
            created_at: now.clone(),
        },
        TranscriptionJobRecord {
            id: job_id,
            audio_chunk_id: chunk_id,
            run_id: spec.run_id.to_string(),
            provider: spec.provider,
            model: Some(spec.model.to_string()),
            state: JobState::Pending,
            attempt_count: 0,
            last_error: None,
            next_retry_at: Some(now.clone()),
            created_at: now,
            started_at: None,
            completed_at: None,
        },
    ))
}

pub fn get_chunk(conn: &Connection, id: &str) -> Result<Option<AudioChunkRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, session_id, source, sequence, started_at, ended_at, file_path, duration_ms, created_at
         FROM audio_chunks WHERE id = ?1",
    )?;
    let mut rows = stmt.query(params![id])?;
    match rows.next()? {
        Some(row) => Ok(Some(chunk_from_row(row)?)),
        None => Ok(None),
    }
}

pub fn list_chunks_for_session(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<AudioChunkRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, session_id, source, sequence, started_at, ended_at, file_path, duration_ms, created_at
         FROM audio_chunks WHERE session_id = ?1 ORDER BY started_at, source, sequence",
    )?;
    let mut rows = stmt.query(params![session_id])?;
    let mut chunks = Vec::new();
    while let Some(row) = rows.next()? {
        chunks.push(chunk_from_row(row)?);
    }
    Ok(chunks)
}

fn chunk_from_row(row: &rusqlite::Row<'_>) -> Result<AudioChunkRecord> {
    let source: String = row.get(2)?;
    let source = crate::storage::source_from_str(&source)?;
    Ok(AudioChunkRecord {
        id: row.get(0)?,
        session_id: row.get(1)?,
        source,
        sequence: row.get::<_, i64>(3)? as u64,
        started_at: row.get(4)?,
        ended_at: row.get(5)?,
        file_path: row.get(6)?,
        duration_ms: row.get(7)?,
        created_at: row.get(8)?,
    })
}

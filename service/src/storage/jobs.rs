use rusqlite::{params, Connection};

use crate::config::ProviderKind;
use crate::error::Result;
use crate::storage::types::{JobState, TranscriptionJobRecord};
use crate::timeutil::now_rfc3339;

pub fn reset_processing_without_event(conn: &Connection) -> Result<usize> {
    let changed = conn.execute(
        "UPDATE transcription_jobs
         SET state = 'pending', started_at = NULL
         WHERE state = 'processing'
           AND id NOT IN (SELECT job_id FROM transcript_events)",
        [],
    )?;
    Ok(changed)
}

pub fn complete_jobs_that_already_have_events(conn: &Connection) -> Result<usize> {
    let now = now_rfc3339();
    let changed = conn.execute(
        "UPDATE transcription_jobs
         SET state = 'completed', completed_at = COALESCE(completed_at, ?1)
         WHERE state = 'processing'
           AND id IN (SELECT job_id FROM transcript_events)",
        params![now],
    )?;
    Ok(changed)
}

pub fn retry_pending(
    conn: &Connection,
    override_provider: Option<(ProviderKind, String)>,
) -> Result<usize> {
    let now = now_rfc3339();
    let changed = if let Some((provider, model)) = override_provider {
        conn.execute(
            "UPDATE transcription_jobs
             SET provider = ?1, model = ?2, next_retry_at = ?3
             WHERE state = 'pending'",
            params![provider.as_str(), model, now],
        )?
    } else {
        conn.execute(
            "UPDATE transcription_jobs SET next_retry_at = ?1 WHERE state = 'pending'",
            params![now],
        )?
    };
    Ok(changed)
}

pub fn queue_retranscribe(
    conn: &mut Connection,
    session_id: &str,
    provider: ProviderKind,
    model: &str,
) -> Result<(String, usize)> {
    let tx = conn.transaction()?;
    if crate::storage::sessions::get_session(&tx, session_id)?.is_none() {
        return Err(crate::error::AppError::other(format!(
            "Session {session_id} not found"
        )));
    }
    let run = crate::storage::sessions::insert_run(&tx, session_id, provider, model)?;
    let chunks = crate::storage::chunks::list_chunks_for_session(&tx, session_id)?;
    let now = now_rfc3339();
    for chunk in &chunks {
        let job_id = uuid::Uuid::new_v4().to_string();
        tx.execute(
            "INSERT INTO transcription_jobs (
                id, audio_chunk_id, run_id, provider, model, state, attempt_count,
                last_error, next_retry_at, created_at, started_at, completed_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, NULL, ?6, ?6, NULL, NULL)",
            params![job_id, chunk.id, run.id, provider.as_str(), model, now],
        )?;
    }
    let run_id = run.id;
    let count = chunks.len();
    tx.commit()?;
    Ok((run_id, count))
}

pub fn retry_session_pending(conn: &Connection, session_id: &str) -> Result<usize> {
    let now = now_rfc3339();
    let changed = conn.execute(
        "UPDATE transcription_jobs SET next_retry_at = ?1
         WHERE state = 'pending'
           AND audio_chunk_id IN (SELECT id FROM audio_chunks WHERE session_id = ?2)",
        params![now, session_id],
    )?;
    Ok(changed)
}

pub fn list_jobs_for_session(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<TranscriptionJobRecord>> {
    let mut stmt = conn.prepare(
        "SELECT j.id, j.audio_chunk_id, j.run_id, j.provider, j.model, j.state, j.attempt_count,
                j.last_error, j.next_retry_at, j.created_at, j.started_at, j.completed_at
         FROM transcription_jobs j
         JOIN audio_chunks c ON c.id = j.audio_chunk_id
         WHERE c.session_id = ?1",
    )?;
    let mut rows = stmt.query(params![session_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(job_from_row(row)?);
    }
    Ok(out)
}

pub fn count_by_state(conn: &Connection, state: JobState) -> Result<i64> {
    let count = conn.query_row(
        "SELECT COUNT(*) FROM transcription_jobs WHERE state = ?1",
        params![state.as_str()],
        |row| row.get(0),
    )?;
    Ok(count)
}

pub fn counts(conn: &Connection) -> Result<(i64, i64, i64)> {
    Ok((
        count_by_state(conn, JobState::Pending)?,
        count_by_state(conn, JobState::Processing)?,
        count_by_state(conn, JobState::Completed)?,
    ))
}

pub fn get_job(conn: &Connection, id: &str) -> Result<Option<TranscriptionJobRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, audio_chunk_id, run_id, provider, model, state, attempt_count,
                last_error, next_retry_at, created_at, started_at, completed_at
         FROM transcription_jobs WHERE id = ?1",
    )?;
    let mut rows = stmt.query(params![id])?;
    if let Some(row) = rows.next()? {
        Ok(Some(job_from_row(row)?))
    } else {
        Ok(None)
    }
}

pub fn job_from_row(row: &rusqlite::Row<'_>) -> Result<TranscriptionJobRecord> {
    Ok(TranscriptionJobRecord {
        id: row.get(0)?,
        audio_chunk_id: row.get(1)?,
        run_id: row.get(2)?,
        provider: ProviderKind::parse(&row.get::<_, String>(3)?)?,
        model: row.get(4)?,
        state: JobState::parse(&row.get::<_, String>(5)?)?,
        attempt_count: row.get(6)?,
        last_error: row.get(7)?,
        next_retry_at: row.get(8)?,
        created_at: row.get(9)?,
        started_at: row.get(10)?,
        completed_at: row.get(11)?,
    })
}

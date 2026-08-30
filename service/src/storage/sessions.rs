use std::fs;
use std::path::Path;

use rusqlite::{params, Connection};
use uuid::Uuid;

use crate::audio::AudioDevice;
use crate::config::ProviderKind;
use crate::error::Result;
use crate::storage::types::{
    DeviceSnapshot, SessionRecord, SessionSnapshot, SessionState, TranscriptionRunRecord,
};
use crate::timeutil::now_rfc3339;

pub fn insert_running_session(
    conn: &Connection,
    id: &str,
    microphone: Option<&AudioDevice>,
    output: Option<&AudioDevice>,
    monitor: Option<&AudioDevice>,
) -> Result<SessionRecord> {
    let now = now_rfc3339();
    conn.execute(
        "INSERT INTO sessions (
            id, state, started_at, ended_at,
            microphone_id, microphone_description,
            output_id, output_description,
            monitor_id, monitor_description,
            created_at, title
        ) VALUES (?1, 'running', ?2, NULL, ?3, ?4, ?5, ?6, ?7, ?8, ?2, NULL)",
        params![
            id,
            now,
            microphone.map(|d| d.id.as_str()),
            microphone.map(|d| d.description.as_str()),
            output.map(|d| d.id.as_str()),
            output.map(|d| d.description.as_str()),
            monitor.map(|d| d.id.as_str()),
            monitor.map(|d| d.description.as_str()),
        ],
    )?;
    get_session(conn, id)?.ok_or_else(|| {
        crate::error::AppError::other("session insert succeeded but the row was not found")
    })
}

pub fn insert_run(
    conn: &Connection,
    session_id: &str,
    provider: ProviderKind,
    model: &str,
) -> Result<TranscriptionRunRecord> {
    let id = Uuid::new_v4().to_string();
    let now = now_rfc3339();
    conn.execute(
        "INSERT INTO transcription_runs (id, session_id, provider, model, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, session_id, provider.as_str(), model, now],
    )?;
    Ok(TranscriptionRunRecord {
        id,
        session_id: session_id.to_string(),
        provider,
        model: Some(model.to_string()),
        created_at: now,
    })
}

pub fn get_session(conn: &Connection, id: &str) -> Result<Option<SessionRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, state, started_at, ended_at,
                microphone_id, microphone_description,
                output_id, output_description,
                monitor_id, monitor_description, created_at, title
         FROM sessions WHERE id = ?1",
    )?;
    let mut rows = stmt.query(params![id])?;
    if let Some(row) = rows.next()? {
        Ok(Some(session_from_row(row)?))
    } else {
        Ok(None)
    }
}

pub fn latest_run(conn: &Connection, session_id: &str) -> Result<Option<TranscriptionRunRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, session_id, provider, model, created_at
         FROM transcription_runs
         WHERE session_id = ?1
         ORDER BY created_at DESC
         LIMIT 1",
    )?;
    let mut rows = stmt.query(params![session_id])?;
    if let Some(row) = rows.next()? {
        Ok(Some(TranscriptionRunRecord {
            id: row.get(0)?,
            session_id: row.get(1)?,
            provider: ProviderKind::parse(&row.get::<_, String>(2)?)?,
            model: row.get(3)?,
            created_at: row.get(4)?,
        }))
    } else {
        Ok(None)
    }
}

pub fn set_session_state(
    conn: &Connection,
    id: &str,
    state: SessionState,
    ended_at: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE sessions SET state = ?1, ended_at = ?2 WHERE id = ?3",
        params![state.as_str(), ended_at, id],
    )?;
    Ok(())
}

pub fn get_running_session(conn: &Connection) -> Result<Option<SessionRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, state, started_at, ended_at,
                microphone_id, microphone_description,
                output_id, output_description,
                monitor_id, monitor_description, created_at, title
         FROM sessions WHERE state = 'running'
         ORDER BY started_at DESC LIMIT 1",
    )?;
    let mut rows = stmt.query([])?;
    if let Some(row) = rows.next()? {
        Ok(Some(session_from_row(row)?))
    } else {
        Ok(None)
    }
}

pub fn interrupt_running_sessions(conn: &Connection) -> Result<usize> {
    let now = now_rfc3339();
    let changed = conn.execute(
        "UPDATE sessions SET state = 'interrupted', ended_at = ?1 WHERE state = 'running'",
        params![now],
    )?;
    Ok(changed)
}

pub fn list_sessions(conn: &Connection) -> Result<Vec<SessionRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, state, started_at, ended_at,
                microphone_id, microphone_description,
                output_id, output_description,
                monitor_id, monitor_description, created_at, title
         FROM sessions
         ORDER BY started_at DESC",
    )?;
    let mut rows = stmt.query([])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(session_from_row(row)?);
    }
    Ok(out)
}

pub fn set_session_title(conn: &Connection, id: &str, title: &str) -> Result<()> {
    let changed = conn.execute(
        "UPDATE sessions SET title = ?1 WHERE id = ?2",
        params![title, id],
    )?;
    if changed == 0 {
        return Err(crate::error::AppError::other(format!(
            "Session {id} not found"
        )));
    }
    Ok(())
}

pub fn write_session_json(session_dir: &Path, snapshot: &SessionSnapshot) -> Result<()> {
    crate::paths::ensure_dir(session_dir)?;
    let tmp = session_dir.join("session.json.tmp");
    let dest = session_dir.join("session.json");
    fs::write(&tmp, serde_json::to_vec_pretty(snapshot)?)?;
    fs::rename(tmp, dest)?;
    Ok(())
}

pub fn snapshot_from_record(record: &SessionRecord) -> SessionSnapshot {
    SessionSnapshot {
        id: record.id.clone(),
        state: record.state,
        started_at: record.started_at.clone(),
        ended_at: record.ended_at.clone(),
        microphone: match (&record.microphone_id, &record.microphone_description) {
            (Some(id), Some(description)) => Some(DeviceSnapshot {
                id: id.clone(),
                description: description.clone(),
            }),
            _ => None,
        },
        output: match (&record.output_id, &record.output_description) {
            (Some(id), Some(description)) => Some(DeviceSnapshot {
                id: id.clone(),
                description: description.clone(),
            }),
            _ => None,
        },
        monitor: match (&record.monitor_id, &record.monitor_description) {
            (Some(id), Some(description)) => Some(DeviceSnapshot {
                id: id.clone(),
                description: description.clone(),
            }),
            _ => None,
        },
        title: record.title.clone(),
    }
}

fn session_from_row(row: &rusqlite::Row<'_>) -> Result<SessionRecord> {
    Ok(SessionRecord {
        id: row.get(0)?,
        state: SessionState::parse(&row.get::<_, String>(1)?)?,
        started_at: row.get(2)?,
        ended_at: row.get(3)?,
        microphone_id: row.get(4)?,
        microphone_description: row.get(5)?,
        output_id: row.get(6)?,
        output_description: row.get(7)?,
        monitor_id: row.get(8)?,
        monitor_description: row.get(9)?,
        created_at: row.get(10)?,
        title: row.get(11)?,
    })
}

use std::cmp::Ordering;

use crate::audio::AudioSource;
use crate::storage::events::TranscriptEventRecord;
use crate::timeutil::parse_rfc3339;

pub fn sort_events(events: &mut [TranscriptEventRecord]) {
    events.sort_by(cmp_events);
}

pub fn cmp_events(a: &TranscriptEventRecord, b: &TranscriptEventRecord) -> Ordering {
    let a_start = parse_rfc3339(&a.started_at).ok();
    let b_start = parse_rfc3339(&b.started_at).ok();
    a_start
        .cmp(&b_start)
        .then_with(|| a.source.as_str().cmp(b.source.as_str()))
        .then_with(|| a.sequence.cmp(&b.sequence))
}

pub fn render_markdown(
    session_id: &str,
    started_at: &str,
    events: &[TranscriptEventRecord],
    omit_single_source_headers: bool,
    title: Option<&str>,
) -> String {
    let started_display = format_started_header(started_at);
    let mut out = String::new();
    out.push_str("# Audio session\n\n");
    if let Some(title) = title.map(str::trim).filter(|value| !value.is_empty()) {
        out.push_str(&format!("Title: {title}\n"));
    }
    out.push_str(&format!("Session: {session_id}\n"));
    out.push_str(&format!("Started: {started_display}\n\n"));
    out.push_str("## Transcript\n");
    let hide_headers = omit_single_source_headers && sources_with_text(events) <= 1;
    for event in events {
        let text = event.text.trim();
        if hide_headers && text.is_empty() {
            continue;
        }
        out.push('\n');
        if !hide_headers {
            let time = format_time_of_day(&event.started_at);
            let label = match event.source {
                AudioSource::Mic => "MIC",
                AudioSource::System => "SYSTEM",
            };
            out.push_str(&format!("{time} [{label}]\n"));
        }
        out.push_str(text);
        out.push('\n');
    }
    out
}

fn sources_with_text(events: &[TranscriptEventRecord]) -> usize {
    let mut mic = false;
    let mut system = false;
    for event in events {
        if event.text.trim().is_empty() {
            continue;
        }
        match event.source {
            AudioSource::Mic => mic = true,
            AudioSource::System => system = true,
        }
    }
    usize::from(mic) + usize::from(system)
}

pub fn render_jsonl(events: &[TranscriptEventRecord]) -> crate::error::Result<String> {
    let mut out = String::new();
    for event in events {
        let started = format_offset(&event.started_at)?;
        let ended = format_offset(&event.ended_at)?;
        let line = serde_json::json!({
            "source": event.source.as_str(),
            "sequence": event.sequence,
            "started_at": started,
            "ended_at": ended,
            "text": event.text,
            "provider": event.provider.as_str(),
            "model": event.model,
        });
        out.push_str(&serde_json::to_string(&line)?);
        out.push('\n');
    }
    Ok(out)
}

pub fn regenerate_session_transcripts(
    db: &crate::storage::Db,
    session_id: &str,
    session_dir: &std::path::Path,
    omit_single_source_headers: bool,
) -> crate::error::Result<()> {
    let (started_at, title, events) = db.with_conn(|conn| {
        crate::storage::events::restamp_canonical_events_from_audio(conn, session_id)?;
        let session =
            crate::storage::sessions::get_session(conn, session_id)?.ok_or_else(|| {
                crate::error::AppError::other(format!("Session {session_id} not found"))
            })?;
        let mut events = crate::storage::events::list_canonical_events(conn, session_id)?;
        sort_events(&mut events);
        Ok((session.started_at, session.title, events))
    })?;
    let markdown = render_markdown(
        session_id,
        &started_at,
        &events,
        omit_single_source_headers,
        title.as_deref(),
    );
    let jsonl = render_jsonl(&events)?;
    atomic_replace(session_dir, "transcript.md", markdown.as_bytes())?;
    atomic_replace(session_dir, "transcript.jsonl", jsonl.as_bytes())?;
    tracing::info!(session_id, events = events.len(), "transcript regenerated");
    Ok(())
}

fn atomic_replace(dir: &std::path::Path, name: &str, bytes: &[u8]) -> crate::error::Result<()> {
    crate::paths::ensure_dir(dir)?;
    let dest = dir.join(name);
    let tmp = dir.join(format!("{name}.tmp"));
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(tmp, dest)?;
    Ok(())
}

fn format_started_header(rfc3339: &str) -> String {
    match parse_rfc3339(rfc3339) {
        Ok(utc) => utc
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S %:z")
            .to_string(),
        Err(_) => rfc3339.to_string(),
    }
}

fn format_time_of_day(rfc3339: &str) -> String {
    match parse_rfc3339(rfc3339) {
        Ok(utc) => utc
            .with_timezone(&chrono::Local)
            .format("%H:%M:%S")
            .to_string(),
        Err(_) => rfc3339.to_string(),
    }
}

fn format_offset(rfc3339: &str) -> crate::error::Result<String> {
    let utc = parse_rfc3339(rfc3339)?;
    Ok(utc
        .with_timezone(&chrono::Local)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

#[cfg(test)]
mod tests {
    use super::{cmp_events, render_jsonl, render_markdown, sort_events};
    use crate::audio::AudioSource;
    use crate::config::ProviderKind;
    use crate::storage::events::TranscriptEventRecord;

    fn event(source: AudioSource, seq: u64, started: &str, text: &str) -> TranscriptEventRecord {
        TranscriptEventRecord {
            id: format!("{source}-{seq}"),
            session_id: "s".into(),
            audio_chunk_id: format!("c{seq}"),
            job_id: format!("j{seq}"),
            source,
            sequence: seq,
            started_at: started.into(),
            ended_at: started.into(),
            text: text.into(),
            provider: ProviderKind::Voxtype,
            model: "configured".into(),
            is_canonical: true,
            created_at: started.into(),
        }
    }

    #[test]
    fn order_by_started_then_source_then_sequence() {
        let later = event(
            AudioSource::Mic,
            1,
            "2026-08-17T14:32:09.000+02:00",
            "later",
        );
        let earlier_sys = event(
            AudioSource::System,
            1,
            "2026-08-17T14:32:04.000+02:00",
            "sys",
        );
        let earlier_mic = event(AudioSource::Mic, 1, "2026-08-17T14:32:04.000+02:00", "mic");
        let mut events = vec![later, earlier_sys.clone(), earlier_mic.clone()];
        sort_events(&mut events);
        assert_eq!(events[0].text, "mic");
        assert_eq!(events[1].text, "sys");
        assert_eq!(events[2].text, "later");
        assert_eq!(
            cmp_events(&earlier_mic, &earlier_sys),
            std::cmp::Ordering::Less
        );
    }

    #[test]
    fn markdown_shape() {
        let events = vec![event(
            AudioSource::Mic,
            12,
            "2026-08-17T12:32:04.120Z",
            "I think we should change the pricing.",
        )];
        let md = render_markdown(
            "90e351d4-24cc-4ca1-bfd7-87d36aa9b021",
            "2026-08-17T12:32:01.000Z",
            &events,
            false,
            None,
        );
        assert!(md.contains("# Audio session"));
        assert!(md.contains("Session: 90e351d4-24cc-4ca1-bfd7-87d36aa9b021"));
        assert!(md.contains("[MIC]"));
        assert!(md.contains("I think we should change the pricing."));
        assert!(!md.contains("gpt-4o-transcribe"));
    }

    #[test]
    fn omit_headers_when_only_one_source_has_text() {
        let events = vec![
            event(AudioSource::System, 1, "2026-08-17T16:43:05.000Z", "   "),
            event(
                AudioSource::Mic,
                1,
                "2026-08-17T16:43:05.000Z",
                "All right, so the first version.",
            ),
            event(AudioSource::System, 2, "2026-08-17T16:43:35.000Z", ""),
            event(
                AudioSource::Mic,
                2,
                "2026-08-17T16:43:35.000Z",
                "It has to be a small icon.",
            ),
        ];
        let md = render_markdown(
            "session-id",
            "2026-08-17T16:43:03.000Z",
            &events,
            true,
            None,
        );
        assert!(md.contains("All right, so the first version."));
        assert!(md.contains("It has to be a small icon."));
        assert!(!md.contains("[MIC]"));
        assert!(!md.contains("[SYSTEM]"));
        assert!(!md.contains("16:43:05"));
        assert!(!md.contains("18:43:05"));
    }

    #[test]
    fn keep_headers_when_both_sources_have_text() {
        let events = vec![
            event(AudioSource::Mic, 1, "2026-08-17T12:32:04.000Z", "hello"),
            event(AudioSource::System, 1, "2026-08-17T12:32:04.000Z", "world"),
        ];
        let md = render_markdown("s", "2026-08-17T12:32:01.000Z", &events, true, None);
        assert!(md.contains("[MIC]"));
        assert!(md.contains("[SYSTEM]"));
        assert!(md.contains("hello"));
        assert!(md.contains("world"));
    }

    #[test]
    fn keep_headers_when_setting_is_off() {
        let events = vec![event(
            AudioSource::Mic,
            1,
            "2026-08-17T12:32:04.000Z",
            "hello",
        )];
        let md = render_markdown("s", "2026-08-17T12:32:01.000Z", &events, false, None);
        assert!(md.contains("[MIC]"));
        assert!(md.contains("hello"));
    }

    #[test]
    fn jsonl_one_object_per_line() {
        let events = vec![event(
            AudioSource::Mic,
            12,
            "2026-08-17T12:32:04.120Z",
            "hello",
        )];
        let jsonl = render_jsonl(&events).unwrap();
        let line = jsonl.trim();
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(value["source"], "mic");
        assert_eq!(value["sequence"], 12);
        assert_eq!(value["text"], "hello");
        assert_eq!(value["provider"], "voxtype");
    }
}

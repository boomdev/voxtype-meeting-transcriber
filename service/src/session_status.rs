use chrono::{DateTime, Utc};

use crate::config::{Config, ProviderKind};
use crate::storage::types::{
    AudioChunkRecord, JobState, SessionRecord, SessionState, TranscriptionJobRecord,
};
use crate::timeutil::parse_rfc3339;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UiSessionStatus {
    Recording,
    Transcribing,
    Complete,
    WaitingRetry,
    Attention,
    Interrupted,
}

impl UiSessionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Recording => "recording",
            Self::Transcribing => "transcribing",
            Self::Complete => "complete",
            Self::WaitingRetry => "waiting_retry",
            Self::Attention => "attention",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn display_label(self) -> &'static str {
        match self {
            Self::Recording => "Recording",
            Self::Transcribing => "Transcribing",
            Self::Complete => "Complete",
            Self::WaitingRetry => "Waiting to retry",
            Self::Attention => "Needs attention",
            Self::Interrupted => "Interrupted",
        }
    }
}

#[derive(Clone, Debug)]
pub struct StatusContext<'a> {
    pub config: &'a Config,
    pub now: DateTime<Utc>,
}

pub fn status_context(config: &Config) -> StatusContext<'_> {
    StatusContext {
        config,
        now: Utc::now(),
    }
}

pub fn derive_ui_status(
    session: &SessionRecord,
    chunks: &[AudioChunkRecord],
    jobs: &[TranscriptionJobRecord],
    transcribed_chunk_ids: &[String],
    ctx: &StatusContext<'_>,
) -> UiSessionStatus {
    if session.state == SessionState::Running {
        return UiSessionStatus::Recording;
    }
    if let Some(_reason) = attention_reason(chunks, jobs, transcribed_chunk_ids, ctx) {
        return UiSessionStatus::Attention;
    }
    let pending: Vec<_> = jobs
        .iter()
        .filter(|job| job.state == JobState::Pending)
        .collect();
    let processing = jobs.iter().any(|job| job.state == JobState::Processing);
    if processing {
        return UiSessionStatus::Transcribing;
    }
    if !pending.is_empty() {
        let waiting = pending.iter().any(|job| {
            job.next_retry_at
                .as_deref()
                .and_then(|value| parse_rfc3339(value).ok())
                .map(|when| when > ctx.now)
                .unwrap_or(false)
        });
        return if waiting {
            UiSessionStatus::WaitingRetry
        } else {
            UiSessionStatus::Transcribing
        };
    }
    if chunks.is_empty()
        || chunks
            .iter()
            .all(|chunk| transcribed_chunk_ids.iter().any(|id| id == &chunk.id))
    {
        return UiSessionStatus::Complete;
    }
    if session.state == SessionState::Interrupted {
        return UiSessionStatus::Interrupted;
    }
    UiSessionStatus::Transcribing
}

pub fn attention_reason(
    chunks: &[AudioChunkRecord],
    jobs: &[TranscriptionJobRecord],
    transcribed_chunk_ids: &[String],
    ctx: &StatusContext<'_>,
) -> Option<String> {
    for chunk in chunks {
        if transcribed_chunk_ids.iter().any(|id| id == &chunk.id) {
            continue;
        }
        if !std::path::Path::new(&chunk.file_path).is_file() {
            return Some(format!("Audio file is missing: {}", chunk.file_path));
        }
    }
    let active_jobs = jobs.iter().filter(|job| job.state != JobState::Completed);
    for job in active_jobs {
        match job.provider {
            ProviderKind::Voxtype => {
                if std::process::Command::new("voxtype")
                    .arg("--version")
                    .output()
                    .is_err()
                {
                    return Some("Voxtype is not installed or unavailable".to_string());
                }
            }
            ProviderKind::Openai => {
                return Some(
                    "this meeting used the removed OpenAI provider; retranscribe with voxtype or whisper_cpp".to_string(),
                );
            }
            ProviderKind::WhisperCpp => {
                let executable = &ctx.config.transcription.whisper_cpp.executable;
                if !executable.exists() {
                    return Some(format!(
                        "whisper-cli was not found at {}",
                        executable.display()
                    ));
                }
                let model = &ctx.config.transcription.whisper_cpp.model;
                if !model.is_file() {
                    return Some(format!(
                        "whisper.cpp model was not found at {}",
                        model.display()
                    ));
                }
            }
        }
    }
    None
}

pub fn display_title(session: &SessionRecord) -> String {
    if let Some(title) = session
        .title
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return title.to_string();
    }
    fallback_title(&session.started_at)
}

pub fn fallback_title(started_at: &str) -> String {
    match parse_rfc3339(started_at) {
        Ok(utc) => format!(
            "Recording {}",
            utc.with_timezone(&chrono::Local).format("%d %b %Y, %H:%M")
        ),
        Err(_) => "Recording".to_string(),
    }
}

pub fn format_duration_secs(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        format!("{secs} sec")
    } else if secs < 3600 {
        format!("{} min", secs / 60)
    } else {
        let hours = secs / 3600;
        let minutes = (secs % 3600) / 60;
        if minutes == 0 {
            format!("{hours} h")
        } else {
            format!("{hours} h {minutes} min")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{derive_ui_status, StatusContext, UiSessionStatus};
    use crate::audio::AudioSource;
    use crate::config::{Config, ProviderKind};
    use crate::storage::types::{
        AudioChunkRecord, JobState, SessionRecord, SessionState, TranscriptionJobRecord,
    };
    use chrono::{TimeZone, Utc};

    fn session(state: SessionState) -> SessionRecord {
        SessionRecord {
            id: "s".into(),
            state,
            started_at: "2026-08-17T12:00:00Z".into(),
            ended_at: None,
            microphone_id: None,
            microphone_description: None,
            output_id: None,
            output_description: None,
            monitor_id: None,
            monitor_description: None,
            created_at: "2026-08-17T12:00:00Z".into(),
            title: None,
        }
    }

    fn chunk(id: &str) -> AudioChunkRecord {
        AudioChunkRecord {
            id: id.into(),
            session_id: "s".into(),
            source: AudioSource::Mic,
            sequence: 1,
            started_at: "2026-08-17T12:00:00Z".into(),
            ended_at: "2026-08-17T12:00:30Z".into(),
            file_path: format!("{}/Cargo.toml", env!("CARGO_MANIFEST_DIR")),
            duration_ms: 30000,
            created_at: "2026-08-17T12:00:00Z".into(),
        }
    }

    fn job(state: JobState, next_retry: Option<&str>) -> TranscriptionJobRecord {
        TranscriptionJobRecord {
            id: "j".into(),
            audio_chunk_id: "c1".into(),
            run_id: "r".into(),
            provider: ProviderKind::Voxtype,
            model: Some("m".into()),
            state,
            attempt_count: 1,
            last_error: Some("network".into()),
            next_retry_at: next_retry.map(str::to_string),
            created_at: "2026-08-17T12:00:00Z".into(),
            started_at: None,
            completed_at: None,
        }
    }

    fn ctx_for(config: &Config) -> StatusContext<'_> {
        StatusContext {
            config,
            now: Utc.with_ymd_and_hms(2026, 8, 17, 12, 0, 0).unwrap(),
        }
    }

    #[test]
    fn running_is_recording() {
        let config = Config::default();
        let status = derive_ui_status(
            &session(SessionState::Running),
            &[],
            &[],
            &[],
            &ctx_for(&config),
        );
        assert_eq!(status, UiSessionStatus::Recording);
    }

    #[test]
    fn complete_when_every_chunk_has_an_event() {
        let config = Config::default();
        let status = derive_ui_status(
            &session(SessionState::Completed),
            &[chunk("c1")],
            &[],
            &["c1".into()],
            &ctx_for(&config),
        );
        assert_eq!(status, UiSessionStatus::Complete);
    }

    #[test]
    fn interrupted_becomes_complete_after_transcription() {
        let config = Config::default();
        let status = derive_ui_status(
            &session(SessionState::Interrupted),
            &[chunk("c1")],
            &[],
            &["c1".into()],
            &ctx_for(&config),
        );
        assert_eq!(status, UiSessionStatus::Complete);
    }

    #[test]
    fn processing_is_transcribing() {
        let config = Config::default();
        let status = derive_ui_status(
            &session(SessionState::Completed),
            &[chunk("c1")],
            &[job(JobState::Processing, None)],
            &[],
            &ctx_for(&config),
        );
        assert_eq!(status, UiSessionStatus::Transcribing);
    }

    #[test]
    fn future_retry_is_waiting() {
        let config = Config::default();
        let status = derive_ui_status(
            &session(SessionState::Completed),
            &[chunk("c1")],
            &[job(JobState::Pending, Some("2026-08-17T13:00:00Z"))],
            &[],
            &ctx_for(&config),
        );
        assert_eq!(status, UiSessionStatus::WaitingRetry);
    }

    #[test]
    fn interrupted_without_transcripts() {
        let config = Config::default();
        let status = derive_ui_status(
            &session(SessionState::Interrupted),
            &[chunk("c1")],
            &[],
            &[],
            &ctx_for(&config),
        );
        assert_eq!(status, UiSessionStatus::Interrupted);
    }

    #[test]
    fn fallback_title_uses_local_start_time() {
        let title = super::fallback_title("2026-08-17T12:32:00Z");
        assert!(title.starts_with("Recording "), "{title}");
        assert!(title.contains("2026"), "{title}");
    }

    #[test]
    fn duration_formatting() {
        assert_eq!(super::format_duration_secs(12), "12 sec");
        assert_eq!(super::format_duration_secs(4 * 60), "4 min");
        assert_eq!(super::format_duration_secs(32 * 60), "32 min");
        assert_eq!(super::format_duration_secs(3600 + 14 * 60), "1 h 14 min");
    }
}

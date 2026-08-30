use serde::{Deserialize, Serialize};

use crate::audio::AudioDevice;
use crate::config::ProviderKind;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Running,
    Completed,
    Interrupted,
}

impl SessionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn parse(value: &str) -> crate::error::Result<Self> {
        match value {
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "interrupted" => Ok(Self::Interrupted),
            other => Err(crate::error::AppError::other(format!(
                "invalid session state '{other}'"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Pending,
    Processing,
    Completed,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Processing => "processing",
            Self::Completed => "completed",
        }
    }

    pub fn parse(value: &str) -> crate::error::Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "processing" => Ok(Self::Processing),
            "completed" => Ok(Self::Completed),
            other => Err(crate::error::AppError::other(format!(
                "invalid job state '{other}'"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{JobState, SessionState};

    #[test]
    fn session_state_round_trip() {
        for state in [
            SessionState::Running,
            SessionState::Completed,
            SessionState::Interrupted,
        ] {
            assert_eq!(SessionState::parse(state.as_str()).unwrap(), state);
        }
        let error = SessionState::parse("failed").unwrap_err();
        assert!(error.to_string().contains("invalid session state"));
    }

    #[test]
    fn job_state_round_trip() {
        for state in [JobState::Pending, JobState::Processing, JobState::Completed] {
            assert_eq!(JobState::parse(state.as_str()).unwrap(), state);
        }
        let error = JobState::parse("failed").unwrap_err();
        assert!(error.to_string().contains("invalid job state"));
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceSnapshot {
    pub id: String,
    pub description: String,
}

impl From<&AudioDevice> for DeviceSnapshot {
    fn from(device: &AudioDevice) -> Self {
        Self {
            id: device.id.clone(),
            description: device.description.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub id: String,
    pub state: SessionState,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub microphone: Option<DeviceSnapshot>,
    pub output: Option<DeviceSnapshot>,
    pub monitor: Option<DeviceSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SessionRecord {
    pub id: String,
    pub state: SessionState,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub microphone_id: Option<String>,
    pub microphone_description: Option<String>,
    pub output_id: Option<String>,
    pub output_description: Option<String>,
    pub monitor_id: Option<String>,
    pub monitor_description: Option<String>,
    pub created_at: String,
    pub title: Option<String>,
}

#[derive(Clone, Debug)]
pub struct TranscriptionRunRecord {
    pub id: String,
    pub session_id: String,
    pub provider: ProviderKind,
    pub model: Option<String>,
    pub created_at: String,
}

#[derive(Clone, Debug)]
pub struct AudioChunkRecord {
    pub id: String,
    pub session_id: String,
    pub source: crate::audio::AudioSource,
    pub sequence: u64,
    pub started_at: String,
    pub ended_at: String,
    pub file_path: String,
    pub duration_ms: i64,
    pub created_at: String,
}

#[derive(Clone, Debug)]
pub struct TranscriptionJobRecord {
    pub id: String,
    pub audio_chunk_id: String,
    pub run_id: String,
    pub provider: ProviderKind,
    pub model: Option<String>,
    pub state: JobState,
    pub attempt_count: i64,
    pub last_error: Option<String>,
    pub next_retry_at: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
}

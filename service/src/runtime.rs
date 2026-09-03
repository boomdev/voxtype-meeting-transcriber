use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::sync::broadcast;

use crate::audio::AudioDevice;
use crate::config::ProviderKind;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioServerHealth {
    Available,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceUiState {
    Idle,
    Capturing,
    Unavailable,
    Reconnecting,
}

#[derive(Clone, Debug)]
pub struct RuntimeStatus {
    pub session_id: Option<String>,
    pub session_started_at: Option<String>,
    pub microphone: Option<AudioDevice>,
    pub output: Option<AudioDevice>,
    pub monitor: Option<AudioDevice>,
    pub microphone_state: SourceUiState,
    pub system_state: SourceUiState,
    pub audio_server: AudioServerHealth,
    pub capture_active: bool,
    pub capture_paused: bool,
    pub capture_stop_reason: Option<String>,
    pub provider: ProviderKind,
}

impl RuntimeStatus {
    pub fn new(provider: ProviderKind) -> Self {
        Self {
            session_id: None,
            session_started_at: None,
            microphone: None,
            output: None,
            monitor: None,
            microphone_state: SourceUiState::Idle,
            system_state: SourceUiState::Idle,
            audio_server: AudioServerHealth::Unavailable,
            capture_active: false,
            capture_paused: false,
            capture_stop_reason: None,
            provider,
        }
    }
}

pub type SharedStatus = Arc<Mutex<RuntimeStatus>>;

pub fn lock_status(status: &SharedStatus) -> std::sync::MutexGuard<'_, RuntimeStatus> {
    match status.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct UiEvent {
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl UiEvent {
    pub fn new(event: impl Into<String>) -> Self {
        Self {
            event: event.into(),
            session_id: None,
            message: None,
        }
    }

    pub fn session(mut self, id: impl Into<String>) -> Self {
        self.session_id = Some(id.into());
        self
    }

    pub fn message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }
}

pub type EventBus = broadcast::Sender<UiEvent>;

pub fn event_bus() -> EventBus {
    let (tx, _) = broadcast::channel(64);
    tx
}

pub fn emit(bus: &EventBus, event: UiEvent) {
    let _ = bus.send(event);
}

pub const AUDIO_RECONNECT_DELAY_SECS: u64 = 5;
pub const DISK_CHECK_INTERVAL_SECS: u64 = 15;
pub const WORKER_SHUTDOWN_GRACE_SECS: u64 = 2;
pub const CONTROL_IDLE_TIMEOUT_SECS: u64 = 30;
pub const CONTROL_MAX_LINE_BYTES: usize = 64 * 1024;
pub const STOP_WAIT_SECS: u64 = 15;

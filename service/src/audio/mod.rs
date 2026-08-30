use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::error::Result;

pub mod convert;
mod devices;
pub mod fake;
mod pulse;
pub mod speech;

pub use devices::print_devices;
pub use pulse::PulseAudioBackend;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioSource {
    Mic,
    System,
}

impl AudioSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mic => "mic",
            Self::System => "system",
        }
    }

    pub fn display_label(self) -> &'static str {
        match self {
            Self::Mic => "MIC",
            Self::System => "SYSTEM",
        }
    }

    pub fn audio_subdir(self) -> &'static str {
        match self {
            Self::Mic => "mic",
            Self::System => "system",
        }
    }
}

impl std::fmt::Display for AudioSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceRole {
    Microphone,
    OutputSink,
    MonitorSource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioDevice {
    pub id: String,
    pub description: String,
    pub role: DeviceRole,
}

impl AudioDevice {
    pub fn summary(&self) -> String {
        if self.description.is_empty() || self.description == self.id {
            self.id.clone()
        } else {
            format!("{} ({})", self.id, self.description)
        }
    }
}

#[derive(Clone, Debug)]
pub struct PcmFrame {
    pub source: AudioSource,
    pub samples: Vec<i16>,
    pub captured_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub enum AudioEvent {
    MicrophoneChanged {
        device: AudioDevice,
    },
    MicrophoneUnavailable {
        device_id: String,
        reason: String,
    },
    MicrophoneAvailable {
        device: AudioDevice,
    },
    OutputChanged {
        device: AudioDevice,
        monitor: AudioDevice,
    },
    OutputUnavailable {
        device_id: String,
        reason: String,
    },
    OutputAvailable {
        device: AudioDevice,
        monitor: AudioDevice,
    },
    AudioServerUnavailable {
        reason: String,
    },
    AudioServerAvailable,
}

#[async_trait::async_trait]
pub trait AudioBackend: Send + Sync {
    fn list_input_devices(&self) -> Result<Vec<AudioDevice>>;
    fn list_output_devices(&self) -> Result<Vec<AudioDevice>>;
    fn list_monitor_sources(&self) -> Result<Vec<AudioDevice>>;
    fn current_microphone(&self) -> Result<AudioDevice>;
    fn current_output_sink(&self) -> Result<AudioDevice>;
    fn current_output_monitor(&self) -> Result<AudioDevice>;
    fn subscribe(&self) -> broadcast::Receiver<AudioEvent>;

    /// `"default"` follows the server default source; any other value is a PulseAudio source name.
    fn resolve_microphone(&self, configured: &str) -> Result<AudioDevice>;

    /// `"default"` follows the server default sink; any other value is a sink name.
    /// SYSTEM always records the sink's monitor source, never the sink itself.
    fn resolve_output(&self, configured: &str) -> Result<(AudioDevice, AudioDevice)>;

    fn server_identity(&self) -> Result<String>;

    async fn capture_microphone(
        &self,
        tx: mpsc::Sender<PcmFrame>,
        shutdown: CancellationToken,
    ) -> Result<()> {
        let device = self.resolve_microphone("default")?;
        self.capture_from(AudioSource::Mic, &device.id, true, tx, shutdown)
            .await
    }

    async fn capture_system_output(
        &self,
        tx: mpsc::Sender<PcmFrame>,
        shutdown: CancellationToken,
    ) -> Result<()> {
        let (_sink, monitor) = self.resolve_output("default")?;
        self.capture_from(AudioSource::System, &monitor.id, true, tx, shutdown)
            .await
    }

    /// Record from an already-resolved device until shutdown, stream error, or a relevant `AudioEvent`.
    async fn capture_from(
        &self,
        source: AudioSource,
        device_id: &str,
        follow_default: bool,
        tx: mpsc::Sender<PcmFrame>,
        shutdown: CancellationToken,
    ) -> Result<()>;
}

/// Whether an audio event should stop the current capture stream so the supervisor can reconnect.
pub fn capture_should_stop(
    source: AudioSource,
    bound_device_id: &str,
    follow_default: bool,
    event: &AudioEvent,
) -> bool {
    match event {
        AudioEvent::AudioServerUnavailable { .. } => true,
        AudioEvent::MicrophoneUnavailable { device_id, .. } if source == AudioSource::Mic => {
            device_id == bound_device_id || device_id.is_empty()
        }
        AudioEvent::MicrophoneChanged { .. } if source == AudioSource::Mic => follow_default,
        AudioEvent::OutputUnavailable { device_id, .. } if source == AudioSource::System => {
            device_id == bound_device_id || device_id.is_empty()
        }
        AudioEvent::OutputChanged { .. } if source == AudioSource::System => follow_default,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::AudioSource;

    #[test]
    fn source_serde() {
        assert_eq!(serde_json::to_string(&AudioSource::Mic).unwrap(), "\"mic\"");
        assert_eq!(
            serde_json::to_string(&AudioSource::System).unwrap(),
            "\"system\""
        );
        assert_eq!(
            serde_json::from_str::<AudioSource>("\"mic\"").unwrap(),
            AudioSource::Mic
        );
    }

    #[test]
    fn capture_stop_rules() {
        use super::{capture_should_stop, AudioDevice, AudioEvent, AudioSource, DeviceRole};

        let mic = AudioDevice {
            id: "mic-a".into(),
            description: "A".into(),
            role: DeviceRole::Microphone,
        };
        let sink = AudioDevice {
            id: "sink-b".into(),
            description: "B".into(),
            role: DeviceRole::OutputSink,
        };
        let monitor = AudioDevice {
            id: "sink-b.monitor".into(),
            description: "B mon".into(),
            role: DeviceRole::MonitorSource,
        };

        assert!(capture_should_stop(
            AudioSource::Mic,
            "mic-a",
            true,
            &AudioEvent::AudioServerUnavailable {
                reason: "down".into()
            }
        ));
        assert!(capture_should_stop(
            AudioSource::Mic,
            "mic-a",
            true,
            &AudioEvent::MicrophoneChanged {
                device: mic.clone()
            }
        ));
        assert!(!capture_should_stop(
            AudioSource::Mic,
            "mic-a",
            false,
            &AudioEvent::MicrophoneChanged {
                device: mic.clone()
            }
        ));
        assert!(!capture_should_stop(
            AudioSource::System,
            "sink-b.monitor",
            true,
            &AudioEvent::MicrophoneChanged { device: mic }
        ));
        assert!(capture_should_stop(
            AudioSource::System,
            "sink-b.monitor",
            true,
            &AudioEvent::OutputChanged {
                device: sink,
                monitor
            }
        ));
    }
}

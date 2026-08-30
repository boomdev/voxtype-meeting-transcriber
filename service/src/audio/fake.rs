use std::f32::consts::PI;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::audio::convert::TARGET_RATE;
use crate::audio::{
    capture_should_stop, AudioBackend, AudioDevice, AudioEvent, AudioSource, DeviceRole, PcmFrame,
};
use crate::error::{AppError, Result};

struct FakeState {
    events: broadcast::Sender<AudioEvent>,
    mic: Mutex<AudioDevice>,
    sink: Mutex<AudioDevice>,
    monitor: Mutex<AudioDevice>,
    fail_mic: AtomicBool,
    fail_system: AtomicBool,
    server_available: AtomicBool,
    mic_frames: AtomicUsize,
    system_frames: AtomicUsize,
    last_mic_device: Mutex<Option<String>>,
    last_system_device: Mutex<Option<String>>,
    samples_per_frame: usize,
}

/// In-process backend that emits a 440 Hz sine (mic) and 880 Hz sine (system).
#[derive(Clone)]
pub struct FakeAudioBackend {
    inner: Arc<FakeState>,
}

impl FakeAudioBackend {
    pub fn new(frames_per_source: usize, samples_per_frame: usize) -> Self {
        let _ = frames_per_source;
        Self::continuous(samples_per_frame)
    }

    pub fn continuous(samples_per_frame: usize) -> Self {
        let (events, _) = broadcast::channel(64);
        Self {
            inner: Arc::new(FakeState {
                events,
                mic: Mutex::new(AudioDevice {
                    id: "fake-mic".into(),
                    description: "Fake microphone".into(),
                    role: DeviceRole::Microphone,
                }),
                sink: Mutex::new(AudioDevice {
                    id: "fake-sink".into(),
                    description: "Fake output".into(),
                    role: DeviceRole::OutputSink,
                }),
                monitor: Mutex::new(AudioDevice {
                    id: "fake-sink.monitor".into(),
                    description: "Fake output monitor".into(),
                    role: DeviceRole::MonitorSource,
                }),
                fail_mic: AtomicBool::new(false),
                fail_system: AtomicBool::new(false),
                server_available: AtomicBool::new(true),
                mic_frames: AtomicUsize::new(0),
                system_frames: AtomicUsize::new(0),
                last_mic_device: Mutex::new(None),
                last_system_device: Mutex::new(None),
                samples_per_frame,
            }),
        }
    }

    pub fn emit(&self, event: AudioEvent) {
        let _ = self.inner.events.send(event);
    }

    pub fn fail_source(&self, source: AudioSource, fail: bool) {
        match source {
            AudioSource::Mic => self.inner.fail_mic.store(fail, Ordering::SeqCst),
            AudioSource::System => self.inner.fail_system.store(fail, Ordering::SeqCst),
        }
    }

    pub fn set_server_available(&self, available: bool) {
        self.inner
            .server_available
            .store(available, Ordering::SeqCst);
        if available {
            let _ = self.inner.events.send(AudioEvent::AudioServerAvailable);
        } else {
            let _ = self.inner.events.send(AudioEvent::AudioServerUnavailable {
                reason: "fake audio server unavailable".into(),
            });
        }
    }

    pub fn set_microphone(&self, device: AudioDevice) {
        *self.inner.mic.lock().expect("fake mic mutex") = device.clone();
        let _ = self
            .inner
            .events
            .send(AudioEvent::MicrophoneChanged { device });
    }

    pub fn set_output(&self, sink: AudioDevice, monitor: AudioDevice) {
        *self.inner.sink.lock().expect("fake sink mutex") = sink.clone();
        *self.inner.monitor.lock().expect("fake monitor mutex") = monitor.clone();
        let _ = self.inner.events.send(AudioEvent::OutputChanged {
            device: sink,
            monitor,
        });
    }

    pub fn hide_output(&self) {
        let id = self
            .inner
            .monitor
            .lock()
            .expect("fake monitor mutex")
            .id
            .clone();
        self.inner.fail_system.store(true, Ordering::SeqCst);
        let _ = self.inner.events.send(AudioEvent::OutputUnavailable {
            device_id: id,
            reason: "fake output disappeared".into(),
        });
    }

    pub fn frames_emitted(&self, source: AudioSource) -> usize {
        match source {
            AudioSource::Mic => self.inner.mic_frames.load(Ordering::SeqCst),
            AudioSource::System => self.inner.system_frames.load(Ordering::SeqCst),
        }
    }

    pub fn last_capture_device(&self, source: AudioSource) -> Option<String> {
        match source {
            AudioSource::Mic => self
                .inner
                .last_mic_device
                .lock()
                .expect("fake last mic")
                .clone(),
            AudioSource::System => self
                .inner
                .last_system_device
                .lock()
                .expect("fake last system")
                .clone(),
        }
    }

    fn device_copy(slot: &Mutex<AudioDevice>) -> Result<AudioDevice> {
        Ok(slot
            .lock()
            .map_err(|_| AppError::audio("fake audio device mutex was poisoned"))?
            .clone())
    }
}

#[async_trait::async_trait]
impl AudioBackend for FakeAudioBackend {
    fn list_input_devices(&self) -> Result<Vec<AudioDevice>> {
        Ok(vec![Self::device_copy(&self.inner.mic)?])
    }

    fn list_output_devices(&self) -> Result<Vec<AudioDevice>> {
        Ok(vec![Self::device_copy(&self.inner.sink)?])
    }

    fn list_monitor_sources(&self) -> Result<Vec<AudioDevice>> {
        Ok(vec![Self::device_copy(&self.inner.monitor)?])
    }

    fn current_microphone(&self) -> Result<AudioDevice> {
        if !self.inner.server_available.load(Ordering::SeqCst) {
            return Err(AppError::audio("fake audio server unavailable"));
        }
        Self::device_copy(&self.inner.mic)
    }

    fn current_output_sink(&self) -> Result<AudioDevice> {
        if !self.inner.server_available.load(Ordering::SeqCst) {
            return Err(AppError::audio("fake audio server unavailable"));
        }
        Self::device_copy(&self.inner.sink)
    }

    fn current_output_monitor(&self) -> Result<AudioDevice> {
        if !self.inner.server_available.load(Ordering::SeqCst) {
            return Err(AppError::audio("fake audio server unavailable"));
        }
        Self::device_copy(&self.inner.monitor)
    }

    fn subscribe(&self) -> broadcast::Receiver<AudioEvent> {
        self.inner.events.subscribe()
    }

    fn resolve_microphone(&self, configured: &str) -> Result<AudioDevice> {
        if !self.inner.server_available.load(Ordering::SeqCst) {
            return Err(AppError::audio("fake audio server unavailable"));
        }
        let mic = Self::device_copy(&self.inner.mic)?;
        if configured == "default" || configured == mic.id {
            Ok(mic)
        } else {
            Err(AppError::audio(format!(
                "microphone source {configured} was not found"
            )))
        }
    }

    fn resolve_output(&self, configured: &str) -> Result<(AudioDevice, AudioDevice)> {
        if !self.inner.server_available.load(Ordering::SeqCst) {
            return Err(AppError::audio("fake audio server unavailable"));
        }
        let sink = Self::device_copy(&self.inner.sink)?;
        let monitor = Self::device_copy(&self.inner.monitor)?;
        if configured == "default" || configured == sink.id {
            Ok((sink, monitor))
        } else {
            Err(AppError::audio(format!(
                "output sink {configured} was not found"
            )))
        }
    }

    fn server_identity(&self) -> Result<String> {
        if !self.inner.server_available.load(Ordering::SeqCst) {
            return Err(AppError::audio("fake audio server unavailable"));
        }
        Ok("FakeAudio".to_string())
    }

    async fn capture_from(
        &self,
        source: AudioSource,
        device_id: &str,
        follow_default: bool,
        tx: mpsc::Sender<PcmFrame>,
        shutdown: CancellationToken,
    ) -> Result<()> {
        if !self.inner.server_available.load(Ordering::SeqCst) {
            return Err(AppError::audio("fake audio server unavailable"));
        }
        match source {
            AudioSource::Mic => {
                *self.inner.last_mic_device.lock().expect("fake last mic") =
                    Some(device_id.to_string());
            }
            AudioSource::System => {
                *self
                    .inner
                    .last_system_device
                    .lock()
                    .expect("fake last system") = Some(device_id.to_string());
            }
        }
        let freq = match source {
            AudioSource::Mic => 440.0,
            AudioSource::System => 880.0,
        };
        let mut events = self.subscribe();
        let mut n = 0u64;
        loop {
            if shutdown.is_cancelled() {
                return Ok(());
            }
            if !self.inner.server_available.load(Ordering::SeqCst) {
                return Err(AppError::audio("fake audio server unavailable"));
            }
            let failed = match source {
                AudioSource::Mic => self.inner.fail_mic.load(Ordering::SeqCst),
                AudioSource::System => self.inner.fail_system.load(Ordering::SeqCst),
            };
            if failed {
                return Err(AppError::audio(format!(
                    "{source} capture unavailable because source {device_id} failed"
                )));
            }

            let mut samples = Vec::with_capacity(self.inner.samples_per_frame);
            for _ in 0..self.inner.samples_per_frame {
                let t = n as f32 / TARGET_RATE as f32;
                let v = (2.0 * PI * freq * t).sin();
                samples.push((v * 8000.0) as i16);
                n += 1;
            }
            tx.send(PcmFrame {
                source,
                samples,
                captured_at: Utc::now(),
            })
            .await
            .map_err(|_| AppError::audio("fake audio channel closed"))?;
            match source {
                AudioSource::Mic => {
                    self.inner.mic_frames.fetch_add(1, Ordering::SeqCst);
                }
                AudioSource::System => {
                    self.inner.system_frames.fetch_add(1, Ordering::SeqCst);
                }
            }

            tokio::select! {
                _ = shutdown.cancelled() => return Ok(()),
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                item = events.recv() => {
                    match item {
                        Ok(event) if capture_should_stop(source, device_id, follow_default, &event) => {
                            return Ok(());
                        }
                        Ok(_) => {}
                        Err(broadcast::error::RecvError::Closed) => return Ok(()),
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                    }
                }
            }
        }
    }
}

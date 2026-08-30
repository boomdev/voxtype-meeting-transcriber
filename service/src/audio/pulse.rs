use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self as std_mpsc, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use pulse::callbacks::ListResult;
use pulse::context::introspect::{SinkInfo, SourceInfo};
use pulse::context::subscribe::InterestMaskSet;
use pulse::context::{Context, FlagSet as ContextFlagSet, State as ContextState};
use pulse::mainloop::threaded::Mainloop;
use pulse::proplist::Proplist;
use pulse::sample::{Format, Spec};
use pulse::stream::{self, PeekResult, Stream};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::audio::convert::{self, PcmFormat};
use crate::audio::{
    capture_should_stop, AudioBackend, AudioDevice, AudioEvent, AudioSource, DeviceRole, PcmFrame,
};
use crate::error::{AppError, Result};

const APP_NAME: &str = "voxtype-meeting-service";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const TARGET_SPEC: Spec = Spec {
    format: Format::S16le,
    channels: 1,
    rate: 16_000,
};

type Reply<T> = std_mpsc::Sender<Result<T>>;

enum Command {
    ListInputs {
        reply: Reply<Vec<AudioDevice>>,
    },
    ListOutputs {
        reply: Reply<Vec<AudioDevice>>,
    },
    ListMonitors {
        reply: Reply<Vec<AudioDevice>>,
    },
    CurrentMic {
        reply: Reply<AudioDevice>,
    },
    CurrentSink {
        reply: Reply<AudioDevice>,
    },
    CurrentMonitor {
        reply: Reply<AudioDevice>,
    },
    ResolveMic {
        configured: String,
        reply: Reply<AudioDevice>,
    },
    ResolveOutput {
        configured: String,
        reply: Reply<(AudioDevice, AudioDevice)>,
    },
    ServerIdentity {
        reply: Reply<String>,
    },
    StartCapture {
        source: AudioSource,
        device: Option<String>,
        frames: mpsc::Sender<PcmFrame>,
        reply: Reply<()>,
    },
    StopCapture {
        source: AudioSource,
        reply: Reply<()>,
    },
    Shutdown,
}

#[derive(Clone)]
pub struct PulseAudioBackend {
    inner: Arc<PulseInner>,
}

struct PulseInner {
    cmd: std_mpsc::Sender<Command>,
    events: broadcast::Sender<AudioEvent>,
}

impl Drop for PulseInner {
    fn drop(&mut self) {
        let _ = self.cmd.send(Command::Shutdown);
    }
}

impl PulseAudioBackend {
    pub fn connect() -> Result<Self> {
        let (cmd_tx, cmd_rx) = std_mpsc::channel();
        let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);
        let (events, _) = broadcast::channel(64);
        let events_thread = events.clone();
        std::thread::Builder::new()
            .name("pulse-mainloop".into())
            .spawn(move || {
                if let Err(error) = pulse_thread(cmd_rx, events_thread, ready_tx) {
                    tracing::error!(error = %error, "PulseAudio thread exited");
                }
            })
            .map_err(|error| {
                AppError::audio(format!("could not start PulseAudio thread: {error}"))
            })?;

        match ready_rx.recv_timeout(CONNECT_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                inner: Arc::new(PulseInner {
                    cmd: cmd_tx,
                    events,
                }),
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(AppError::audio(
                "timed out after 5s waiting for PulseAudio/pipewire-pulse to become ready",
            )),
        }
    }

    fn rpc<T>(&self, build: impl FnOnce(Reply<T>) -> Command) -> Result<T> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        self.inner
            .cmd
            .send(build(reply_tx))
            .map_err(|_| AppError::audio("PulseAudio thread is not running"))?;
        match reply_rx.recv_timeout(COMMAND_TIMEOUT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(AppError::audio(
                "timed out waiting for a PulseAudio introspection result",
            )),
            Err(RecvTimeoutError::Disconnected) => Err(AppError::audio(
                "PulseAudio thread disconnected while waiting for a result",
            )),
        }
    }
}

#[async_trait::async_trait]
impl AudioBackend for PulseAudioBackend {
    fn list_input_devices(&self) -> Result<Vec<AudioDevice>> {
        self.rpc(|reply| Command::ListInputs { reply })
    }

    fn list_output_devices(&self) -> Result<Vec<AudioDevice>> {
        self.rpc(|reply| Command::ListOutputs { reply })
    }

    fn list_monitor_sources(&self) -> Result<Vec<AudioDevice>> {
        self.rpc(|reply| Command::ListMonitors { reply })
    }

    fn current_microphone(&self) -> Result<AudioDevice> {
        self.rpc(|reply| Command::CurrentMic { reply })
    }

    fn current_output_sink(&self) -> Result<AudioDevice> {
        self.rpc(|reply| Command::CurrentSink { reply })
    }

    fn current_output_monitor(&self) -> Result<AudioDevice> {
        self.rpc(|reply| Command::CurrentMonitor { reply })
    }

    fn subscribe(&self) -> broadcast::Receiver<AudioEvent> {
        self.inner.events.subscribe()
    }

    fn resolve_microphone(&self, configured: &str) -> Result<AudioDevice> {
        let configured = configured.to_string();
        self.rpc(|reply| Command::ResolveMic { configured, reply })
    }

    fn resolve_output(&self, configured: &str) -> Result<(AudioDevice, AudioDevice)> {
        let configured = configured.to_string();
        self.rpc(|reply| Command::ResolveOutput { configured, reply })
    }

    fn server_identity(&self) -> Result<String> {
        self.rpc(|reply| Command::ServerIdentity { reply })
    }

    async fn capture_from(
        &self,
        source: AudioSource,
        device_id: &str,
        follow_default: bool,
        tx: mpsc::Sender<PcmFrame>,
        shutdown: CancellationToken,
    ) -> Result<()> {
        self.rpc(|reply| Command::StartCapture {
            source,
            device: Some(device_id.to_string()),
            frames: tx,
            reply,
        })?;
        let mut events = self.subscribe();
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                item = events.recv() => {
                    match item {
                        Ok(event) if capture_should_stop(source, device_id, follow_default, &event) => {
                            break;
                        }
                        Ok(_) => {}
                        Err(broadcast::error::RecvError::Closed) => break,
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                    }
                }
            }
        }
        self.rpc(|reply| Command::StopCapture { source, reply })
    }
}

struct ThreadState {
    mainloop: Rc<RefCell<Mainloop>>,
    context: Rc<RefCell<Context>>,
    streams: HashMap<AudioSource, Rc<RefCell<Stream>>>,
    stream_devices: HashMap<AudioSource, String>,
    events: broadcast::Sender<AudioEvent>,
    last_mic: Option<AudioDevice>,
    last_sink: Option<AudioDevice>,
    last_monitor: Option<AudioDevice>,
    refresh_needed: Arc<AtomicBool>,
    context_failed: Arc<AtomicBool>,
}

fn pulse_thread(
    cmd_rx: std_mpsc::Receiver<Command>,
    events: broadcast::Sender<AudioEvent>,
    ready_tx: std_mpsc::SyncSender<Result<()>>,
) -> Result<()> {
    let mut announced = false;
    loop {
        match run_connected_session(&cmd_rx, &events, &ready_tx, &mut announced) {
            ThreadExit::Shutdown => return Ok(()),
            ThreadExit::Disconnected => {
                let _ = events.send(AudioEvent::AudioServerUnavailable {
                    reason: "PulseAudio/pipewire-pulse connection was lost".into(),
                });
                tracing::warn!("audio server unavailable; retrying every 5s");
                if wait_reconnect(&cmd_rx) == ThreadExit::Shutdown {
                    return Ok(());
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ThreadExit {
    Shutdown,
    Disconnected,
}

fn wait_reconnect(cmd_rx: &std_mpsc::Receiver<Command>) -> ThreadExit {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return ThreadExit::Disconnected;
        }
        match cmd_rx.recv_timeout(remaining) {
            Ok(Command::Shutdown) => return ThreadExit::Shutdown,
            Ok(command) => reply_server_down(command),
            Err(RecvTimeoutError::Timeout) => return ThreadExit::Disconnected,
            Err(RecvTimeoutError::Disconnected) => return ThreadExit::Shutdown,
        }
    }
}

fn reply_server_down(command: Command) {
    fn down() -> AppError {
        AppError::audio("PulseAudio/pipewire-pulse is unavailable; retrying connection")
    }
    match command {
        Command::ListInputs { reply } => {
            let _ = reply.send(Err(down()));
        }
        Command::ListOutputs { reply } => {
            let _ = reply.send(Err(down()));
        }
        Command::ListMonitors { reply } => {
            let _ = reply.send(Err(down()));
        }
        Command::CurrentMic { reply } => {
            let _ = reply.send(Err(down()));
        }
        Command::CurrentSink { reply } => {
            let _ = reply.send(Err(down()));
        }
        Command::CurrentMonitor { reply } => {
            let _ = reply.send(Err(down()));
        }
        Command::ResolveMic { reply, .. } => {
            let _ = reply.send(Err(down()));
        }
        Command::ResolveOutput { reply, .. } => {
            let _ = reply.send(Err(down()));
        }
        Command::ServerIdentity { reply } => {
            let _ = reply.send(Err(down()));
        }
        Command::StartCapture { reply, .. } => {
            let _ = reply.send(Err(down()));
        }
        Command::StopCapture { reply, .. } => {
            let _ = reply.send(Ok(()));
        }
        Command::Shutdown => {}
    }
}

fn run_connected_session(
    cmd_rx: &std_mpsc::Receiver<Command>,
    events: &broadcast::Sender<AudioEvent>,
    ready_tx: &std_mpsc::SyncSender<Result<()>>,
    announced: &mut bool,
) -> ThreadExit {
    let refresh_needed = Arc::new(AtomicBool::new(false));
    let context_failed = Arc::new(AtomicBool::new(false));

    let setup = setup_pulse(events, refresh_needed.clone(), context_failed.clone());
    let mut state = match setup {
        Ok(state) => state,
        Err(error) => {
            if !*announced {
                let _ = ready_tx.send(Err(AppError::audio(error.to_string())));
                return ThreadExit::Shutdown;
            }
            tracing::warn!(error = %error, "audio server reconnect failed");
            return ThreadExit::Disconnected;
        }
    };

    if !*announced {
        let _ = ready_tx.send(Ok(()));
        *announced = true;
    } else {
        tracing::info!("audio server available again");
        let _ = events.send(AudioEvent::AudioServerAvailable);
    }

    loop {
        match cmd_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(Command::Shutdown) => {
                teardown_pulse(&mut state);
                return ThreadExit::Shutdown;
            }
            Ok(command) => {
                state.mainloop.borrow_mut().lock();
                if !context_ready(&state) {
                    state.mainloop.borrow_mut().unlock();
                    teardown_pulse(&mut state);
                    return ThreadExit::Disconnected;
                }
                handle_command(&mut state, command);
                state.mainloop.borrow_mut().unlock();
            }
            Err(RecvTimeoutError::Timeout) => {
                state.mainloop.borrow_mut().lock();
                let failed = state.context_failed.load(Ordering::SeqCst) || !context_ready(&state);
                if state.refresh_needed.swap(false, Ordering::SeqCst) && !failed {
                    refresh_devices(&mut state);
                }
                state.mainloop.borrow_mut().unlock();
                if failed {
                    teardown_pulse(&mut state);
                    return ThreadExit::Disconnected;
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                teardown_pulse(&mut state);
                return ThreadExit::Shutdown;
            }
        }
    }
}

fn context_ready(state: &ThreadState) -> bool {
    matches!(state.context.borrow().get_state(), ContextState::Ready)
}

fn setup_pulse(
    events: &broadcast::Sender<AudioEvent>,
    refresh_needed: Arc<AtomicBool>,
    context_failed: Arc<AtomicBool>,
) -> Result<ThreadState> {
    let mut proplist = Proplist::new()
        .ok_or_else(|| AppError::audio("could not create PulseAudio property list"))?;
    let _ = proplist.set_str(pulse::proplist::properties::APPLICATION_NAME, APP_NAME);

    let mainloop = Rc::new(RefCell::new(Mainloop::new().ok_or_else(|| {
        AppError::audio("could not create PulseAudio threaded mainloop")
    })?));

    let context = Rc::new(RefCell::new(
        Context::new_with_proplist(&*mainloop.borrow(), APP_NAME, &proplist)
            .ok_or_else(|| AppError::audio("could not create PulseAudio context"))?,
    ));

    context
        .borrow_mut()
        .connect(None, ContextFlagSet::NOAUTOSPAWN, None)
        .map_err(|error| {
            AppError::audio(format!(
                "PulseAudio connect failed: {error}. Is pipewire-pulse or PulseAudio running?"
            ))
        })?;

    mainloop.borrow_mut().lock();
    mainloop.borrow_mut().start().map_err(|error| {
        AppError::audio(format!("could not start PulseAudio mainloop: {error}"))
    })?;

    let deadline = std::time::Instant::now() + CONNECT_TIMEOUT;
    loop {
        match context.borrow().get_state() {
            ContextState::Ready => break,
            ContextState::Failed | ContextState::Terminated => {
                mainloop.borrow_mut().unlock();
                return Err(AppError::audio(
                    "PulseAudio connection failed or terminated before becoming ready",
                ));
            }
            _ => {
                if std::time::Instant::now() > deadline {
                    mainloop.borrow_mut().unlock();
                    return Err(AppError::audio(
                        "timed out after 5s waiting for PulseAudio/pipewire-pulse to become ready",
                    ));
                }
                poll_mainloop(&mainloop);
            }
        }
    }

    let refresh_cb = refresh_needed.clone();
    context.borrow_mut().set_subscribe_callback(Some(Box::new(
        move |facility, operation, _index| {
            tracing::debug!(?facility, ?operation, "PulseAudio subscription event");
            refresh_cb.store(true, Ordering::SeqCst);
        },
    )));
    let failed_cb = context_failed.clone();
    let context_for_cb = context.clone();
    context
        .borrow_mut()
        .set_state_callback(Some(Box::new(move || {
            let Ok(ctx) = context_for_cb.try_borrow() else {
                return;
            };
            if matches!(
                ctx.get_state(),
                ContextState::Failed | ContextState::Terminated
            ) {
                failed_cb.store(true, Ordering::SeqCst);
            }
        })));
    context_failed.store(false, Ordering::SeqCst);

    context.borrow_mut().subscribe(
        InterestMaskSet::SINK | InterestMaskSet::SOURCE | InterestMaskSet::SERVER,
        |_| {},
    );

    let _ = events.send(AudioEvent::AudioServerAvailable);
    mainloop.borrow_mut().unlock();

    let mut state = ThreadState {
        mainloop,
        context,
        streams: HashMap::new(),
        stream_devices: HashMap::new(),
        events: events.clone(),
        last_mic: None,
        last_sink: None,
        last_monitor: None,
        refresh_needed,
        context_failed,
    };
    state.mainloop.borrow_mut().lock();
    refresh_devices(&mut state);
    state.mainloop.borrow_mut().unlock();
    Ok(state)
}

fn teardown_pulse(state: &mut ThreadState) {
    state.mainloop.borrow_mut().lock();
    for stream in state.streams.values() {
        stream.borrow_mut().set_read_callback(None);
        stream.borrow_mut().set_state_callback(None);
        let _ = stream.borrow_mut().disconnect();
    }
    state.streams.clear();
    state.context.borrow_mut().set_state_callback(None);
    state.context.borrow_mut().set_subscribe_callback(None);
    state.context.borrow_mut().disconnect();
    state.mainloop.borrow_mut().unlock();
    state.mainloop.borrow_mut().stop();
}

fn handle_command(state: &mut ThreadState, command: Command) {
    match command {
        Command::ListInputs { reply } => {
            let _ = reply.send(list_sources(state, false));
        }
        Command::ListOutputs { reply } => {
            let _ = reply.send(list_sinks(state));
        }
        Command::ListMonitors { reply } => {
            let _ = reply.send(list_sources(state, true));
        }
        Command::CurrentMic { reply } => {
            let _ = reply.send(current_microphone(state));
        }
        Command::CurrentSink { reply } => {
            let _ = reply.send(current_sink(state));
        }
        Command::CurrentMonitor { reply } => {
            let _ = reply.send(current_monitor(state));
        }
        Command::ResolveMic { configured, reply } => {
            let _ = reply.send(resolve_microphone_configured(state, &configured));
        }
        Command::ResolveOutput { configured, reply } => {
            let _ = reply.send(resolve_output_configured(state, &configured));
        }
        Command::ServerIdentity { reply } => {
            let _ = reply.send(server_identity(state));
        }
        Command::StartCapture {
            source,
            device,
            frames,
            reply,
        } => {
            let _ = reply.send(start_capture(state, source, device, frames));
        }
        Command::StopCapture { source, reply } => {
            let result = stop_capture(state, source);
            let _ = reply.send(result);
        }
        Command::Shutdown => {}
    }
}

enum Collect<T> {
    Item(T),
    End,
    Error,
}

fn poll_mainloop(mainloop: &Rc<RefCell<Mainloop>>) {
    mainloop.borrow_mut().unlock();
    std::thread::sleep(Duration::from_millis(5));
    mainloop.borrow_mut().lock();
}

fn wait_collect<T: Send + 'static>(
    state: &ThreadState,
    rx: std_mpsc::Receiver<Collect<T>>,
) -> Result<Vec<T>> {
    let mut items = Vec::new();
    let deadline = std::time::Instant::now() + COMMAND_TIMEOUT;
    loop {
        match rx.try_recv() {
            Ok(Collect::Item(item)) => items.push(item),
            Ok(Collect::End) => return Ok(items),
            Ok(Collect::Error) => {
                return Err(AppError::audio(
                    "PulseAudio introspection failed while listing devices",
                ));
            }
            Err(std_mpsc::TryRecvError::Empty) => {
                if std::time::Instant::now() > deadline {
                    return Err(AppError::audio(
                        "timed out waiting for PulseAudio device list",
                    ));
                }
                poll_mainloop(&state.mainloop);
            }
            Err(std_mpsc::TryRecvError::Disconnected) => {
                return Err(AppError::audio(
                    "PulseAudio introspection channel closed unexpectedly",
                ));
            }
        }
    }
}

fn list_sources(state: &ThreadState, monitors_only: bool) -> Result<Vec<AudioDevice>> {
    let (tx, rx) = std_mpsc::channel();
    let _op = state
        .context
        .borrow_mut()
        .introspect()
        .get_source_info_list(move |result| match result {
            ListResult::Item(info) => {
                if let Some(device) = source_device(info) {
                    let is_monitor = device.role == DeviceRole::MonitorSource;
                    if is_monitor == monitors_only {
                        let _ = tx.send(Collect::Item(device));
                    }
                }
            }
            ListResult::End => {
                let _ = tx.send(Collect::End);
            }
            ListResult::Error => {
                let _ = tx.send(Collect::Error);
            }
        });
    wait_collect(state, rx)
}

fn list_sinks(state: &ThreadState) -> Result<Vec<AudioDevice>> {
    let (tx, rx) = std_mpsc::channel();
    let _op =
        state
            .context
            .borrow_mut()
            .introspect()
            .get_sink_info_list(move |result| match result {
                ListResult::Item(info) => {
                    if let Some(device) = sink_device(info) {
                        let _ = tx.send(Collect::Item(device));
                    }
                }
                ListResult::End => {
                    let _ = tx.send(Collect::End);
                }
                ListResult::Error => {
                    let _ = tx.send(Collect::Error);
                }
            });
    wait_collect(state, rx)
}

fn server_defaults(state: &ThreadState) -> Result<(Option<String>, Option<String>)> {
    let (tx, rx) = std_mpsc::channel::<Result<(Option<String>, Option<String>)>>();
    let _op = state
        .context
        .borrow_mut()
        .introspect()
        .get_server_info(move |info| {
            let _ = tx.send(Ok((
                info.default_source_name.as_ref().map(|s| s.to_string()),
                info.default_sink_name.as_ref().map(|s| s.to_string()),
            )));
        });
    let deadline = std::time::Instant::now() + COMMAND_TIMEOUT;
    loop {
        match rx.try_recv() {
            Ok(result) => return result,
            Err(std_mpsc::TryRecvError::Empty) => {
                if std::time::Instant::now() > deadline {
                    return Err(AppError::audio(
                        "timed out waiting for PulseAudio server info",
                    ));
                }
                poll_mainloop(&state.mainloop);
            }
            Err(std_mpsc::TryRecvError::Disconnected) => {
                return Err(AppError::audio(
                    "PulseAudio server info channel closed unexpectedly",
                ));
            }
        }
    }
}

fn current_microphone(state: &ThreadState) -> Result<AudioDevice> {
    let (default_source, _) = server_defaults(state)?;
    let Some(name) = default_source else {
        return Err(AppError::audio(
            "PulseAudio has no default microphone (default source is unset)",
        ));
    };
    source_by_name(state, &name)
}

fn current_sink(state: &ThreadState) -> Result<AudioDevice> {
    let (_, default_sink) = server_defaults(state)?;
    let Some(name) = default_sink else {
        return Err(AppError::audio("PulseAudio has no default output sink"));
    };
    sink_by_name(state, &name)
}

fn current_monitor(state: &ThreadState) -> Result<AudioDevice> {
    let sink = current_sink(state)?;
    monitor_for_sink(state, &sink.id)
}

fn resolve_microphone_configured(state: &ThreadState, configured: &str) -> Result<AudioDevice> {
    if configured.is_empty() || configured == "default" {
        current_microphone(state)
    } else {
        source_by_name(state, configured)
    }
}

fn resolve_output_configured(
    state: &ThreadState,
    configured: &str,
) -> Result<(AudioDevice, AudioDevice)> {
    let sink = if configured.is_empty() || configured == "default" {
        current_sink(state)?
    } else {
        sink_by_name(state, configured)?
    };
    let monitor = monitor_for_sink(state, &sink.id)?;
    Ok((sink, monitor))
}

fn server_identity(state: &ThreadState) -> Result<String> {
    let (tx, rx) = std_mpsc::channel::<Result<String>>();
    let _op = state
        .context
        .borrow_mut()
        .introspect()
        .get_server_info(move |info| {
            let name = info
                .server_name
                .as_ref()
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let version = info
                .server_version
                .as_ref()
                .map(|value| value.to_string())
                .unwrap_or_default();
            let identity = if version.is_empty() {
                name
            } else {
                format!("{name} {version}")
            };
            let _ = tx.send(Ok(identity));
        });
    let deadline = std::time::Instant::now() + COMMAND_TIMEOUT;
    loop {
        match rx.try_recv() {
            Ok(result) => return result,
            Err(std_mpsc::TryRecvError::Empty) => {
                if std::time::Instant::now() > deadline {
                    return Err(AppError::audio(
                        "timed out waiting for PulseAudio server identity",
                    ));
                }
                poll_mainloop(&state.mainloop);
            }
            Err(std_mpsc::TryRecvError::Disconnected) => {
                return Err(AppError::audio(
                    "PulseAudio server identity channel closed unexpectedly",
                ));
            }
        }
    }
}

fn refresh_devices(state: &mut ThreadState) {
    let mic = current_microphone(state).ok();
    let sink = current_sink(state).ok();
    let monitor = match &sink {
        Some(sink) => monitor_for_sink(state, &sink.id).ok(),
        None => None,
    };

    match (&state.last_mic, &mic) {
        (Some(old), Some(new)) if old.id != new.id => {
            tracing::info!(from = %old.id, to = %new.id, "microphone changed");
            let _ = state.events.send(AudioEvent::MicrophoneChanged {
                device: new.clone(),
            });
        }
        (Some(old), None) => {
            tracing::warn!(device_id = %old.id, "microphone unavailable");
            let _ = state.events.send(AudioEvent::MicrophoneUnavailable {
                device_id: old.id.clone(),
                reason: format!("microphone source {} disappeared", old.id),
            });
        }
        (None, Some(new)) if state.last_mic.is_none() && state.last_sink.is_some() => {
            tracing::info!(device_id = %new.id, "microphone available");
            let _ = state.events.send(AudioEvent::MicrophoneAvailable {
                device: new.clone(),
            });
        }
        _ => {}
    }

    match (&state.last_sink, &sink, &state.last_monitor, &monitor) {
        (Some(old_sink), Some(new_sink), _, Some(new_mon))
            if old_sink.id != new_sink.id
                || state
                    .last_monitor
                    .as_ref()
                    .is_some_and(|old| old.id != new_mon.id) =>
        {
            tracing::info!(from = %old_sink.id, to = %new_sink.id, "output changed");
            let _ = state.events.send(AudioEvent::OutputChanged {
                device: new_sink.clone(),
                monitor: new_mon.clone(),
            });
        }
        (Some(old_sink), None, _, _) => {
            tracing::warn!(device_id = %old_sink.id, "output unavailable");
            let _ = state.events.send(AudioEvent::OutputUnavailable {
                device_id: old_sink.id.clone(),
                reason: format!("output sink {} disappeared", old_sink.id),
            });
        }
        (None, Some(new_sink), _, Some(new_mon)) if state.last_mic.is_some() => {
            tracing::info!(device_id = %new_sink.id, "output available");
            let _ = state.events.send(AudioEvent::OutputAvailable {
                device: new_sink.clone(),
                monitor: new_mon.clone(),
            });
        }
        _ => {}
    }

    state.last_mic = mic;
    state.last_sink = sink;
    state.last_monitor = monitor;
}

fn source_by_name(state: &ThreadState, name: &str) -> Result<AudioDevice> {
    let (tx, rx) = std_mpsc::channel();
    let wanted = name.to_string();
    let _op = state
        .context
        .borrow_mut()
        .introspect()
        .get_source_info_by_name(name, move |result| match result {
            ListResult::Item(info) => {
                if let Some(device) = source_device(info) {
                    let _ = tx.send(Collect::Item(device));
                }
            }
            ListResult::End => {
                let _ = tx.send(Collect::End);
            }
            ListResult::Error => {
                let _ = tx.send(Collect::Error);
            }
        });
    let devices = wait_collect(state, rx)?;
    devices
        .into_iter()
        .next()
        .ok_or_else(|| AppError::audio(format!("microphone source {wanted} was not found")))
}

fn sink_by_name(state: &ThreadState, name: &str) -> Result<AudioDevice> {
    let (tx, rx) = std_mpsc::channel();
    let wanted = name.to_string();
    let _op = state
        .context
        .borrow_mut()
        .introspect()
        .get_sink_info_by_name(name, move |result| match result {
            ListResult::Item(info) => {
                if let Some(device) = sink_device(info) {
                    let _ = tx.send(Collect::Item(device));
                }
            }
            ListResult::End => {
                let _ = tx.send(Collect::End);
            }
            ListResult::Error => {
                let _ = tx.send(Collect::Error);
            }
        });
    let devices = wait_collect(state, rx)?;
    devices
        .into_iter()
        .next()
        .ok_or_else(|| AppError::audio(format!("output sink {wanted} was not found")))
}

fn monitor_for_sink(state: &ThreadState, sink_name: &str) -> Result<AudioDevice> {
    let (tx, rx) = std_mpsc::channel::<Result<String>>();
    let sink_name = sink_name.to_string();
    let sink_name_cb = sink_name.clone();
    let _op = state
        .context
        .borrow_mut()
        .introspect()
        .get_sink_info_by_name(&sink_name, move |result| match result {
            ListResult::Item(info) => {
                let monitor = info
                    .monitor_source_name
                    .as_ref()
                    .map(|name| name.to_string());
                match monitor {
                    Some(name) => {
                        let _ = tx.send(Ok(name));
                    }
                    None => {
                        let _ = tx.send(Err(AppError::audio(format!(
                            "output sink {} has no monitor source",
                            info.name.as_deref().unwrap_or("unknown")
                        ))));
                    }
                }
            }
            ListResult::End => {}
            ListResult::Error => {
                let _ = tx.send(Err(AppError::audio(format!(
                    "failed to look up monitor source for sink {sink_name_cb}"
                ))));
            }
        });
    let deadline = std::time::Instant::now() + COMMAND_TIMEOUT;
    let monitor_name = loop {
        match rx.try_recv() {
            Ok(result) => break result?,
            Err(std_mpsc::TryRecvError::Empty) => {
                if std::time::Instant::now() > deadline {
                    return Err(AppError::audio(format!(
                        "timed out looking up monitor source for sink {sink_name}"
                    )));
                }
                poll_mainloop(&state.mainloop);
            }
            Err(std_mpsc::TryRecvError::Disconnected) => {
                return Err(AppError::audio(format!(
                    "monitor lookup channel closed for sink {sink_name}"
                )));
            }
        }
    };
    source_by_name(state, &monitor_name).map(|mut device| {
        device.role = DeviceRole::MonitorSource;
        device
    })
}

fn start_capture(
    state: &mut ThreadState,
    source: AudioSource,
    device: Option<String>,
    frames: mpsc::Sender<PcmFrame>,
) -> Result<()> {
    if state.streams.contains_key(&source) {
        stop_capture(state, source)?;
    }

    let source_name = match source {
        AudioSource::Mic => match device {
            Some(name) => name,
            None => current_microphone(state)?.id,
        },
        AudioSource::System => match device {
            Some(name) => name,
            None => current_monitor(state)?.id,
        },
    };

    tracing::info!(source = %source, device = %source_name, "starting capture");

    let stream = Stream::new(
        &mut state.context.borrow_mut(),
        &format!("voxtype-meeting-service-{source}"),
        &TARGET_SPEC,
        None,
    )
    .ok_or_else(|| {
        AppError::audio(format!(
            "could not create PulseAudio record stream for {source}"
        ))
    })?;

    let stream = Rc::new(RefCell::new(stream));
    {
        let stream_ref = stream.clone();
        let frames = frames.clone();
        stream
            .borrow_mut()
            .set_read_callback(Some(Box::new(move |_| {
                drain_stream(&stream_ref, source, &frames);
            })));
    }
    {
        let stream_ref = stream.clone();
        let events = state.events.clone();
        let device_id = source_name.clone();
        stream.borrow_mut().set_state_callback(Some(Box::new(move || {
            let failed = stream_ref
                .try_borrow()
                .ok()
                .is_some_and(|stream| {
                    matches!(
                        stream.get_state(),
                        stream::State::Failed | stream::State::Terminated
                    )
                });
            if failed {
                tracing::warn!(source = %source, device_id, "capture stream failed");
                let event = match source {
                    AudioSource::Mic => AudioEvent::MicrophoneUnavailable {
                        device_id: device_id.clone(),
                        reason: format!("microphone source {device_id} failed"),
                    },
                    AudioSource::System => AudioEvent::OutputUnavailable {
                        device_id: device_id.clone(),
                        reason: format!(
                            "system audio capture unavailable because monitor source {device_id} disappeared"
                        ),
                    },
                };
                let _ = events.send(event);
            }
        })));
    }

    stream
        .borrow_mut()
        .connect_record(Some(&source_name), None, stream::FlagSet::START_UNMUTED)
        .map_err(|error| {
            let kind = match source {
                AudioSource::Mic => "microphone",
                AudioSource::System => "system audio",
            };
            AppError::audio(format!(
                "{kind} capture unavailable because source {source_name} could not be connected: {error}"
            ))
        })?;

    let deadline = std::time::Instant::now() + CONNECT_TIMEOUT;
    loop {
        let ready = {
            let s = stream.borrow();
            matches!(s.get_state(), stream::State::Ready)
        };
        let failed = {
            let s = stream.borrow();
            matches!(
                s.get_state(),
                stream::State::Failed | stream::State::Terminated
            )
        };
        if ready {
            break;
        }
        if failed {
            return Err(AppError::audio(format!(
                "{} capture unavailable because source {source_name} failed to enter the ready state",
                match source {
                    AudioSource::Mic => "microphone",
                    AudioSource::System => "system audio",
                }
            )));
        }
        if std::time::Instant::now() > deadline {
            return Err(AppError::audio(format!(
                "timed out connecting {} capture to source {source_name}",
                match source {
                    AudioSource::Mic => "microphone",
                    AudioSource::System => "system audio",
                }
            )));
        }
        poll_mainloop(&state.mainloop);
    }

    state.stream_devices.insert(source, source_name);
    state.streams.insert(source, stream);
    Ok(())
}

fn stop_capture(state: &mut ThreadState, source: AudioSource) -> Result<()> {
    state.stream_devices.remove(&source);
    if let Some(stream) = state.streams.remove(&source) {
        stream.borrow_mut().set_read_callback(None);
        stream.borrow_mut().set_state_callback(None);
        if let Err(error) = stream.borrow_mut().disconnect() {
            tracing::warn!(source = %source, error = %error, "error disconnecting capture stream");
        }
        tracing::info!(source = %source, "stopped capture");
    }
    Ok(())
}

fn drain_stream(
    stream: &Rc<RefCell<Stream>>,
    source: AudioSource,
    frames: &mpsc::Sender<PcmFrame>,
) {
    loop {
        let peeked = {
            let mut stream = stream.borrow_mut();
            match stream.peek() {
                Ok(PeekResult::Data(bytes)) => {
                    let spec = stream.get_sample_spec().copied();
                    let bytes = bytes.to_vec();
                    let _ = stream.discard();
                    Some((bytes, spec))
                }
                Ok(PeekResult::Hole(_)) => {
                    let _ = stream.discard();
                    continue;
                }
                Ok(PeekResult::Empty) => None,
                Err(error) => {
                    tracing::warn!(source = %source, error = %error, "PulseAudio peek failed");
                    None
                }
            }
        };
        let Some((bytes, spec)) = peeked else {
            break;
        };
        let Some(spec) = spec else {
            continue;
        };
        let format = match map_format(spec.format) {
            Some(format) => format,
            None => {
                tracing::warn!(
                    source = %source,
                    ?spec.format,
                    "unsupported PulseAudio sample format"
                );
                continue;
            }
        };
        match convert::to_i16_mono_16k(&bytes, format, spec.channels.into(), spec.rate) {
            Ok(samples) if !samples.is_empty() => {
                let frame = PcmFrame {
                    source,
                    samples,
                    captured_at: Utc::now(),
                };
                if frames.try_send(frame).is_err() {
                    tracing::warn!(
                        source = %source,
                        "dropping in-memory PCM because the capture channel is full"
                    );
                }
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(source = %source, error = %error, "PCM conversion failed"),
        }
    }
}

fn map_format(format: Format) -> Option<PcmFormat> {
    match format {
        Format::U8 => Some(PcmFormat::U8),
        Format::S16le => Some(PcmFormat::S16Le),
        Format::S16be => Some(PcmFormat::S16Be),
        Format::S24le => Some(PcmFormat::S24Le),
        Format::S24_32le => Some(PcmFormat::S24_32Le),
        Format::S32le => Some(PcmFormat::S32Le),
        Format::F32le => Some(PcmFormat::F32Le),
        _ => None,
    }
}

fn source_device(info: &SourceInfo<'_>) -> Option<AudioDevice> {
    let id = info.name.as_ref()?.to_string();
    let description = info
        .description
        .as_ref()
        .map(|value| value.to_string())
        .unwrap_or_else(|| id.clone());
    let is_monitor = info.monitor_of_sink.is_some();
    Some(AudioDevice {
        id,
        description,
        role: if is_monitor {
            DeviceRole::MonitorSource
        } else {
            DeviceRole::Microphone
        },
    })
}

fn sink_device(info: &SinkInfo<'_>) -> Option<AudioDevice> {
    let id = info.name.as_ref()?.to_string();
    let description = info
        .description
        .as_ref()
        .map(|value| value.to_string())
        .unwrap_or_else(|| id.clone());
    Some(AudioDevice {
        id,
        description,
        role: DeviceRole::OutputSink,
    })
}

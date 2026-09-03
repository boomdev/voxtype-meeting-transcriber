use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::time::{timeout_at, Instant};
use tokio_util::sync::CancellationToken;

use crate::audio::AudioBackend;
use crate::config::Config;
use crate::daemon::RecordingChannels;
use crate::error::{AppError, Result};
use crate::paths::{self, PathResolver};
use crate::runtime::{
    lock_status, AudioServerHealth, EventBus, SharedStatus, CONTROL_IDLE_TIMEOUT_SECS,
    CONTROL_MAX_LINE_BYTES,
};
use crate::storage::Db;

pub mod handlers;
pub mod protocol;

use protocol::{encode_ok, encode_response, parse_request, Response, StatusPayload};

#[derive(Clone)]
pub struct ControlExtras {
    pub config: Arc<RwLock<Config>>,
    pub events: EventBus,
    pub recording: RecordingChannels,
    pub backend: Arc<dyn AudioBackend>,
}

pub struct BoundSocket {
    listener: UnixListener,
    path: PathBuf,
}

impl Drop for BoundSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub async fn bind_or_fail(paths: &PathResolver) -> Result<BoundSocket> {
    let dir = paths.runtime_dir()?;
    paths::ensure_private_dir(&dir)?;
    let path = paths.control_socket()?;
    if path.exists() {
        match std::os::unix::net::UnixStream::connect(&path) {
            Ok(_) => {
                return Err(AppError::control(
                    "voxtype-meeting-service is already running",
                ));
            }
            Err(_) => {
                tracing::warn!(
                    path = %path.display(),
                    "removing stale control socket"
                );
                std::fs::remove_file(&path)?;
            }
        }
    }
    let listener = UnixListener::bind(&path).map_err(|error| {
        AppError::control(format!(
            "could not bind control socket {}: {error}",
            path.display()
        ))
    })?;
    Ok(BoundSocket { listener, path })
}

pub async fn serve(
    socket: BoundSocket,
    paths: PathResolver,
    db_path: PathBuf,
    status: SharedStatus,
    shutdown: CancellationToken,
    extras: Option<ControlExtras>,
) -> Result<()> {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            accepted = socket.listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let paths = paths.clone();
                        let db_path = db_path.clone();
                        let status = status.clone();
                        let shutdown = shutdown.clone();
                        let extras = extras.clone();
                        tokio::spawn(async move {
                            handle_client(stream, paths, db_path, status, shutdown, extras).await;
                        });
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "control socket accept failed");
                    }
                }
            }
        }
    }
    Ok(())
}

async fn handle_client(
    stream: UnixStream,
    paths: PathResolver,
    db_path: PathBuf,
    status: SharedStatus,
    shutdown: CancellationToken,
    extras: Option<ControlExtras>,
) {
    let deadline = Instant::now() + Duration::from_secs(CONTROL_IDLE_TIMEOUT_SECS);
    let (mut reader, mut writer) = stream.into_split();
    let line = match read_limited_line(&mut reader, CONTROL_MAX_LINE_BYTES, deadline).await {
        Ok(line) => line,
        Err(LineReadError::TooLong) => {
            let mut payload = encode_response(&Response::error(
                "control message exceeds protocol maximum",
            ));
            payload.push('\n');
            let _ = timeout_at(deadline, writer.write_all(payload.as_bytes())).await;
            return;
        }
        Err(_) => return,
    };
    let request = match parse_request(line.trim()) {
        Err(error) => {
            let mut payload = encode_response(&Response::error(error));
            payload.push('\n');
            let _ = writer.write_all(payload.as_bytes()).await;
            return;
        }
        Ok(request) => request,
    };
    match handlers::handle_request(
        request,
        &paths,
        &db_path,
        &status,
        extras.as_ref(),
        &shutdown,
    )
    .await
    {
        handlers::HandlerOut::Line(mut payload) => {
            payload.push('\n');
            let _ = writer.write_all(payload.as_bytes()).await;
        }
        handlers::HandlerOut::Subscribe => {
            let mut payload = encode_ok(serde_json::json!({ "subscribed": true }));
            payload.push('\n');
            let _ = writer.write_all(payload.as_bytes()).await;
            let Some(extras) = extras else {
                return;
            };
            let mut rx = extras.events.subscribe();
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    event = rx.recv() => {
                        match event {
                            Ok(event) => {
                                if let Ok(mut line) = serde_json::to_string(&event) {
                                    line.push('\n');
                                    if writer.write_all(line.as_bytes()).await.is_err() {
                                        break;
                                    }
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(_) => break,
                        }
                    }
                }
            }
        }
    }
}

pub fn collect_status(
    paths: &PathResolver,
    db_path: &Path,
    status: &SharedStatus,
) -> Result<StatusPayload> {
    let snapshot = lock_status(status).clone();
    let db = Db::open(db_path)?;
    let (pending, processing, completed) = db.with_conn(crate::storage::jobs::counts)?;
    let stored = crate::disk::stored_audio_bytes(&paths.sessions_dir())?;
    let free = crate::disk::free_bytes(&paths.data_dir())
        .map(crate::disk::format_bytes)
        .unwrap_or_else(|_| "unknown".to_string());
    let transcript_path = snapshot.session_id.as_ref().map(|id| {
        paths
            .session_dir(id)
            .join("transcript.md")
            .display()
            .to_string()
    });
    Ok(StatusPayload {
        session_id: snapshot.session_id,
        session_started: snapshot.session_started_at,
        microphone: snapshot.microphone.as_ref().map(|d| d.summary()),
        output: snapshot.output.as_ref().map(|d| d.summary()),
        monitor: snapshot.monitor.as_ref().map(|d| d.summary()),
        audio_server: match snapshot.audio_server {
            AudioServerHealth::Available => "available".into(),
            AudioServerHealth::Unavailable => "unavailable".into(),
        },
        provider: snapshot.provider.as_str().to_string(),
        pending_jobs: pending,
        processing_jobs: processing,
        completed_jobs: completed,
        stored_audio: crate::disk::format_bytes(stored),
        free_disk: free,
        transcript_path,
        capture_active: snapshot.capture_active,
        capture_stop_reason: snapshot.capture_stop_reason,
    })
}

pub fn format_status_text(service: &str, payload: Option<&StatusPayload>) -> String {
    let mut out = format!("Service: {service}\n");
    match payload {
        None => out.push_str("No active session.\n"),
        Some(status) => match &status.session_id {
            Some(id) => {
                out.push_str(&format!("Session ID: {id}\n"));
                if let Some(started) = &status.session_started {
                    out.push_str(&format!("Session started: {started}\n"));
                }
                out.push_str(&format!(
                    "Microphone device: {}\n",
                    status.microphone.as_deref().unwrap_or("unknown")
                ));
                out.push_str(&format!(
                    "System output device: {}\n",
                    status.output.as_deref().unwrap_or("unknown")
                ));
                if let Some(monitor) = &status.monitor {
                    out.push_str(&format!("Monitor source: {monitor}\n"));
                }
                out.push_str(&format!("Audio server status: {}\n", status.audio_server));
                out.push_str(&format!("Transcription provider: {}\n", status.provider));
                out.push_str(&format!("Pending jobs: {}\n", status.pending_jobs));
                out.push_str(&format!("Processing jobs: {}\n", status.processing_jobs));
                out.push_str(&format!("Completed jobs: {}\n", status.completed_jobs));
                out.push_str(&format!("Stored audio size: {}\n", status.stored_audio));
                out.push_str(&format!("Free disk space: {}\n", status.free_disk));
                if let Some(path) = &status.transcript_path {
                    out.push_str(&format!("Transcript path: {path}\n"));
                }
                if let Some(reason) = &status.capture_stop_reason {
                    out.push_str(&format!("Capture stopped: {reason}\n"));
                }
            }
            None => {
                out.push_str("No active session.\n");
                out.push_str(&format!("Pending jobs: {}\n", status.pending_jobs));
                out.push_str(&format!("Processing jobs: {}\n", status.processing_jobs));
                out.push_str(&format!("Completed jobs: {}\n", status.completed_jobs));
                out.push_str(&format!("Stored audio size: {}\n", status.stored_audio));
                out.push_str(&format!("Free disk space: {}\n", status.free_disk));
            }
        },
    }
    out
}

pub async fn send_request(path: &Path, op: &str) -> Result<Response> {
    let line = send_json(path, &serde_json::json!({ "op": op })).await?;
    serde_json::from_str(&line)
        .map_err(|error| AppError::control(format!("invalid control socket response: {error}")))
}

pub async fn send_json(path: &Path, body: &serde_json::Value) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(CONTROL_IDLE_TIMEOUT_SECS);
    let stream = timeout_at(deadline, UnixStream::connect(path))
        .await
        .map_err(|_| AppError::control("timed out connecting to control socket"))?
        .map_err(|error| {
            AppError::control(format!(
                "could not connect to control socket {}: {error}",
                path.display()
            ))
        })?;
    let (mut reader, mut writer) = stream.into_split();
    let mut payload = serde_json::to_string(body)?;
    if payload.len() > CONTROL_MAX_LINE_BYTES {
        return Err(AppError::control(
            "control message exceeds protocol maximum",
        ));
    }
    payload.push('\n');
    timeout_at(deadline, writer.write_all(payload.as_bytes()))
        .await
        .map_err(|_| AppError::control("timed out writing control socket request"))?
        .map_err(|error| AppError::control(format!("control socket write failed: {error}")))?;
    timeout_at(deadline, writer.shutdown())
        .await
        .map_err(|_| AppError::control("timed out closing control socket write"))?
        .map_err(|error| AppError::control(format!("control socket shutdown failed: {error}")))?;
    read_limited_line(&mut reader, CONTROL_MAX_LINE_BYTES, deadline)
        .await
        .map_err(LineReadError::into_control)
}

#[derive(Debug)]
enum LineReadError {
    Timeout,
    Closed,
    TooLong,
    InvalidUtf8,
    Io(std::io::Error),
}

impl LineReadError {
    fn into_control(self) -> AppError {
        match self {
            Self::Timeout => AppError::control("timed out waiting for control socket response"),
            Self::Closed => AppError::control("control socket closed without a response"),
            Self::TooLong => AppError::control("control message exceeds protocol maximum"),
            Self::InvalidUtf8 => AppError::control("control message is not valid UTF-8"),
            Self::Io(error) => AppError::control(format!("control socket read failed: {error}")),
        }
    }
}

async fn read_limited_line<R: AsyncRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
    deadline: Instant,
) -> std::result::Result<String, LineReadError> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if Instant::now() >= deadline {
            return Err(LineReadError::Timeout);
        }
        let n = match timeout_at(deadline, reader.read(&mut chunk)).await {
            Err(_) => return Err(LineReadError::Timeout),
            Ok(Err(error)) => return Err(LineReadError::Io(error)),
            Ok(Ok(0)) => return Err(LineReadError::Closed),
            Ok(Ok(n)) => n,
        };
        let bytes = &chunk[..n];
        if let Some(pos) = bytes.iter().position(|&b| b == b'\n') {
            if buf.len().saturating_add(pos) > max_bytes {
                return Err(LineReadError::TooLong);
            }
            buf.extend_from_slice(&bytes[..pos]);
            return String::from_utf8(buf).map_err(|_| LineReadError::InvalidUtf8);
        }
        if buf.len().saturating_add(bytes.len()) > max_bytes {
            return Err(LineReadError::TooLong);
        }
        buf.extend_from_slice(bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::{bind_or_fail, read_limited_line, serve, LineReadError};
    use crate::config::ProviderKind;
    use crate::paths::PathResolver;
    use crate::runtime::{RuntimeStatus, CONTROL_MAX_LINE_BYTES};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tempfile::tempdir;
    use tokio::io::AsyncWriteExt;
    use tokio::time::Instant;
    use tokio_util::sync::CancellationToken;

    fn test_paths(root: &std::path::Path) -> PathResolver {
        PathResolver::from_parts(
            root.to_path_buf(),
            Some(root.join("config")),
            Some(root.join("data")),
            Some(root.join("runtime")),
            1000,
        )
    }

    #[tokio::test]
    async fn status_and_stop_round_trip() {
        let root = tempdir().unwrap();
        let paths = test_paths(root.path());
        crate::paths::ensure_dir(&paths.data_dir()).unwrap();
        let db = crate::storage::Db::open(paths.db_path()).unwrap();
        drop(db);
        let socket = bind_or_fail(&paths).await.unwrap();
        let status = Arc::new(Mutex::new(RuntimeStatus::new(ProviderKind::Openai)));
        {
            let mut s = status.lock().unwrap();
            s.session_id = Some("sess".into());
            s.session_started_at = Some("now".into());
            s.capture_active = true;
        }
        let shutdown = CancellationToken::new();
        let server = {
            let paths = paths.clone();
            let db_path = paths.db_path();
            let status = status.clone();
            let shutdown = shutdown.clone();
            tokio::spawn(async move { serve(socket, paths, db_path, status, shutdown, None).await })
        };
        let response = super::send_request(&paths.control_socket().unwrap(), "status")
            .await
            .unwrap();
        match response {
            super::protocol::Response::OkStatus { ok, status } => {
                assert!(ok);
                assert_eq!(status.session_id.as_deref(), Some("sess"));
            }
            other => panic!("unexpected {other:?}"),
        }
        let _ = super::send_request(&paths.control_socket().unwrap(), "stop")
            .await
            .unwrap();
        assert!(shutdown.is_cancelled());
        server.abort();
    }

    #[tokio::test]
    async fn second_bind_fails_when_socket_is_live() {
        let root = tempdir().unwrap();
        let paths = test_paths(root.path());
        let _first = bind_or_fail(&paths).await.unwrap();
        let second = bind_or_fail(&paths).await;
        let err = match second {
            Err(error) => error,
            Ok(_) => panic!("second bind should fail while the first socket is live"),
        };
        assert!(err.to_string().contains("already running"), "{err}");
    }

    #[tokio::test]
    async fn limited_line_accepts_payload_at_cap() {
        let (client, mut server) = tokio::io::duplex(128);
        let deadline = Instant::now() + Duration::from_secs(1);
        tokio::spawn(async move {
            let mut payload = vec![b'a'; 8];
            payload.push(b'\n');
            server.write_all(&payload).await.unwrap();
        });
        let mut reader = client;
        let line = read_limited_line(&mut reader, 8, deadline).await.unwrap();
        assert_eq!(line.len(), 8);
    }

    #[tokio::test]
    async fn limited_line_rejects_overflow_while_streaming() {
        let (client, mut server) = tokio::io::duplex(4096);
        let deadline = Instant::now() + Duration::from_secs(1);
        tokio::spawn(async move {
            server.write_all(&vec![b'x'; 64]).await.unwrap();
        });
        let mut reader = client;
        let error = read_limited_line(&mut reader, 16, deadline)
            .await
            .unwrap_err();
        assert!(matches!(error, LineReadError::TooLong));
    }

    #[tokio::test]
    async fn limited_line_uses_total_deadline() {
        let (client, _server) = tokio::io::duplex(64);
        let deadline = Instant::now() + Duration::from_millis(25);
        let mut reader = client;
        let error = read_limited_line(&mut reader, 16, deadline)
            .await
            .unwrap_err();
        assert!(matches!(error, LineReadError::Timeout));
    }

    #[tokio::test]
    async fn oversized_request_is_rejected() {
        let root = tempdir().unwrap();
        let paths = test_paths(root.path());
        crate::paths::ensure_dir(&paths.data_dir()).unwrap();
        let db = crate::storage::Db::open(paths.db_path()).unwrap();
        drop(db);
        let socket = bind_or_fail(&paths).await.unwrap();
        let status = Arc::new(Mutex::new(RuntimeStatus::new(ProviderKind::Openai)));
        let shutdown = CancellationToken::new();
        let server = {
            let paths = paths.clone();
            let db_path = paths.db_path();
            let status = status.clone();
            let shutdown = shutdown.clone();
            tokio::spawn(async move { serve(socket, paths, db_path, status, shutdown, None).await })
        };
        let path = paths.control_socket().unwrap();
        let mut stream = tokio::net::UnixStream::connect(&path).await.unwrap();
        stream
            .write_all(&vec![b'x'; CONTROL_MAX_LINE_BYTES + 1])
            .await
            .unwrap();
        stream.write_all(b"\n").await.ok();
        let mut reply = Vec::new();
        let _ = tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut reply).await;
        server.abort();
        let text = String::from_utf8_lossy(&reply);
        assert!(
            text.contains("protocol maximum"),
            "unexpected reply: {text}"
        );
    }
}

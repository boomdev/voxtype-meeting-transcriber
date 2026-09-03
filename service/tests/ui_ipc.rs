use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use tempfile::tempdir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use voxtype_meeting_service::audio::fake::FakeAudioBackend;
use voxtype_meeting_service::config::Config;
use voxtype_meeting_service::control::{bind_or_fail, send_json, serve, ControlExtras};
use voxtype_meeting_service::daemon::{RecordingChannels, StartRequest, StopOutcome};
use voxtype_meeting_service::paths::PathResolver;
use voxtype_meeting_service::runtime::{event_bus, RuntimeStatus, UiEvent};
use voxtype_meeting_service::storage::sessions::insert_running_session;
use voxtype_meeting_service::storage::types::SessionState;
use voxtype_meeting_service::storage::Db;

fn test_paths(root: &std::path::Path) -> PathResolver {
    PathResolver::from_parts(
        root.to_path_buf(),
        Some(root.join("config")),
        Some(root.join("data")),
        Some(root.join("runtime")),
        1000,
    )
}

fn extras() -> ControlExtras {
    let (start_tx, mut start_rx) = mpsc::channel::<StartRequest>(4);
    let (stop_tx, mut stop_rx) =
        mpsc::channel::<oneshot::Sender<voxtype_meeting_service::Result<StopOutcome>>>(4);
    tokio::spawn(async move {
        while let Some(start) = start_rx.recv().await {
            let _ = start.reply.send(Ok("sess-started".into()));
        }
    });
    tokio::spawn(async move {
        while let Some(reply) = stop_rx.recv().await {
            let _ = reply.send(Ok(StopOutcome {
                session_id: "sess-started".into(),
                duration_secs: 32,
                transcription_pending: true,
            }));
        }
    });
    ControlExtras {
        config: Arc::new(RwLock::new(Config::default())),
        events: event_bus(),
        recording: RecordingChannels {
            start: start_tx,
            stop: stop_tx,
        },
        backend: Arc::new(FakeAudioBackend::new(1, 160)),
    }
}

async fn parse_ok(path: &std::path::Path, body: serde_json::Value) -> serde_json::Value {
    let line = send_json(path, &body).await.expect("ipc");
    let value: serde_json::Value = serde_json::from_str(&line).expect("json");
    assert_eq!(
        value.get("ok"),
        Some(&serde_json::Value::Bool(true)),
        "{line}"
    );
    value
}

#[tokio::test]
async fn ui_ipc_state_start_stop_list_config_events() {
    let root = tempdir().unwrap();
    let paths = test_paths(root.path());
    voxtype_meeting_service::paths::ensure_dir(&paths.data_dir()).unwrap();
    voxtype_meeting_service::paths::ensure_dir(&paths.config_dir()).unwrap();
    voxtype_meeting_service::paths::ensure_dir(&paths.sessions_dir()).unwrap();
    let db = Db::open(paths.db_path()).unwrap();
    db.with_conn(|conn| {
        let session = insert_running_session(conn, "listed-session", None, None, None)?;
        voxtype_meeting_service::storage::sessions::set_session_state(
            conn,
            &session.id,
            SessionState::Completed,
            Some(session.started_at.as_str()),
        )?;
        voxtype_meeting_service::storage::sessions::set_session_title(
            conn,
            &session.id,
            "Meeting with Paul",
        )?;
        Ok(())
    })
    .unwrap();
    drop(db);

    let extras = extras();
    let events = extras.events.clone();
    let socket = bind_or_fail(&paths).await.unwrap();
    let status = Arc::new(Mutex::new(RuntimeStatus::new(
        voxtype_meeting_service::config::ProviderKind::Voxtype,
    )));
    let shutdown = CancellationToken::new();
    let server = {
        let paths = paths.clone();
        let db_path = paths.db_path();
        let status = status.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(
            async move { serve(socket, paths, db_path, status, shutdown, Some(extras)).await },
        )
    };
    let sock = paths.control_socket().unwrap();

    let state = parse_ok(&sock, serde_json::json!({"op": "get_state"})).await;
    assert_eq!(state["recording"]["active"], false);
    assert!(state["panel"]["state"].is_string());
    assert_eq!(state["recent_sessions"][0]["title"], "Meeting with Paul");
    assert!(state["panel"]["pending_jobs"].is_number());

    let started = parse_ok(&sock, serde_json::json!({"op": "start_recording"})).await;
    assert_eq!(started["session_id"], "sess-started");

    let stopped = parse_ok(&sock, serde_json::json!({"op": "stop_recording"})).await;
    assert_eq!(stopped["session_id"], "sess-started");
    assert_eq!(stopped["duration_secs"], 32);
    assert_eq!(stopped["transcription_pending"], true);

    let listed = parse_ok(&sock, serde_json::json!({"op": "list_sessions"})).await;
    assert_eq!(listed["sessions"][0]["stored_title"], "Meeting with Paul");
    assert_eq!(listed["sessions"][0]["ui_status"], "complete");

    let cfg = parse_ok(&sock, serde_json::json!({"op": "get_config"})).await;
    assert!(cfg["config"]["transcript"]["omit_single_source_headers"].is_boolean());
    assert!(cfg["config"]["transcription"]["languages"].is_array());
    assert_eq!(cfg["config"]["transcription"]["provider"], "voxtype");

    let updated = parse_ok(
        &sock,
        serde_json::json!({
            "op": "update_config",
            "config": { "general": { "minimum_free_space_mb": 512 } }
        }),
    )
    .await;
    assert_eq!(updated["config"]["general"]["minimum_free_space_mb"], 512);
    assert_eq!(
        updated["config"]["transcript"]["omit_single_source_headers"],
        cfg["config"]["transcript"]["omit_single_source_headers"]
    );

    let rejected = send_json(
        &sock,
        &serde_json::json!({
            "op": "update_config",
            "config": { "transcription": { "max_concurrent_jobs": 99 } }
        }),
    )
    .await
    .unwrap();
    let rejected: serde_json::Value = serde_json::from_str(&rejected).unwrap();
    assert_eq!(rejected["ok"], false);
    assert_eq!(rejected["code"], "validation");
    assert!(rejected["fields"]["transcription.max_concurrent_jobs"]
        .as_str()
        .unwrap()
        .contains("max_concurrent_jobs"));

    let stream = UnixStream::connect(&sock).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    writer.write_all(b"{\"op\":\"subscribe\"}\n").await.unwrap();
    let mut lines = BufReader::new(reader).lines();
    let ack = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(ack.contains("subscribed"), "{ack}");
    events
        .send(UiEvent::new("recording_started").session("sess-started"))
        .unwrap();
    let event_line = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(event_line.contains("recording_started"), "{event_line}");

    shutdown.cancel();
    server.abort();
}

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::tempdir;
use tokio_util::sync::CancellationToken;
use voxtype_meeting_service::audio::fake::FakeAudioBackend;
use voxtype_meeting_service::audio::{AudioDevice, AudioSource, DeviceRole};
use voxtype_meeting_service::capture::run_session_with_backend;
use voxtype_meeting_service::config::Config;
use voxtype_meeting_service::paths::PathResolver;
use voxtype_meeting_service::runtime::RuntimeStatus;
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

#[tokio::test(start_paused = true)]
async fn system_failure_does_not_stop_mic_or_complete_session() {
    let root = tempdir().unwrap();
    let paths = test_paths(root.path());
    voxtype_meeting_service::paths::ensure_dir(&paths.sessions_dir()).unwrap();
    let db = Db::open(paths.db_path()).unwrap();
    let config = Config::default();
    let backend = FakeAudioBackend::continuous(1600);
    let shutdown = CancellationToken::new();
    let status = Arc::new(Mutex::new(RuntimeStatus::new(
        config.transcription.provider,
    )));
    let driver = {
        let backend = backend.clone();
        let shutdown = shutdown.clone();
        let db_path = paths.db_path();
        async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            let mic_before = backend.frames_emitted(AudioSource::Mic);
            let sys_before = backend.frames_emitted(AudioSource::System);
            assert!(mic_before > 0, "mic should have started");
            assert!(sys_before > 0, "system should have started");
            backend.fail_source(AudioSource::System, true);
            backend.hide_output();
            tokio::time::sleep(Duration::from_millis(80)).await;
            let mic_after = backend.frames_emitted(AudioSource::Mic);
            let sys_after = backend.frames_emitted(AudioSource::System);
            assert!(mic_after > mic_before, "mic should keep capturing");
            assert!(
                sys_after <= sys_before + 3,
                "system should stop producing after failure"
            );
            let running: i64 = {
                let db = Db::open(&db_path).unwrap();
                db.with_conn(|conn| {
                    Ok(conn.query_row(
                        "SELECT COUNT(*) FROM sessions WHERE state = 'running'",
                        [],
                        |row| row.get(0),
                    )?)
                })
                .unwrap()
            };
            assert_eq!(running, 1);
            shutdown.cancel();
        }
    };
    let (session, ()) = tokio::join!(
        run_session_with_backend(
            Arc::new(backend.clone()),
            &db,
            &paths,
            &config,
            shutdown,
            status,
        ),
        driver
    );
    let session_id = session.unwrap();
    let record = db
        .with_conn(|conn| {
            voxtype_meeting_service::storage::sessions::get_session(conn, &session_id.to_string())
        })
        .unwrap()
        .unwrap();
    assert_eq!(record.state, SessionState::Completed);
}

#[tokio::test(start_paused = true)]
async fn output_changed_reconnects_system_same_session() {
    let root = tempdir().unwrap();
    let paths = test_paths(root.path());
    voxtype_meeting_service::paths::ensure_dir(&paths.sessions_dir()).unwrap();
    let db = Db::open(paths.db_path()).unwrap();
    let config = Config::default();
    let backend = FakeAudioBackend::continuous(800);
    let shutdown = CancellationToken::new();
    let status = Arc::new(Mutex::new(RuntimeStatus::new(
        config.transcription.provider,
    )));
    let driver = {
        let backend = backend.clone();
        let shutdown = shutdown.clone();
        let db_path = paths.db_path();
        async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let session_id: String = {
                let db = Db::open(&db_path).unwrap();
                db.with_conn(|conn| {
                    Ok(conn
                        .query_row("SELECT id FROM sessions", [], |row| row.get::<_, String>(0))?)
                })
                .unwrap()
            };
            backend.set_output(
                AudioDevice {
                    id: "headphones".into(),
                    description: "Headphones".into(),
                    role: DeviceRole::OutputSink,
                },
                AudioDevice {
                    id: "headphones.monitor".into(),
                    description: "Headphones monitor".into(),
                    role: DeviceRole::MonitorSource,
                },
            );
            tokio::time::sleep(Duration::from_millis(80)).await;
            assert_eq!(
                backend.last_capture_device(AudioSource::System).as_deref(),
                Some("headphones.monitor")
            );
            let still: String = {
                let db = Db::open(&db_path).unwrap();
                db.with_conn(|conn| {
                    Ok(conn.query_row("SELECT id FROM sessions", [], |row| row.get(0))?)
                })
                .unwrap()
            };
            assert_eq!(still, session_id);
            shutdown.cancel();
        }
    };
    let (result, ()) = tokio::join!(
        run_session_with_backend(
            Arc::new(backend.clone()),
            &db,
            &paths,
            &config,
            shutdown,
            status,
        ),
        driver
    );
    result.unwrap();
}

#[tokio::test(start_paused = true)]
async fn audio_server_unavailable_then_available_reconnects_both() {
    let root = tempdir().unwrap();
    let paths = test_paths(root.path());
    voxtype_meeting_service::paths::ensure_dir(&paths.sessions_dir()).unwrap();
    let db = Db::open(paths.db_path()).unwrap();
    let config = Config::default();
    let backend = FakeAudioBackend::continuous(800);
    let shutdown = CancellationToken::new();
    let status = Arc::new(Mutex::new(RuntimeStatus::new(
        config.transcription.provider,
    )));
    let driver = {
        let backend = backend.clone();
        let shutdown = shutdown.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            let mic_before = backend.frames_emitted(AudioSource::Mic);
            backend.set_server_available(false);
            tokio::time::sleep(Duration::from_millis(20)).await;
            let mic_mid = backend.frames_emitted(AudioSource::Mic);
            tokio::time::sleep(Duration::from_secs(5)).await;
            backend.set_server_available(true);
            tokio::time::sleep(Duration::from_millis(80)).await;
            let mic_after = backend.frames_emitted(AudioSource::Mic);
            let sys_after = backend.frames_emitted(AudioSource::System);
            assert!(mic_after > mic_mid.max(mic_before));
            assert!(sys_after > 0);
            shutdown.cancel();
        }
    };
    let (result, ()) = tokio::join!(
        run_session_with_backend(
            Arc::new(backend.clone()),
            &db,
            &paths,
            &config,
            shutdown,
            status,
        ),
        driver
    );
    result.unwrap();
}

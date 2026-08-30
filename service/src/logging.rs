use tracing_subscriber::fmt;
use tracing_subscriber::EnvFilter;

pub fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("voxtype-meeting-service=info,warn"));

    let _ = fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_ansi(true)
        .try_init();
}

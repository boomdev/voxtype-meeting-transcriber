pub mod audio;
pub mod capture;
pub mod cleanup;
pub mod cli;
pub mod commands;
pub mod config;
pub mod control;
pub mod daemon;
pub mod disk;
pub mod distro;
pub mod doctor;
pub mod encode;
pub mod error;
pub mod logging;
pub mod paths;
pub mod runtime;
pub mod service;
pub mod session_status;
pub mod storage;
pub mod timeutil;
pub mod transcript;
pub mod transcription;

pub use error::{AppError, Result};

use clap::Parser;
use cli::{Cli, Command};

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    logging::init_logging();
    tracing::info!("starting voxtype-meeting-service");
    dispatch(cli).await
}

async fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Run => daemon::cmd_run().await,
        Command::Start => service::cmd_start(),
        Command::Stop => service::cmd_stop(&crate::paths::PathResolver::from_env()?).await,
        Command::Status => service::cmd_status(&crate::paths::PathResolver::from_env()?).await,
        Command::Devices => capture::cmd_devices().await,
        Command::Doctor => doctor::cmd_doctor(),
        Command::Record { action } => commands::cmd_record(action).await,
        Command::Retry { provider } => commands::cmd_retry(provider.map(Into::into)),
        Command::Retranscribe {
            session_id,
            provider,
        } => commands::cmd_retranscribe(&session_id, provider.into()),
        Command::Rebuild { session_id } => commands::cmd_rebuild(&session_id),
        Command::Cleanup { apply } => cleanup::cmd_cleanup(apply),
    }
}

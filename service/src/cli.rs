use clap::{Parser, Subcommand, ValueEnum};

use crate::config::ProviderKind;

#[derive(Debug, Parser)]
#[command(
    name = "voxtype-meeting-service",
    version,
    about = "Local Linux audio capture and transcription"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the idle daemon (capture starts only via record start / UI)
    Run,
    /// Start the systemd user service
    Start,
    /// Stop the running application through its control socket
    Stop,
    /// Show service, session, and transcription status
    Status,
    /// List microphone, output, and monitor devices
    Devices,
    /// Run diagnostics
    Doctor,
    /// Control recording on a running daemon
    Record {
        #[command(subcommand)]
        action: RecordCommand,
    },
    /// Retry incomplete transcription jobs
    Retry {
        #[arg(long, value_enum)]
        provider: Option<ProviderArg>,
    },
    /// Retranscribe every stored chunk in a session with a provider
    Retranscribe {
        session_id: String,
        #[arg(long, value_enum)]
        provider: ProviderArg,
    },
    /// Regenerate transcript.md and transcript.jsonl from SQLite
    Rebuild { session_id: String },
    /// Show (or apply) cleanup of fully transcribed completed sessions
    Cleanup {
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum RecordCommand {
    /// Start a recording session on the running daemon
    Start,
    /// Stop the active recording without stopping the daemon
    Stop,
    /// Show whether the daemon is recording
    Status,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum ProviderArg {
    Voxtype,
    WhisperCpp,
}

impl From<ProviderArg> for ProviderKind {
    fn from(value: ProviderArg) -> Self {
        match value {
            ProviderArg::Voxtype => ProviderKind::Voxtype,
            ProviderArg::WhisperCpp => ProviderKind::WhisperCpp,
        }
    }
}

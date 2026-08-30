use thiserror::Error;

pub type Result<T> = std::result::Result<T, AppError>;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("{0}")]
    Config(String),

    #[error("{0}")]
    Path(String),

    #[error("{0}")]
    Distro(String),

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("{0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Audio(String),

    #[error("{0}")]
    Encode(String),

    #[error("{0}")]
    Transcription(String),

    #[error("{0}")]
    Control(String),

    #[error("{0}")]
    Json(#[from] serde_json::Error),

    #[error("{0}")]
    Other(String),
}

impl AppError {
    pub fn config(message: impl Into<String>) -> Self {
        Self::Config(message.into())
    }

    pub fn path(message: impl Into<String>) -> Self {
        Self::Path(message.into())
    }

    pub fn distro(message: impl Into<String>) -> Self {
        Self::Distro(message.into())
    }

    pub fn audio(message: impl Into<String>) -> Self {
        Self::Audio(message.into())
    }

    pub fn encode(message: impl Into<String>) -> Self {
        Self::Encode(message.into())
    }

    pub fn transcription(message: impl Into<String>) -> Self {
        Self::Transcription(message.into())
    }

    pub fn control(message: impl Into<String>) -> Self {
        Self::Control(message.into())
    }

    pub fn other(message: impl Into<String>) -> Self {
        Self::Other(message.into())
    }
}

use std::path::PathBuf;

use chrono::{DateTime, Utc};

use crate::audio::AudioSource;
use crate::config::{Config, ProviderKind};
use crate::error::Result;
use crate::storage::types::AudioChunkRecord;

pub mod bounded;
pub mod fake;
pub mod retry;
pub mod voxtype;
pub mod whisper_cpp;
pub mod worker;

#[derive(Clone, Debug)]
pub struct AudioChunkRef {
    pub id: String,
    pub file_path: PathBuf,
    pub source: AudioSource,
    pub sequence: u64,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
}

impl AudioChunkRef {
    pub fn from_record(record: &AudioChunkRecord) -> Result<Self> {
        Ok(Self {
            id: record.id.clone(),
            file_path: PathBuf::from(&record.file_path),
            source: record.source,
            sequence: record.sequence,
            started_at: crate::timeutil::parse_rfc3339(&record.started_at)?,
            ended_at: crate::timeutil::parse_rfc3339(&record.ended_at)?,
        })
    }
}

#[derive(Clone, Debug)]
pub struct TranscriptionResult {
    pub text: String,
    pub provider: ProviderKind,
    pub model: String,
    pub provider_metadata: Option<serde_json::Value>,
}

#[async_trait::async_trait]
pub trait TranscriptionProvider: Send + Sync {
    fn name(&self) -> ProviderKind;
    async fn transcribe(&self, chunk: &AudioChunkRef) -> Result<TranscriptionResult>;
}

pub fn provider_from_kind(
    kind: ProviderKind,
    config: &Config,
) -> Result<Box<dyn TranscriptionProvider>> {
    provider_for_job(kind, None, config)
}

pub fn provider_for_job(
    kind: ProviderKind,
    model: Option<&str>,
    config: &Config,
) -> Result<Box<dyn TranscriptionProvider>> {
    match kind {
        ProviderKind::Voxtype => Ok(Box::new(voxtype::VoxtypeProvider::new(
            model.unwrap_or(&config.transcription.model).to_string(),
        ))),
        ProviderKind::Openai => Err(crate::error::AppError::transcription(
            "the OpenAI remote provider has been removed; use voxtype or whisper_cpp",
        )),
        ProviderKind::WhisperCpp => {
            let model_path = model
                .filter(|value| !value.is_empty())
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| config.transcription.whisper_cpp.model.clone());
            Ok(Box::new(whisper_cpp::WhisperCppProvider::new(
                config.transcription.whisper_cpp.executable.clone(),
                model_path,
                crate::config::whisper_cli_language(&config.transcription.languages)?,
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_toml_with_home;
    use std::path::Path;

    #[test]
    fn factory_builds_local_kinds() {
        let config =
            parse_toml_with_home(crate::config::DEFAULT_CONFIG_TOML, Path::new("/home/xa"))
                .unwrap();
        let voxtype = provider_from_kind(ProviderKind::Voxtype, &config).unwrap();
        assert_eq!(voxtype.name(), ProviderKind::Voxtype);
        let whisper = provider_from_kind(ProviderKind::WhisperCpp, &config).unwrap();
        assert_eq!(whisper.name(), ProviderKind::WhisperCpp);
        match provider_from_kind(ProviderKind::Openai, &config) {
            Err(error) => assert!(
                error
                    .to_string()
                    .contains("OpenAI remote provider has been removed"),
                "{error}"
            ),
            Ok(_) => panic!("OpenAI provider should be unavailable"),
        }
    }

    #[test]
    fn whisper_factory_rejects_multiple_live_languages() {
        let config = parse_toml_with_home(
            r#"
[transcription]
provider = "voxtype"
model = "configured"
languages = ["fr", "en"]
"#,
            Path::new("/home/xa"),
        )
        .unwrap();
        let error = provider_from_kind(ProviderKind::WhisperCpp, &config)
            .err()
            .expect("whisper.cpp must reject multiple languages");
        assert!(
            error
                .to_string()
                .contains("whisper.cpp accepts at most one language"),
            "{error}"
        );
    }
}

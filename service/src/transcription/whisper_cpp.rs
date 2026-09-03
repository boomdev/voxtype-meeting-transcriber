use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;

use crate::config::{Config, ProviderKind};
use crate::error::{AppError, Result};
use crate::transcription::bounded::{
    check_audio_file_size, limit_transcript, read_capped_file, run_capped_command, JSON_MAX_BYTES,
};
use crate::transcription::{AudioChunkRef, TranscriptionProvider, TranscriptionResult};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(600);

pub struct WhisperCppProvider {
    executable: PathBuf,
    model: PathBuf,
    language: String,
}

impl WhisperCppProvider {
    pub fn from_config(config: &Config) -> Result<Self> {
        Ok(Self {
            executable: config.transcription.whisper_cpp.executable.clone(),
            model: config.transcription.whisper_cpp.model.clone(),
            language: crate::config::whisper_cli_language(&config.transcription.languages)?,
        })
    }

    pub fn new(executable: PathBuf, model: PathBuf, language: impl Into<String>) -> Self {
        Self {
            executable,
            model,
            language: language.into(),
        }
    }

    fn verify_inputs(&self, flac: &Path) -> Result<()> {
        if !self.executable.exists() {
            return Err(AppError::transcription(format!(
                "whisper-cli executable not found at {}",
                self.executable.display()
            )));
        }
        let mode = std::fs::metadata(&self.executable)?.permissions().mode();
        if mode & 0o111 == 0 {
            return Err(AppError::transcription(format!(
                "whisper-cli at {} is not executable",
                self.executable.display()
            )));
        }
        if !self.model.is_file() {
            return Err(AppError::transcription(format!(
                "whisper.cpp model not found or not readable at {}",
                self.model.display()
            )));
        }
        if !flac.is_file() {
            return Err(AppError::transcription(format!(
                "FLAC chunk not found at {}",
                flac.display()
            )));
        }
        check_audio_file_size(flac)?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl TranscriptionProvider for WhisperCppProvider {
    fn name(&self) -> ProviderKind {
        ProviderKind::WhisperCpp
    }

    async fn transcribe(&self, chunk: &AudioChunkRef) -> Result<TranscriptionResult> {
        self.verify_inputs(&chunk.file_path)?;
        let temp_dir =
            std::env::temp_dir().join(format!("voxtype-meeting-service-whisper-{}", chunk.id));
        tokio::fs::create_dir_all(&temp_dir).await?;
        let prefix = temp_dir.join("out");
        let result = run_whisper(
            &self.executable,
            &self.model,
            &chunk.file_path,
            &prefix,
            &self.language,
        )
        .await;
        let text = match result {
            Ok(text) => Ok(text),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "whisper-cli failed on FLAC; retrying with a temporary WAV"
                );
                match decode_and_retry_wav(
                    &self.executable,
                    &self.model,
                    &chunk.file_path,
                    &temp_dir,
                    &prefix,
                    &self.language,
                )
                .await
                {
                    Ok(text) => Ok(text),
                    Err(_) => Err(error),
                }
            }
        };
        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
        let text = text?;
        Ok(TranscriptionResult {
            text,
            provider: ProviderKind::WhisperCpp,
            model: self.model.to_string_lossy().into_owned(),
            provider_metadata: None,
        })
    }
}

async fn run_whisper(
    executable: &Path,
    model: &Path,
    flac: &Path,
    prefix: &Path,
    language: &str,
) -> Result<String> {
    let mut command = Command::new(executable);
    command
        .arg("-m")
        .arg(model)
        .arg("-f")
        .arg(flac)
        .arg("-l")
        .arg(language)
        .arg("-np")
        .arg("-oj")
        .arg("-of")
        .arg(prefix);
    let output = run_capped_command(command, PROCESS_TIMEOUT).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: String = stderr
            .chars()
            .rev()
            .take(400)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        return Err(AppError::transcription(format!(
            "whisper-cli exited with {}: {tail}",
            output.status
        )));
    }
    let json_path = prefix.with_extension("json");
    let json = read_capped_file(&json_path, JSON_MAX_BYTES).await?;
    let json = std::str::from_utf8(&json).map_err(|error| {
        AppError::transcription(format!(
            "whisper-cli JSON {} was not valid UTF-8: {error}",
            json_path.display()
        ))
    })?;
    parse_whisper_json(json)
}

async fn decode_and_retry_wav(
    executable: &Path,
    model: &Path,
    flac: &Path,
    temp_dir: &Path,
    prefix: &Path,
    language: &str,
) -> Result<String> {
    let flac = flac.to_path_buf();
    let samples =
        tokio::task::spawn_blocking(move || crate::encode::decode_flac_i16_mono_16k(&flac))
            .await
            .map_err(|error| {
                AppError::transcription(format!("FLAC decode task failed: {error}"))
            })??;
    let wav = temp_dir.join("chunk.wav");
    crate::encode::write_wav_i16_mono_16k(&wav, &samples)?;
    let text = run_whisper(executable, model, &wav, prefix, language).await?;
    let _ = tokio::fs::remove_file(&wav).await;
    Ok(text)
}

fn parse_whisper_json(json: &str) -> Result<String> {
    let value: serde_json::Value = serde_json::from_str(json).map_err(|error| {
        AppError::transcription(format!("whisper-cli JSON was invalid: {error}"))
    })?;
    if let Some(text) = value.get("transcription").and_then(|v| v.as_str()) {
        return limit_transcript(text.trim().to_string());
    }
    if let Some(text) = value.get("text").and_then(|v| v.as_str()) {
        return limit_transcript(text.trim().to_string());
    }
    if let Some(segments) = value
        .pointer("/result/segments")
        .or_else(|| value.get("segments"))
        .and_then(|v| v.as_array())
    {
        let text = segments
            .iter()
            .filter_map(|seg| seg.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join("")
            .trim()
            .to_string();
        return limit_transcript(text);
    }
    Err(AppError::transcription(
        "whisper-cli JSON did not contain transcription text",
    ))
}

#[cfg(test)]
mod tests {
    use super::{parse_whisper_json, WhisperCppProvider};
    use crate::audio::AudioSource;
    use crate::transcription::{AudioChunkRef, TranscriptionProvider};
    use chrono::Utc;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use tempfile::tempdir;

    #[test]
    fn parse_transcription_field() {
        let text = parse_whisper_json(r#"{"transcription":"hello world"}"#).unwrap();
        assert_eq!(text, "hello world");
    }

    #[tokio::test]
    async fn missing_executable_errors() {
        let provider = WhisperCppProvider::new(
            PathBuf::from("/no/such/whisper-cli"),
            PathBuf::from("/no/such/model.bin"),
            "auto",
        );
        let chunk = AudioChunkRef {
            id: "job".into(),
            file_path: PathBuf::from("/no/such.flac"),
            source: AudioSource::Mic,
            sequence: 1,
            started_at: Utc::now(),
            ended_at: Utc::now(),
        };
        let error = provider.transcribe(&chunk).await.unwrap_err();
        assert!(error
            .to_string()
            .contains("whisper-cli executable not found"));
    }

    #[tokio::test]
    async fn fake_script_returns_text() {
        let dir = tempdir().unwrap();
        let script = dir.path().join("whisper-cli");
        std::fs::write(
            &script,
            r#"#!/bin/sh
of=""
l=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -of) of="$2"; shift 2 ;;
    -l) l="$2"; shift 2 ;;
    *) shift ;;
  esac
done
[ "$l" = "auto" ] || { echo "expected -l auto, got '$l'" >&2; exit 1; }
printf '{"transcription":"hello from whisper"}' > "${of}.json"
"#,
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        let model = dir.path().join("model.bin");
        std::fs::write(&model, b"fake").unwrap();
        let flac = dir.path().join("a.flac");
        std::fs::write(&flac, b"fLaCfake").unwrap();

        let provider = WhisperCppProvider::new(script, model, "auto");
        let chunk = AudioChunkRef {
            id: "job1".into(),
            file_path: flac,
            source: AudioSource::Mic,
            sequence: 1,
            started_at: Utc::now(),
            ended_at: Utc::now(),
        };
        let result = provider.transcribe(&chunk).await.unwrap();
        assert_eq!(result.text, "hello from whisper");
        assert!(!dir
            .path()
            .join("voxtype-meeting-service-whisper-job1")
            .exists());
    }

    #[tokio::test]
    async fn flac_rejection_retries_with_wav() {
        let dir = tempdir().unwrap();
        let script = dir.path().join("whisper-cli");
        std::fs::write(
            &script,
            r#"#!/bin/sh
of=""
f=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    -of) of="$2"; shift 2 ;;
    -f) f="$2"; shift 2 ;;
    *) shift ;;
  esac
done
case "$f" in
  *.wav)
    printf '{"transcription":"from wav"}' > "${of}.json"
    exit 0
    ;;
  *)
    echo "unsupported flac" >&2
    exit 1
    ;;
esac
"#,
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        let model = dir.path().join("model.bin");
        std::fs::write(&model, b"fake").unwrap();
        let flac = dir.path().join("a.flac");
        let bytes = crate::encode::encode_flac_i16_mono_16k(&[0; 1600]).unwrap();
        std::fs::write(&flac, bytes).unwrap();

        let provider = WhisperCppProvider::new(script, model, "auto");
        let chunk = AudioChunkRef {
            id: "job-wav".into(),
            file_path: flac,
            source: AudioSource::Mic,
            sequence: 1,
            started_at: Utc::now(),
            ended_at: Utc::now(),
        };
        let result = provider.transcribe(&chunk).await.unwrap();
        assert_eq!(result.text, "from wav");
    }
}

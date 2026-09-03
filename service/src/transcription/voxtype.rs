use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;

use crate::config::ProviderKind;
use crate::error::{AppError, Result};
use crate::transcription::bounded::{
    check_audio_file_size, limit_transcript, read_capped_bytes_sync, run_capped_command,
    CONFIG_MAX_BYTES, MEETING_LANGUAGE_MAX_BYTES,
};
use crate::transcription::{AudioChunkRef, TranscriptionProvider, TranscriptionResult};

const TIMEOUT: Duration = Duration::from_secs(600);
const MEETING_LANGUAGE_FILE: &str = "meeting-language";

pub struct VoxtypeProvider {
    model: String,
}

impl VoxtypeProvider {
    pub fn new(model: String) -> Self {
        Self { model }
    }
}

pub fn snapshot_config(home: &Path, session_dir: &Path, language: Option<&str>) -> Result<String> {
    let source = home.join(".config/voxtype/config.toml");
    let contents = match read_capped_bytes_sync(&source, CONFIG_MAX_BYTES) {
        Ok(bytes) => String::from_utf8(bytes).map_err(|error| {
            AppError::config(format!(
                "Voxtype config {} was not valid UTF-8: {error}",
                source.display()
            ))
        })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            "engine = \"whisper\"\n[whisper]\nmodel = \"base.en\"\nlanguage = \"auto\"\n"
                .to_string()
        }
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            return Err(AppError::config(format!(
                "Voxtype config {} exceeds the {CONFIG_MAX_BYTES} byte limit",
                source.display()
            )))
        }
        Err(error) => {
            return Err(AppError::config(format!(
                "could not read Voxtype config {}: {error}",
                source.display()
            )))
        }
    };
    let parsed: toml::Value = toml::from_str(&contents)
        .map_err(|e| AppError::config(format!("invalid Voxtype config: {e}")))?;
    let engine = parsed
        .get("engine")
        .and_then(toml::Value::as_str)
        .unwrap_or("whisper");
    const LOCAL_ENGINES: &[&str] = &[
        "whisper",
        "parakeet",
        "moonshine",
        "sensevoice",
        "paraformer",
        "dolphin",
        "omnilingual",
        "cohere",
    ];
    if !LOCAL_ENGINES.contains(&engine) {
        return Err(AppError::config(format!(
            "Voxtype engine '{engine}' is not a supported local transcription engine"
        )));
    }
    let model = parsed
        .get(engine)
        .and_then(|v| v.get("model"))
        .and_then(toml::Value::as_str)
        .or_else(|| {
            parsed
                .get("whisper")
                .and_then(|v| v.get("model"))
                .and_then(toml::Value::as_str)
        })
        .unwrap_or("configured");
    let language = language.map(str::trim).filter(|code| !code.is_empty());
    let snapshot = match language {
        Some(code) => apply_language_override(&contents, engine, code),
        None => contents,
    };
    std::fs::write(session_dir.join("voxtype-config.toml"), snapshot)?;
    if let Some(code) = language {
        std::fs::write(session_dir.join(MEETING_LANGUAGE_FILE), format!("{code}\n"))?;
    }
    Ok(format!("{engine}:{model}"))
}

fn read_meeting_language(session_dir: &Path) -> Option<String> {
    let bytes = read_capped_bytes_sync(
        &session_dir.join(MEETING_LANGUAGE_FILE),
        MEETING_LANGUAGE_MAX_BYTES,
    )
    .ok()?;
    let code = std::str::from_utf8(&bytes).ok()?.trim();
    if code.is_empty() {
        None
    } else {
        Some(code.to_string())
    }
}

pub fn voxtype_transcribe_args(snapshot: &Path, wav: &Path, language: Option<&str>) -> Vec<String> {
    let mut args = vec!["-c".to_string(), snapshot.display().to_string()];
    if let Some(code) = language.map(str::trim).filter(|code| !code.is_empty()) {
        args.push("--language".to_string());
        args.push(code.to_string());
    }
    args.push("transcribe".to_string());
    args.push(wav.display().to_string());
    args
}

/// Replace or insert `language = "..."` in the active engine section without
/// rewriting the rest of the file, so comments in the snapshot stay intact.
pub fn apply_language_override(contents: &str, engine: &str, language: &str) -> String {
    let header = format!("[{engine}]");
    let quoted_header = format!("[\"{engine}\"]");
    let replacement = format!("language = \"{language}\"");
    let mut lines: Vec<String> = contents.lines().map(str::to_string).collect();
    let mut in_section = false;
    let mut replaced = false;
    let mut insert_at: Option<usize> = None;

    for index in 0..lines.len() {
        let trimmed = lines[index].trim().to_string();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if in_section && !replaced {
                insert_at = Some(index);
                break;
            }
            in_section = trimmed.eq_ignore_ascii_case(&header)
                || trimmed.eq_ignore_ascii_case(&quoted_header);
            if in_section {
                insert_at = Some(index + 1);
            }
            continue;
        }
        if in_section && is_language_assignment(&trimmed) {
            let indent_len = lines[index].len() - lines[index].trim_start().len();
            let indent = lines[index][..indent_len].to_string();
            lines[index] = format!("{indent}{replacement}");
            replaced = true;
            break;
        }
    }

    if !replaced {
        if let Some(index) = insert_at {
            lines.insert(index, replacement);
        } else {
            if !lines.is_empty() && !lines.last().map(|l| l.is_empty()).unwrap_or(true) {
                lines.push(String::new());
            }
            lines.push(header);
            lines.push(replacement);
        }
    }

    let mut out = lines.join("\n");
    if contents.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn is_language_assignment(trimmed: &str) -> bool {
    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with("language") && lower[8..].trim_start().starts_with('=')
}

#[async_trait::async_trait]
impl TranscriptionProvider for VoxtypeProvider {
    fn name(&self) -> ProviderKind {
        ProviderKind::Voxtype
    }

    async fn transcribe(&self, chunk: &AudioChunkRef) -> Result<TranscriptionResult> {
        let session_dir = session_dir_from_chunk(&chunk.file_path)?;
        let snapshot = session_dir.join("voxtype-config.toml");
        if !snapshot.is_file() {
            return Err(AppError::transcription(format!(
                "Voxtype configuration snapshot is missing at {}",
                snapshot.display()
            )));
        }
        let temp_dir = std::env::temp_dir().join(format!("voxtype-meeting-{}", chunk.id));
        tokio::fs::create_dir_all(&temp_dir).await?;
        check_audio_file_size(&chunk.file_path)?;
        let wav = temp_dir.join("utterance.wav");
        let flac = chunk.file_path.clone();
        let wav_copy = wav.clone();
        tokio::task::spawn_blocking(move || {
            let samples = crate::encode::decode_flac_i16_mono_16k(&flac)?;
            crate::encode::write_wav_i16_mono_16k(&wav_copy, &samples)
        })
        .await
        .map_err(|e| AppError::transcription(format!("audio conversion failed: {e}")))??;

        let args = voxtype_transcribe_args(
            &snapshot,
            &wav,
            read_meeting_language(&session_dir).as_deref(),
        );
        let mut command = Command::new("voxtype");
        command.args(&args);
        let output = match run_capped_command(command, TIMEOUT).await {
            Ok(output) => output,
            Err(error) => {
                let _ = tokio::fs::remove_dir_all(&temp_dir).await;
                return Err(error);
            }
        };
        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(AppError::transcription(if error.is_empty() {
                format!("voxtype exited with {}", output.status)
            } else {
                error
            }));
        }
        let text = parse_output(&String::from_utf8_lossy(&output.stdout));
        Ok(TranscriptionResult {
            text: limit_transcript(text)?,
            provider: ProviderKind::Voxtype,
            model: self.model.clone(),
            provider_metadata: None,
        })
    }
}

fn session_dir_from_chunk(path: &Path) -> Result<PathBuf> {
    path.parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| AppError::transcription("invalid meeting audio path"))
}

fn parse_output(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let start = lines
        .iter()
        .rposition(|line| line.trim().is_empty())
        .map(|i| i + 1)
        .unwrap_or(0);
    lines[start..].join("\n").trim().to_string()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        apply_language_override, parse_output, snapshot_config, voxtype_transcribe_args,
        MEETING_LANGUAGE_FILE,
    };

    #[test]
    fn extracts_final_transcript_block() {
        let output = "Loading audio file\nProcessing 1 samples\n\nHi there.\nHow are you?\n";
        assert_eq!(parse_output(output), "Hi there.\nHow are you?");
    }

    #[test]
    fn language_override_replaces_existing_assignment() {
        let source = "# keep me\nengine = \"whisper\"\n[whisper]\nmodel = \"base.en\"\nlanguage = \"auto\"\ntranslate = false\n";
        let updated = apply_language_override(source, "whisper", "en");
        assert!(updated.contains("# keep me"));
        assert!(updated.contains("model = \"base.en\""));
        assert!(updated.contains("language = \"en\""));
        assert!(!updated.contains("language = \"auto\""));
        assert!(updated.contains("translate = false"));
    }

    #[test]
    fn language_override_inserts_when_missing() {
        let source = "engine = \"sensevoice\"\n[sensevoice]\nmodel = \"sensevoice-small\"\n";
        let updated = apply_language_override(source, "sensevoice", "ja");
        assert!(updated.contains("[sensevoice]"));
        assert!(updated.contains("language = \"ja\""));
        assert!(updated.contains("model = \"sensevoice-small\""));
    }

    #[test]
    fn snapshot_rejects_oversized_config() {
        let home = tempfile::tempdir().unwrap();
        let config_dir = home.path().join(".config/voxtype");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.toml"),
            vec![b'x'; crate::transcription::bounded::CONFIG_MAX_BYTES + 1],
        )
        .unwrap();
        let session = tempfile::tempdir().unwrap();
        let error = snapshot_config(home.path(), session.path(), None).unwrap_err();
        assert!(error.to_string().contains("exceeds"), "{error}");
    }

    #[test]
    fn snapshot_writes_overridden_language() {
        let home = tempfile::tempdir().unwrap();
        let config_dir = home.path().join(".config/voxtype");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.toml"),
            "engine = \"whisper\"\n[whisper]\nmodel = \"small\"\nlanguage = \"auto\"\n",
        )
        .unwrap();
        let session = tempfile::tempdir().unwrap();
        let label = snapshot_config(home.path(), session.path(), Some("fr")).unwrap();
        assert_eq!(label, "whisper:small");
        let snapshot = std::fs::read_to_string(session.path().join("voxtype-config.toml")).unwrap();
        assert!(snapshot.contains("language = \"fr\""));
        assert!(!snapshot.contains("language = \"auto\""));
        assert_eq!(
            std::fs::read_to_string(session.path().join(MEETING_LANGUAGE_FILE))
                .unwrap()
                .trim(),
            "fr"
        );
    }

    #[test]
    fn language_override_skips_commented_language_lines() {
        let source = concat!(
            "engine = \"whisper\"\n",
            "[whisper]\n",
            "# Language for transcription\n",
            "# Use \"en\" for English, \"auto\" for auto-detection\n",
            "language = \"en\"\n",
            "translate = false\n",
        );
        let updated = apply_language_override(source, "whisper", "fr");
        assert!(updated.contains("language = \"fr\""));
        assert!(!updated.contains("language = \"en\""));
        assert!(updated.contains("# Language for transcription"));
    }

    #[test]
    fn transcribe_args_pass_global_language_before_subcommand() {
        let args = voxtype_transcribe_args(
            Path::new("/tmp/voxtype-config.toml"),
            Path::new("/tmp/utterance.wav"),
            Some("fr"),
        );
        assert_eq!(
            args,
            vec![
                "-c",
                "/tmp/voxtype-config.toml",
                "--language",
                "fr",
                "transcribe",
                "/tmp/utterance.wav",
            ]
        );
    }
}

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{AppError, Result};
use crate::paths::PathResolver;

pub const DEFAULT_CONFIG_TOML: &str = r#"# Voxtype Meeting Service configuration
# See docs/architecture.md for the full schema.

[general]
minimum_free_space_mb = 1024

[audio]
microphone = "default"
system_output = "default"
source = "both"
retain_audio = false

[transcription]
provider = "voxtype"
model = "configured"
max_concurrent_jobs = 1

[transcription.whisper_cpp]
executable = "/usr/local/bin/whisper-cli"
model = "~/.local/share/voxtype-meeting-service/models/ggml-large-v3-turbo.bin"

[transcript]
# When only one of MIC/SYSTEM has non-empty text, omit `HH:MM:SS [SOURCE]` lines in transcript.md.
omit_single_source_headers = true
"#;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Voxtype,
    Openai,
    WhisperCpp,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Voxtype => "voxtype",
            Self::Openai => "openai",
            Self::WhisperCpp => "whisper_cpp",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "voxtype" => Ok(Self::Voxtype),
            "whisper_cpp" => Ok(Self::WhisperCpp),
            "openai" => Err(AppError::config(
                "the OpenAI remote provider has been removed; use provider = \"voxtype\" or \"whisper_cpp\"",
            )),
            other => Err(AppError::config(format!(
                "invalid transcription.provider '{other}'; expected voxtype or whisper_cpp"
            ))),
        }
    }

    pub fn parse_stored(value: &str) -> Result<Self> {
        match value {
            "openai" => Ok(Self::Openai),
            other => Self::parse(other),
        }
    }
}

impl std::fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub general: GeneralConfig,
    pub audio: AudioConfig,
    pub transcription: TranscriptionConfig,
    pub transcript: TranscriptConfig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneralConfig {
    pub minimum_free_space_mb: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioConfig {
    pub microphone: String,
    pub system_output: String,
    pub source: CaptureSource,
    pub retain_audio: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureSource {
    Mic,
    System,
    Both,
}

impl CaptureSource {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "mic" => Ok(Self::Mic),
            "system" => Ok(Self::System),
            "both" => Ok(Self::Both),
            other => Err(AppError::config(format!("invalid audio.source '{other}'"))),
        }
    }
    pub fn includes_mic(self) -> bool {
        matches!(self, Self::Mic | Self::Both)
    }
    pub fn includes_system(self) -> bool {
        matches!(self, Self::System | Self::Both)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mic => "mic",
            Self::System => "system",
            Self::Both => "both",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptionConfig {
    pub provider: ProviderKind,
    pub model: String,
    pub max_concurrent_jobs: u32,
    /// Expected spoken languages (ISO 639-1, e.g. `fr`, `en`). Empty means auto-detect.
    pub languages: Vec<String>,
    pub whisper_cpp: WhisperCppConfig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WhisperCppConfig {
    pub executable: PathBuf,
    pub model: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptConfig {
    /// Drop `HH:MM:SS [MIC]` / `[SYSTEM]` lines in Markdown when only one source has text.
    pub omit_single_source_headers: bool,
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    general: RawGeneral,
    #[serde(default)]
    audio: RawAudio,
    #[serde(default)]
    transcription: RawTranscription,
    #[serde(default)]
    transcript: RawTranscript,
}

#[derive(Debug, Deserialize)]
struct RawGeneral {
    #[serde(default = "default_minimum_free_space_mb")]
    minimum_free_space_mb: u64,
}

impl Default for RawGeneral {
    fn default() -> Self {
        Self {
            minimum_free_space_mb: default_minimum_free_space_mb(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawAudio {
    #[serde(default = "default_device")]
    microphone: String,
    #[serde(default = "default_device")]
    system_output: String,
    #[serde(default = "default_source")]
    source: String,
    #[serde(default)]
    retain_audio: bool,
}

impl Default for RawAudio {
    fn default() -> Self {
        Self {
            microphone: default_device(),
            system_output: default_device(),
            source: default_source(),
            retain_audio: false,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawTranscription {
    #[serde(default = "default_provider")]
    provider: String,
    #[serde(default = "default_model")]
    model: String,
    #[serde(default = "default_max_concurrent_jobs")]
    max_concurrent_jobs: u32,
    #[serde(default)]
    languages: Option<Vec<String>>,
    #[serde(default)]
    whisper_cpp: RawWhisperCpp,
}

impl Default for RawTranscription {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            model: default_model(),
            max_concurrent_jobs: default_max_concurrent_jobs(),
            languages: None,
            whisper_cpp: RawWhisperCpp::default(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawWhisperCpp {
    #[serde(default = "default_whisper_executable")]
    executable: String,
    #[serde(default = "default_whisper_model")]
    model: String,
}

impl Default for RawWhisperCpp {
    fn default() -> Self {
        Self {
            executable: default_whisper_executable(),
            model: default_whisper_model(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawTranscript {
    #[serde(default = "default_omit_single_source_headers")]
    omit_single_source_headers: bool,
}

impl Default for RawTranscript {
    fn default() -> Self {
        Self {
            omit_single_source_headers: default_omit_single_source_headers(),
        }
    }
}

fn default_minimum_free_space_mb() -> u64 {
    1024
}
fn default_device() -> String {
    "default".to_string()
}
fn default_source() -> String {
    "both".into()
}
fn default_provider() -> String {
    "voxtype".to_string()
}
fn default_model() -> String {
    "configured".to_string()
}
fn default_max_concurrent_jobs() -> u32 {
    1
}
fn default_whisper_executable() -> String {
    "/usr/local/bin/whisper-cli".to_string()
}
fn default_whisper_model() -> String {
    "~/.local/share/voxtype-meeting-service/models/ggml-large-v3-turbo.bin".to_string()
}
fn default_omit_single_source_headers() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        parse_toml(DEFAULT_CONFIG_TOML).expect("built-in default config must be valid")
    }
}

impl Config {
    pub fn load(paths: &PathResolver) -> Result<Self> {
        load_from_path(&paths.config_file(), paths.home())
    }

    pub fn load_or_default(paths: &PathResolver) -> Result<Self> {
        if paths.config_file().exists() {
            Self::load(paths)
        } else {
            Ok(Self::default())
        }
    }

    pub fn load_or_write_default(paths: &PathResolver) -> Result<Self> {
        let file = paths.config_file();
        if file.exists() {
            return load_from_path(&file, paths.home());
        }
        crate::paths::ensure_dir(&paths.config_dir())?;
        fs::write(&file, DEFAULT_CONFIG_TOML)?;
        tracing::info!(path = %file.display(), "wrote default configuration");
        load_from_path(&file, paths.home())
    }

    pub fn model_for_provider(&self, provider: ProviderKind) -> String {
        match provider {
            ProviderKind::Voxtype | ProviderKind::Openai => self.transcription.model.clone(),
            ProviderKind::WhisperCpp => self
                .transcription
                .whisper_cpp
                .model
                .to_string_lossy()
                .into_owned(),
        }
    }
}

pub fn load_from_path(path: &Path, home: &Path) -> Result<Config> {
    let contents = fs::read_to_string(path).map_err(|error| {
        AppError::config(format!(
            "could not read configuration file {}: {error}",
            path.display()
        ))
    })?;
    parse_toml_with_home(&contents, home).map_err(|error| match error {
        AppError::Config(message) => AppError::config(format!(
            "invalid configuration file {}: {message}",
            path.display()
        )),
        other => other,
    })
}

pub fn parse_toml(contents: &str) -> Result<Config> {
    let home = env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));
    parse_toml_with_home(contents, &home)
}

pub fn parse_toml_with_home(contents: &str, home: &Path) -> Result<Config> {
    let raw: RawConfig = toml::from_str(contents).map_err(|error| {
        AppError::config(format!("could not parse configuration TOML: {error}"))
    })?;
    raw.into_config(home)
}

impl RawConfig {
    fn into_config(self, home: &Path) -> Result<Config> {
        if self.general.minimum_free_space_mb == 0 {
            return Err(AppError::config(
                "minimum_free_space_mb must be greater than 0",
            ));
        }
        if self.transcription.max_concurrent_jobs == 0 {
            return Err(AppError::config(
                "max_concurrent_jobs must be greater than 0",
            ));
        }

        let provider = ProviderKind::parse(&self.transcription.provider)?;
        let languages = normalize_languages(self.transcription.languages)?;
        assert_languages_compatible(provider, &self.transcription.model, &languages)?;

        Ok(Config {
            general: GeneralConfig {
                minimum_free_space_mb: self.general.minimum_free_space_mb,
            },
            audio: AudioConfig {
                microphone: self.audio.microphone,
                system_output: self.audio.system_output,
                source: CaptureSource::parse(&self.audio.source)?,
                retain_audio: self.audio.retain_audio,
            },
            transcription: TranscriptionConfig {
                provider,
                model: self.transcription.model,
                max_concurrent_jobs: self.transcription.max_concurrent_jobs,
                languages,
                whisper_cpp: WhisperCppConfig {
                    executable: expand_tilde(&self.transcription.whisper_cpp.executable, home),
                    model: expand_tilde(&self.transcription.whisper_cpp.model, home),
                },
            },
            transcript: TranscriptConfig {
                omit_single_source_headers: self.transcript.omit_single_source_headers,
            },
        })
    }
}

pub fn expand_tilde(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home.join(rest);
    }
    PathBuf::from(path)
}

pub fn whisper_cli_language(languages: &[String]) -> Result<String> {
    match languages {
        [] => Ok("auto".to_string()),
        [one] => Ok(one.clone()),
        _ => Err(AppError::config(
            "whisper.cpp accepts at most one language code (languages = [\"fr\"]); omit languages or use [] for auto-detect",
        )),
    }
}

pub fn assert_languages_compatible(
    provider: ProviderKind,
    _model: &str,
    languages: &[String],
) -> Result<()> {
    match provider {
        ProviderKind::Voxtype | ProviderKind::Openai => Ok(()),
        ProviderKind::WhisperCpp => {
            whisper_cli_language(languages)?;
            Ok(())
        }
    }
}

fn normalize_languages(raw: Option<Vec<String>>) -> Result<Vec<String>> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for value in raw {
        let code = value.trim().to_ascii_lowercase();
        if code.is_empty() {
            return Err(AppError::config(
                "transcription.languages entries must be non-empty language codes such as \"fr\" or \"en\"",
            ));
        }
        if code
            .chars()
            .any(|ch| !(ch.is_ascii_alphanumeric() || ch == '-'))
        {
            return Err(AppError::config(format!(
                "invalid language code '{value}'; use ISO codes such as \"fr\" or \"en\""
            )));
        }
        out.push(code);
    }
    if out.iter().any(|code| code == "auto") {
        if out.len() == 1 {
            return Ok(Vec::new());
        }
        return Err(AppError::config(
            "\"auto\" cannot be mixed with other language codes; omit languages for auto-detect",
        ));
    }
    Ok(out)
}

pub fn to_toml(config: &Config) -> String {
    let languages = if config.transcription.languages.is_empty() {
        String::new()
    } else {
        let items = config
            .transcription
            .languages
            .iter()
            .map(|code| format!("\"{code}\""))
            .collect::<Vec<_>>()
            .join(", ");
        format!("languages = [{items}]\n")
    };
    format!(
        "# Voxtype Meeting Service configuration\n\
         # See docs/architecture.md for the full schema.\n\
         \n\
         [general]\n\
         minimum_free_space_mb = {}\n\
         \n\
         [audio]\n\
         microphone = \"{}\"\n\
         system_output = \"{}\"\n\
         source = \"{}\"\n\
         retain_audio = {}\n\
         \n\
         [transcription]\n\
         provider = \"{}\"\n\
         model = \"{}\"\n\
         max_concurrent_jobs = {}\n\
         {languages}\
         \n\
         [transcription.whisper_cpp]\n\
         executable = \"{}\"\n\
         model = \"{}\"\n\
         \n\
         [transcript]\n\
         omit_single_source_headers = {}\n",
        config.general.minimum_free_space_mb,
        escape_toml(&config.audio.microphone),
        escape_toml(&config.audio.system_output),
        config.audio.source.as_str(),
        config.audio.retain_audio,
        config.transcription.provider.as_str(),
        escape_toml(&config.transcription.model),
        config.transcription.max_concurrent_jobs,
        escape_toml(
            &config
                .transcription
                .whisper_cpp
                .executable
                .to_string_lossy()
        ),
        escape_toml(&config.transcription.whisper_cpp.model.to_string_lossy()),
        config.transcript.omit_single_source_headers,
    )
}

fn escape_toml(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

pub fn write_atomic(paths: &PathResolver, config: &Config) -> Result<()> {
    crate::paths::ensure_dir(&paths.config_dir())?;
    let dest = paths.config_file();
    let tmp = paths.config_dir().join("config.toml.tmp");
    fs::write(&tmp, to_toml(config))?;
    fs::rename(tmp, dest)?;
    Ok(())
}

#[derive(Debug, Default, Deserialize)]
pub struct ConfigPatch {
    #[serde(default)]
    pub general: Option<GeneralPatch>,
    #[serde(default)]
    pub audio: Option<AudioPatch>,
    #[serde(default)]
    pub transcription: Option<TranscriptionPatch>,
    #[serde(default)]
    pub transcript: Option<TranscriptPatch>,
}

#[derive(Debug, Default, Deserialize)]
pub struct GeneralPatch {
    pub minimum_free_space_mb: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
pub struct AudioPatch {
    pub microphone: Option<String>,
    pub system_output: Option<String>,
    pub source: Option<String>,
    pub retain_audio: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
pub struct TranscriptionPatch {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub max_concurrent_jobs: Option<u32>,
    pub languages: Option<Vec<String>>,
    pub whisper_cpp: Option<WhisperPatch>,
}

#[derive(Debug, Default, Deserialize)]
pub struct WhisperPatch {
    pub executable: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct TranscriptPatch {
    pub omit_single_source_headers: Option<bool>,
}

pub fn apply_patch(base: &Config, patch: ConfigPatch, home: &Path) -> Result<Config> {
    let mut next = base.clone();
    if let Some(general) = patch.general {
        if let Some(mb) = general.minimum_free_space_mb {
            if mb < 256 {
                return Err(AppError::config(
                    "minimum_free_space_mb must be at least 256",
                ));
            }
            next.general.minimum_free_space_mb = mb;
        }
    }
    if let Some(audio) = patch.audio {
        if let Some(microphone) = audio.microphone {
            next.audio.microphone = microphone;
        }
        if let Some(system_output) = audio.system_output {
            next.audio.system_output = system_output;
        }
        if let Some(source) = audio.source {
            next.audio.source = CaptureSource::parse(&source)?;
        }
        if let Some(retain) = audio.retain_audio {
            next.audio.retain_audio = retain;
        }
    }
    if let Some(transcription) = patch.transcription {
        if let Some(provider) = transcription.provider {
            next.transcription.provider = ProviderKind::parse(&provider)?;
        }
        if let Some(model) = transcription.model {
            if model.trim().is_empty() {
                return Err(AppError::config("model must not be empty"));
            }
            next.transcription.model = model;
        }
        if let Some(jobs) = transcription.max_concurrent_jobs {
            if !(1..=8).contains(&jobs) {
                return Err(AppError::config(
                    "max_concurrent_jobs must be between 1 and 8",
                ));
            }
            next.transcription.max_concurrent_jobs = jobs;
        }
        if let Some(languages) = transcription.languages {
            next.transcription.languages = normalize_languages(Some(languages))?;
        }
        if let Some(whisper) = transcription.whisper_cpp {
            if let Some(executable) = whisper.executable {
                next.transcription.whisper_cpp.executable = expand_tilde(&executable, home);
            }
            if let Some(model) = whisper.model {
                next.transcription.whisper_cpp.model = expand_tilde(&model, home);
            }
        }
    }
    if let Some(transcript) = patch.transcript {
        if let Some(omit) = transcript.omit_single_source_headers {
            next.transcript.omit_single_source_headers = omit;
        }
    }
    assert_languages_compatible(
        next.transcription.provider,
        &next.transcription.model,
        &next.transcription.languages,
    )?;
    if next.transcription.provider == ProviderKind::WhisperCpp {
        if !next.transcription.whisper_cpp.executable.exists() {
            return Err(AppError::config(format!(
                "whisper-cli was not found at {}",
                next.transcription.whisper_cpp.executable.display()
            )));
        }
        if !next.transcription.whisper_cpp.model.is_file() {
            return Err(AppError::config(format!(
                "whisper.cpp model was not found at {}",
                next.transcription.whisper_cpp.model.display()
            )));
        }
    }
    Ok(next)
}

pub fn validation_field(message: &str) -> Option<&'static str> {
    if message.contains("minimum_free_space_mb") {
        Some("general.minimum_free_space_mb")
    } else if message.contains("max_concurrent_jobs") {
        Some("transcription.max_concurrent_jobs")
    } else if message.contains("model must not be empty") {
        Some("transcription.model")
    } else if message.contains("whisper-cli") {
        Some("transcription.whisper_cpp.executable")
    } else if message.contains("whisper.cpp model") {
        Some("transcription.whisper_cpp.model")
    } else if message.contains("provider") {
        Some("transcription.provider")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_toml_with_home, ProviderKind, DEFAULT_CONFIG_TOML};
    use std::path::Path;

    #[test]
    fn parse_default_toml() {
        let config =
            parse_toml_with_home(DEFAULT_CONFIG_TOML, Path::new("/home/xa")).expect("default");
        assert_eq!(config.general.minimum_free_space_mb, 1024);
        assert_eq!(config.audio.microphone, "default");
        assert_eq!(config.audio.system_output, "default");
        assert_eq!(config.audio.source, super::CaptureSource::Both);
        assert!(!config.audio.retain_audio);
        assert_eq!(config.transcription.provider, ProviderKind::Voxtype);
        assert_eq!(config.transcription.model, "configured");
        assert_eq!(config.transcription.max_concurrent_jobs, 1);
        assert!(config.transcription.languages.is_empty());
        assert!(config.transcript.omit_single_source_headers);
        assert_eq!(
            config.transcription.whisper_cpp.executable.as_os_str(),
            "/usr/local/bin/whisper-cli"
        );
        assert_eq!(
            config.transcription.whisper_cpp.model.as_os_str(),
            "/home/xa/.local/share/voxtype-meeting-service/models/ggml-large-v3-turbo.bin"
        );
    }

    #[test]
    fn reject_invalid_source() {
        let toml = r#"
[audio]
source = "mixed"
"#;
        let error = parse_toml_with_home(toml, Path::new("/home/xa")).unwrap_err();
        assert!(
            error.to_string().contains("invalid audio.source 'mixed'"),
            "{error}"
        );
    }

    #[test]
    fn reject_provider() {
        let toml = r#"
[transcription]
provider = "foo"
"#;
        let error = parse_toml_with_home(toml, Path::new("/home/xa")).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("invalid transcription.provider 'foo'; expected voxtype or whisper_cpp"),
            "{error}"
        );
    }

    #[test]
    fn expand_tilde() {
        let toml = r#"
[transcription]
provider = "whisper_cpp"

[transcription.whisper_cpp]
executable = "~/bin/whisper-cli"
model = "~/models/x.bin"
"#;
        let config = parse_toml_with_home(toml, Path::new("/home/xa")).expect("parse");
        assert_eq!(
            config.transcription.whisper_cpp.executable.as_os_str(),
            "/home/xa/bin/whisper-cli"
        );
        assert_eq!(
            config.transcription.whisper_cpp.model.as_os_str(),
            "/home/xa/models/x.bin"
        );
    }

    #[test]
    fn reject_removed_openai_provider() {
        let toml = r#"
[transcription]
provider = "openai"
"#;
        let error = parse_toml_with_home(toml, Path::new("/home/xa")).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("OpenAI remote provider has been removed"),
            "{error}"
        );
        assert_eq!(
            super::ProviderKind::parse_stored("openai").unwrap(),
            super::ProviderKind::Openai
        );
    }

    #[test]
    fn languages_auto_is_empty() {
        let toml = r#"
[transcription]
languages = ["auto"]
"#;
        let config = parse_toml_with_home(toml, Path::new("/home/xa")).expect("parse");
        assert!(config.transcription.languages.is_empty());
    }

    #[test]
    fn whisper_rejects_multiple_languages() {
        let toml = r#"
[transcription]
provider = "whisper_cpp"
languages = ["fr", "en"]
"#;
        let error = parse_toml_with_home(toml, Path::new("/home/xa")).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("whisper.cpp accepts at most one language"),
            "{error}"
        );
    }

    #[test]
    fn whisper_accepts_single_language() {
        let toml = r#"
[transcription]
provider = "whisper_cpp"
languages = ["FR"]
"#;
        let config = parse_toml_with_home(toml, Path::new("/home/xa")).expect("parse");
        assert_eq!(config.transcription.languages, vec!["fr"]);
        assert_eq!(
            super::whisper_cli_language(&config.transcription.languages).unwrap(),
            "fr"
        );
    }

    #[test]
    fn empty_languages_array_is_auto() {
        let toml = r#"
[transcription]
languages = []
"#;
        let config = parse_toml_with_home(toml, Path::new("/home/xa")).expect("parse");
        assert!(config.transcription.languages.is_empty());
        assert_eq!(
            super::whisper_cli_language(&config.transcription.languages).unwrap(),
            "auto"
        );
    }

    #[test]
    fn reject_mixed_auto_and_codes() {
        let toml = r#"
[transcription]
languages = ["auto", "fr"]
"#;
        let error = parse_toml_with_home(toml, Path::new("/home/xa")).unwrap_err();
        assert!(error.to_string().contains("cannot be mixed"), "{error}");
    }

    #[test]
    fn reject_invalid_language_code() {
        let toml = r#"
[transcription]
languages = ["fr/en"]
"#;
        let error = parse_toml_with_home(toml, Path::new("/home/xa")).unwrap_err();
        assert!(
            error.to_string().contains("invalid language code"),
            "{error}"
        );
    }

    #[test]
    fn omit_single_source_headers_can_be_disabled() {
        let toml = r#"
[transcript]
omit_single_source_headers = false
"#;
        let config = parse_toml_with_home(toml, Path::new("/home/xa")).expect("parse");
        assert!(!config.transcript.omit_single_source_headers);
    }

    #[test]
    fn apply_patch_rejects_jobs_outside_range() {
        let base = super::Config::default();
        let patch = super::ConfigPatch {
            transcription: Some(super::TranscriptionPatch {
                max_concurrent_jobs: Some(9),
                ..Default::default()
            }),
            ..Default::default()
        };
        let error = super::apply_patch(&base, patch, Path::new("/home/xa")).unwrap_err();
        assert!(error.to_string().contains("max_concurrent_jobs"));
        assert_eq!(
            super::validation_field(&error.to_string()),
            Some("transcription.max_concurrent_jobs")
        );
    }

    #[test]
    fn apply_patch_keeps_languages_when_omitted() {
        let mut base = super::Config::default();
        base.transcription.languages = vec!["fr".into()];
        base.transcript.omit_single_source_headers = false;
        let patch = super::ConfigPatch {
            audio: Some(super::AudioPatch {
                microphone: Some("hw:1".into()),
                system_output: None,
                source: None,
                retain_audio: None,
            }),
            ..Default::default()
        };
        let next = super::apply_patch(&base, patch, Path::new("/home/xa")).unwrap();
        assert_eq!(next.audio.microphone, "hw:1");
        assert_eq!(next.transcription.languages, vec!["fr"]);
        assert!(!next.transcript.omit_single_source_headers);
    }
}

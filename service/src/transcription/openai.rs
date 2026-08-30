use std::time::Duration;

use reqwest::multipart::{Form, Part};
use reqwest::Client;

use crate::config::{openai_api_key, Config, ProviderKind};
use crate::error::{AppError, Result};
use crate::transcription::{AudioChunkRef, TranscriptionProvider, TranscriptionResult};

const DEFAULT_ENDPOINT: &str = "https://api.openai.com/v1/audio/transcriptions";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const ERROR_BODY_LIMIT: usize = 512;

pub struct OpenAIProvider {
    client: Client,
    model: String,
    endpoint: String,
    languages: Vec<String>,
}

impl OpenAIProvider {
    pub fn from_config(config: &Config) -> Result<Self> {
        Self::new(
            &config.transcription.model,
            DEFAULT_ENDPOINT,
            config.transcription.languages.clone(),
        )
    }

    pub fn from_model(model: &str) -> Result<Self> {
        Self::from_model_and_languages(model, Vec::new())
    }

    pub fn from_model_and_languages(model: &str, languages: Vec<String>) -> Result<Self> {
        Self::new(model, DEFAULT_ENDPOINT, languages)
    }

    pub fn new(model: &str, endpoint: &str, languages: Vec<String>) -> Result<Self> {
        crate::config::assert_languages_compatible(ProviderKind::Openai, model, &languages)?;
        let client = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| {
                AppError::transcription(format!("could not build OpenAI HTTP client: {error}"))
            })?;
        Ok(Self {
            client,
            model: model.to_string(),
            endpoint: endpoint.to_string(),
            languages,
        })
    }
}

#[async_trait::async_trait]
impl TranscriptionProvider for OpenAIProvider {
    fn name(&self) -> ProviderKind {
        ProviderKind::Openai
    }

    async fn transcribe(&self, chunk: &AudioChunkRef) -> Result<TranscriptionResult> {
        let Some(api_key) = openai_api_key() else {
            return Err(AppError::transcription(
                "OpenAI API key missing (set OPENAI_API_KEY)",
            ));
        };
        let bytes = tokio::fs::read(&chunk.file_path).await.map_err(|error| {
            AppError::transcription(format!(
                "could not read FLAC {} for OpenAI transcription: {error}",
                chunk.file_path.display()
            ))
        })?;
        let file_name = chunk
            .file_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("chunk.flac")
            .to_string();
        post_transcription(
            &self.client,
            &self.endpoint,
            &api_key,
            &self.model,
            &file_name,
            bytes,
            &self.languages,
        )
        .await
    }
}

async fn post_transcription(
    client: &Client,
    endpoint: &str,
    api_key: &str,
    model: &str,
    file_name: &str,
    bytes: Vec<u8>,
    languages: &[String],
) -> Result<TranscriptionResult> {
    let part = Part::bytes(bytes)
        .file_name(file_name.to_string())
        .mime_str("audio/flac")
        .map_err(|error| {
            AppError::transcription(format!("could not attach FLAC to OpenAI request: {error}"))
        })?;
    let mut form = Form::new()
        .part("file", part)
        .text("model", model.to_string());
    form = attach_language_fields(form, model, languages)?;
    let response = client
        .post(endpoint)
        .bearer_auth(api_key)
        .multipart(form)
        .send()
        .await
        .map_err(|error| {
            AppError::transcription(format!(
                "OpenAI transcription request failed (network): {error}"
            ))
        })?;
    let status = response.status();
    let body = response.text().await.map_err(|error| {
        AppError::transcription(format!(
            "OpenAI transcription response could not be read: {error}"
        ))
    })?;
    if !status.is_success() {
        let truncated: String = body.chars().take(ERROR_BODY_LIMIT).collect();
        return Err(AppError::transcription(format!(
            "OpenAI transcription failed with HTTP {status}: {truncated}"
        )));
    }
    let parsed: serde_json::Value = serde_json::from_str(&body).map_err(|error| {
        AppError::transcription(format!("OpenAI transcription JSON was invalid: {error}"))
    })?;
    let text = parsed
        .get("text")
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            AppError::transcription("OpenAI transcription JSON did not contain a text field")
        })?;
    Ok(TranscriptionResult {
        text: text.to_string(),
        provider: ProviderKind::Openai,
        model: model.to_string(),
        provider_metadata: None,
    })
}

fn attach_language_fields(mut form: Form, model: &str, languages: &[String]) -> Result<Form> {
    crate::config::assert_languages_compatible(ProviderKind::Openai, model, languages)?;
    if languages.is_empty() {
        return Ok(form);
    }
    if crate::config::openai_uses_languages_array(model) {
        for code in languages {
            form = form.text("languages[]", code.clone());
        }
    } else {
        form = form.text("language", languages[0].clone());
    }
    Ok(form)
}

#[cfg(test)]
mod tests {
    use super::OpenAIProvider;
    use crate::audio::AudioSource;
    use crate::transcription::{AudioChunkRef, TranscriptionProvider};
    use chrono::Utc;
    use std::path::PathBuf;

    #[tokio::test]
    async fn missing_key_errors_without_panic() {
        if crate::config::openai_api_key().is_some() {
            return;
        }
        let provider =
            OpenAIProvider::new("gpt-4o-transcribe", "http://127.0.0.1:9", Vec::new()).unwrap();
        let chunk = AudioChunkRef {
            id: "x".into(),
            file_path: PathBuf::from("/tmp/missing.flac"),
            source: AudioSource::Mic,
            sequence: 1,
            started_at: Utc::now(),
            ended_at: Utc::now(),
        };
        let error = provider.transcribe(&chunk).await.unwrap_err();
        assert!(
            error.to_string().contains("OpenAI API key missing"),
            "{error}"
        );
    }

    #[test]
    fn gpt_4o_transcribe_rejects_language_array() {
        let error = OpenAIProvider::new(
            "gpt-4o-transcribe",
            "http://127.0.0.1:9",
            vec!["fr".into(), "en".into()],
        )
        .err()
        .expect("gpt-4o-transcribe must reject multiple languages");
        assert!(
            error.to_string().contains("accepts at most one language"),
            "{error}"
        );
    }
}

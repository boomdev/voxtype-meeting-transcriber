use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::config::ProviderKind;
use crate::error::{AppError, Result};
use crate::transcription::{AudioChunkRef, TranscriptionProvider, TranscriptionResult};

pub struct FakeTranscriptionProvider {
    pub kind: ProviderKind,
    pub text: String,
    pub fail_times: usize,
    attempts: AtomicUsize,
}

impl FakeTranscriptionProvider {
    pub fn always(text: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            kind: ProviderKind::Openai,
            text: text.into(),
            fail_times: 0,
            attempts: AtomicUsize::new(0),
        })
    }

    pub fn fail_then_succeed(fail_times: usize, text: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            kind: ProviderKind::Openai,
            text: text.into(),
            fail_times,
            attempts: AtomicUsize::new(0),
        })
    }
}

#[async_trait::async_trait]
impl TranscriptionProvider for FakeTranscriptionProvider {
    fn name(&self) -> ProviderKind {
        self.kind
    }

    async fn transcribe(&self, _chunk: &AudioChunkRef) -> Result<TranscriptionResult> {
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
        if attempt <= self.fail_times {
            return Err(AppError::transcription(format!(
                "fake provider failure on attempt {attempt}"
            )));
        }
        Ok(TranscriptionResult {
            text: self.text.clone(),
            provider: self.kind,
            model: "fake-model".into(),
            provider_metadata: None,
        })
    }
}

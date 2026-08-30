use crate::audio::convert::TARGET_RATE;
use crate::audio::{AudioSource, PcmFrame};
use chrono::{DateTime, Utc};
use std::collections::VecDeque;

const SILENCE_MS: usize = 700;
const PRE_ROLL_MS: usize = 200;
const POST_ROLL_MS: usize = 100;
const MAX_TURN_MS: usize = 30_000;
const MIN_SPEECH_MS: usize = 100;
const FLOOR_RMS: f64 = 250.0;
fn samples_for(ms: usize) -> usize {
    TARGET_RATE as usize * ms / 1000
}

#[derive(Debug, Clone)]
pub struct CompletedChunk {
    pub source: AudioSource,
    pub sequence: u64,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub samples: Vec<i16>,
}
impl CompletedChunk {
    pub fn duration_ms(&self) -> i64 {
        (self.samples.len() as i64 * 1000) / i64::from(TARGET_RATE)
    }
}

pub struct Chunker {
    source: AudioSource,
    next_sequence: u64,
    pre_roll: VecDeque<PcmFrame>,
    pre_roll_samples: usize,
    active: Vec<i16>,
    started_at: Option<DateTime<Utc>>,
    silence_samples: usize,
    speech_samples: usize,
}

impl Chunker {
    pub fn new(source: AudioSource) -> Self {
        Self::starting_at(source, 1)
    }
    pub fn starting_at(source: AudioSource, next_sequence: u64) -> Self {
        Self {
            source,
            next_sequence: next_sequence.max(1),
            pre_roll: VecDeque::new(),
            pre_roll_samples: 0,
            active: Vec::new(),
            started_at: None,
            silence_samples: 0,
            speech_samples: 0,
        }
    }
    pub fn push(&mut self, frame: &PcmFrame) -> Vec<CompletedChunk> {
        if frame.samples.is_empty() {
            return Vec::new();
        }
        let speech = rms(&frame.samples) >= FLOOR_RMS;
        if self.started_at.is_none() {
            if !speech {
                self.push_pre_roll(frame.clone());
                return Vec::new();
            }
            self.started_at = self
                .pre_roll
                .front()
                .map(|f| f.captured_at)
                .or(Some(frame.captured_at));
            while let Some(prior) = self.pre_roll.pop_front() {
                self.active.extend(prior.samples);
            }
            self.pre_roll_samples = 0;
        }
        self.active.extend_from_slice(&frame.samples);
        if speech {
            self.speech_samples += frame.samples.len();
            self.silence_samples = 0;
        } else {
            self.silence_samples += frame.samples.len();
        }
        if self.silence_samples >= samples_for(SILENCE_MS) {
            let trim = self
                .silence_samples
                .saturating_sub(samples_for(POST_ROLL_MS));
            return self.finish(trim).into_iter().collect();
        }
        if self.active.len() >= samples_for(MAX_TURN_MS) {
            return self.finish(0).into_iter().collect();
        }
        Vec::new()
    }
    pub fn flush(&mut self) -> Option<CompletedChunk> {
        self.finish(0)
    }
    fn push_pre_roll(&mut self, frame: PcmFrame) {
        self.pre_roll_samples += frame.samples.len();
        self.pre_roll.push_back(frame);
        while self.pre_roll_samples > samples_for(PRE_ROLL_MS) {
            if let Some(old) = self.pre_roll.pop_front() {
                self.pre_roll_samples -= old.samples.len();
            }
        }
    }
    fn finish(&mut self, trim: usize) -> Option<CompletedChunk> {
        if trim > 0 && trim < self.active.len() {
            self.active.truncate(self.active.len() - trim);
        }
        let started_at = self.started_at.take();
        let samples = std::mem::take(&mut self.active);
        let enough_speech = self.speech_samples >= samples_for(MIN_SPEECH_MS);
        self.silence_samples = 0;
        self.speech_samples = 0;
        self.pre_roll.clear();
        self.pre_roll_samples = 0;
        if !enough_speech || samples.is_empty() {
            return None;
        }
        let started_at = started_at.unwrap_or_else(Utc::now);
        let ended_at = started_at
            + chrono::Duration::milliseconds(
                (samples.len() as i64 * 1000) / i64::from(TARGET_RATE),
            );
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        Some(CompletedChunk {
            source: self.source,
            sequence,
            started_at,
            ended_at,
            samples,
        })
    }
}
fn rms(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum = samples
        .iter()
        .map(|v| {
            let n = i64::from(*v);
            n * n
        })
        .sum::<i64>();
    (sum as f64 / samples.len() as f64).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    fn frame(ms: usize, amplitude: i16, at_ms: i64) -> PcmFrame {
        PcmFrame {
            source: AudioSource::Mic,
            samples: vec![amplitude; samples_for(ms)],
            captured_at: Utc.timestamp_millis_opt(at_ms).unwrap(),
        }
    }
    #[test]
    fn splits_after_turn_silence() {
        let mut c = Chunker::new(AudioSource::Mic);
        assert!(c.push(&frame(200, 0, 0)).is_empty());
        assert!(c.push(&frame(500, 2000, 200)).is_empty());
        let done = c.push(&frame(700, 0, 700));
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].sequence, 1);
    }
    #[test]
    fn silence_never_creates_a_turn() {
        let mut c = Chunker::new(AudioSource::System);
        assert!(c.push(&frame(1000, 0, 0)).is_empty());
        assert!(c.flush().is_none());
    }
    #[test]
    fn pause_flushes_partial_speech() {
        let mut c = Chunker::new(AudioSource::Mic);
        c.push(&frame(300, 2000, 0));
        assert_eq!(c.flush().unwrap().duration_ms(), 300);
    }
}

use crate::audio::convert::TARGET_RATE;
use crate::audio::{AudioSource, PcmFrame};
use chrono::{DateTime, Utc};
use std::collections::VecDeque;
use std::env;

const SILENCE_MS: usize = 800;
const PRE_ROLL_MS: usize = 400;
const POST_ROLL_MS: usize = 100;
const MAX_TURN_MS: usize = 30_000;
const MIN_SPEECH_MS: usize = 100;

/// Absolute RMS floor on the 16-bit scale. About -59 dBFS, which Parakeet
/// still transcribes reliably.
const DEFAULT_MIN_RMS: f64 = 35.0;
/// Speech must be this many times louder than the measured room floor
/// (1.4 = about +3 dB) before a turn starts.
const DEFAULT_NOISE_RATIO: f64 = 1.4;
/// Once a turn is open, it stays open while audio is only marginally above
/// the room floor (1.12 = about +1 dB). Without this, the quiet syllables in
/// the middle of a sentence end the turn and the rest of it is lost.
const DEFAULT_KEEP_RATIO: f64 = 1.12;
/// Do not trust the estimate until this much audio has been observed.
const MIN_NOISE_SAMPLES_MS: usize = 500;
/// How fast the estimated room floor may climb. Speech is full of short dips
/// that pull the estimate straight back down, so a steady room noise level or
/// a noisy room is followed within a second or two while ordinary speech does
/// not drag the estimate up with it. The old percentile-of-recent-audio
/// estimate did the opposite: talking raised the "noise floor", which raised
/// the gate, which cut off the speech that had raised it.
const NOISE_RISE_DB_PER_SEC: f64 = 12.0;
/// Upper bound on how far the estimate may move in a single callback. Keeps a
/// large audio fragment from carrying the estimate up by several dB at once.
const MAX_NOISE_RISE_DB_PER_CALL: f64 = 3.0;
/// The gate analyses windows no longer than this.
const ANALYSIS_MS: usize = 100;
/// The gate the service used before the adaptive floor existed, kept as the
/// ceiling for the first-frame seed.
const LEGACY_FLOOR_RMS: f64 = 250.0;

fn samples_for(ms: usize) -> usize {
    TARGET_RATE as usize * ms / 1000
}

fn env_f64(name: &str, default: f64) -> f64 {
    env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(default)
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
    min_rms: f64,
    noise_ratio: f64,
    keep_ratio: f64,
    noise_floor: f64,
    noise_samples: usize,
}

impl Chunker {
    pub fn new(source: AudioSource) -> Self {
        Self::starting_at(source, 1)
    }
    pub fn starting_at(source: AudioSource, next_sequence: u64) -> Self {
        let min_rms = env_f64("VOXTYPE_MEETING_VAD_MIN_RMS", DEFAULT_MIN_RMS);
        let noise_ratio = env_f64("VOXTYPE_MEETING_VAD_RATIO", DEFAULT_NOISE_RATIO);
        let keep_ratio = env_f64("VOXTYPE_MEETING_VAD_KEEP_RATIO", DEFAULT_KEEP_RATIO);
        Self {
            source,
            next_sequence: next_sequence.max(1),
            pre_roll: VecDeque::new(),
            pre_roll_samples: 0,
            active: Vec::new(),
            started_at: None,
            silence_samples: 0,
            speech_samples: 0,
            min_rms,
            noise_ratio,
            keep_ratio,
            noise_floor: min_rms,
            noise_samples: 0,
        }
    }
    pub fn push(&mut self, frame: &PcmFrame) -> Vec<CompletedChunk> {
        if frame.samples.is_empty() {
            return Vec::new();
        }
        let window = samples_for(ANALYSIS_MS);
        if frame.samples.len() <= window {
            return self.push_window(frame).into_iter().collect();
        }
        // PipeWire hands us fragments of a second or more. Gate on short
        // windows so the level envelope follows speech instead of being
        // averaged away across a whole sentence.
        let mut done = Vec::new();
        let mut offset = 0usize;
        while offset < frame.samples.len() {
            let end = (offset + window).min(frame.samples.len());
            let offset_ms = (offset as i64 * 1000) / i64::from(TARGET_RATE);
            let sub = PcmFrame {
                source: frame.source,
                samples: frame.samples[offset..end].to_vec(),
                captured_at: frame.captured_at + chrono::Duration::milliseconds(offset_ms),
            };
            done.extend(self.push_window(&sub));
            offset = end;
        }
        done
    }

    fn push_window(&mut self, frame: &PcmFrame) -> Option<CompletedChunk> {
        if frame.samples.is_empty() {
            return None;
        }
        let level = rms(&frame.samples);
        self.observe_noise(level, frame.samples.len());
        let threshold = if self.started_at.is_some() {
            self.keep_threshold()
        } else {
            self.threshold()
        };
        let speech = level >= threshold;
        if self.started_at.is_none() {
            if !speech {
                self.push_pre_roll(frame.clone());
                return None;
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
            return self.finish(trim);
        }
        if self.active.len() >= samples_for(MAX_TURN_MS) {
            return self.finish(0);
        }
        None
    }
    pub fn flush(&mut self) -> Option<CompletedChunk> {
        self.finish(0)
    }
    fn threshold(&self) -> f64 {
        (self.noise_floor * self.noise_ratio).max(self.min_rms)
    }
    fn keep_threshold(&self) -> f64 {
        (self.noise_floor * self.keep_ratio).max(self.min_rms)
    }
    fn observe_noise(&mut self, level: f64, samples: usize) {
        if self.noise_samples == 0 {
            // First frame of the session seeds the estimate. Capping the seed
            // means a session that starts mid-speech still opens a turn, and a
            // session that starts in a noisy room does not sit above the
            // threshold forever.
            self.noise_samples = samples;
            self.noise_floor = level.min(LEGACY_FLOOR_RMS);
            return;
        }
        if self.noise_samples < samples_for(MIN_NOISE_SAMPLES_MS) {
            // Learn the room quickly at the start, then stop chasing speech.
            self.noise_samples += samples;
            self.noise_floor = self.noise_floor.min(level);
            return;
        }
        if level <= self.noise_floor {
            // Attack: follow any quieter moment immediately.
            self.noise_floor = level;
            return;
        }
        let secs = samples as f64 / f64::from(TARGET_RATE);
        let rise_db = (NOISE_RISE_DB_PER_SEC * secs).min(MAX_NOISE_RISE_DB_PER_CALL);
        let ceiling = self.noise_floor * 10f64.powf(rise_db / 20.0);
        self.noise_floor = level.min(ceiling);
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
        let done = c.push(&frame(900, 0, 700));
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
    #[test]
    fn quiet_speech_above_room_floor_is_detected() {
        let mut c = Chunker::new(AudioSource::Mic);
        assert!(c.push(&frame(1000, 100, 0)).is_empty());
        assert!(c.push(&frame(400, 300, 1000)).is_empty());
        let done = c.push(&frame(900, 100, 1400));
        assert_eq!(done.len(), 1);
        assert!(done[0].duration_ms() >= 300);
    }
    #[test]
    fn steady_room_noise_does_not_create_turns() {
        let mut c = Chunker::new(AudioSource::Mic);
        for i in 0..20 {
            assert!(c.push(&frame(500, 100, i * 500)).is_empty());
        }
    }
    #[test]
    fn sustained_speech_with_dips_stays_one_turn() {
        // A quiet room at RMS ~40, then one sentence whose level wanders
        // between RMS 90 and 60 with short dips. The old percentile floor
        // rose past the speech and chopped this into pieces.
        let mut c = Chunker::new(AudioSource::Mic);
        let mut done = Vec::new();
        let mut t = 0i64;
        for _ in 0..6 {
            done.extend(c.push(&frame(100, 40, t)));
            t += 100;
        }
        let levels = [90, 120, 70, 65, 110, 80, 62, 95, 105, 68, 88, 60];
        for level in levels {
            done.extend(c.push(&frame(100, level, t)));
            t += 100;
        }
        for _ in 0..9 {
            done.extend(c.push(&frame(100, 40, t)));
            t += 100;
        }
        assert_eq!(done.len(), 1, "the whole sentence must stay in one turn");
        assert!(
            done[0].duration_ms() >= 1_100,
            "turn was truncated to {} ms",
            done[0].duration_ms()
        );
    }
    #[test]
    fn noisy_room_floor_is_learned_at_start() {
        // Session starts in a noisy room (RMS 300). The gate must settle
        // above it instead of transcribing the room forever.
        let mut c = Chunker::new(AudioSource::Mic);
        let mut chunks = Vec::new();
        for i in 0..100 {
            chunks.extend(c.push(&frame(100, 300, i * 100)));
        }
        let total: i64 = chunks.iter().map(|c| c.duration_ms()).sum();
        assert!(total <= 1_500, "steady noise produced {total} ms of audio");
    }
    #[test]
    fn long_server_fragments_are_analysed_in_short_windows() {
        // PipeWire hands over fragments of two seconds and more. Taking one
        // RMS for such a block averages a quiet sentence against the silence
        // around it, so the gate sees "no speech" and the sentence is lost.
        let mut samples = Vec::new();
        samples.extend(std::iter::repeat(40i16).take(samples_for(200)));
        samples.extend(std::iter::repeat(400i16).take(samples_for(1_600)));
        samples.extend(std::iter::repeat(40i16).take(samples_for(200)));
        let block = PcmFrame {
            source: AudioSource::Mic,
            samples,
            captured_at: Utc.timestamp_millis_opt(0).unwrap(),
        };
        let mut c = Chunker::new(AudioSource::Mic);
        let mut done = c.push(&block);
        done.extend(c.push(&frame(1_000, 40, 2_000)));
        assert_eq!(done.len(), 1);
        assert!(done[0].duration_ms() >= 1_500);
    }
    #[test]
    fn quiet_system_audio_is_detected() {
        let mut c = Chunker::new(AudioSource::System);
        assert!(c.push(&frame(1000, 0, 0)).is_empty());
        assert!(c.push(&frame(400, 100, 1000)).is_empty());
        let done = c.push(&frame(900, 0, 1400));
        assert_eq!(done.len(), 1);
    }
}

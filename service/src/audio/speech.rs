use chrono::{DateTime, Utc};

use super::convert::TARGET_RATE;

/// RMS window used to find speech inside a capture chunk.
const WINDOW_MS: u64 = 20;
/// Consecutive windows required before a region counts as speech (~80 ms).
const MIN_SPEECH_WINDOWS: usize = 4;
/// Pull the event start back slightly so the header is not after the first phone.
const LEAD_PAD_MS: u64 = 50;
/// Keep the event end a little after the last voiced window.
const TRAIL_PAD_MS: u64 = 100;
/// Ignore residual capture noise below this RMS.
const FLOOR_RMS: f64 = 250.0;
/// Also require a fraction of the chunk's peak RMS so loud files stay relative.
const PEAK_FRACTION: f64 = 0.04;

/// First and last milliseconds that contain speech inside `samples`.
///
/// `None` means the buffer is effectively silent (or too short to judge).
pub fn speech_range_ms(samples: &[i16], sample_rate: u32) -> Option<(u64, u64)> {
    if samples.is_empty() || sample_rate == 0 {
        return None;
    }
    let window = window_samples(sample_rate);
    if window == 0 {
        return None;
    }

    let rms: Vec<f64> = samples.chunks(window).map(window_rms).collect();
    let peak = rms.iter().copied().fold(0.0_f64, f64::max);
    if peak < FLOOR_RMS {
        return None;
    }
    let thresh = FLOOR_RMS.max(peak * PEAK_FRACTION);

    let mut run = 0usize;
    let mut first = None;
    let mut last = None;
    for (index, value) in rms.iter().copied().enumerate() {
        if value >= thresh {
            run = run.saturating_add(1);
            if run >= MIN_SPEECH_WINDOWS {
                if first.is_none() {
                    first = Some(index + 1 - MIN_SPEECH_WINDOWS);
                }
                last = Some(index);
            }
        } else {
            run = 0;
        }
    }

    let first_win = first?;
    let last_win = last?;
    let duration_ms = duration_ms(samples.len(), sample_rate);
    let start_ms = (first_win as u64)
        .saturating_mul(WINDOW_MS)
        .saturating_sub(LEAD_PAD_MS);
    let end_ms = (last_win as u64)
        .saturating_add(1)
        .saturating_mul(WINDOW_MS)
        .saturating_add(TRAIL_PAD_MS)
        .min(duration_ms);
    if end_ms <= start_ms {
        return None;
    }
    Some((start_ms, end_ms))
}

/// Shift a chunk window to the detected speech region, clamped to the chunk.
pub fn align_chunk_times(
    chunk_started_at: DateTime<Utc>,
    chunk_ended_at: DateTime<Utc>,
    samples: &[i16],
    sample_rate: u32,
) -> (DateTime<Utc>, DateTime<Utc>) {
    let Some((start_ms, end_ms)) = speech_range_ms(samples, sample_rate) else {
        return (chunk_started_at, chunk_ended_at);
    };
    let start = add_ms(chunk_started_at, start_ms).min(chunk_ended_at);
    let end = add_ms(chunk_started_at, end_ms)
        .min(chunk_ended_at)
        .max(start);
    (start, end)
}

pub fn align_chunk_times_16k(
    chunk_started_at: DateTime<Utc>,
    chunk_ended_at: DateTime<Utc>,
    samples: &[i16],
) -> (DateTime<Utc>, DateTime<Utc>) {
    align_chunk_times(chunk_started_at, chunk_ended_at, samples, TARGET_RATE)
}

fn window_samples(sample_rate: u32) -> usize {
    usize::try_from(u64::from(sample_rate).saturating_mul(WINDOW_MS) / 1000).unwrap_or(0)
}

fn duration_ms(len: usize, sample_rate: u32) -> u64 {
    (len as u64).saturating_mul(1000) / u64::from(sample_rate)
}

fn window_rms(chunk: &[i16]) -> f64 {
    if chunk.is_empty() {
        return 0.0;
    }
    let sum_sq: i64 = chunk
        .iter()
        .map(|sample| i64::from(*sample).saturating_mul(i64::from(*sample)))
        .sum();
    (sum_sq as f64 / chunk.len() as f64).sqrt()
}

fn add_ms(value: DateTime<Utc>, ms: u64) -> DateTime<Utc> {
    let ms = i64::try_from(ms).unwrap_or(i64::MAX);
    value + chrono::Duration::milliseconds(ms)
}

#[cfg(test)]
mod tests {
    use super::{align_chunk_times, speech_range_ms, TARGET_RATE};
    use chrono::{TimeZone, Utc};

    fn sine(len: usize, freq: f32, amp: i16) -> Vec<i16> {
        (0..len)
            .map(|i| {
                let t = i as f32 / TARGET_RATE as f32;
                (f32::from(amp) * (2.0 * std::f32::consts::PI * freq * t).sin()) as i16
            })
            .collect()
    }

    #[test]
    fn silence_has_no_speech_range() {
        assert_eq!(
            speech_range_ms(&vec![0; TARGET_RATE as usize], TARGET_RATE),
            None
        );
    }

    #[test]
    fn speech_from_start_stays_near_zero() {
        let samples = sine(TARGET_RATE as usize, 440.0, 8_000);
        let (start_ms, end_ms) = speech_range_ms(&samples, TARGET_RATE).unwrap();
        assert!(start_ms <= 50, "start_ms={start_ms}");
        assert!(end_ms >= 900, "end_ms={end_ms}");
    }

    #[test]
    fn silence_then_tone_shifts_start() {
        let mut samples = vec![0i16; TARGET_RATE as usize];
        samples.extend(sine(TARGET_RATE as usize / 5, 440.0, 8_000));
        let (start_ms, end_ms) = speech_range_ms(&samples, TARGET_RATE).unwrap();
        assert!(
            (900..=1_050).contains(&start_ms),
            "expected onset near 1s after 50ms pad, got {start_ms}"
        );
        assert!(end_ms > start_ms);
        assert!(end_ms <= 1_200);
    }

    #[test]
    fn align_clamps_to_chunk_and_is_idempotent_on_pcm() {
        let start = Utc.with_ymd_and_hms(2026, 8, 17, 18, 45, 39).unwrap();
        let mut samples = vec![0i16; TARGET_RATE as usize];
        samples.extend(sine(TARGET_RATE as usize / 5, 440.0, 8_000));
        let end = start + chrono::Duration::milliseconds(1_200);
        let (aligned_start, aligned_end) = align_chunk_times(start, end, &samples, TARGET_RATE);
        let offset = (aligned_start - start).num_milliseconds();
        assert!((900..=1_050).contains(&offset), "aligned offset {offset}ms");
        assert!(aligned_end <= end);
        assert!(aligned_start >= start);
        let again = align_chunk_times(start, end, &samples, TARGET_RATE);
        assert_eq!((aligned_start, aligned_end), again);
    }
}

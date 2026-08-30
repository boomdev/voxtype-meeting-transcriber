use crate::error::{AppError, Result};

pub const TARGET_RATE: u32 = 16_000;
pub const TARGET_CHANNELS: u16 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PcmFormat {
    U8,
    S16Le,
    S16Be,
    S24Le,
    S24_32Le,
    S32Le,
    F32Le,
}

/// Convert a raw PCM buffer to 16 kHz 16-bit mono samples.
///
/// Rate conversion uses per-buffer linear interpolation. PulseAudio fragment
/// sizes vary, so a fixed-chunk resampler is a poor fit for the capture path.
pub fn to_i16_mono_16k(
    bytes: &[u8],
    format: PcmFormat,
    channels: u16,
    rate: u32,
) -> Result<Vec<i16>> {
    if channels == 0 {
        return Err(AppError::audio(
            "audio conversion failed because the stream has zero channels",
        ));
    }
    if rate == 0 {
        return Err(AppError::audio(
            "audio conversion failed because the stream sample rate is 0",
        ));
    }

    if channels == 1 && rate == TARGET_RATE && format == PcmFormat::S16Le {
        return decode_i16_raw(bytes, false);
    }

    let mono = decode_to_mono_f32(bytes, format, channels)?;
    let resampled = if rate == TARGET_RATE {
        mono
    } else {
        resample_linear(&mono, rate, TARGET_RATE)
    };
    Ok(resampled.into_iter().map(f32_to_i16).collect())
}

fn decode_to_mono_f32(bytes: &[u8], format: PcmFormat, channels: u16) -> Result<Vec<f32>> {
    let channels = channels as usize;
    let samples = match format {
        PcmFormat::U8 => decode_u8(bytes),
        PcmFormat::S16Le => decode_i16(bytes, false)?,
        PcmFormat::S16Be => decode_i16(bytes, true)?,
        PcmFormat::S24Le => decode_i24(bytes)?,
        PcmFormat::S24_32Le => decode_i32_as_24(bytes)?,
        PcmFormat::S32Le => decode_i32(bytes)?,
        PcmFormat::F32Le => decode_f32(bytes)?,
    };
    Ok(downmix_mono(&samples, channels))
}

fn decode_u8(bytes: &[u8]) -> Vec<f32> {
    bytes.iter().map(|b| (*b as f32 - 128.0) / 128.0).collect()
}

fn decode_i16_raw(bytes: &[u8], big_endian: bool) -> Result<Vec<i16>> {
    if !bytes.len().is_multiple_of(2) {
        return Err(AppError::audio(
            "audio conversion failed because S16 PCM length is not a multiple of 2",
        ));
    }
    Ok(bytes
        .chunks_exact(2)
        .map(|c| {
            if big_endian {
                i16::from_be_bytes([c[0], c[1]])
            } else {
                i16::from_le_bytes([c[0], c[1]])
            }
        })
        .collect())
}

fn decode_i16(bytes: &[u8], big_endian: bool) -> Result<Vec<f32>> {
    if !bytes.len().is_multiple_of(2) {
        return Err(AppError::audio(
            "audio conversion failed because S16 PCM length is not a multiple of 2",
        ));
    }
    Ok(bytes
        .chunks_exact(2)
        .map(|c| {
            let v = if big_endian {
                i16::from_be_bytes([c[0], c[1]])
            } else {
                i16::from_le_bytes([c[0], c[1]])
            };
            v as f32 / 32768.0
        })
        .collect())
}

fn decode_i24(bytes: &[u8]) -> Result<Vec<f32>> {
    if !bytes.len().is_multiple_of(3) {
        return Err(AppError::audio(
            "audio conversion failed because S24 PCM length is not a multiple of 3",
        ));
    }
    Ok(bytes
        .chunks_exact(3)
        .map(|c| {
            let mut padded = [0u8; 4];
            padded[0] = c[0];
            padded[1] = c[1];
            padded[2] = c[2];
            if c[2] & 0x80 != 0 {
                padded[3] = 0xff;
            }
            let v = i32::from_le_bytes(padded);
            v as f32 / 8_388_608.0
        })
        .collect())
}

fn decode_i32_as_24(bytes: &[u8]) -> Result<Vec<f32>> {
    decode_i32(bytes)
}

fn decode_i32(bytes: &[u8]) -> Result<Vec<f32>> {
    if !bytes.len().is_multiple_of(4) {
        return Err(AppError::audio(
            "audio conversion failed because S32 PCM length is not a multiple of 4",
        ));
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f32 / 2_147_483_648.0)
        .collect())
}

fn decode_f32(bytes: &[u8]) -> Result<Vec<f32>> {
    if !bytes.len().is_multiple_of(4) {
        return Err(AppError::audio(
            "audio conversion failed because F32 PCM length is not a multiple of 4",
        ));
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

fn downmix_mono(samples: &[f32], channels: usize) -> Vec<f32> {
    if channels == 1 {
        return samples.to_vec();
    }
    samples
        .chunks(channels)
        .filter(|frame| frame.len() == channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect()
}

fn resample_linear(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if input.is_empty() {
        return Vec::new();
    }
    let ratio = from as f64 / to as f64;
    let out_len = ((input.len() as f64) / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src = i as f64 * ratio;
        let i0 = src.floor() as usize;
        let frac = (src - i0 as f64) as f32;
        let s0 = input[i0.min(input.len() - 1)];
        let s1 = input.get(i0 + 1).copied().unwrap_or(s0);
        out.push(s0 + (s1 - s0) * frac);
    }
    out
}

fn f32_to_i16(sample: f32) -> i16 {
    let scaled = sample * 32767.0;
    scaled.clamp(i16::MIN as f32, i16::MAX as f32) as i16
}

#[cfg(test)]
mod tests {
    use super::{to_i16_mono_16k, PcmFormat, TARGET_RATE};

    #[test]
    fn s16le_mono_passthrough() {
        let samples: Vec<i16> = (0..160).map(|i| i * 20).collect();
        let mut bytes = Vec::new();
        for s in &samples {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        let out = to_i16_mono_16k(&bytes, PcmFormat::S16Le, 1, TARGET_RATE).unwrap();
        assert_eq!(out, samples);
    }

    #[test]
    fn stereo_48k_to_mono_16k_length() {
        let frames = 48_000usize;
        let mut bytes = Vec::with_capacity(frames * 4);
        for n in 0..frames {
            let t = n as f32 / 48_000.0;
            let sample = (2.0 * std::f32::consts::PI * 440.0 * t).sin();
            let v = (sample * 1000.0) as i16;
            bytes.extend_from_slice(&v.to_le_bytes());
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        let out = to_i16_mono_16k(&bytes, PcmFormat::S16Le, 2, 48_000).unwrap();
        assert!(
            (out.len() as i32 - 16_000).abs() <= 2,
            "expected ~16000 samples, got {}",
            out.len()
        );
        assert!(out.iter().any(|s| *s != 0));
    }
}

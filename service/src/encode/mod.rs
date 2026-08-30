use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, SecondsFormat, Utc};
use flacenc::bitsink::ByteSink;
use flacenc::component::BitRepr;
use flacenc::config::Encoder;
use flacenc::encode_with_fixed_block_size;
use flacenc::error::Verify;
use flacenc::source::MemSource;

use crate::audio::convert::TARGET_RATE;
use crate::error::{AppError, Result};

const FLAC_BLOCK_SIZE: usize = 4096;
const BITS_PER_SAMPLE: usize = 16;
const CHANNELS: usize = 1;

pub fn audio_file_name(sequence: u64, started_at: DateTime<Utc>) -> String {
    let stamp = started_at
        .to_rfc3339_opts(SecondsFormat::Millis, true)
        .replace(':', "-");
    format!("{sequence:08}_{stamp}.flac")
}

pub fn encode_flac_i16_mono_16k(samples: &[i16]) -> Result<Vec<u8>> {
    if samples.is_empty() {
        return Err(AppError::encode(
            "cannot encode an empty PCM buffer as FLAC",
        ));
    }
    let signal: Vec<i32> = samples.iter().copied().map(i32::from).collect();
    let source = MemSource::from_samples(&signal, CHANNELS, BITS_PER_SAMPLE, TARGET_RATE as usize);
    let config = Encoder::default().into_verified().map_err(|error| {
        AppError::encode(format!("invalid FLAC encoder configuration: {error:?}"))
    })?;
    let stream = encode_with_fixed_block_size(&config, source, FLAC_BLOCK_SIZE)
        .map_err(|error| AppError::encode(format!("FLAC encoding failed: {error}")))?;
    let mut sink = ByteSink::new();
    stream
        .write(&mut sink)
        .map_err(|error| AppError::encode(format!("FLAC bitstream write failed: {error}")))?;
    Ok(sink.as_slice().to_vec())
}

pub fn decode_flac_i16_mono_16k(path: &Path) -> Result<Vec<i16>> {
    let mut reader = claxon::FlacReader::open(path).map_err(|error| {
        AppError::encode(format!("could not decode FLAC {}: {error}", path.display()))
    })?;
    let info = reader.streaminfo();
    if info.sample_rate != TARGET_RATE {
        return Err(AppError::encode(format!(
            "FLAC {} is {} Hz; expected {TARGET_RATE} Hz",
            path.display(),
            info.sample_rate
        )));
    }
    if info.channels != 1 {
        return Err(AppError::encode(format!(
            "FLAC {} has {} channels; expected mono",
            path.display(),
            info.channels
        )));
    }
    let bits = info.bits_per_sample;
    let mut samples = Vec::new();
    for sample in reader.samples() {
        let value = sample.map_err(|error| {
            AppError::encode(format!(
                "FLAC {} contained an invalid sample: {error}",
                path.display()
            ))
        })?;
        samples.push(scale_pcm_to_i16(value, bits));
    }
    Ok(samples)
}

fn scale_pcm_to_i16(sample: i32, bits: u32) -> i16 {
    if bits == 16 {
        return sample.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
    }
    if bits == 0 || bits > 32 {
        return 0;
    }
    let shift = 16u32.saturating_sub(bits.min(16));
    if bits <= 16 {
        (sample << shift) as i16
    } else {
        (sample >> (bits - 16)) as i16
    }
}

pub fn write_wav_i16_mono_16k(path: &Path, samples: &[i16]) -> Result<()> {
    let data_len = u32::try_from(samples.len().saturating_mul(2))
        .map_err(|_| AppError::encode("WAV payload is too large to encode"))?;
    let mut bytes = Vec::with_capacity(44 + samples.len() * 2);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36u32.saturating_add(data_len)).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(b"fmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&TARGET_RATE.to_le_bytes());
    bytes.extend_from_slice(&(TARGET_RATE * 2).to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    if let Some(parent) = path.parent() {
        crate::paths::ensure_dir(parent)?;
    }
    let mut file = File::create(path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

pub fn write_flac_atomic(dir: &Path, file_name: &str, bytes: &[u8]) -> Result<PathBuf> {
    crate::paths::ensure_dir(dir)?;
    let dest = dir.join(file_name);
    if dest.exists() {
        return Err(AppError::encode(format!(
            "refusing to overwrite existing audio chunk {}",
            dest.display()
        )));
    }
    let tmp = dir.join(format!("{file_name}.tmp"));
    if tmp.exists() {
        std::fs::remove_file(&tmp)?;
    }
    {
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

pub fn persist_chunk(
    dir: &Path,
    sequence: u64,
    started_at: DateTime<Utc>,
    samples: &[i16],
) -> Result<PathBuf> {
    let file_name = audio_file_name(sequence, started_at);
    let bytes = encode_flac_i16_mono_16k(samples)?;
    write_flac_atomic(dir, &file_name, &bytes)
}

#[cfg(test)]
mod tests {
    use super::{
        audio_file_name, decode_flac_i16_mono_16k, encode_flac_i16_mono_16k, write_flac_atomic,
        write_wav_i16_mono_16k,
    };
    use chrono::{TimeZone, Utc};
    use tempfile::tempdir;

    #[test]
    fn filename_matches_spec_example() {
        let started = Utc.with_ymd_and_hms(2026, 8, 17, 12, 32, 4).unwrap()
            + chrono::Duration::milliseconds(120);
        assert_eq!(
            audio_file_name(12, started),
            "00000012_2026-08-17T12-32-04.120Z.flac"
        );
    }

    #[test]
    fn flac_roundtrip_and_wav_header() {
        let samples: Vec<i16> = (0..1600).map(|i| (i * 13) as i16).collect();
        let dir = tempdir().unwrap();
        let flac = dir.path().join("a.flac");
        let bytes = encode_flac_i16_mono_16k(&samples).unwrap();
        std::fs::write(&flac, bytes).unwrap();
        let decoded = decode_flac_i16_mono_16k(&flac).unwrap();
        assert_eq!(decoded.len(), samples.len());
        let max_err = samples
            .iter()
            .zip(&decoded)
            .map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs())
            .max()
            .unwrap_or(0);
        assert!(max_err <= 2, "flac roundtrip error {max_err}");
        let wav = dir.path().join("a.wav");
        write_wav_i16_mono_16k(&wav, &samples).unwrap();
        let wav_bytes = std::fs::read(&wav).unwrap();
        assert_eq!(&wav_bytes[..4], b"RIFF");
        assert_eq!(&wav_bytes[8..12], b"WAVE");
    }

    #[test]
    fn encode_flac_magic() {
        let samples = vec![0i16; 1600];
        let bytes = encode_flac_i16_mono_16k(&samples).unwrap();
        assert!(bytes.len() > 4);
        assert_eq!(&bytes[..4], b"fLaC");
    }

    #[test]
    fn atomic_write_leaves_tmp_on_pre_rename_path() {
        let dir = tempdir().unwrap();
        let bytes = encode_flac_i16_mono_16k(&[0; 1600]).unwrap();
        let path = write_flac_atomic(dir.path(), "00000001_test.flac", &bytes).unwrap();
        assert!(path.exists());
        assert!(!dir.path().join("00000001_test.flac.tmp").exists());
        let err = write_flac_atomic(dir.path(), "00000001_test.flac", &bytes).unwrap_err();
        assert!(err.to_string().contains("refusing to overwrite"));
    }
}

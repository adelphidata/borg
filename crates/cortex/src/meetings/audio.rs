//! The audio a meeting chunk carries: 16-bit PCM WAV, checked before it is
//! sent anywhere.

use anyhow::{bail, Context, Result};

/// What a chunk's WAV header and data say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WavInfo {
    /// Hz.
    pub sample_rate: u32,
    /// Interleaved channels.
    pub channels: u16,
    /// Length of the audio, from the samples actually present.
    pub duration_ms: i64,
}

/// Read a chunk's WAV, rejecting anything that is not 16-bit integer PCM or
/// holds no audio. The length comes from counting the samples, so a body cut
/// short in transit is an error rather than a duration the header made up.
pub fn inspect_wav(bytes: &[u8]) -> Result<WavInfo> {
    let (spec, samples) = pcm_i16(bytes)?;
    let frames = samples.len() as i64 / i64::from(spec.channels.max(1));
    Ok(WavInfo {
        sample_rate: spec.sample_rate,
        channels: spec.channels,
        duration_ms: frames * 1000 / i64::from(spec.sample_rate.max(1)),
    })
}

/// The chunk's spec and samples, for appending to the kept track file.
pub fn pcm_i16(bytes: &[u8]) -> Result<(hound::WavSpec, Vec<i16>)> {
    let reader =
        hound::WavReader::new(std::io::Cursor::new(bytes)).context("not a readable WAV")?;
    let spec = reader.spec();
    if spec.sample_format != hound::SampleFormat::Int || spec.bits_per_sample != 16 {
        bail!(
            "expected 16-bit PCM WAV, got {}-bit {:?}",
            spec.bits_per_sample,
            spec.sample_format
        );
    }
    if spec.sample_rate == 0 || spec.channels == 0 {
        bail!("WAV header names no sample rate or channels");
    }
    let samples = reader
        .into_samples::<i16>()
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("WAV data is truncated or corrupt")?;
    if samples.is_empty() {
        bail!("WAV holds no audio");
    }
    Ok((spec, samples))
}

#[cfg(test)]
pub(crate) mod test_wav {
    /// A 16 kHz mono 16-bit WAV of `ms` milliseconds of a quiet tone.
    pub(crate) fn wav_ms(ms: u32) -> Vec<u8> {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut cursor = std::io::Cursor::new(Vec::new());
        {
            let mut w = hound::WavWriter::new(&mut cursor, spec).unwrap();
            for i in 0..(16 * ms) {
                w.write_sample(((i % 40) as i16 - 20) * 100).unwrap();
            }
            w.finalize().unwrap();
        }
        cursor.into_inner()
    }
}

#[cfg(test)]
mod tests {
    use super::test_wav::wav_ms;
    use super::*;

    #[test]
    fn duration_comes_from_the_samples_present() {
        let info = inspect_wav(&wav_ms(2_500)).unwrap();
        assert_eq!(info.sample_rate, 16_000);
        assert_eq!(info.channels, 1);
        assert_eq!(info.duration_ms, 2_500);
    }

    #[test]
    fn rejects_float_empty_truncated_and_non_wav_bytes() {
        let float = {
            let spec = hound::WavSpec {
                channels: 1,
                sample_rate: 16_000,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            };
            let mut c = std::io::Cursor::new(Vec::new());
            {
                let mut w = hound::WavWriter::new(&mut c, spec).unwrap();
                w.write_sample(0.5f32).unwrap();
                w.finalize().unwrap();
            }
            c.into_inner()
        };
        let empty = wav_ms(0);
        let full = wav_ms(1_000);
        let truncated = &full[..full.len() - 1_001];
        for (label, bytes) in [
            ("float", float.as_slice()),
            ("empty", empty.as_slice()),
            ("truncated", truncated),
            ("garbage", b"definitely not audio".as_slice()),
        ] {
            assert!(inspect_wav(bytes).is_err(), "{label} must be rejected");
        }
    }
}

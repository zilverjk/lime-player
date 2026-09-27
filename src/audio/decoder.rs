use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoderOptions};
use symphonia::core::audio::sample::SampleFormat;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, SeekMode, SeekTo, TrackType};
use symphonia::core::formats::probe::Hint;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::Timestamp;
use wavpack::WavpackReader;

const WAVPACK_MODE_FLOAT: i32 = 0x8;
const WAVPACK_MODE_HYBRID: i32 = 0x4;
const DECODE_CHUNK_FRAMES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioInfo {
    pub sample_rate: u32,
    pub duration_ms: Option<u64>,
    pub source_channels: u16,
    pub bits_per_sample: u32,
    pub is_float: bool,
    pub integer_pcm: bool,
    pub format: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PcmSample {
    Integer(i32),
    Float(f32),
}

#[derive(Debug, thiserror::Error)]
pub enum DecoderError {
    #[error("Unsupported audio format: {0}")]
    UnsupportedFormat(String),
    #[error("WavPack correction files (.wvc) and hybrid WavPack tracks are unsupported; refusing incomplete lossy playback")]
    WavPackCorrectionUnsupported,
    #[error("The file has unsupported channel count {0}; only mono and stereo are supported in this milestone")]
    UnsupportedChannels(u16),
    #[error("Audio file has no sample rate")]
    MissingSampleRate,
    #[error("Audio file has no decodable audio track")]
    MissingAudioTrack,
    #[error("Decoder error: {0}")]
    Decode(String),
    #[error("File error: {0}")]
    Io(#[from] std::io::Error),
    #[error("WavPack error: {0}")]
    WavPack(#[from] wavpack::Error),
}

pub fn probe_file(path: &Path) -> Result<AudioInfo, DecoderError> {
    match extension(path).as_deref() {
        Some("wv") | Some("wavpack") => probe_wavpack(path),
        Some("wvc") => Err(DecoderError::WavPackCorrectionUnsupported),
        Some("flac") | Some("wav") | Some("mp3") => probe_symphonia(path),
        Some(other) => Err(DecoderError::UnsupportedFormat(other.to_owned())),
        None => Err(DecoderError::UnsupportedFormat("unknown extension".into())),
    }
}

/// `expected` is the `AudioInfo` the caller already committed to (the output stream is configured
/// from it before this ever runs, per `spawn_track_preparation`/`start_track_prepared`). Re-probing
/// here can observe a file that was replaced under the same path since it was scanned — most
/// plausible over a NAS share — so a mismatch on any property the output route depends on is
/// refused instead of decoded at the stream's now-stale configuration, which would otherwise play
/// back at the wrong speed instead of being refused (`§1.4`, "refuse rather than silently degrade").
pub fn decode_file(
    path: &Path,
    expected: &AudioInfo,
    cancelled: &AtomicBool,
    preserve_integer: bool,
    start_frame: u64,
    mut output: impl FnMut(PcmSample) -> Result<(), String>,
) -> Result<AudioInfo, DecoderError> {
    let info = probe_file(path)?;
    if info.sample_rate != expected.sample_rate
        || info.source_channels != expected.source_channels
        || info.integer_pcm != expected.integer_pcm
        || info.bits_per_sample != expected.bits_per_sample
    {
        return Err(DecoderError::Decode("The file changed since it was added; re-open it".into()));
    }
    match info.format.as_str() {
        "WavPack" => decode_wavpack(path, &info, cancelled, preserve_integer, start_frame, &mut output)?,
        _ => decode_symphonia(path, &info, cancelled, preserve_integer, start_frame, &mut output)?,
    }
    Ok(info)
}

fn extension(path: &Path) -> Option<String> {
    path.extension()?.to_str().map(str::to_ascii_lowercase)
}

fn probe_wavpack(path: &Path) -> Result<AudioInfo, DecoderError> {
    let mut reader = WavpackReader::open(path)?.build()?;
    if reader.get_mode()? & WAVPACK_MODE_HYBRID != 0 || correction_file_exists(path) {
        return Err(DecoderError::WavPackCorrectionUnsupported);
    }
    info_from_wavpack(&mut reader)
}

fn correction_file_exists(path: &Path) -> bool {
    path.with_extension("wvc").is_file()
}

fn info_from_wavpack(reader: &mut WavpackReader) -> Result<AudioInfo, DecoderError> {
    let channels = reader
        .get_num_channels()?
        .try_into()
        .map_err(|_| DecoderError::UnsupportedChannels(0))?;
    validate_channels(channels)?;
    let sample_rate = reader.get_sample_rate()?;
    if sample_rate == 0 {
        return Err(DecoderError::MissingSampleRate);
    }
    let is_float = reader.get_mode()? & WAVPACK_MODE_FLOAT != 0;
    let duration_ms = u64::try_from(reader.get_num_samples()?)
        .ok()
        .and_then(|frames| duration_millis_from_frames(frames, sample_rate));
    Ok(AudioInfo {
        sample_rate,
        duration_ms,
        source_channels: channels,
        bits_per_sample: reader.get_bits_per_sample()?.max(0) as u32,
        is_float,
        integer_pcm: !is_float,
        format: "WavPack".into(),
    })
}

fn probe_symphonia(path: &Path) -> Result<AudioInfo, DecoderError> {
    let (format, track_id, codec_params) = open_symphonia(path)?;
    let sample_rate = codec_params.sample_rate.ok_or(DecoderError::MissingSampleRate)?;
    let channels = codec_params
        .channels
        .ok_or(DecoderError::MissingAudioTrack)?
        .count()
        .try_into()
        .map_err(|_| DecoderError::UnsupportedChannels(u16::MAX))?;
    validate_channels(channels)?;
    let name = match extension(path).as_deref() {
        Some("flac") => "FLAC",
        Some("wav") => "WAV",
        Some("mp3") => "MP3",
        _ => "Audio",
    };
    let is_float = matches!(codec_params.sample_format, Some(SampleFormat::F32 | SampleFormat::F64));
    let _ = track_id;
    let integer_pcm = !is_float && matches!(name, "FLAC" | "WAV");
    Ok(AudioInfo {
        sample_rate,
        duration_ms: symphonia_duration_millis(format.as_ref()),
        source_channels: channels,
        bits_per_sample: codec_params.bits_per_sample.unwrap_or(16),
        is_float,
        integer_pcm,
        format: name.into(),
    })
}

fn duration_millis_from_frames(frames: u64, sample_rate: u32) -> Option<u64> {
    if sample_rate == 0 {
        return None;
    }
    let millis = u128::from(frames) * 1_000 / u128::from(sample_rate);
    u64::try_from(millis).ok()
}

fn symphonia_duration_millis(format: &dyn symphonia::core::formats::FormatReader) -> Option<u64> {
    let media = format.media_info();
    let duration = media.duration?;
    let time = media.time_base?.calc_duration(duration)?;
    u64::try_from(time.as_nanos().max(0) / 1_000_000).ok()
}

pub(crate) fn open_symphonia(
    path: &Path,
) -> Result<
    (
        Box<dyn symphonia::core::formats::FormatReader>,
        u32,
        AudioCodecParameters,
    ),
    DecoderError,
> {
    let file = std::fs::File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = extension(path) {
        hint.with_extension(&ext);
    }
    let format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| DecoderError::Decode(e.to_string()))?;
    let (track_id, codec_params) = {
        let track = format
            .default_track(TrackType::Audio)
            .ok_or(DecoderError::MissingAudioTrack)?;
        let codec_params = track
            .codec_params
            .as_ref()
            .ok_or(DecoderError::MissingAudioTrack)?
            .audio()
            .ok_or(DecoderError::MissingAudioTrack)?
            .clone();
        (track.id, codec_params)
    };
    Ok((format, track_id, codec_params))
}

fn decode_symphonia(
    path: &Path,
    info: &AudioInfo,
    cancelled: &AtomicBool,
    preserve_integer: bool,
    start_frame: u64,
    output: &mut impl FnMut(PcmSample) -> Result<(), String>,
) -> Result<(), DecoderError> {
    let (mut format, track_id, codec_params) = open_symphonia(path)?;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&codec_params, &AudioDecoderOptions::default())
        .map_err(|e| DecoderError::Decode(e.to_string()))?;
    let preserve_integer = preserve_integer && info.integer_pcm;
    if preserve_integer && info.source_channels != 2 {
        return Err(DecoderError::Decode(
            "Strict integer output requires stereo PCM; mono-to-stereo conversion is not allowed.".into(),
        ));
    }

    // `seeking` stays true only until the first packet overlapping `start_frame` is found; from
    // then on every later packet is forwarded whole, exactly like an unseeked decode (§4.2).
    let mut seeking = start_frame > 0;
    if seeking {
        let time_base = format.tracks().iter().find(|track| track.id == track_id).and_then(|track| track.time_base);
        let is_unit_time_base =
            time_base.is_some_and(|base| base.numer.get() == 1 && base.denom.get() == info.sample_rate);
        if !is_unit_time_base {
            return Err(DecoderError::Decode("Seeking is not supported for this stream".into()));
        }
        let seeked = format
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(start_frame as i64), track_id })
            .map_err(|e| DecoderError::Decode(e.to_string()))?;
        // Symphonia's FLAC linear search can overshoot on some streams
        // (symphonia-bundle-flac-0.6.1 demuxer.rs:375-378); refuse rather than start late.
        if seeked.actual_ts.get() > seeked.required_ts.get() {
            return Err(DecoderError::Decode("Seek landed past the requested position".into()));
        }
        decoder.reset();
    }

    let mut integer_interleaved = Vec::<i32>::with_capacity(DECODE_CHUNK_FRAMES * info.source_channels as usize);
    let mut float_interleaved = Vec::<f32>::with_capacity(DECODE_CHUNK_FRAMES * info.source_channels as usize);

    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Ok(());
        }
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(SymphoniaError::ResetRequired) => {
                return Err(DecoderError::Decode("Chained audio streams are not supported".into()))
            }
            Err(err) => return Err(DecoderError::Decode(err.to_string())),
        };
        if packet.track_id != track_id {
            continue;
        }
        let decoded = decoder
            .decode(&packet)
            .map_err(|e| DecoderError::Decode(e.to_string()))?;
        let channels = decoded.spec().channels().count() as u16;
        validate_channels(channels)?;
        let frames_in_packet = decoded.samples_interleaved() / channels as usize;

        let mut skip_frames = 0usize;
        if seeking {
            let pts = packet.pts.get().max(0) as u64;
            if pts + frames_in_packet as u64 <= start_frame {
                continue; // Entirely before the target: skip the whole packet.
            }
            if pts > start_frame {
                // The seek landed at or after this packet without ever overlapping
                // `start_frame`: starting here would silently begin late.
                return Err(DecoderError::Decode("Seek landed past the requested position".into()));
            }
            skip_frames = (start_frame - pts) as usize;
            seeking = false;
        }
        let skip_samples = skip_frames * channels as usize;

        if preserve_integer {
            integer_interleaved.resize(decoded.samples_interleaved(), 0);
            decoded.copy_to_slice_interleaved(&mut integer_interleaved);
            for sample in &integer_interleaved[skip_samples..] {
                validate_left_justified_sample(*sample, info.bits_per_sample)?;
                output(PcmSample::Integer(*sample)).map_err(DecoderError::Decode)?;
            }
        } else {
            float_interleaved.resize(decoded.samples_interleaved(), 0.0);
            decoded.copy_to_slice_interleaved(&mut float_interleaved);
            forward_float_interleaved(&float_interleaved[skip_samples..], channels, output)
                .map_err(DecoderError::Decode)?;
        }
    }
    if seeking {
        // `start_frame` was never reached: the stream ended (or every remaining packet's
        // timestamp stayed before it) without ever overlapping the target. Some demuxers (for
        // example symphonia-format-riff's WAV seek) accept `start_frame == total_frames` even
        // though there is nothing left to decode; refuse rather than silently return an empty,
        // "successful" decode (§4.2).
        return Err(DecoderError::Decode("Seek target is beyond the end of the stream".into()));
    }
    Ok(())
}

fn decode_wavpack(
    path: &Path,
    info: &AudioInfo,
    cancelled: &AtomicBool,
    preserve_integer: bool,
    start_frame: u64,
    output: &mut impl FnMut(PcmSample) -> Result<(), String>,
) -> Result<(), DecoderError> {
    let mut reader = if start_frame == 0 {
        WavpackReader::open(path)?.streaming().build()?
    } else {
        // Seeking requires random access, so the reader is opened without `.streaming()`.
        let mut reader = WavpackReader::open(path)?.build()?;
        if reader.seek_sample(start_frame as i64)? != 1 {
            return Err(DecoderError::Decode("WavPack seek failed".into()));
        }
        if reader.get_sample_index()? != start_frame as i64 {
            return Err(DecoderError::Decode("WavPack seek failed".into()));
        }
        reader
    };
    let channels = info.source_channels as usize;
    let preserve_integer = preserve_integer && info.integer_pcm;
    if preserve_integer && channels != 2 {
        return Err(DecoderError::Decode(
            "Strict integer output requires stereo PCM; mono-to-stereo conversion is not allowed.".into(),
        ));
    }
    let mut raw = vec![0i32; DECODE_CHUNK_FRAMES * channels];
    let scale = 2.0f64.powi(info.bits_per_sample.saturating_sub(1) as i32);
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Ok(());
        }
        let frames = reader.unpack_samples(&mut raw)? as usize;
        if frames == 0 {
            break;
        }
        let sample_count = frames * channels;
        if info.is_float {
            for frame in raw[..sample_count].chunks_exact(channels) {
                let left = f32::from_bits(frame[0] as u32);
                let right = if channels == 1 { left } else { f32::from_bits(frame[1] as u32) };
                output(PcmSample::Float(left)).map_err(DecoderError::Decode)?;
                output(PcmSample::Float(right)).map_err(DecoderError::Decode)?;
            }
        } else if preserve_integer {
            for sample in &raw[..sample_count] {
                let sample = left_justify_integer(*sample, info.bits_per_sample)?;
                output(PcmSample::Integer(sample)).map_err(DecoderError::Decode)?;
            }
        } else {
            for frame in raw[..sample_count].chunks_exact(channels) {
                let left = (frame[0] as f64 / scale) as f32;
                let right = if channels == 1 { left } else { (frame[1] as f64 / scale) as f32 };
                output(PcmSample::Float(left)).map_err(DecoderError::Decode)?;
                output(PcmSample::Float(right)).map_err(DecoderError::Decode)?;
            }
        }
    }
    Ok(())
}

fn validate_channels(channels: u16) -> Result<(), DecoderError> {
    if channels != 1 && channels != 2 {
        return Err(DecoderError::UnsupportedChannels(channels));
    }
    Ok(())
}

fn forward_float_interleaved(
    samples: &[f32],
    channels: u16,
    output: &mut impl FnMut(PcmSample) -> Result<(), String>,
) -> Result<(), String> {
    match channels {
        1 => {
            for sample in samples {
                output(PcmSample::Float(*sample))?;
                output(PcmSample::Float(*sample))?;
            }
        }
        2 => {
            for sample in samples {
                output(PcmSample::Float(*sample))?;
            }
        }
        _ => return Err(format!("Unsupported channel count: {channels}")),
    }
    Ok(())
}

fn left_justify_integer(sample: i32, bits: u32) -> Result<i32, DecoderError> {
    if !(1..=32).contains(&bits) {
        return Err(DecoderError::Decode(format!("Unsupported integer PCM depth: {bits} bits")));
    }
    if bits < 32 {
        let min = -(1i64 << (bits - 1));
        let max = (1i64 << (bits - 1)) - 1;
        if !(min..=max).contains(&i64::from(sample)) {
            return Err(DecoderError::Decode(format!("WavPack sample exceeds its declared {bits}-bit range")));
        }
    }
    Ok(sample.wrapping_shl(32 - bits))
}

fn validate_left_justified_sample(sample: i32, bits: u32) -> Result<(), DecoderError> {
    if !(1..=32).contains(&bits) {
        return Err(DecoderError::Decode(format!("Unsupported integer PCM depth: {bits} bits")));
    }
    if bits < 32 {
        let padding_mask = (1u32 << (32 - bits)) - 1;
        if sample as u32 & padding_mask != 0 {
            return Err(DecoderError::Decode(format!(
                "Integer decoder returned nonzero padding bits for {bits}-bit PCM"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_millis_uses_sample_frames_and_rejects_zero_rate() {
        assert_eq!(duration_millis_from_frames(44_100, 44_100), Some(1_000));
        assert_eq!(duration_millis_from_frames(88_200, 44_100), Some(2_000));
        assert_eq!(duration_millis_from_frames(0, 44_100), Some(0));
        assert_eq!(duration_millis_from_frames(1, 0), None);
    }
    use std::sync::atomic::AtomicBool;

    fn sample_magnitude(sample: PcmSample) -> f32 {
        match sample {
            PcmSample::Integer(value) => value as f32 / i32::MAX as f32,
            PcmSample::Float(value) => value,
        }
    }

    #[test]
    fn decodes_flac_fixture() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/decoder-tone.flac");
        let expected_info = probe_file(&path).unwrap();
        let mut decoded = Vec::new();
        let info = decode_file(&path, &expected_info, &AtomicBool::new(false), false, 0, |sample| {
            decoded.push(sample);
            Ok(())
        })
        .unwrap();

        assert_eq!(info.format, "FLAC");
        assert_eq!(info.sample_rate, 44_100);
        assert_eq!(info.source_channels, 1);
        assert_eq!(decoded.len(), 4410 * 2);
        assert!(decoded.iter().any(|sample| sample_magnitude(*sample).abs() > 0.1));
    }

    #[test]
    fn decodes_24_bit_flac_to_exact_strict_integer_words() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/strict-24-bit-reference.flac");
        let expected_info = probe_file(&path).unwrap();
        let mut decoded = Vec::new();
        let info = decode_file(&path, &expected_info, &AtomicBool::new(false), true, 0, |sample| {
            decoded.push(sample);
            Ok(())
        })
        .unwrap();

        assert_eq!(info.format, "FLAC");
        assert_eq!(info.sample_rate, 44_100);
        assert_eq!(info.source_channels, 2);
        assert_eq!(info.bits_per_sample, 24);
        assert!(info.integer_pcm);

        // The fixture is losslessly encoded from these known 24-bit interleaved samples.
        let mut expected: Vec<PcmSample> = [
            1, -1, 2, -2, 0x123456, -0x123456, 0x7fffff, -0x800000,
            0x0000ff, -0x0000ff, 0x654321, -0x654321,
        ]
        .into_iter()
        .map(|sample| PcmSample::Integer(sample << 8))
        .collect();
        expected.resize(64 * 2, PcmSample::Integer(0));
        assert_eq!(decoded, expected);
    }

    #[test]
    fn decodes_wav_fixture() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/decoder-tone.wav");
        let expected_info = probe_file(&path).unwrap();
        let mut decoded = Vec::new();
        let info = decode_file(&path, &expected_info, &AtomicBool::new(false), false, 0, |sample| {
            decoded.push(sample);
            Ok(())
        })
        .unwrap();

        assert_eq!(info.format, "WAV");
        assert_eq!(info.sample_rate, 44_100);
        assert_eq!(info.source_channels, 2);
        assert_eq!(decoded.len(), 4410 * 2);
        assert!(decoded.iter().any(|sample| sample_magnitude(*sample).abs() > 0.05));
    }

    #[test]
    fn decodes_mp3_fixture() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/decoder-tone.mp3");
        let expected_info = probe_file(&path).unwrap();
        let mut decoded = Vec::new();
        let info = decode_file(&path, &expected_info, &AtomicBool::new(false), false, 0, |sample| {
            decoded.push(sample);
            Ok(())
        })
        .unwrap();

        assert_eq!(info.format, "MP3");
        assert_eq!(info.sample_rate, 44_100);
        assert_eq!(info.source_channels, 2);
        assert!(!decoded.is_empty());
        assert!(decoded.iter().any(|sample| sample_magnitude(*sample).abs() > 0.05));
    }

    #[test]
    fn decodes_wavpack_fixture() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/decoder-tone.wv");
        let expected_info = probe_file(&path).unwrap();
        let mut decoded = Vec::new();
        let info = decode_file(&path, &expected_info, &AtomicBool::new(false), false, 0, |sample| {
            decoded.push(sample);
            Ok(())
        })
        .unwrap();

        assert_eq!(info.format, "WavPack");
        assert_eq!(info.sample_rate, 48_000);
        assert_eq!(info.source_channels, 2);
        assert_eq!(decoded.len(), 4800 * 2);
        assert!(decoded.iter().any(|sample| sample_magnitude(*sample).abs() > 0.05));
    }

    #[test]
    fn decodes_float_wavpack_fixture() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/decoder-tone-float.wv");
        let expected_info = probe_file(&path).unwrap();
        let mut decoded = Vec::new();
        let info = decode_file(&path, &expected_info, &AtomicBool::new(false), false, 0, |sample| {
            decoded.push(sample);
            Ok(())
        })
        .unwrap();

        assert_eq!(info.format, "WavPack");
        assert_eq!(info.sample_rate, 48_000);
        assert!(info.is_float);
        assert_eq!(info.source_channels, 2);
        assert!(!decoded.is_empty());
        assert!(decoded.iter().any(|sample| sample_magnitude(*sample).abs() > 0.05));
    }

    #[test]
    fn decodes_generated_wavpack_16_24_and_32_bit_words_exactly() {
        let test_cases: [(u32, Vec<i32>); 3] = [
            (16, vec![i16::MIN.into(), i16::MAX.into(), -1, 1]),
            (24, vec![-8_388_608, 8_388_607, -1, 1]),
            (32, vec![i32::MIN, i32::MAX, -1, 0x1234_5678]),
        ];
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        for (bits, mut input) in test_cases {
            let path = std::env::temp_dir().join(format!(
                "lime-player-wavpack-{bits}-{}-{suffix}.wv",
                std::process::id()
            ));
            let expected: Vec<PcmSample> = input
                .iter()
                .map(|sample| PcmSample::Integer(left_justify_integer(*sample, bits).unwrap()))
                .collect();
            {
                let mut writer = wavpack::WavpackWriter::create(&path)
                    .unwrap()
                    .add_bytes_per_sample((bits.div_ceil(8)) as i32)
                    .add_bits_per_sample(bits as i32)
                    .add_num_channels(2)
                    .add_channel_mask(3)
                    .add_sample_rate(44_100)
                    .build()
                    .unwrap();
                writer.pack_samples(&mut input).unwrap();
            }

            let expected_info = probe_file(&path).unwrap();
            let mut decoded = Vec::new();
            let info = decode_file(&path, &expected_info, &AtomicBool::new(false), true, 0, |sample| {
                decoded.push(sample);
                Ok(())
            })
            .unwrap();

            assert_eq!(info.format, "WavPack");
            assert_eq!(info.sample_rate, 44_100);
            assert!(info.duration_ms.is_some());
            assert_eq!(info.source_channels, 2);
            assert_eq!(info.bits_per_sample, bits);
            assert_eq!(decoded, expected, "failed exact sample mapping at {bits} bits");
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn decodes_integer_wav_to_left_justified_i32_samples() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/decoder-tone.wav");
        let expected_info = probe_file(&path).unwrap();
        let mut decoded = Vec::new();
        let info = decode_file(&path, &expected_info, &AtomicBool::new(false), true, 0, |sample| {
            decoded.push(sample);
            Ok(())
        })
        .unwrap();

        assert!(info.integer_pcm);
        assert_eq!(info.bits_per_sample, 16);
        assert!(info.duration_ms.is_some());
        assert!(!decoded.is_empty());
        for sample in decoded {
            let PcmSample::Integer(word) = sample else {
                panic!("integer WAV unexpectedly decoded to floating-point samples");
            };
            assert_eq!(word as u32 & 0xFFFF, 0);
        }
    }

    #[test]
    fn left_justifies_signed_16_24_and_32_bit_samples_exactly() {
        assert_eq!(left_justify_integer(1, 16).unwrap(), 0x0001_0000);
        assert_eq!(left_justify_integer(-1, 16).unwrap(), -0x0001_0000);
        assert_eq!(left_justify_integer(i16::MIN.into(), 16).unwrap(), i32::MIN);
        assert_eq!(left_justify_integer(i16::MAX.into(), 16).unwrap(), 0x7fff_0000);

        assert_eq!(left_justify_integer(1, 24).unwrap(), 0x0000_0100);
        assert_eq!(left_justify_integer(-1, 24).unwrap(), -0x0000_0100);
        assert_eq!(left_justify_integer(-8_388_608, 24).unwrap(), i32::MIN);
        assert_eq!(left_justify_integer(8_388_607, 24).unwrap(), 0x7fff_ff00);

        assert_eq!(left_justify_integer(0x1234_5678, 32).unwrap(), 0x1234_5678);
        assert_eq!(left_justify_integer(-0x1234_5678, 32).unwrap(), -0x1234_5678);
        assert_eq!(left_justify_integer(i32::MIN, 32).unwrap(), i32::MIN);
    }

    /// A NAS file replaced under the same path with a different sample rate must be refused, not
    /// decoded at the caller's now-stale expectation (`§1.4`, "refuse rather than silently
    /// degrade"): the output stream is already configured from `expected` by the time this runs.
    #[test]
    fn decode_refuses_when_the_file_no_longer_matches_the_expected_info() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/decoder-tone.flac");
        let mut stale_expected = probe_file(&path).unwrap();
        stale_expected.sample_rate += 1;

        let result = decode_file(&path, &stale_expected, &AtomicBool::new(false), false, 0, |_| Ok(()));

        match result {
            Err(DecoderError::Decode(message)) => {
                assert!(message.contains("changed since it was added"), "unexpected message: {message}");
            }
            other => panic!("expected a refusal for a stale AudioInfo, got {other:?}"),
        }
    }

    #[test]
    fn accepts_only_supported_extensions() {
        let err = probe_file(Path::new("track.ogg")).unwrap_err();
        assert!(matches!(err, DecoderError::UnsupportedFormat(_)));
    }

    #[test]
    fn rejects_wavpack_correction_files_explicitly() {
        let err = probe_file(Path::new("track.wvc")).unwrap_err();
        assert!(matches!(err, DecoderError::WavPackCorrectionUnsupported));
    }

    #[test]
    fn refuses_wavpack_track_with_correction_sidecar() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("lime-player-wavpack-sidecar-{suffix}.wv"));
        let sidecar = path.with_extension("wvc");
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/decoder-tone.wv");
        std::fs::copy(fixture, &path).unwrap();
        std::fs::write(&sidecar, b"correction data intentionally unavailable").unwrap();

        let err = probe_file(&path).unwrap_err();

        std::fs::remove_file(path).unwrap();
        std::fs::remove_file(sidecar).unwrap();
        assert!(matches!(err, DecoderError::WavPackCorrectionUnsupported));
    }

    /// Seeking discards leading source frames but must never alter the words that follow: for
    /// every `n` in `frame_counts`, `decode(n)` must equal `decode(0)`'s suffix starting at frame
    /// `n` (`§4.2`). Output is always stereo, so the sample offset is `n * 2`.
    fn assert_seek_matches_full_decode_suffix(path: &Path, preserve_integer: bool, frame_counts: &[u64]) {
        let expected_info = probe_file(path).unwrap();
        let mut full = Vec::new();
        decode_file(path, &expected_info, &AtomicBool::new(false), preserve_integer, 0, |sample| {
            full.push(sample);
            Ok(())
        })
        .unwrap();

        for &n in frame_counts {
            let mut seeked = Vec::new();
            decode_file(path, &expected_info, &AtomicBool::new(false), preserve_integer, n, |sample| {
                seeked.push(sample);
                Ok(())
            })
            .unwrap();
            let offset = n as usize * 2;
            assert_eq!(
                seeked,
                full[offset..],
                "seeking to frame {n} in {path:?} did not match the full-decode suffix"
            );
        }
    }

    #[test]
    fn seek_start_frame_matches_full_decode_suffix_flac_integer() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/strict-24-bit-reference.flac");
        assert_seek_matches_full_decode_suffix(&path, true, &[0, 1, 5, 63]);
    }

    #[test]
    fn seek_start_frame_matches_full_decode_suffix_flac_multi_frame() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/decoder-tone.flac");
        assert_seek_matches_full_decode_suffix(&path, false, &[1, 4095, 4096, 4100, 4409]);
    }

    #[test]
    fn seek_start_frame_matches_full_decode_suffix_wav_integer() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/decoder-tone.wav");
        assert_seek_matches_full_decode_suffix(&path, true, &[1, 2_205, 4_409]);
    }

    #[test]
    fn seek_start_frame_matches_full_decode_suffix_generated_wavpack() {
        let total_frames: i32 = 100_000;
        let mut samples = Vec::with_capacity(total_frames as usize * 2);
        for i in 0..total_frames {
            let left = i - 50_000; // small, unique-per-frame values well within 24-bit range
            samples.push(left);
            samples.push(-left);
        }
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir()
            .join(format!("lime-player-wavpack-seek-{}-{suffix}.wv", std::process::id()));
        {
            let mut writer = wavpack::WavpackWriter::create(&path)
                .unwrap()
                .add_bytes_per_sample(3)
                .add_bits_per_sample(24)
                .add_num_channels(2)
                .add_channel_mask(3)
                .add_sample_rate(44_100)
                .build()
                .unwrap();
            writer.pack_samples(&mut samples).unwrap();
        }

        assert_seek_matches_full_decode_suffix(&path, true, &[1, 44_099, 50_000, 99_999]);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn wavpack_seek_beyond_end_is_refused() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/decoder-tone.wv");
        let expected_info = probe_file(&path).unwrap();
        // The fixture decodes to 4800 frames (`decodes_wavpack_fixture`); seeking at the last
        // frame must be refused outright, never silently clamped and never a panic.
        let result = decode_file(&path, &expected_info, &AtomicBool::new(false), true, 4_800, |_| Ok(()));
        assert!(result.is_err(), "seeking to/past the last frame must be refused, got {result:?}");
    }

    #[test]
    fn symphonia_seek_at_end_is_refused() {
        // Both fixtures decode to 4410 frames (`decodes_flac_fixture`, `decodes_wav_fixture`).
        // symphonia-format-riff's WAV seek only refuses `start_frame > total_frames`, so
        // `start_frame == total_frames` must be caught after the packet loop instead (§4.2).
        let wav = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/decoder-tone.wav");
        let flac = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/decoder-tone.flac");
        for path in [&wav, &flac] {
            let expected_info = probe_file(path).unwrap();
            let result = decode_file(path, &expected_info, &AtomicBool::new(false), false, 4_410, |_| Ok(()));
            assert!(result.is_err(), "seeking to the end of {path:?} must be refused, got {result:?}");
        }
    }

    #[test]
    fn mp3_seek_decodes_without_error_or_refuses_cleanly() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/decoder-tone.mp3");
        let expected_info = probe_file(&path).unwrap();
        let result = decode_file(&path, &expected_info, &AtomicBool::new(false), false, 200, |_| Ok(()));
        match result {
            Ok(_) => {}
            Err(DecoderError::Decode(message)) => {
                assert!(message.contains("Seek landed past"), "unexpected decode error: {message}");
            }
            Err(other) => panic!("expected a clean decode-error refusal, got {other:?}"),
        }
    }
}

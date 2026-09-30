//! Repair of FLAC files whose sample rate no audio device accepts (a PS1/XA rip at 37 800 Hz is the
//! motivating case).
//!
//! The strict integer route asks CoreAudio for the file's exact rate and refuses playback when the
//! device cannot provide it (`player::exact_output_config`, "no hidden sample-rate fallback"). A
//! device only offers a handful of rates, so a file at an unusual rate can never play. This module
//! rewrites such a file once, on disk, to the nearest rate devices actually offer, and leaves the
//! playback path untouched.
//!
//! The rewrite is deliberately non-destructive and all-or-nothing (`repair_flac_sample_rate`):
//!
//! 1. Everything is prepared in a temp file next to the original (same volume, so the final rename
//!    is atomic): decode -> resample (`rubato`, synchronous FFT) -> FLAC encode (`flacenc`).
//! 2. The temp file is verified by decoding it back: expected rate, channels, bit depth, duration
//!    and byte-identical kept metadata blocks.
//! 3. Only then is the original copied into the backup directory (the copy is flushed to stable
//!    storage too, so the rename never runs ahead of a durable backup) and replaced by the rename.
//!
//! Any failure before the rename leaves the original untouched. Tags and embedded artwork are kept
//! by copying the original's `VORBIS_COMMENT`/`PICTURE`/`APPLICATION` metadata blocks verbatim into
//! the new file, so nothing is re-encoded or normalized. A file with an embedded `CUESHEET` block is
//! refused instead (its offsets are in samples of the old rate and would need rescaling).

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use audioadapter_buffers::direct::InterleavedSlice;
use flacenc::bitsink::ByteSink;
use flacenc::component::BitRepr;
use flacenc::config::Encoder as EncoderConfig;
use flacenc::error::{SourceError, Verify};
use flacenc::source::{Fill, Source};
use rubato::{Fft, FixedSync, Indexing, Resampler};
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::formats::FormatReader;

use super::decoder::{AudioInfo, DecoderError, open_symphonia, probe_file};

/// Sample rates treated as standard: a file at any other rate is a repair candidate.
pub const STANDARD_SAMPLE_RATES: [u32; 15] = [
    8_000, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000, 88_200, 96_000, 176_400,
    192_000, 352_800, 384_000,
];

/// The rates a repaired file may end up at: the subset of the standard rates that real output
/// devices offer for stereo playback (a repaired file must be playable, not merely "standard").
const REPAIR_TARGET_RATES: [u32; 8] = [
    44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000,
];

/// Frames handed to the resampler per call, and FLAC block size of the re-encoded file.
const RESAMPLE_CHUNK_FRAMES: usize = 4_096;
const FLAC_BLOCK_SIZE: usize = 4_096;

const FLAC_MAGIC: &[u8; 4] = b"fLaC";
const BLOCK_STREAMINFO: u8 = 0;
const BLOCK_PADDING: u8 = 1;
const BLOCK_SEEKTABLE: u8 = 3;
const BLOCK_CUESHEET: u8 = 5;
const BLOCK_INVALID: u8 = 127;
const STREAMINFO_LEN: usize = 34;
/// The output duration may differ from `input * ratio` by this many frames (or 1 ms, whichever is
/// larger) before verification fails: the resampler produces `round(input * ratio)` frames, so this
/// only guards against truncated or padded output.
const DURATION_TOLERANCE_FRAMES: u64 = 2;

pub fn is_standard_sample_rate(rate: u32) -> bool {
    STANDARD_SAMPLE_RATES.contains(&rate)
}

/// Whether `info` describes a file this module repairs: FLAC at a rate outside `STANDARD_SAMPLE_RATES`.
/// Other formats (WAV, MP3, WavPack) are intentionally left alone; see `CLAUDE.md`.
pub fn needs_rate_repair(info: &AudioInfo) -> bool {
    info.format == "FLAC" && info.sample_rate != 0 && !is_standard_sample_rate(info.sample_rate)
}

/// The rate a file at `rate` is converted to: the nearest (in ratio terms) of `REPAIR_TARGET_RATES`,
/// preferring the higher one on a tie. `None` for rate 0 or a rate that is already standard.
pub fn repair_target_rate(rate: u32) -> Option<u32> {
    if rate == 0 || is_standard_sample_rate(rate) {
        return None;
    }
    let distance = |candidate: u32| (f64::from(candidate) / f64::from(rate)).ln().abs();
    REPAIR_TARGET_RATES
        .iter()
        .copied()
        .min_by(|a, b| distance(*a).total_cmp(&distance(*b)).then(b.cmp(a)))
}

#[derive(Clone, Debug)]
pub struct RepairOptions {
    /// Where the untouched original is copied before it is replaced.
    pub backup_dir: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepairReport {
    pub from_rate: u32,
    pub to_rate: u32,
    pub backup: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum RepairError {
    /// The file is fine as it is (not FLAC, or already at a standard rate): not a failure.
    #[error("no sample-rate repair needed")]
    NotNeeded,
    #[error("cannot repair this file: {0}")]
    Unsupported(String),
    #[error("{0}")]
    NotWritable(String),
    #[error("{context}: {source}")]
    Io {
        context: &'static str,
        source: io::Error,
    },
    #[error("decoding failed: {0}")]
    Decode(String),
    #[error("encoding failed: {0}")]
    Encode(String),
    #[error("verification of the repaired file failed: {0}")]
    Verification(String),
}

impl RepairError {
    fn io(context: &'static str) -> impl FnOnce(io::Error) -> Self {
        move |source| Self::Io { context, source }
    }
}

/// Converts the FLAC at `path` to a rate output devices accept, keeping its tags and artwork, after
/// backing the original up into `options.backup_dir`. See the module docs for the guarantees.
pub fn repair_flac_sample_rate(
    path: &Path,
    options: &RepairOptions,
) -> Result<RepairReport, RepairError> {
    // Operate on the real file: renaming over a symlink would replace the link, not its target.
    let path =
        fs::canonicalize(path).map_err(RepairError::io("could not resolve the file path"))?;
    let info = probe_file(&path).map_err(|error| RepairError::Decode(error.to_string()))?;
    if !needs_rate_repair(&info) {
        return Err(RepairError::NotNeeded);
    }
    let to_rate = repair_target_rate(info.sample_rate).ok_or(RepairError::NotNeeded)?;
    if !info.integer_pcm || info.is_float || !(1..=2).contains(&info.source_channels) {
        return Err(RepairError::Unsupported(format!(
            "only mono/stereo integer FLAC is supported (found {} channels)",
            info.source_channels
        )));
    }
    if !(8..=24).contains(&info.bits_per_sample) {
        return Err(RepairError::Unsupported(format!(
            "{}-bit samples are not supported",
            info.bits_per_sample
        )));
    }

    let kept_blocks = read_kept_metadata_blocks(&path)?;

    let original_permissions = fs::metadata(&path)
        .map_err(RepairError::io("could not read the file's permissions"))?
        .permissions();
    if original_permissions.readonly() {
        return Err(RepairError::NotWritable("the file is read-only".into()));
    }
    let mut temp = TempFile::create_beside(&path)?;

    let frames_in = encode_resampled(&path, &info, to_rate, &kept_blocks, temp.file_mut())?;
    flush_durably(temp.file_mut()).map_err(flush_error("could not flush the repaired file"))?;
    verify_repaired(temp.path(), &info, to_rate, frames_in, &kept_blocks)?;

    let backup = backup_original(&path, info.sample_rate, &options.backup_dir)?;

    fs::set_permissions(temp.path(), original_permissions)
        .map_err(RepairError::io("could not copy the file's permissions"))?;
    temp.replace(&path)?;
    Ok(RepairReport {
        from_rate: info.sample_rate,
        to_rate,
        backup,
    })
}

// ---------------------------------------------------------------------------------------------
// Temp file
// ---------------------------------------------------------------------------------------------

/// A hidden temp file in the original's directory, deleted on drop unless `replace` consumed it.
/// Being on the same volume as the original is what makes the final rename atomic. A read-only
/// volume or directory fails right here, before any expensive work.
struct TempFile {
    path: PathBuf,
    file: Option<File>,
    armed: bool,
}

impl TempFile {
    fn create_beside(original: &Path) -> Result<Self, RepairError> {
        let dir = original
            .parent()
            .ok_or_else(|| RepairError::NotWritable("the file has no parent directory".into()))?;
        let stem = original
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        let extension = original
            .extension()
            .map(|ext| ext.to_string_lossy().into_owned())
            .unwrap_or_else(|| "flac".into());
        // Hidden (the folder walk skips it) yet still ending in `.flac`, which `probe_file` needs to
        // recognize the format when verifying the temp file.
        let path = dir.join(format!(".lime-rate-repair-{stem}.{extension}"));
        let open = || OpenOptions::new().write(true).create_new(true).open(&path);
        let file = match open() {
            Ok(file) => file,
            // A leftover from an interrupted run (the name is ours): replace it.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                fs::remove_file(&path)
                    .map_err(RepairError::io("could not remove a stale temp file"))?;
                open().map_err(|error| {
                    not_writable("could not create a temp file next to the original", &error)
                })?
            }
            Err(error) => {
                return Err(not_writable(
                    "could not create a temp file next to the original",
                    &error,
                ));
            }
        };
        Ok(Self {
            path,
            file: Some(file),
            armed: true,
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn file_mut(&mut self) -> &mut File {
        self.file
            .as_mut()
            .expect("the temp file stays open until replace")
    }

    /// Atomically moves the temp file over `target`.
    fn replace(mut self, target: &Path) -> Result<(), RepairError> {
        self.file = None;
        fs::rename(&self.path, target)
            .map_err(RepairError::io("could not replace the original file"))?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.armed {
            self.file = None;
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn not_writable(context: &str, error: &io::Error) -> RepairError {
    RepairError::NotWritable(format!("{context}: {error}"))
}

// ---------------------------------------------------------------------------------------------
// Durable flush
// ---------------------------------------------------------------------------------------------

/// Flushes `file` to stable storage as durably as its filesystem allows: the temp file before it is
/// verified and swapped in for the original, and the backup before the original is replaced.
fn flush_durably(file: &File) -> io::Result<()> {
    flush_with_fallback(|| file.sync_all(), || plain_fsync(file))
}

/// Flushes the directory `dir`, so entries created or renamed in it (the backup's name) survive a
/// crash as well as the file's contents do. Directories cannot be opened for flushing off unix.
#[cfg(unix)]
fn flush_directory(dir: &Path) -> io::Result<()> {
    flush_durably(&File::open(dir)?)
}

#[cfg(not(unix))]
fn flush_directory(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Flushes the file at `path`. On unix `fsync` needs no write access, so a read-only backup is fine.
fn flush_path_durably(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    let file = File::open(path)?;
    #[cfg(not(unix))]
    let file = OpenOptions::new().write(true).open(path)?;
    flush_durably(&file)
}

/// `full` is the strongest flush available (`File::sync_all`: `F_FULLFSYNC` on macOS, `fsync(2)`
/// elsewhere) and `plain` is ordinary `fsync(2)`. macOS smbfs does not implement `F_FULLFSYNC` and
/// answers ENOTSUP, so a NAS share failed every repair at this step; a filesystem that cannot do the
/// strong flush is retried with the weaker one. Any other failure (EIO, ENOSPC, EDQUOT, ...) is a
/// real write problem and fails the repair.
///
/// If the filesystem supports neither, the flush fails with that "not supported" error and the
/// repair is refused (see `flush_error`): the flush is the only place a deferred write error can
/// still surface (a descriptor's `close` error is dropped when `File` goes away), and
/// `verify_repaired` cannot stand in for it because it re-reads the file through this client's own
/// cache, not from what the server stored. Refusing costs a repair on a volume that has no flush at
/// all, which is rare; proceeding could put a truncated file over the original.
fn flush_with_fallback(
    full: impl FnOnce() -> io::Result<()>,
    plain: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    match full() {
        Err(error) if is_flush_unsupported(&error) => plain(),
        result => result,
    }
}

/// Maps a failed flush to a `RepairError`, spelling out the volume that cannot flush at all.
fn flush_error(context: &'static str) -> impl FnOnce(io::Error) -> RepairError {
    move |source| {
        if is_flush_unsupported(&source) {
            RepairError::Unsupported(format!(
                "{context}: the volume supports neither F_FULLFSYNC nor fsync, so the data cannot be \
                 made durable before the original is replaced ({source})"
            ))
        } else {
            RepairError::Io { context, source }
        }
    }
}

/// Whether a flush failed only because the filesystem does not implement it. Both spellings are
/// checked because macOS defines ENOTSUP (what smbfs returns) and EOPNOTSUPP as different values.
#[cfg(unix)]
fn is_flush_unsupported(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code) if code == libc::ENOTSUP || code == libc::EOPNOTSUPP
    )
}

#[cfg(not(unix))]
fn is_flush_unsupported(_error: &io::Error) -> bool {
    false
}

/// Ordinary `fsync(2)`. `File::sync_all` cannot stand in for it on macOS, where it is
/// `F_FULLFSYNC`, which is exactly the call smbfs rejects.
#[cfg(unix)]
fn plain_fsync(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    loop {
        // SAFETY: the descriptor is owned by `file`, which outlives the call.
        if unsafe { libc::fsync(file.as_raw_fd()) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(not(unix))]
fn plain_fsync(file: &File) -> io::Result<()> {
    file.sync_all()
}

// ---------------------------------------------------------------------------------------------
// FLAC metadata blocks
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
struct MetadataBlock {
    kind: u8,
    data: Vec<u8>,
}

fn read_metadata_blocks(reader: &mut impl Read) -> Result<Vec<MetadataBlock>, RepairError> {
    let mut magic = [0u8; 4];
    reader
        .read_exact(&mut magic)
        .map_err(RepairError::io("could not read the FLAC header"))?;
    if &magic != FLAC_MAGIC {
        return Err(RepairError::Unsupported(
            "the file does not start with a plain FLAC header (leading ID3v2 tag?)".into(),
        ));
    }
    let mut blocks = Vec::new();
    loop {
        let mut header = [0u8; 4];
        reader.read_exact(&mut header).map_err(RepairError::io(
            "could not read a FLAC metadata block header",
        ))?;
        let is_last = header[0] & 0x80 != 0;
        let kind = header[0] & 0x7f;
        let length = u32::from_be_bytes([0, header[1], header[2], header[3]]) as usize;
        if kind == BLOCK_INVALID {
            return Err(RepairError::Unsupported(
                "invalid FLAC metadata block type".into(),
            ));
        }
        let mut data = vec![0u8; length];
        reader
            .read_exact(&mut data)
            .map_err(RepairError::io("could not read a FLAC metadata block"))?;
        blocks.push(MetadataBlock { kind, data });
        if is_last {
            return Ok(blocks);
        }
    }
}

/// The original's metadata blocks that survive the rewrite verbatim: everything except the stream
/// info (regenerated), padding and the seek table (both refer to the old stream). An embedded
/// `CUESHEET` cannot be carried over unchanged, so it refuses the repair.
fn read_kept_metadata_blocks(path: &Path) -> Result<Vec<MetadataBlock>, RepairError> {
    let file = File::open(path).map_err(RepairError::io("could not open the file"))?;
    let blocks = read_metadata_blocks(&mut BufReader::new(file))?;
    if blocks.iter().any(|block| block.kind == BLOCK_CUESHEET) {
        return Err(RepairError::Unsupported(
            "the file has an embedded CUESHEET block whose offsets would need rescaling".into(),
        ));
    }
    Ok(blocks
        .into_iter()
        .filter(|block| {
            !matches!(
                block.kind,
                BLOCK_STREAMINFO | BLOCK_PADDING | BLOCK_SEEKTABLE
            )
        })
        .collect())
}

fn write_block(out: &mut impl Write, kind: u8, data: &[u8], is_last: bool) -> io::Result<()> {
    let length = u32::try_from(data.len())
        .ok()
        .filter(|length| *length < (1 << 24))
        .ok_or_else(|| io::Error::other("metadata block too large"))?;
    let flag = if is_last { 0x80 } else { 0 };
    out.write_all(&[flag | kind])?;
    out.write_all(&length.to_be_bytes()[1..])?;
    out.write_all(data)
}

/// Splits `flacenc`'s serialized stream into its `STREAMINFO` payload and the byte offset where the
/// audio frames start.
///
/// The payload's minimum block size is set to the maximum one. `flacenc` records the shorter final
/// block there, but the FLAC format defines the minimum as excluding the last block (the reference
/// encoder writes min == max for fixed-size streams), and Symphonia — the decoder this app plays
/// with — only accepts fixed-size frames when the two are equal, so it would otherwise refuse to
/// even open the repaired file.
fn split_encoded_stream(bytes: &[u8]) -> Result<([u8; STREAMINFO_LEN], usize), RepairError> {
    let bad = || RepairError::Encode("the encoder produced an unexpected stream layout".into());
    let mut cursor = io::Cursor::new(bytes);
    let mut magic = [0u8; 4];
    cursor.read_exact(&mut magic).map_err(|_| bad())?;
    if &magic != FLAC_MAGIC {
        return Err(bad());
    }
    let mut stream_info = None;
    loop {
        let mut header = [0u8; 4];
        cursor.read_exact(&mut header).map_err(|_| bad())?;
        let length = u32::from_be_bytes([0, header[1], header[2], header[3]]) as usize;
        let mut data = vec![0u8; length];
        cursor.read_exact(&mut data).map_err(|_| bad())?;
        if header[0] & 0x7f == BLOCK_STREAMINFO {
            stream_info =
                Some(<[u8; STREAMINFO_LEN]>::try_from(data.as_slice()).map_err(|_| bad())?);
        }
        if header[0] & 0x80 != 0 {
            break;
        }
    }
    let offset = usize::try_from(cursor.position()).map_err(|_| bad())?;
    let mut stream_info = stream_info.ok_or_else(bad)?;
    stream_info.copy_within(2..4, 0);
    Ok((stream_info, offset))
}

// ---------------------------------------------------------------------------------------------
// Decode -> resample -> encode
// ---------------------------------------------------------------------------------------------

/// Decodes `path`, resamples it to `to_rate`, FLAC-encodes it and writes the complete new file
/// (`kept_blocks` carried over) to `out`. Returns the number of source frames that were decoded.
fn encode_resampled(
    path: &Path,
    info: &AudioInfo,
    to_rate: u32,
    kept_blocks: &[MetadataBlock],
    out: &mut File,
) -> Result<u64, RepairError> {
    let mut source = ResamplingSource::open(path, info, to_rate)?;
    let config = EncoderConfig::default()
        .into_verified()
        .map_err(|(_, error)| RepairError::Encode(error.to_string()))?;
    let stream = flacenc::encode_with_fixed_block_size(&config, &mut source, FLAC_BLOCK_SIZE);
    if let Some(error) = source.error.take() {
        return Err(RepairError::Decode(error));
    }
    let stream = stream.map_err(|error| RepairError::Encode(error.to_string()))?;
    let mut sink = ByteSink::new();
    stream
        .write(&mut sink)
        .map_err(|error| RepairError::Encode(error.to_string()))?;
    let encoded = sink.as_slice();
    let (stream_info, frames_offset) = split_encoded_stream(encoded)?;

    let write = |out: &mut File| -> io::Result<()> {
        out.write_all(FLAC_MAGIC)?;
        write_block(out, BLOCK_STREAMINFO, &stream_info, kept_blocks.is_empty())?;
        for (index, block) in kept_blocks.iter().enumerate() {
            write_block(out, block.kind, &block.data, index + 1 == kept_blocks.len())?;
        }
        out.write_all(&encoded[frames_offset..])
    };
    write(out).map_err(RepairError::io("could not write the repaired file"))?;
    Ok(source.frames_in)
}

/// Pulls decoded PCM from a FLAC file, resamples it in fixed chunks and serves it to `flacenc`
/// block by block, so memory stays bounded by the chunk size rather than the track length.
struct ResamplingSource {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    channels: usize,
    bits: usize,
    from_rate: u32,
    to_rate: u32,
    resampler: Fft<f64>,
    /// Interleaved input not yet consumed by the resampler.
    input: Vec<f64>,
    /// Scratch for the resampler's interleaved output.
    output: Vec<f64>,
    /// Quantized, ready-to-encode interleaved samples.
    ready: VecDeque<i32>,
    decode_scratch: Vec<i32>,
    block_scratch: Vec<i32>,
    /// Output frames still to drop: the resampler's start-up delay.
    frames_to_trim: usize,
    frames_in: u64,
    frames_out: u64,
    /// Set once the input ended: the exact number of frames the output must have.
    expected_out: Option<u64>,
    input_done: bool,
    finished: bool,
    error: Option<String>,
}

impl ResamplingSource {
    fn open(path: &Path, info: &AudioInfo, to_rate: u32) -> Result<Self, RepairError> {
        let (format, track_id, codec_params) =
            open_symphonia(path).map_err(|error| RepairError::Decode(error.to_string()))?;
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(&codec_params, &AudioDecoderOptions::default())
            .map_err(|error| RepairError::Decode(error.to_string()))?;
        let channels = usize::from(info.source_channels);
        let resampler = Fft::<f64>::new(
            info.sample_rate as usize,
            to_rate as usize,
            RESAMPLE_CHUNK_FRAMES,
            channels,
            FixedSync::Input,
        )
        .map_err(|error| RepairError::Encode(format!("could not create the resampler: {error}")))?;
        let frames_to_trim = resampler.output_delay();
        let output = vec![0.0; resampler.output_frames_max() * channels];
        Ok(Self {
            format,
            decoder,
            track_id,
            channels,
            bits: info.bits_per_sample as usize,
            from_rate: info.sample_rate,
            to_rate,
            resampler,
            input: Vec::new(),
            output,
            ready: VecDeque::new(),
            decode_scratch: Vec::new(),
            block_scratch: Vec::new(),
            frames_to_trim,
            frames_in: 0,
            frames_out: 0,
            expected_out: None,
            input_done: false,
            finished: false,
            error: None,
        })
    }

    /// Decodes one more packet into `input`; marks the input finished at end of stream. Any decode
    /// error is fatal: a repaired file must never silently lose audio.
    fn decode_next_packet(&mut self) -> Result<(), String> {
        let packet = match self.format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => {
                self.input_done = true;
                self.expected_out =
                    Some(scaled_frames(self.frames_in, self.to_rate, self.from_rate));
                return Ok(());
            }
            Err(error) => return Err(error.to_string()),
        };
        if packet.track_id != self.track_id {
            return Ok(());
        }
        let decoded = self
            .decoder
            .decode(&packet)
            .map_err(|error| error.to_string())?;
        if decoded.spec().channels().count() != self.channels {
            return Err("the channel count changed mid-stream".into());
        }
        self.decode_scratch.resize(decoded.samples_interleaved(), 0);
        decoded.copy_to_slice_interleaved(&mut self.decode_scratch);
        // Symphonia hands integers back left-justified in an i32, whatever the source bit depth.
        self.input.extend(
            self.decode_scratch
                .iter()
                .map(|sample| f64::from(*sample) / 2_147_483_648.0),
        );
        self.frames_in += (self.decode_scratch.len() / self.channels) as u64;
        Ok(())
    }

    /// Runs the resampler once over the next chunk (the whole `input_frames_next()` window, zero-padded
    /// when the input ended) and moves its trimmed, quantized output into `ready`.
    fn resample_chunk(&mut self, valid_frames: usize) -> Result<(), String> {
        let needed = self.resampler.input_frames_next();
        let mut chunk: Vec<f64> = self.input.drain(..valid_frames * self.channels).collect();
        chunk.resize(needed * self.channels, 0.0);
        let indexing = Indexing {
            input_offset: 0,
            output_offset: 0,
            active_channels_mask: None,
            partial_len: (valid_frames < needed).then_some(valid_frames),
        };
        let input_adapter = InterleavedSlice::new(&chunk, self.channels, needed)
            .map_err(|error| error.to_string())?;
        let capacity = self.output.len() / self.channels;
        let mut output_adapter =
            InterleavedSlice::new_mut(&mut self.output, self.channels, capacity)
                .map_err(|error| error.to_string())?;
        let (_, produced) = self
            .resampler
            .process_into_buffer(&input_adapter, &mut output_adapter, Some(&indexing))
            .map_err(|error| error.to_string())?;

        let skip = self.frames_to_trim.min(produced);
        self.frames_to_trim -= skip;
        let mut emit = produced - skip;
        if let Some(expected) = self.expected_out {
            emit = emit.min(expected.saturating_sub(self.frames_out) as usize);
        }
        let scale = f64::from(1u32 << (self.bits - 1));
        let (low, high) = (-scale, scale - 1.0);
        let start = skip * self.channels;
        self.ready.extend(
            self.output[start..start + emit * self.channels]
                .iter()
                .map(|value| (value * scale).round().clamp(low, high) as i32),
        );
        self.frames_out += emit as u64;
        Ok(())
    }

    /// Produces resampled samples until at least `frames` frames are ready or the stream is done.
    fn fill(&mut self, frames: usize) -> Result<(), String> {
        while self.ready.len() < frames * self.channels && !self.finished {
            let needed = self.resampler.input_frames_next();
            if !self.input_done {
                if self.input.len() >= needed * self.channels {
                    self.resample_chunk(needed)?;
                } else {
                    self.decode_next_packet()?;
                }
                continue;
            }
            // Input ended: flush the partial tail, then pump silence until the output reaches its
            // exact expected length (the resampler's delay is still draining out).
            let expected = self.expected_out.unwrap_or(0);
            if self.frames_out >= expected {
                self.finished = true;
                break;
            }
            let tail = (self.input.len() / self.channels).min(needed);
            self.resample_chunk(tail)?;
        }
        Ok(())
    }
}

/// `round(frames * numerator / denominator)`.
fn scaled_frames(frames: u64, numerator: u32, denominator: u32) -> u64 {
    let denominator = u128::from(denominator);
    ((u128::from(frames) * u128::from(numerator) + denominator / 2) / denominator) as u64
}

impl Source for ResamplingSource {
    fn channels(&self) -> usize {
        self.channels
    }

    fn bits_per_sample(&self) -> usize {
        self.bits
    }

    fn sample_rate(&self) -> usize {
        self.to_rate as usize
    }

    fn read_samples<F: Fill>(
        &mut self,
        block_size: usize,
        dest: &mut F,
    ) -> Result<usize, SourceError> {
        if let Err(error) = self.fill(block_size) {
            self.error = Some(error.clone());
            return Err(SourceError::from_io_error(io::Error::other(error)));
        }
        let frames = (self.ready.len() / self.channels).min(block_size);
        if frames == 0 {
            return Ok(0);
        }
        self.block_scratch.clear();
        self.block_scratch
            .extend(self.ready.drain(..frames * self.channels));
        dest.fill_interleaved(&self.block_scratch)?;
        Ok(frames)
    }
}

// ---------------------------------------------------------------------------------------------
// Verification and backup
// ---------------------------------------------------------------------------------------------

/// Decodes the finished temp file end to end and checks it against what the repair promised. The
/// original is only replaced when this passes.
fn verify_repaired(
    repaired: &Path,
    original: &AudioInfo,
    to_rate: u32,
    frames_in: u64,
    kept_blocks: &[MetadataBlock],
) -> Result<(), RepairError> {
    let fail = |message: String| Err(RepairError::Verification(message));
    let info =
        probe_file(repaired).map_err(|error| RepairError::Verification(error.to_string()))?;
    if info.format != "FLAC" || info.sample_rate != to_rate {
        return fail(format!(
            "expected FLAC at {to_rate} Hz, found {} at {} Hz",
            info.format, info.sample_rate
        ));
    }
    if info.source_channels != original.source_channels {
        return fail(format!(
            "channel count changed from {} to {}",
            original.source_channels, info.source_channels
        ));
    }
    if info.bits_per_sample > original.bits_per_sample || info.bits_per_sample == 0 {
        return fail(format!(
            "bit depth changed from {} to {}",
            original.bits_per_sample, info.bits_per_sample
        ));
    }

    // The source itself must have decoded to the length its header declared.
    if let Some(declared_ms) = original.duration_ms {
        let decoded_ms = frames_in * 1_000 / u64::from(original.sample_rate);
        if decoded_ms.abs_diff(declared_ms) > 2 {
            return fail(format!(
                "the original decoded to {decoded_ms} ms but declares {declared_ms} ms"
            ));
        }
    }

    let frames_out = count_decodable_frames(repaired)
        .map_err(|error| RepairError::Verification(error.to_string()))?;
    let expected = scaled_frames(frames_in, to_rate, original.sample_rate);
    let tolerance = DURATION_TOLERANCE_FRAMES.max(u64::from(to_rate) / 1_000);
    if frames_out.abs_diff(expected) > tolerance {
        return fail(format!(
            "expected about {expected} frames, decoded {frames_out}"
        ));
    }

    let file =
        File::open(repaired).map_err(|error| RepairError::Verification(error.to_string()))?;
    let written = read_metadata_blocks(&mut BufReader::new(file))?;
    let written_kept: Vec<&MetadataBlock> = written
        .iter()
        .filter(|block| block.kind != BLOCK_STREAMINFO)
        .collect();
    if written_kept.len() != kept_blocks.len()
        || written_kept
            .iter()
            .zip(kept_blocks)
            .any(|(written, kept)| *written != kept)
    {
        return fail("the tags or artwork were not carried over unchanged".into());
    }
    Ok(())
}

/// Decodes every packet of a FLAC file, failing on the first error, and returns the frame count.
fn count_decodable_frames(path: &Path) -> Result<u64, DecoderError> {
    let (mut format, track_id, codec_params) = open_symphonia(path)?;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&codec_params, &AudioDecoderOptions::default())
        .map_err(|error| DecoderError::Decode(error.to_string()))?;
    let mut frames = 0u64;
    while let Some(packet) = format
        .next_packet()
        .map_err(|error| DecoderError::Decode(error.to_string()))?
    {
        if packet.track_id != track_id {
            continue;
        }
        let decoded = decoder
            .decode(&packet)
            .map_err(|error| DecoderError::Decode(error.to_string()))?;
        frames += (decoded.samples_interleaved() / decoded.spec().channels().count().max(1)) as u64;
    }
    Ok(frames)
}

/// Copies `original` into `backup_dir` as `<stem>.original-<rate>-<hash>.<ext>` and returns the
/// backup's path. The hash of the file's full path keeps same-named tracks from different folders
/// apart; an identical backup that already exists (an earlier interrupted run) is reused.
///
/// The backup is flushed to stable storage (its contents and its directory entry, a reused one
/// too: an interrupted run may have left it only in the page cache) before this returns, because
/// the caller renames the repaired file over the original right after, and the rename on a network
/// volume is committed by the server at once. A power loss then cannot leave a replaced original
/// next to a backup that never reached the disk.
fn backup_original(
    original: &Path,
    original_rate: u32,
    backup_dir: &Path,
) -> Result<PathBuf, RepairError> {
    fs::create_dir_all(backup_dir)
        .map_err(|error| not_writable("could not create the backup directory", &error))?;
    let stem = original
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "track".into());
    let extension = original
        .extension()
        .map(|ext| ext.to_string_lossy().into_owned())
        .unwrap_or_else(|| "flac".into());
    let hash = path_hash(original);
    let mut attempt = 0u32;
    let backup = loop {
        let suffix = if attempt == 0 {
            String::new()
        } else {
            format!("-{attempt}")
        };
        let candidate = backup_dir.join(format!(
            "{stem}.original-{original_rate}-{hash:08x}{suffix}.{extension}"
        ));
        if !candidate.exists() {
            let partial = candidate.with_extension(format!("{extension}.partial"));
            fs::copy(original, &partial)
                .map_err(|error| not_writable("could not back up the original file", &error))?;
            fs::rename(&partial, &candidate)
                .map_err(|error| not_writable("could not finish the backup", &error))?;
            break candidate;
        }
        if files_identical(original, &candidate).unwrap_or(false) {
            break candidate;
        }
        attempt += 1;
    };
    if !files_identical(original, &backup).unwrap_or(false) {
        return Err(RepairError::Verification(
            "the backup copy does not match the original".into(),
        ));
    }
    flush_path_durably(&backup).map_err(flush_error("could not flush the backup"))?;
    flush_directory(backup_dir).map_err(flush_error("could not flush the backup directory"))?;
    Ok(backup)
}

/// FNV-1a over the path's bytes, folded to 32 bits: a stable, dependency-free disambiguator.
fn path_hash(path: &Path) -> u32 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    (hash ^ (hash >> 32)) as u32
}

fn files_identical(a: &Path, b: &Path) -> io::Result<bool> {
    if fs::metadata(a)?.len() != fs::metadata(b)?.len() {
        return Ok(false);
    }
    let (mut a, mut b) = (
        BufReader::new(File::open(a)?),
        BufReader::new(File::open(b)?),
    );
    let (mut buffer_a, mut buffer_b) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    loop {
        let read = a.read(&mut buffer_a)?;
        if read == 0 {
            return Ok(true);
        }
        b.read_exact(&mut buffer_b[..read])?;
        if buffer_a[..read] != buffer_b[..read] {
            return Ok(false);
        }
    }
}

/// Synthetic FLAC fixtures shared with the scanner tests: real streams at any (non-standard) rate,
/// built with the same encoder and block writer the repair uses, so no binary fixture is committed.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use flacenc::source::MemSource;

    pub(super) fn temp_dir(name: &str) -> PathBuf {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "lime-player-rate-repair-{name}-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Frequencies of the synthetic tone per channel; distinct so a swapped channel is detectable.
    pub(super) const TONE_HZ: [f64; 2] = [1_000.0, 440.0];
    pub(super) const TONE_AMPLITUDE: f64 = 0.5;

    pub(super) fn tone_samples(rate: u32, channels: usize, bits: usize, frames: usize) -> Vec<i32> {
        let scale = f64::from(1u32 << (bits - 1)) * TONE_AMPLITUDE;
        let mut samples = Vec::with_capacity(frames * channels);
        for frame in 0..frames {
            for tone_hz in &TONE_HZ[..channels] {
                let phase = 2.0 * std::f64::consts::PI * tone_hz * frame as f64 / f64::from(rate);
                samples.push((phase.sin() * scale).round() as i32);
            }
        }
        samples
    }

    /// A stereo 16-bit tone at `rate` with a tag block and an embedded cover.
    pub(crate) fn write_tagged_test_flac(path: &Path, rate: u32, frames: usize) {
        write_test_flac(path, rate, 2, 16, frames, &tagged_blocks());
    }

    pub(super) fn vorbis_comment_block(comments: &[&str]) -> MetadataBlock {
        let vendor = b"lime-player-test";
        let mut data = Vec::new();
        data.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        data.extend_from_slice(vendor);
        data.extend_from_slice(&(comments.len() as u32).to_le_bytes());
        for comment in comments {
            data.extend_from_slice(&(comment.len() as u32).to_le_bytes());
            data.extend_from_slice(comment.as_bytes());
        }
        MetadataBlock { kind: 4, data }
    }

    pub(super) fn png_bytes() -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(8, 8, image::Rgba([200, 30, 30, 255]));
        let mut bytes = Vec::new();
        image
            .write_to(&mut io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        bytes
    }

    pub(super) fn picture_block(image: &[u8]) -> MetadataBlock {
        let mime = b"image/png";
        let mut data = Vec::new();
        data.extend_from_slice(&3u32.to_be_bytes()); // front cover
        data.extend_from_slice(&(mime.len() as u32).to_be_bytes());
        data.extend_from_slice(mime);
        data.extend_from_slice(&0u32.to_be_bytes()); // description length
        for value in [8u32, 8, 32, 0] {
            data.extend_from_slice(&value.to_be_bytes());
        }
        data.extend_from_slice(&(image.len() as u32).to_be_bytes());
        data.extend_from_slice(image);
        MetadataBlock { kind: 6, data }
    }

    /// A minimal valid CUESHEET: one audio track with one index, plus the lead-out track.
    pub(super) fn cuesheet_block() -> MetadataBlock {
        let mut data = vec![0u8; 128 + 8 + 1 + 258];
        data.push(2); // tracks, including the lead-out
        let mut track = |number: u8, indices: u8| {
            data.extend_from_slice(&0u64.to_be_bytes()); // offset
            data.push(number);
            data.extend_from_slice(&[0u8; 12 + 1 + 13]); // ISRC, flags, reserved
            data.push(indices);
            for index in 0..indices {
                data.extend_from_slice(&0u64.to_be_bytes());
                data.push(index + 1);
                data.extend_from_slice(&[0u8; 3]);
            }
        };
        track(1, 1);
        track(170, 0);
        MetadataBlock {
            kind: BLOCK_CUESHEET,
            data,
        }
    }

    pub(super) fn tagged_blocks() -> Vec<MetadataBlock> {
        vec![
            // Mixed-case, non-standard keys on purpose: they must come back byte for byte.
            vorbis_comment_block(&[
                "TITLE=Heat - The Heat Is On",
                "ARTIST=Bust A Groove",
                "album_artist=Bust A Groove",
                "track=2",
            ]),
            picture_block(&png_bytes()),
        ]
    }

    /// Writes a synthetic FLAC using the same encoder and block writer the repair uses, so the
    /// fixture is a real FLAC stream at exactly the requested (possibly non-standard) rate.
    pub(super) fn write_test_flac(
        path: &Path,
        rate: u32,
        channels: usize,
        bits: usize,
        frames: usize,
        blocks: &[MetadataBlock],
    ) {
        let samples = tone_samples(rate, channels, bits, frames);
        let source = MemSource::from_samples(&samples, channels, bits, rate as usize);
        let config = EncoderConfig::default().into_verified().unwrap();
        let stream =
            flacenc::encode_with_fixed_block_size(&config, source, FLAC_BLOCK_SIZE).unwrap();
        let mut sink = ByteSink::new();
        stream.write(&mut sink).unwrap();
        let (stream_info, frames_offset) = split_encoded_stream(sink.as_slice()).unwrap();
        let mut file = File::create(path).unwrap();
        file.write_all(FLAC_MAGIC).unwrap();
        write_block(&mut file, BLOCK_STREAMINFO, &stream_info, blocks.is_empty()).unwrap();
        for (index, block) in blocks.iter().enumerate() {
            write_block(
                &mut file,
                block.kind,
                &block.data,
                index + 1 == blocks.len(),
            )
            .unwrap();
        }
        file.write_all(&sink.as_slice()[frames_offset..]).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    fn options(dir: &Path) -> RepairOptions {
        RepairOptions {
            backup_dir: dir.join("backups"),
        }
    }

    /// Fully decodes a FLAC into per-channel floats in [-1, 1).
    fn decode_channels(path: &Path) -> Vec<Vec<f64>> {
        let (mut format, track_id, codec_params) = open_symphonia(path).unwrap();
        let mut decoder = symphonia::default::get_codecs()
            .make_audio_decoder(&codec_params, &AudioDecoderOptions::default())
            .unwrap();
        let mut channels: Vec<Vec<f64>> = Vec::new();
        let mut scratch = Vec::<i32>::new();
        while let Some(packet) = format.next_packet().unwrap() {
            if packet.track_id != track_id {
                continue;
            }
            let decoded = decoder.decode(&packet).unwrap();
            let count = decoded.spec().channels().count();
            channels.resize_with(count, Vec::new);
            scratch.resize(decoded.samples_interleaved(), 0);
            decoded.copy_to_slice_interleaved(&mut scratch);
            for frame in scratch.chunks(count) {
                for (channel, sample) in frame.iter().enumerate() {
                    channels[channel].push(f64::from(*sample) / 2_147_483_648.0);
                }
            }
        }
        channels
    }

    /// Frequency estimate from rising zero crossings, ignoring the first and last 5% of the signal.
    fn estimate_hz(samples: &[f64], rate: u32) -> f64 {
        let start = samples.len() / 20;
        let end = samples.len() - start;
        let window = &samples[start..end];
        let crossings = window
            .windows(2)
            .filter(|pair| pair[0] < 0.0 && pair[1] >= 0.0)
            .count();
        crossings as f64 * f64::from(rate) / window.len() as f64
    }

    fn peak(samples: &[f64]) -> f64 {
        samples
            .iter()
            .fold(0.0f64, |peak, sample| peak.max(sample.abs()))
    }

    fn only_files_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn standard_rate_whitelist_is_exactly_the_agreed_set() {
        for rate in [
            8_000, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000, 88_200, 96_000,
            176_400, 192_000, 352_800, 384_000,
        ] {
            assert!(is_standard_sample_rate(rate), "{rate} must be standard");
            assert_eq!(
                repair_target_rate(rate),
                None,
                "{rate} must never be repaired"
            );
        }
        for rate in [
            0, 1, 37_800, 44_101, 47_999, 50_000, 64_000, 100_000, 384_001,
        ] {
            assert!(
                !is_standard_sample_rate(rate),
                "{rate} must be non-standard"
            );
        }
    }

    #[test]
    fn repair_target_is_the_nearest_playable_rate() {
        assert_eq!(
            repair_target_rate(37_800),
            Some(44_100),
            "the PS1/XA rate goes to 44.1 kHz"
        );
        assert_eq!(
            repair_target_rate(18_900),
            Some(44_100),
            "PS1 half rate: still the lowest playable rate"
        );
        assert_eq!(repair_target_rate(45_000), Some(44_100));
        assert_eq!(repair_target_rate(47_000), Some(48_000));
        assert_eq!(repair_target_rate(50_000), Some(48_000));
        assert_eq!(repair_target_rate(70_000), Some(88_200));
        assert_eq!(repair_target_rate(100_000), Some(96_000));
        assert_eq!(repair_target_rate(500_000), Some(384_000));
        assert_eq!(repair_target_rate(0), None);
    }

    #[test]
    fn only_flac_at_a_non_standard_rate_needs_repair() {
        let info = |format: &str, sample_rate: u32| AudioInfo {
            sample_rate,
            duration_ms: Some(1_000),
            source_channels: 2,
            bits_per_sample: 16,
            is_float: false,
            integer_pcm: true,
            format: format.into(),
        };
        assert!(needs_rate_repair(&info("FLAC", 37_800)));
        assert!(!needs_rate_repair(&info("FLAC", 44_100)));
        assert!(
            !needs_rate_repair(&info("WAV", 37_800)),
            "WAV is out of scope"
        );
        assert!(
            !needs_rate_repair(&info("WavPack", 37_800)),
            "WavPack is out of scope"
        );
        assert!(
            !needs_rate_repair(&info("MP3", 37_800)),
            "MP3 is out of scope"
        );
        assert!(!needs_rate_repair(&info("FLAC", 0)));
    }

    #[test]
    fn repairs_a_37800_hz_file_keeping_tone_duration_tags_and_cover() {
        let dir = temp_dir("repair-tone");
        let path = dir.join("02. Heat - The Heat Is On.flac");
        let frames_in = 56_700; // 1.5 s at 37 800 Hz
        let blocks = tagged_blocks();
        write_test_flac(&path, 37_800, 2, 16, frames_in, &blocks);
        let original_bytes = fs::read(&path).unwrap();
        assert_eq!(probe_file(&path).unwrap().sample_rate, 37_800);

        let report = repair_flac_sample_rate(&path, &options(&dir)).expect("repair succeeds");
        assert_eq!((report.from_rate, report.to_rate), (37_800, 44_100));

        // The repaired file is a valid stereo 16-bit FLAC at 44.1 kHz with the same duration.
        let info = probe_file(&path).unwrap();
        assert_eq!(
            (info.sample_rate, info.source_channels, info.bits_per_sample),
            (44_100, 2, 16)
        );
        let channels = decode_channels(&path);
        let expected_frames = frames_in * 44_100 / 37_800;
        assert!(
            channels[0].len().abs_diff(expected_frames) <= 2,
            "{} vs {expected_frames}",
            channels[0].len()
        );
        assert_eq!(channels[0].len(), channels[1].len());

        // The tones keep their pitch (not their length-in-samples) and their level, per channel.
        for (channel, expected_hz) in TONE_HZ.iter().enumerate() {
            let hz = estimate_hz(&channels[channel], 44_100);
            assert!(
                (hz - expected_hz).abs() / expected_hz < 0.01,
                "channel {channel}: {hz} Hz, expected {expected_hz} Hz"
            );
            assert!(
                (peak(&channels[channel]) - TONE_AMPLITUDE).abs() < 0.01,
                "channel {channel} level changed"
            );
        }

        // Tags and the cover survive byte for byte and are readable by the app's own tag reader.
        let blocks_after = read_kept_metadata_blocks(&path).unwrap();
        assert_eq!(blocks_after, blocks);
        let metadata = crate::audio::metadata::read_metadata(&path).unwrap();
        assert_eq!(
            metadata.tags.title.as_deref(),
            Some("Heat - The Heat Is On")
        );
        assert_eq!(metadata.tags.artist.as_deref(), Some("Bust A Groove"));
        assert_eq!(metadata.picture.expect("cover kept").data, png_bytes());

        // The untouched original sits in the backup dir, named after its file and original rate.
        let backup_name = report
            .backup
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            backup_name.starts_with("02. Heat - The Heat Is On.original-37800-"),
            "{backup_name}"
        );
        assert!(backup_name.ends_with(".flac"), "{backup_name}");
        assert_eq!(fs::read(&report.backup).unwrap(), original_bytes);
        assert_eq!(
            only_files_in(&dir),
            vec![
                "02. Heat - The Heat Is On.flac".to_owned(),
                "backups".to_owned()
            ],
            "no temp file may remain"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn repairing_twice_is_a_no_op() {
        let dir = temp_dir("idempotent");
        let path = dir.join("track.flac");
        write_test_flac(&path, 37_800, 2, 16, 20_000, &tagged_blocks());
        repair_flac_sample_rate(&path, &options(&dir)).unwrap();
        let repaired_bytes = fs::read(&path).unwrap();

        assert!(matches!(
            repair_flac_sample_rate(&path, &options(&dir)),
            Err(RepairError::NotNeeded)
        ));
        assert_eq!(
            fs::read(&path).unwrap(),
            repaired_bytes,
            "a repaired file must not be rewritten"
        );
        assert_eq!(
            fs::read_dir(dir.join("backups")).unwrap().count(),
            1,
            "no second backup"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_at_a_standard_rate_is_left_alone() {
        let dir = temp_dir("standard");
        let path = dir.join("track.flac");
        write_test_flac(&path, 44_100, 2, 16, 20_000, &[]);
        let before = fs::read(&path).unwrap();
        assert!(matches!(
            repair_flac_sample_rate(&path, &options(&dir)),
            Err(RepairError::NotNeeded)
        ));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(!dir.join("backups").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn repairs_a_mono_24_bit_file_and_preserves_bit_depth_and_channels() {
        let dir = temp_dir("mono24");
        let path = dir.join("mono.flac");
        write_test_flac(&path, 30_000, 1, 24, 30_000, &[]);
        let report = repair_flac_sample_rate(&path, &options(&dir)).unwrap();
        assert_eq!(report.to_rate, 44_100);
        let info = probe_file(&path).unwrap();
        assert_eq!(
            (info.sample_rate, info.source_channels, info.bits_per_sample),
            (44_100, 1, 24)
        );
        let channels = decode_channels(&path);
        assert!(channels[0].len().abs_diff(44_100) <= 2);
        let hz = estimate_hz(&channels[0], 44_100);
        assert!((hz - 1_000.0).abs() < 10.0, "{hz}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_read_only_file_is_refused_and_left_untouched() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("readonly-file");
        let path = dir.join("track.flac");
        write_test_flac(&path, 37_800, 2, 16, 20_000, &[]);
        let before = fs::read(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();

        let error = repair_flac_sample_rate(&path, &options(&dir)).unwrap_err();
        assert!(matches!(error, RepairError::NotWritable(_)), "{error}");
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(!dir.join("backups").exists());
        assert_eq!(only_files_in(&dir), vec!["track.flac".to_owned()]);

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_read_only_directory_is_refused_before_any_work() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("readonly-dir");
        let album = dir.join("album");
        fs::create_dir_all(&album).unwrap();
        let path = album.join("track.flac");
        write_test_flac(&path, 37_800, 2, 16, 20_000, &[]);
        let before = fs::read(&path).unwrap();
        fs::set_permissions(&album, fs::Permissions::from_mode(0o555)).unwrap();

        let result = repair_flac_sample_rate(&path, &options(&dir));
        fs::set_permissions(&album, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            matches!(result, Err(RepairError::NotWritable(_))),
            "{result:?}"
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(only_files_in(&album), vec!["track.flac".to_owned()]);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_backup_leaves_the_original_and_no_temp_file() {
        let dir = temp_dir("backup-fails");
        let path = dir.join("track.flac");
        write_test_flac(&path, 37_800, 2, 16, 20_000, &tagged_blocks());
        let before = fs::read(&path).unwrap();
        // A regular file where the backup directory should be: creating it must fail.
        let blocker = dir.join("not-a-directory");
        fs::write(&blocker, b"x").unwrap();

        let error = repair_flac_sample_rate(
            &path,
            &RepairOptions {
                backup_dir: blocker.join("backups"),
            },
        )
        .unwrap_err();
        assert!(matches!(error, RepairError::NotWritable(_)), "{error}");
        assert_eq!(
            fs::read(&path).unwrap(),
            before,
            "the original must be intact"
        );
        assert_eq!(
            only_files_in(&dir),
            vec!["not-a-directory".to_owned(), "track.flac".to_owned()]
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_corrupt_stream_is_refused_and_the_original_is_kept() {
        let dir = temp_dir("corrupt");
        let path = dir.join("track.flac");
        write_test_flac(&path, 37_800, 2, 16, 40_000, &tagged_blocks());
        let mut bytes = fs::read(&path).unwrap();
        let middle = bytes.len() * 3 / 4;
        for byte in &mut bytes[middle..middle + 64] {
            *byte ^= 0xA5;
        }
        fs::write(&path, &bytes).unwrap();

        let error = repair_flac_sample_rate(&path, &options(&dir)).unwrap_err();
        assert!(
            matches!(error, RepairError::Decode(_) | RepairError::Verification(_)),
            "{error}"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "the original must be intact"
        );
        assert_eq!(
            only_files_in(&dir),
            vec!["track.flac".to_owned()],
            "no temp file, no backup"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_with_an_embedded_cuesheet_is_refused_instead_of_losing_it() {
        let dir = temp_dir("cuesheet");
        let path = dir.join("track.flac");
        let mut blocks = tagged_blocks();
        blocks.push(cuesheet_block());
        write_test_flac(&path, 37_800, 2, 16, 20_000, &blocks);
        let before = fs::read(&path).unwrap();

        let error = repair_flac_sample_rate(&path, &options(&dir)).unwrap_err();
        assert!(matches!(error, RepairError::Unsupported(_)), "{error}");
        assert_eq!(fs::read(&path).unwrap(), before);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_stale_temp_file_from_an_interrupted_run_is_replaced() {
        let dir = temp_dir("stale-temp");
        let path = dir.join("track.flac");
        write_test_flac(&path, 37_800, 2, 16, 20_000, &[]);
        fs::write(dir.join(".lime-rate-repair-track.flac"), b"half written").unwrap();

        repair_flac_sample_rate(&path, &options(&dir))
            .expect("a stale temp file must not block the repair");
        assert_eq!(probe_file(&path).unwrap().sample_rate, 44_100);
        assert_eq!(
            only_files_in(&dir),
            vec!["backups".to_owned(), "track.flac".to_owned()]
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn repairing_through_a_symlink_rewrites_the_target_and_keeps_the_link() {
        let dir = temp_dir("symlink");
        let real = dir.join("real.flac");
        let link = dir.join("link.flac");
        write_test_flac(&real, 37_800, 2, 16, 20_000, &[]);
        std::os::unix::fs::symlink(&real, &link).unwrap();

        repair_flac_sample_rate(&link, &options(&dir)).unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link itself must survive"
        );
        assert_eq!(probe_file(&real).unwrap().sample_rate, 44_100);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn backups_of_same_named_tracks_in_different_folders_do_not_collide() {
        let dir = temp_dir("backup-names");
        let backups = dir.join("backups");
        for album in ["album-a", "album-b"] {
            fs::create_dir_all(dir.join(album)).unwrap();
            let path = dir.join(album).join("01.flac");
            write_test_flac(&path, 37_800, 2, 16, 10_000, &[]);
            repair_flac_sample_rate(&path, &options(&dir)).unwrap();
        }
        assert_eq!(fs::read_dir(&backups).unwrap().count(), 2);
        fs::remove_dir_all(&dir).unwrap();
    }

    // -- durable flush ------------------------------------------------------------------------

    fn os_error(code: i32) -> io::Error {
        io::Error::from_raw_os_error(code)
    }

    #[test]
    fn the_strong_flush_is_all_that_runs_when_it_works() {
        let plain_calls = std::cell::Cell::new(0);
        flush_with_fallback(
            || Ok(()),
            || {
                plain_calls.set(plain_calls.get() + 1);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(plain_calls.get(), 0);
    }

    #[test]
    fn an_unsupported_strong_flush_falls_back_to_plain_fsync() {
        // smbfs answers F_FULLFSYNC with ENOTSUP; EOPNOTSUPP is the other spelling of the same thing.
        for code in [libc::ENOTSUP, libc::EOPNOTSUPP] {
            let plain_calls = std::cell::Cell::new(0);
            flush_with_fallback(
                || Err(os_error(code)),
                || {
                    plain_calls.set(plain_calls.get() + 1);
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(plain_calls.get(), 1, "errno {code}");
        }
    }

    #[test]
    fn a_volume_that_supports_no_flush_is_refused() {
        // Neither flush works: the "not supported" error comes back instead of being swallowed, and
        // `flush_error` turns it into a refusal the caller reports (nothing is renamed over the
        // original), with the underlying errno kept in the message.
        for code in [libc::ENOTSUP, libc::EOPNOTSUPP] {
            let error = flush_with_fallback(|| Err(os_error(code)), || Err(os_error(code)))
                .expect_err("a flush nothing supports must not pass as done");
            assert!(is_flush_unsupported(&error), "errno {code}");
            let refusal = flush_error("could not flush the repaired file")(error);
            assert!(
                matches!(refusal, RepairError::Unsupported(_)),
                "errno {code}: {refusal:?}"
            );
            assert!(
                refusal.to_string().contains("neither F_FULLFSYNC nor fsync"),
                "{refusal}"
            );
        }
    }

    #[test]
    fn a_genuine_io_error_still_fails_the_flush() {
        // From the strong flush: no fallback is attempted at all.
        let plain_calls = std::cell::Cell::new(0);
        let error = flush_with_fallback(
            || Err(os_error(libc::EIO)),
            || {
                plain_calls.set(plain_calls.get() + 1);
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EIO));
        assert_eq!(plain_calls.get(), 0, "EIO is not a reason to try again");

        // From the fallback: the strong flush was unsupported, but the write itself is failing.
        for code in [libc::EIO, libc::ENOSPC] {
            let error =
                flush_with_fallback(|| Err(os_error(libc::ENOTSUP)), || Err(os_error(code)))
                    .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(code));
        }
    }

    #[test]
    fn a_genuine_flush_failure_keeps_its_io_error_kind() {
        let refusal = flush_error("could not flush the backup")(os_error(libc::EIO));
        assert!(
            matches!(&refusal, RepairError::Io { source, .. } if source.raw_os_error() == Some(libc::EIO)),
            "{refusal:?}"
        );
    }

    #[test]
    fn flushing_a_real_file_succeeds() {
        let dir = temp_dir("flush");
        let mut file = File::create(dir.join("data.bin")).unwrap();
        file.write_all(&[0u8; 4096]).unwrap();
        flush_durably(&file).unwrap();
        plain_fsync(&file).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_read_only_file_and_its_directory_can_be_flushed_by_path() {
        // The backup path: flushing must not need write access to a reused, read-only backup.
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("flush-path");
        let file = dir.join("backup.flac");
        fs::write(&file, [0u8; 4096]).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o444)).unwrap();
        flush_path_durably(&file).unwrap();
        flush_directory(&dir).unwrap();
        assert!(flush_path_durably(&dir.join("missing.flac")).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_leftover_identical_backup_is_reused_and_flushed() {
        // An interrupted earlier run left the backup behind: the retry must accept it (and flush it)
        // instead of writing a second copy.
        let dir = temp_dir("backup-reuse");
        let path = dir.join("track.flac");
        write_test_flac(&path, 37_800, 2, 16, 10_000, &[]);
        let backups = dir.join("backups");

        let first = backup_original(&path, 37_800, &backups).unwrap();
        let second = backup_original(&path, 37_800, &backups).unwrap();
        assert_eq!(first, second);
        assert_eq!(fs::read_dir(&backups).unwrap().count(), 1);
        assert_eq!(fs::read(&first).unwrap(), fs::read(&path).unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }

    // -- real-volume check --------------------------------------------------------------------

    /// Manual regression check for a real volume (a NAS share mounted over SMB, say): runs the
    /// engine on an actual file and checks the outcome the way a user would notice it. It rewrites
    /// `LIME_REPAIR_FILE` in place, so point it at a COPY, ideally on the volume under test.
    /// `LIME_REPAIR_BACKUP_DIR` is where the original is backed up (a fresh temp dir when unset).
    ///
    /// Run: `LIME_REPAIR_FILE=<copy.flac> LIME_REPAIR_BACKUP_DIR=<dir> cargo test --bin lime-player
    /// repair_real_file -- --ignored --nocapture`. Skips, with a message, when `LIME_REPAIR_FILE` is
    /// unset, so a normal `cargo test` run never needs a real file.
    #[test]
    #[ignore]
    fn repair_real_file() {
        let Ok(file) = std::env::var("LIME_REPAIR_FILE") else {
            eprintln!(
                "LIME_REPAIR_FILE is not set; skipping repair_real_file (see src/audio/rate_repair.rs)"
            );
            return;
        };
        let path = PathBuf::from(file);
        let backup_dir = std::env::var_os("LIME_REPAIR_BACKUP_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| temp_dir("real-file").join("backups"));
        let before = probe_file(&path).expect("LIME_REPAIR_FILE must be a readable audio file");
        eprintln!("before: {before:?}");

        let report = repair_flac_sample_rate(&path, &RepairOptions { backup_dir })
            .unwrap_or_else(|error| panic!("repair failed: {error}"));
        eprintln!("report: {report:?}");

        let after = probe_file(&path).expect("the repaired file must still be readable");
        eprintln!("after: {after:?}");
        assert_eq!(after.sample_rate, report.to_rate);
        assert_eq!(after.source_channels, before.source_channels);
        assert!(report.backup.is_file(), "no backup at {:?}", report.backup);
        assert_eq!(
            probe_file(&report.backup).unwrap().sample_rate,
            before.sample_rate,
            "the backup must still hold the original"
        );
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".lime-rate-repair-")
            })
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
    }
}

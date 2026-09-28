use std::collections::VecDeque;
use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, DeviceId, OutputStreamTimestamp, SampleFormat, Stream, StreamConfig, StreamInstant};
use crossbeam_channel::{Receiver, Sender, unbounded};
use rtrb::{Consumer, RingBuffer};

use super::coreaudio::{self, DacState, HogLease};
use super::decoder::{AudioInfo, PcmSample, decode_file};
use crate::library::TrackKey;

const RING_SAMPLES: usize = 65_536;
const PREBUFFER_MILLISECONDS: usize = 60;
const DIAGNOSTIC_REPORT_INTERVAL: Duration = Duration::from_secs(1);
const PLAYBACK_TIMELINE_REPORT_INTERVAL: Duration = Duration::from_millis(50);
const OUTPUT_DRAIN_GUARD: Duration = Duration::from_millis(90);
const SHUTDOWN_DECODER_GRACE: Duration = Duration::from_millis(250);
/// Session history cap (`§3.7`); the oldest entry is dropped first.
const HISTORY_LIMIT: usize = 200;
/// Refuse an immediate stop (seek, `ReplaceQueue`, Previous, a restart-from-queue) rather than pile
/// up unbounded stopping decoders (`§4.2`; `CLAUDE.md` "one current decoder plus at most one
/// speculative lookahead, plus a bounded number of retiring ones").
const MAX_RETIRING_DECODERS: usize = 4;
/// Shared refusal text for every immediate-stop path guarded by `MAX_RETIRING_DECODERS`.
const RETIRING_DECODERS_BUSY_MESSAGE: &str = "Still stopping earlier audio reads; try again in a moment.";
const FIRST_OUTPUT_TIMESTAMP_EMPTY: u8 = 0;
const FIRST_OUTPUT_TIMESTAMP_WRITING: u8 = 1;
const FIRST_OUTPUT_TIMESTAMP_READY: u8 = 2;
// 30 s leaves room for NAS disk spin-up before a prepared track is declared unreadable.
const PREBUFFER_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to wait for the DAC's *physical* stream rate to settle after
/// `set_and_verify_sample_rate` confirms the nominal rate (`§` bugfix: stale AudioUnit after a
/// strict-integer/float route change). Same order of magnitude as the physical-format settle loop
/// in `ensure_exact_integer_output`.
const PHYSICAL_RATE_SETTLE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_OUTPUT_LATENCY_NANOS: u64 = 5_000_000_000; // 5 s: covers AirPlay/Bluetooth (~2 s)
const MISMATCH_FLOAT_IN_I32: u8 = 1;
const MISMATCH_INTEGER_IN_F32: u8 = 2;
/// Cheap HAL re-read cadence for the device-volume poll (`§4.5`).
const VOLUME_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Below this, a level change is noise, not a real move worth an event (`§4.5`).
const VOLUME_CHANGE_EPSILON: f32 = 0.005;

#[derive(Default)]
struct AudioDiagnostics {
    decoder_nonzero_sample: AtomicBool,
    callback_nonzero_sample: AtomicBool,
    first_output_timestamp_state: AtomicU8,
    first_output_playback_secs: AtomicU64,
    first_output_playback_nanos: AtomicU32,
    underrun_samples: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct AudioDiagnosticsSnapshot {
    decoder_nonzero_sample: bool,
    callback_nonzero_sample: bool,
    underrun_samples: u64,
}

impl AudioDiagnostics {
    fn snapshot(&self) -> AudioDiagnosticsSnapshot {
        AudioDiagnosticsSnapshot {
            decoder_nonzero_sample: self.decoder_nonzero_sample.load(Ordering::Acquire),
            callback_nonzero_sample: self.callback_nonzero_sample.load(Ordering::Acquire),
            underrun_samples: self.underrun_samples.load(Ordering::Acquire),
        }
    }
}

impl AudioDiagnosticsSnapshot {
    fn status(self) -> String {
        format!(
            "Audio path · decoded nonzero: {} · output callback nonzero: {} · underrun samples: {}",
            yes_no(self.decoder_nonzero_sample),
            yes_no(self.callback_nonzero_sample),
            self.underrun_samples
        )
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn mark_first_output_pcm_playback_time(
    state: &AtomicU8,
    seconds: &AtomicU64,
    nanoseconds: &AtomicU32,
    playback_time: StreamInstant,
    delivered_pcm_samples: u64,
) {
    if delivered_pcm_samples == 0
        || state
            .compare_exchange(
                FIRST_OUTPUT_TIMESTAMP_EMPTY,
                FIRST_OUTPUT_TIMESTAMP_WRITING,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_err()
    {
        return;
    }

    let total_nanos = playback_time.as_nanos();
    seconds.store((total_nanos / 1_000_000_000) as u64, Ordering::Relaxed);
    nanoseconds.store((total_nanos % 1_000_000_000) as u32, Ordering::Relaxed);
    state.store(FIRST_OUTPUT_TIMESTAMP_READY, Ordering::Release);
}

fn first_output_pcm_playback_time(diagnostics: &AudioDiagnostics) -> Option<StreamInstant> {
    if diagnostics
        .first_output_timestamp_state
        .load(Ordering::Acquire)
        != FIRST_OUTPUT_TIMESTAMP_READY
    {
        return None;
    }
    Some(StreamInstant::new(
        diagnostics.first_output_playback_secs.load(Ordering::Relaxed),
        diagnostics.first_output_playback_nanos.load(Ordering::Relaxed),
    ))
}

fn unix_time_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn first_output_pcm_callback_status(
    playback_estimate_from_play_return: Duration,
    playback_estimate_preceded_play_return: bool,
    play_returned_unix_ms: u128,
    observed_unix_ms: u128,
) -> String {
    let sign = if playback_estimate_preceded_play_return { "-" } else { "+" };
    format!(
        "First output PCM DAC playback estimate · play-return-to-scheduled-playback {sign}{} ms · wall-clock correlation: play return unix-ms {} · worker observation unix-ms {}",
        playback_estimate_from_play_return.as_millis(),
        play_returned_unix_ms,
        observed_unix_ms
    )
}

fn playback_estimate_from_play_return(
    playback_time: StreamInstant,
    play_return_time: StreamInstant,
) -> (Duration, bool) {
    match playback_time.checked_duration_since(play_return_time) {
        Some(elapsed) => (elapsed, false),
        None => (play_return_time.duration_since(playback_time), true),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputDevice {
    pub id: String,
    pub label: String,
}

#[derive(Clone, Debug)]
pub struct PlayerSettings {
    pub output_device_id: Option<String>,
    pub hog_mode_enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueTrackSnapshot {
    pub key: TrackKey,
    pub title: String,
    pub parent_folder: String,
    pub format: String,
    pub sample_rate: u32,
    pub bits_per_sample: u32,
    pub is_float: bool,
    pub duration_ms: Option<u64>,
}

impl Default for PlayerSettings {
    fn default() -> Self {
        Self {
            output_device_id: None,
            hog_mode_enabled: true,
        }
    }
}

#[derive(Clone, Debug)]
pub enum PlaybackEvent {
    Devices(Vec<OutputDevice>),
    Started {
        key: TrackKey,
        info: AudioInfo,
        title: String,
        album: String,
        artist: String,
        output: String,
        hog_mode: bool,
        integer_output_verified: bool,
    },
    Timeline {
        position_ms: u64,
        duration_ms: Option<u64>,
    },
    Playing(bool),
    /// The UI clears its pending seek immediately on receiving this (`§5.10`).
    SeekRejected,
    QueueSnapshot(Vec<QueueTrackSnapshot>),
    /// Device-volume state (`§4.5`): `level` is meaningful only while `available` is true.
    Volume {
        available: bool,
        level: f32,
        detail: String,
    },
    Stopped,
    /// `self.active` became `None` without a new track starting, but the queue itself was not
    /// cleared (unlike `Stopped`, which only fires once the queue drains empty or the worker
    /// shuts down): a failed seek, a retained playback failure, or a failed automatic advance.
    /// The UI must treat this exactly like the end of the active track — clear the route status,
    /// `is-playing`, `can-seek` and the now-playing accent — without touching queue rows or the
    /// last status message, since the failure's own `Status` explains what happened (`§1.4`).
    Inactive,
    Status(String),
    /// Developer timing/diagnostic lines. Never shown in the UI; main.rs only `eprintln!`s them.
    Diagnostic(String),
}

enum Command {
    /// Tracks already probed by the library scanner (`§6` Stage 3); the worker never calls
    /// `probe_file` itself outside tests.
    Enqueue(Vec<PreparedTrack>),
    PlayNext(Vec<PreparedTrack>),
    ReplaceQueue(Vec<PreparedTrack>),
    Previous,
    /// `key` guards against a stale seek racing a track transition.
    Seek { key: TrackKey, position_ms: u64 },
    /// 0.0..=1.0 device volume scalar (`§4.5`).
    SetVolume(f32),
    SelectOutput(Option<String>),
    SetHogMode(bool),
    TogglePlayback,
    Next,
    Shutdown,
}

pub struct AudioPlayer {
    commands: Sender<Command>,
    events: Receiver<PlaybackEvent>,
}

impl AudioPlayer {
    pub fn new(settings: PlayerSettings) -> Self {
        let (command_tx, command_rx) = unbounded();
        let (event_tx, event_rx) = unbounded();
        thread::Builder::new()
            .name("lime-audio-controller".into())
            .spawn(move || PlaybackWorker::new(settings, command_rx, event_tx).run())
            .expect("could not start Lime Player audio controller");
        Self {
            commands: command_tx,
            events: event_rx,
        }
    }

    pub fn events(&self) -> Receiver<PlaybackEvent> {
        self.events.clone()
    }

    /// `tracks` are already probed by the library scanner (`§6` Stage 3).
    pub fn enqueue(&self, tracks: Vec<PreparedTrack>) {
        let _ = self.commands.send(Command::Enqueue(tracks));
    }

    /// Not yet wired to the UI (arrives with the library views in a later stage); the worker
    /// side is implemented and tested now so the command surface lands with the seek/transport
    /// engine work.
    #[allow(dead_code)]
    pub fn play_next(&self, tracks: Vec<PreparedTrack>) {
        let _ = self.commands.send(Command::PlayNext(tracks));
    }

    /// Stops the current track immediately and plays `tracks` from the first item (`§3.7`); used
    /// by `track-activated(TrackListKind.queue, i)` (`§6` Stage 5) to jump to a queued row.
    pub fn replace_queue(&self, tracks: Vec<PreparedTrack>) {
        let _ = self.commands.send(Command::ReplaceQueue(tracks));
    }

    pub fn previous(&self) {
        let _ = self.commands.send(Command::Previous);
    }

    pub fn seek(&self, key: TrackKey, position_ms: u64) {
        let _ = self.commands.send(Command::Seek { key, position_ms });
    }

    /// `level` is a 0.0..=1.0 device volume scalar (`§4.5`); ignored by the worker when the
    /// current output has no settable volume.
    pub fn set_volume(&self, level: f32) {
        let _ = self.commands.send(Command::SetVolume(level));
    }

    pub fn select_output(&self, id: Option<String>) {
        let _ = self.commands.send(Command::SelectOutput(id));
    }

    pub fn set_hog_mode(&self, enabled: bool) {
        let _ = self.commands.send(Command::SetHogMode(enabled));
    }

    pub fn toggle_playback(&self) {
        let _ = self.commands.send(Command::TogglePlayback);
    }

    pub fn next(&self) {
        let _ = self.commands.send(Command::Next);
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
    }
}

pub fn enumerate_outputs() -> Result<Vec<OutputDevice>, String> {
    let host = cpal::default_host();
    let devices = host.output_devices().map_err(|e| e.to_string())?;
    let mut outputs = Vec::new();
    for device in devices {
        let description = device.description().map_err(|e| e.to_string())?;
        let device_id = device.id().map_err(|e| e.to_string())?;
        outputs.push(OutputDevice {
            id: device_id.to_string(),
            label: description.name().to_owned(),
        });
    }
    let mut labels = std::collections::HashMap::<String, usize>::new();
    for output in &mut outputs {
        let count = labels.entry(output.label.clone()).or_insert(0);
        *count += 1;
        if *count > 1 {
            output.label = format!("{} ({})", output.label, *count);
        }
    }
    Ok(outputs)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedTrack {
    pub key: TrackKey,
    pub info: AudioInfo,
}

struct ActivePlayback {
    prepared: PreparedTrack,
    stream: Option<Stream>,
    decoder: Option<JoinHandle<()>>,
    cancel: Arc<AtomicBool>,
    drained: Arc<AtomicBool>,
    decoder_failed: Arc<AtomicBool>,
    output_failed: Arc<AtomicBool>,
    format_mismatch: Arc<AtomicU8>,
    diagnostics: Arc<AudioDiagnostics>,
    reported_diagnostics: AudioDiagnosticsSnapshot,
    reported_underrun_samples: u64,
    underrun_reported_at: Instant,
    stream_play_returned_stream_time: StreamInstant,
    stream_play_returned_unix_ms: u128,
    first_output_pcm_callback_reported: bool,
    consumed_output_samples: Arc<AtomicU64>,
    output_latency_nanos: Arc<AtomicU64>,
    start_frame: u64,
    timeline_reported_samples: u64,
    timeline_reported_at: Instant,
    drain_started: Option<Instant>,
    playing: bool,
    skip_requested: bool,
    /// The exact `DeviceId` string used to open this stream. A seek pins to it (`§4.2`).
    output_id: String,
    /// The Hog preference used at this start. A seek pins to it too, so it never re-acquires,
    /// releases or steals Hog (`§1.4`).
    hog_mode: bool,
}

/// Why a track is being (re)started: a normal queue advance, or a seek within the same track.
/// Reason-dependent events differ (`§4.2`): a `NewTrack` start emits `Started`/`Timeline{0}` and
/// the handoff diagnostic; a `Seek` emits neither, only a `Timeline` at the seek target.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StartReason {
    NewTrack,
    Seek,
}

/// The device and Hog mode a seek is pinned to: the ones already active, never the current
/// selection, so a seek can never trigger a Hog acquire/release/steal (`§1.4`, `§4.2`).
#[derive(Clone)]
struct PinnedOutput {
    device_id: String,
    hog_mode: bool,
}

#[derive(Clone)]
struct StartOptions {
    start_frame: u64,
    start_playing: bool,
    reason: StartReason,
    pinned_output: Option<PinnedOutput>,
}

/// Recorded by `stop_active_now` so the next start on the same device can apply the 90 ms
/// rate-change guard (`§4.6.5`), and so the next start on the same device can tell whether the
/// route (strict integer vs. float) actually changed, even when the HAL's own rate/format
/// readback already looks unchanged (`needs_dac_prime`).
struct StoppedOutput {
    at: Instant,
    device_id: String,
    rate: u32,
    integer_output: bool,
}

struct AudioPreparation {
    consumer: Option<Consumer<PcmSample>>,
    decoder: Option<JoinHandle<()>>,
    cancel: Arc<AtomicBool>,
    eof: Arc<AtomicBool>,
    drained: Arc<AtomicBool>,
    decoder_failed: Arc<AtomicBool>,
    buffered: Arc<AtomicUsize>,
    diagnostics: Arc<AudioDiagnostics>,
    decoder_error: Arc<Mutex<Option<String>>>,
    prebuffer_samples: usize,
}

impl AudioPreparation {
    fn cancel(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.consumer.take();
    }

    fn decoder_finished(&self) -> bool {
        self.decoder.as_ref().is_none_or(JoinHandle::is_finished)
    }

    fn reap_finished_decoder(&mut self) -> bool {
        if !self.decoder_finished() {
            return false;
        }
        if let Some(decoder) = self.decoder.take() {
            let _ = decoder.join();
        }
        true
    }

    fn finish_for_shutdown(&mut self, deadline: Instant) {
        self.cancel();
        if let Some(decoder) = self.decoder.take() {
            join_decoder_until(decoder, deadline, SHUTDOWN_DECODER_GRACE, "speculative NAS decoder");
        }
    }

    fn wait_until_prebuffered(&self) -> Result<Duration, String> {
        self.wait_until_prebuffered_within(PREBUFFER_TIMEOUT)
    }

    fn wait_until_prebuffered_within(&self, timeout: Duration) -> Result<Duration, String> {
        let started_at = Instant::now();
        while self.buffered.load(Ordering::Acquire) < self.prebuffer_samples
            && !self.eof.load(Ordering::Acquire)
        {
            if self.cancel.load(Ordering::Acquire) {
                return Err("Audio preparation was cancelled.".into());
            }
            if started_at.elapsed() >= timeout {
                return Err(format!(
                    "The file could not be read in time ({} s). Check the NAS connection.",
                    timeout.as_secs()
                ));
            }
            thread::sleep(Duration::from_millis(2));
        }
        if self.buffered.load(Ordering::Acquire) == 0 {
            let error = self.decoder_error.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
            return Err(error
                .map(|error| format!("Audio decoding failed before playback: {error}"))
                .unwrap_or_else(|| "The selected file contains no decodable audio samples.".into()));
        }
        Ok(started_at.elapsed())
    }
}

impl Drop for AudioPreparation {
    fn drop(&mut self) {
        // Only an unstarted preparation still owns its decoder handle. After a successful
        // start, `decoder.take()` moved it into ActivePlayback, which SHARES this `cancel`
        // Arc. An unconditional store here would stop the live track.
        if self.decoder.is_some() {
            self.cancel.store(true, Ordering::Release);
        }
    }
}

struct TrackPreparation {
    prepared: PreparedTrack,
    audio: AudioPreparation,
    started_at: Instant,
    // Playback-worker observation time, not the decoder's exact PCM enqueue time.
    prebuffer_ready_observed_at: Option<Instant>,
}

impl TrackPreparation {
    fn mark_prebuffer_ready(&mut self) -> Option<(Duration, usize)> {
        if self.prebuffer_ready_observed_at.is_some() {
            return None;
        }
        let buffered = self.audio.buffered.load(Ordering::Acquire);
        if buffered < self.audio.prebuffer_samples && !self.audio.eof.load(Ordering::Acquire) {
            return None;
        }
        let ready_at = Instant::now();
        self.prebuffer_ready_observed_at = Some(ready_at);
        Some((ready_at.duration_since(self.started_at), buffered))
    }

    fn cancel(&mut self) {
        self.audio.cancel();
    }

    fn finish_for_shutdown(&mut self, deadline: Instant) {
        self.audio.finish_for_shutdown(deadline);
    }
}

#[derive(Default)]
struct TrackStartTimings {
    prebuffer_wait: Duration,
    hog_setup: Option<Duration>,
    nominal_rate_setup: Option<Duration>,
    physical_format_setup: Option<Duration>,
    stream_build: Duration,
    stream_play_call: Duration,
    worker_observed_preparation_to_prebuffer_ready: Duration,
}

fn optional_duration_millis(duration: Option<Duration>) -> String {
    duration
        .map(|duration| duration.as_millis().to_string())
        .unwrap_or_else(|| "n/a".into())
}

fn handoff_timing_diagnostic(
    track: &str,
    format: &str,
    target_rate_hz: u32,
    worker_drain_observation_to_play_return: Duration,
    timings: &TrackStartTimings,
) -> String {
    format!(
        "audio_handoff_timing track={track:?} format={format:?} target_rate_hz={target_rate_hz} worker_drain_observation_to_play_return_ms={} worker_observed_prep_to_prebuffer_ready_ms={} prebuffer_wait_ms={} hog_setup_macos_ms={} nominal_rate_setup_macos_ms={} physical_format_setup_macos_ms={} stream_build_ms={} stream_play_call_ms={} acoustic_gap=unmeasured",
        worker_drain_observation_to_play_return.as_millis(),
        timings.worker_observed_preparation_to_prebuffer_ready.as_millis(),
        timings.prebuffer_wait.as_millis(),
        optional_duration_millis(timings.hog_setup),
        optional_duration_millis(timings.nominal_rate_setup),
        optional_duration_millis(timings.physical_format_setup),
        timings.stream_build.as_millis(),
        timings.stream_play_call.as_millis(),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlaybackFailure {
    Decoder,
    Output,
}

impl PlaybackFailure {
    fn status(self) -> &'static str {
        match self {
            Self::Decoder => "Audio decoding failed; the current track was retained. Press Play to retry or Next to skip.",
            Self::Output => "Audio output failed; the current track was retained. Choose an output or press Play to retry; use Next to skip.",
        }
    }
}

impl ActivePlayback {
    fn stop_output(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(stream) = self.stream.take() {
            let _ = stream.pause();
            drop(stream);
        }
    }

    fn finish(mut self) {
        self.stop_output();
        if let Some(decoder) = self.decoder.take() {
            let _ = decoder.join();
        }
    }

    fn finish_for_shutdown(mut self, deadline: Instant) {
        self.stop_output();
        if let Some(decoder) = self.decoder.take() {
            join_decoder_until(decoder, deadline, SHUTDOWN_DECODER_GRACE, "active audio decoder");
        }
    }
}

/// Latency-compensated, TRACK-RELATIVE audible position for the currently active track (0 at this
/// track's own `key.start_frame`, per `CLAUDE.md` CUE sheet support §4 — the UI's timeline is
/// always track-relative, never the underlying physical file's absolute position). Computes the
/// absolute position first (`active.start_frame`, the absolute decode start this stream was built
/// with), then subtracts `key.start_frame`'s time equivalent exactly once via
/// `track_relative_millis`, so every caller of this helper gets the subtraction automatically and
/// it can never be double-applied or forgotten.
fn audible_position_for(active: &ActivePlayback) -> u64 {
    let absolute_ms = audible_position_millis(
        active.consumed_output_samples.load(Ordering::Acquire),
        active.start_frame,
        active.output_latency_nanos.load(Ordering::Relaxed),
        active.prepared.info.sample_rate,
    )
    .unwrap_or(0);
    track_relative_millis(absolute_ms, active.prepared.key.start_frame, active.prepared.info.sample_rate)
}

/// Converts an absolute physical-file-frame position (already in milliseconds) to a position
/// relative to `key_start_frame` (an active `TrackKey`'s own `start_frame`) — the CUE-track
/// track-relative Timeline subtraction point (`CLAUDE.md` §4). A whole-file track's
/// `key_start_frame` is always 0, so this is a no-op subtraction for every non-CUE track.
/// Saturates rather than underflows: `absolute_ms` should never fall behind `key_start_ms` in
/// practice, but a clamp is cheaper and safer than a panic on a hot event-emission path.
fn track_relative_millis(absolute_ms: u64, key_start_frame: u64, sample_rate: u32) -> u64 {
    absolute_ms.saturating_sub(frame_to_millis(key_start_frame, sample_rate))
}

/// Frame count -> milliseconds at `sample_rate`, using the same u128-rounding pattern as
/// `consumed_samples_position_millis`/`seek_target_frame` to avoid overflow. `frame` here is a
/// plain (non-interleaved) frame count, unlike `consumed_samples_position_millis`'s interleaved
/// sample count.
fn frame_to_millis(frame: u64, sample_rate: u32) -> u64 {
    if sample_rate == 0 {
        return 0;
    }
    (u128::from(frame) * 1_000 / u128::from(sample_rate)) as u64
}

fn join_decoder_until(
    decoder: JoinHandle<()>,
    deadline: Instant,
    total_grace: Duration,
    role: &str,
) -> bool {
    let started_at = Instant::now();
    while !decoder.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(2));
    }
    if decoder.is_finished() {
        let _ = decoder.join();
        true
    } else {
        // File/NAS reads are not interruptible through Symphonia/WavPack. Cancellation is
        // already set by the caller; detach only after this bounded shutdown grace expires.
        eprintln!(
            "Lime Player shutdown: detached {role} at the {} ms total shutdown deadline (waited {} ms); cancellation was requested, but a blocked file/NAS read cannot be interrupted.",
            total_grace.as_millis(),
            started_at.elapsed().as_millis(),
        );
        drop(decoder);
        false
    }
}

fn prebuffer_samples(sample_rate: u32) -> usize {
    (sample_rate as usize * 2 * PREBUFFER_MILLISECONDS / 1000)
        .min(RING_SAMPLES / 2)
        .max(256)
}

fn consumed_samples_position_millis(consumed_samples: u64, sample_rate: u32) -> Option<u64> {
    if sample_rate == 0 {
        return None;
    }
    let complete_stereo_frames = u128::from(consumed_samples / 2);
    let millis = complete_stereo_frames * 1_000 / u128::from(sample_rate);
    u64::try_from(millis).ok()
}

/// `active`'s absolute consumed-samples position (no latency compensation, unlike
/// `audible_position_for`), converted to TRACK-RELATIVE milliseconds via `track_relative_millis`
/// (`CLAUDE.md` CUE sheet support §4). Used at the two Timeline-emission sites (a natural/graceful
/// drain, a retained playback failure) that report the exact consumed-samples position rather than
/// the latency-compensated audible one.
fn active_track_relative_position_ms(active: &ActivePlayback) -> u64 {
    let absolute_ms = consumed_samples_position_millis(
        active.consumed_output_samples.load(Ordering::Acquire),
        active.prepared.info.sample_rate,
    )
    .unwrap_or(0);
    track_relative_millis(absolute_ms, active.prepared.key.start_frame, active.prepared.info.sample_rate)
}

fn record_output_samples_consumed(counter: &AtomicU64, delivered_samples: u64) {
    if delivered_samples > 0 {
        counter.fetch_add(delivered_samples, Ordering::Release);
    }
}

/// RT-safe: pure arithmetic and one relaxed store, no allocation, lock or syscall.
/// `checked_duration_since` avoids `StreamInstant`'s panicking `Sub` path.
fn record_output_latency(latency: &AtomicU64, ts: OutputStreamTimestamp) {
    let nanos = ts
        .playback
        .checked_duration_since(ts.callback)
        .map(|d| d.as_nanos().min(u128::from(MAX_OUTPUT_LATENCY_NANOS)) as u64)
        .unwrap_or(0);
    latency.store(nanos, Ordering::Relaxed);
}

/// Estimated audible position, compensated for one output-buffer's worth of latency.
/// Accuracy is bounded by one hardware buffer; this is an estimate, not a measured
/// acoustic position. Clamped to `start_frame` so a seek's counter never reports behind
/// its own target.
fn audible_position_millis(
    consumed_samples: u64,
    start_frame: u64,
    latency_nanos: u64,
    sample_rate: u32,
) -> Option<u64> {
    if sample_rate == 0 {
        return None;
    }
    let frames = consumed_samples / 2; // output is always stereo
    let latency_frames = (u128::from(latency_nanos.min(MAX_OUTPUT_LATENCY_NANOS)) * u128::from(sample_rate)
        / 1_000_000_000) as u64;
    let audible = frames.saturating_sub(latency_frames).max(start_frame);
    u64::try_from(u128::from(audible) * 1_000 / u128::from(sample_rate)).ok()
}

fn uses_strict_integer_route(info: &AudioInfo) -> bool {
    cfg!(target_os = "macos") && info.integer_pcm
}

/// Pure decision: after `build_stream` returns, was the just-built stream constructed against a
/// physical DAC format that differs from what was explicitly selected right before `build_stream`
/// was called? CPAL only reports its virtual callback format and may itself change the device's
/// physical format while constructing the AudioUnit; an AudioUnit built against a stale physical
/// format stays stale even once the format is corrected afterward (root cause of the
/// "sampling config stays stuck" bug switching between the strict integer and float routes), so a
/// detected drift can never be safely "fixed" post hoc for an already-built stream — the caller
/// refuses this attempt rather than calling `play()` on it. Only the strict integer route pins an
/// exact physical format; the float route only needs the physical *rate*, checked separately.
fn needs_rebuild_after_post_build_check(
    integer_output: bool,
    pre_build_physical_format: &str,
    post_build_physical_format: &str,
) -> bool {
    integer_output && pre_build_physical_format != post_build_physical_format
}

/// Which physical output route a track uses. Derived from `uses_strict_integer_route` rather
/// than reimplemented, so a `Route` is always consistent with the one real predicate (`rg -n
/// uses_strict_integer_route src` must keep showing only its definition, its tests, and the two
/// production call sites in `spawn_track_preparation` and `start_track_prepared`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Strict,
    Float,
}

impl Route {
    fn from_integer_output(integer_output: bool) -> Route {
        if integer_output { Route::Strict } else { Route::Float }
    }
}

/// Pure decision: does this track start need a DAC-priming cycle (`§` DAC prime, `start_track_prepared`)
/// before the real stream is built? HAL property state (`before`/`after`, captured via
/// `coreaudio::capture_dac_state` and the post-settle snapshot) can already read "correct" at this
/// point even though the DAC hardware itself did not actually reconfigure — most notably on a
/// route change (strict integer <-> float), where the float route never pins an exact physical
/// format, so a before/after format-string comparison alone can miss it. `previous_route` is the
/// route the *previous* stream on this same device actually used (`None` when nothing has played
/// on it yet this session); comparing it against `new_route` catches that case independently of
/// the rate/format snapshot.
fn needs_dac_prime(
    before: &DacState,
    after: &DacState,
    previous_route: Option<Route>,
    new_route: Route,
) -> Option<&'static str> {
    if before.nominal_rate != after.nominal_rate {
        return Some("rate_changed");
    }
    if before.physical_format != after.physical_format {
        return Some("format_changed");
    }
    if let Some(prev) = previous_route
        && prev != new_route
    {
        return Some("route_changed");
    }
    None
}

/// Reads `LIME_DAC_PRIME_SETTLE_MS` once (defaulting to 150 ms on an unset or unparsable value)
/// and caches it for the process lifetime; `0` disables DAC priming entirely. There is no other
/// production-code env-var read in this crate (`src/ui_snapshot.rs`'s is test-only), so this is
/// deliberately the first: a defensive parse with a fallback rather than a hard failure, since a
/// malformed value must never block playback.
fn dac_prime_settle_ms() -> u64 {
    static SETTLE_MS: OnceLock<u64> = OnceLock::new();
    *SETTLE_MS.get_or_init(|| {
        std::env::var("LIME_DAC_PRIME_SETTLE_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(150)
    })
}

/// Pure: clamps a raw parsed LIME_DAC_PREROLL_MS value into the supported range. Extracted from
/// the OnceLock env reader below purely so it is independently unit-testable (unlike
/// `dac_prime_settle_ms`, whose OnceLock + env-var global state this repo does not unit test
/// directly).
fn clamp_preroll_ms(raw: Option<u64>) -> u64 {
    raw.unwrap_or(1000).min(3000)
}

/// Reads `LIME_DAC_PREROLL_MS` once (defaulting to 1000 ms on an unset or unparsable value),
/// clamps it to 0..=3000, and caches it for the process lifetime. `0` disables pre-roll entirely.
/// A defensive parse with a fallback, never a hard failure — malformed input must never block
/// playback (same policy as `dac_prime_settle_ms`).
fn dac_preroll_ms() -> u64 {
    static PREROLL_MS: OnceLock<u64> = OnceLock::new();
    *PREROLL_MS.get_or_init(|| {
        clamp_preroll_ms(std::env::var("LIME_DAC_PREROLL_MS").ok().and_then(|value| value.parse::<u64>().ok()))
    })
}

/// Pure: converts a pre-roll duration in milliseconds to a frame count at `sample_rate`, using
/// the same u128 rounding pattern as `seek_target_frame`/`audible_position_millis` to avoid
/// overflow.
fn dac_preroll_frames(preroll_ms: u64, sample_rate: u32) -> usize {
    ((u128::from(sample_rate) * u128::from(preroll_ms)) / 1000) as usize
}

/// Converts a requested seek position into a sample-accurate start frame, always leaving at
/// least 500 ms of audio so the track can prebuffer (`§4.2`).
fn seek_target_frame(position_ms: u64, sample_rate: u32, duration_ms: u64) -> u64 {
    let max_ms = duration_ms.saturating_sub(500);
    let ms = position_ms.min(max_ms);
    (u128::from(ms) * u128::from(sample_rate) / 1_000) as u64
}

/// The device and Hog mode a seek must pin to: whatever the active stream is already using,
/// never `self.selected_output`/`self.settings` (`§4.2`). Kept as a pure function so the
/// construction itself is unit-testable without touching CoreAudio.
fn pinned_output(active: &ActivePlayback) -> PinnedOutput {
    PinnedOutput { device_id: active.output_id.clone(), hog_mode: active.hog_mode }
}

/// The raw `DeviceId` string the volume slider should control (`§4.5`): the active stream's own
/// output while a track plays, otherwise the selected output. Never falls back further than
/// that, so idle-with-nothing-selected correctly resolves to `None`.
fn volume_target_raw_id(active_output_id: Option<&str>, selected_output: Option<&str>) -> Option<String> {
    active_output_id.or(selected_output).map(str::to_owned)
}

/// Parses `volume_target_raw_id`'s result into the bare CoreAudio device UID CoreAudio calls
/// expect, discarding the host prefix (`§4.5`). Pure string parsing: never touches CoreAudio, so
/// it is safe to unit-test directly, including with the fixture device-id strings tests already
/// use elsewhere in this module.
fn volume_target_uid(active_output_id: Option<&str>, selected_output: Option<&str>) -> Option<String> {
    volume_target_raw_id(active_output_id, selected_output)
        .and_then(|raw| DeviceId::from_str(&raw).ok())
        .map(|device_id| device_id.id().to_owned())
}

/// Whether a `Volume` event is worth emitting (`§4.5`): forced, or availability flipped, or the
/// level moved by more than the noise floor.
fn volume_changed_enough(forced: bool, available_changed: bool, previous_level: f32, new_level: f32) -> bool {
    forced || available_changed || (new_level - previous_level).abs() > VOLUME_CHANGE_EPSILON
}

/// The three `detail` strings `refresh_volume` can report (`§1.4`, `§4.5`). Hoisted into constants
/// so the honesty test asserts against what production actually emits instead of a hand-copied
/// duplicate (`§6` Stage 4 fix).
const VOLUME_DETAIL_NO_TARGET: &str = "Select an output device to control its volume.";
const VOLUME_DETAIL_AVAILABLE: &str = "Device volume (CoreAudio) on the current output.";
const VOLUME_DETAIL_NO_CONTROL: &str =
    "This device has no volume control. Adjust the level on your DAC or amplifier.";

/// `None` unless `uid` is the same device `last` stopped on with a different rate: same device,
/// same rate needs no guard at all, and a different device was never using this rate window
/// (`§4.6.5`). Otherwise, the remaining time (if any) left in the 90 ms drain guard.
fn rate_change_guard_wait(last: Option<&StoppedOutput>, uid: &str, new_rate: u32, now: Instant) -> Option<Duration> {
    let last = last?;
    let same_device = DeviceId::from_str(&last.device_id).ok().is_some_and(|d| d.id() == uid);
    if !same_device || last.rate == new_rate {
        return None;
    }
    OUTPUT_DRAIN_GUARD.checked_sub(now.duration_since(last.at)).filter(|d| !d.is_zero())
}

/// Drains same-key `Seek` commands already queued behind the one just received, keeping only
/// the latest target so a burst of scrub events collapses into a single seek. The first
/// non-matching command is returned (to be replayed as `pending_command`) so command order is
/// preserved (`§4.2`).
fn drain_same_key_seeks(commands: &Receiver<Command>, key: &TrackKey, initial_position_ms: u64) -> (u64, Option<Command>) {
    let mut position_ms = initial_position_ms;
    loop {
        match commands.try_recv() {
            Ok(Command::Seek { key: next_key, position_ms: next_position_ms }) if next_key == *key => {
                position_ms = next_position_ms;
            }
            Ok(other) => return (position_ms, Some(other)),
            Err(_) => return (position_ms, None),
        }
    }
}

fn format_mismatch_message(code: u8) -> Option<&'static str> {
    match code {
        MISMATCH_FLOAT_IN_I32 => Some("Strict I32 output received floating-point PCM."),
        MISMATCH_INTEGER_IN_F32 => {
            Some("F32 output received integer PCM without an explicit fallback conversion.")
        }
        _ => None,
    }
}

fn underrun_message(added_samples: u64, sample_rate: u32) -> String {
    let ms = if sample_rate == 0 { 0 } else { added_samples / 2 * 1_000 / u64::from(sample_rate) };
    format!("Playback underrun: the file could not be read fast enough (\u{2248}{ms} ms of silence inserted).")
}

/// Rate-limited underrun report: `Some` only when the count has grown since the last
/// report and at least `DIAGNOSTIC_REPORT_INTERVAL` has passed. Returns the message and
/// the sample count to remember as `reported` for the next call.
fn underrun_report(
    reported: u64,
    current: u64,
    last_at: Instant,
    now: Instant,
    sample_rate: u32,
) -> Option<(String, u64)> {
    if current <= reported || now.saturating_duration_since(last_at) < DIAGNOSTIC_REPORT_INTERVAL {
        return None;
    }
    Some((underrun_message(current - reported, sample_rate), current))
}

/// Unconditional underrun flush, bypassing the interval gate: `Some` whenever the count has
/// grown since the last report, regardless of how recently one was sent. Used right before a
/// track leaves `self.active` (completion or failure), so a pending underrun that the rate
/// limiter was still holding back is never silently dropped (`§1.4`: underruns stay visible).
fn flush_underrun_report(reported: u64, current: u64, sample_rate: u32) -> Option<(String, u64)> {
    if current <= reported {
        return None;
    }
    Some((underrun_message(current - reported, sample_rate), current))
}

/// Sets `eof` on every decoder exit, panic included, so `wait_until_prebuffered` and the
/// drain path never hang on a decoder that unwound without reaching its normal `Ok`/`Err`
/// return.
struct DecoderExitGuard {
    eof: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    error_slot: Arc<Mutex<Option<String>>>,
}

impl Drop for DecoderExitGuard {
    fn drop(&mut self) {
        if std::thread::panicking() && !self.cancel.load(Ordering::Acquire) {
            self.failed.store(true, Ordering::Release);
            *self.error_slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
                Some("the decoder stopped unexpectedly (internal error)".into());
        }
        self.eof.store(true, Ordering::Release);
    }
}

fn spawn_track_preparation(
    prepared: PreparedTrack,
    start_frame: u64,
    end_frame: Option<u64>,
    events: Sender<PlaybackEvent>,
) -> Result<TrackPreparation, String> {
    let integer_output = uses_strict_integer_route(&prepared.info);
    let (mut producer, consumer) = RingBuffer::<PcmSample>::new(RING_SAMPLES);
    let cancel = Arc::new(AtomicBool::new(false));
    let eof = Arc::new(AtomicBool::new(false));
    let drained = Arc::new(AtomicBool::new(false));
    let decoder_failed = Arc::new(AtomicBool::new(false));
    let buffered = Arc::new(AtomicUsize::new(0));
    let diagnostics = Arc::new(AudioDiagnostics::default());
    let decoder_error = Arc::new(Mutex::new(None));
    let prebuffer_samples = prebuffer_samples(prepared.info.sample_rate);
    let decoder_cancel = cancel.clone();
    let decoder_eof = eof.clone();
    let decoder_buffered = buffered.clone();
    let decoder_failure_flag = decoder_failed.clone();
    let decoder_diagnostics = diagnostics.clone();
    let decoder_error_slot = decoder_error.clone();
    let decoder_path = prepared.key.path.clone();
    // The output stream is configured from this below (`start_track_prepared`); the decoder must
    // refuse rather than decode a file that no longer matches it (`§1.4`).
    let decoder_expected_info = prepared.info.clone();
    let started_at = Instant::now();
    let decoder = thread::Builder::new()
        .name("lime-audio-decoder".into())
        .spawn(move || {
            let _exit_guard = DecoderExitGuard {
                eof: decoder_eof,
                failed: decoder_failure_flag.clone(),
                cancel: decoder_cancel.clone(),
                error_slot: decoder_error_slot.clone(),
            };
            let mut decoder_found_nonzero_sample = false;
            let result = decode_file(&decoder_path, &decoder_expected_info, &decoder_cancel, integer_output, start_frame, end_frame, |sample| {
                if decoder_cancel.load(Ordering::Acquire) {
                    return Err("Playback cancelled".into());
                }
                if !decoder_found_nonzero_sample && sample_is_nonzero(sample) {
                    decoder_found_nonzero_sample = true;
                    decoder_diagnostics.decoder_nonzero_sample.store(true, Ordering::Release);
                }
                while producer.is_full() {
                    if decoder_cancel.load(Ordering::Relaxed) {
                        return Err("Playback cancelled".into());
                    }
                    thread::sleep(Duration::from_millis(1));
                }
                producer.push(sample).map_err(|e| format!("PCM queue failed: {e}"))?;
                decoder_buffered.fetch_add(1, Ordering::Release);
                Ok(())
            });
            if let Err(error) = result {
                if !decoder_cancel.load(Ordering::Acquire) {
                    decoder_failure_flag.store(true, Ordering::Release);
                    *decoder_error_slot
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error.to_string());
                    let _ = events.send(PlaybackEvent::Status(format!("Decoder stopped: {error}")));
                }
            }
            // `_exit_guard`'s Drop sets `eof` here (and on every other exit path, panic
            // included).
        })
        .map_err(|e| format!("Could not start decoder thread: {e}"))?;

    Ok(TrackPreparation {
        prepared,
        audio: AudioPreparation {
            consumer: Some(consumer),
            decoder: Some(decoder),
            cancel,
            eof,
            drained,
            decoder_failed,
            buffered,
            diagnostics,
            decoder_error,
            prebuffer_samples,
        },
        started_at,
        prebuffer_ready_observed_at: None,
    })
}

/// Device-volume state the worker tracks between polls (`§4.5`). `uid` is the bare CoreAudio
/// device UID `refresh_volume` last resolved (`volume_target_uid`); `SetVolume` re-checks this
/// against the live target before every set (`§6` Stage 4 fix), since it is stale whenever
/// `self.active` changes without an intervening poll (track stop/drain).
struct VolumeState {
    uid: Option<String>,
    available: bool,
    level: f32,
    detail: String,
    polled_at: Instant,
}

impl VolumeState {
    fn new() -> Self {
        Self {
            uid: None,
            available: false,
            level: 0.0,
            detail: String::new(),
            polled_at: Instant::now(),
        }
    }
}

struct PlaybackWorker {
    settings: PlayerSettings,
    commands: Receiver<Command>,
    events: Sender<PlaybackEvent>,
    queue: VecDeque<PreparedTrack>,
    lookahead: Option<TrackPreparation>,
    failed_preparation_key: Option<TrackKey>,
    waiting_for_decoder_cancel: bool,
    last_completed: Option<PreparedTrack>,
    blocked_head: bool,
    active: Option<ActivePlayback>,
    selected_output: Option<String>,
    hog_lease: Option<(String, HogLease)>,
    volume: VolumeState,
    shutdown: bool,
    /// Decoders from tracks stopped by a seek/Previous/ReplaceQueue: never joined inline (a NAS
    /// read may block), reaped once finished by `reap_retiring_decoders` (`§4.2`).
    retiring_decoders: Vec<JoinHandle<()>>,
    /// The device/rate of the most recent `stop_active_now`, for the 90 ms rate-change guard
    /// (`§4.6.5`).
    last_stop: Option<StoppedOutput>,
    /// Tracks that left `self.active` by natural completion, a completed graceful skip, or a
    /// `ReplaceQueue` stop; capped at 200, oldest dropped first (`§3.7`).
    history: Vec<PreparedTrack>,
    /// A command dequeued by `drain_same_key_seeks` while coalescing same-key seeks, held so
    /// command order is preserved across the next `run()` iteration (`§4.2`).
    pending_command: Option<Command>,
}

impl PlaybackWorker {
    fn new(settings: PlayerSettings, commands: Receiver<Command>, events: Sender<PlaybackEvent>) -> Self {
        Self {
            selected_output: settings.output_device_id.clone(),
            settings,
            commands,
            events,
            queue: VecDeque::new(),
            lookahead: None,
            failed_preparation_key: None,
            waiting_for_decoder_cancel: false,
            last_completed: None,
            blocked_head: false,
            active: None,
            hog_lease: None,
            volume: VolumeState::new(),
            shutdown: false,
            retiring_decoders: Vec::new(),
            last_stop: None,
            history: Vec::new(),
            pending_command: None,
        }
    }

    fn run(mut self) {
        if let Ok(devices) = enumerate_outputs() {
            self.emit(PlaybackEvent::Devices(devices));
        }
        // First reading, forced: the UI has nothing to show until this arrives (`§4.5`).
        self.refresh_volume(true);
        while !self.shutdown {
            self.advance_if_drained();
            self.emit_first_output_pcm_callback_timing();
            self.emit_playback_timeline_if_due();
            self.emit_volume_if_due();
            self.reap_retiring_decoders();
            if let Some(command) = self.pending_command.take() {
                self.handle(command);
            } else {
                match self.commands.recv_timeout(Duration::from_millis(15)) {
                    Ok(command) => self.handle(command),
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => self.shutdown = true,
                }
            }
            self.reconcile_lookahead();
            self.resume_start_after_decoder_cancel();
        }
        self.shutdown_audio_workers();
        self.queue.clear();
        self.emit_queue_snapshot();
        self.hog_lease.take();
        self.emit(PlaybackEvent::Stopped);
    }

    /// Joins every retiring decoder that has finished; keeps the rest for a later tick.
    fn reap_retiring_decoders(&mut self) {
        let mut remaining = Vec::with_capacity(self.retiring_decoders.len());
        for handle in self.retiring_decoders.drain(..) {
            if handle.is_finished() {
                let _ = handle.join();
            } else {
                remaining.push(handle);
            }
        }
        self.retiring_decoders = remaining;
    }

    /// Adds `prepared` to the bounded session history (`§3.7`); the oldest entry is dropped
    /// first once the 200-track cap is exceeded.
    fn push_history(&mut self, prepared: PreparedTrack) {
        self.history.push(prepared);
        if self.history.len() > HISTORY_LIMIT {
            self.history.remove(0);
        }
    }

    fn shutdown_audio_workers(&mut self) {
        let deadline = Instant::now() + SHUTDOWN_DECODER_GRACE;
        // Signal speculative work first. A NAS read may be inside a blocking OS call, so the
        // active decoder join must never happen while lookahead is still uncancelled.
        if let Some(preparation) = self.lookahead.as_mut() {
            preparation.cancel();
        }
        // Retiring decoders already have their cancel flags set; join with the shared shutdown
        // deadline before touching the active decoder, same reasoning as the lookahead above.
        for handle in self.retiring_decoders.drain(..) {
            join_decoder_until(handle, deadline, SHUTDOWN_DECODER_GRACE, "retired decoder");
        }
        if let Some(active) = self.active.take() {
            active.finish_for_shutdown(deadline);
        }
        if let Some(mut preparation) = self.lookahead.take() {
            // This reaps immediately if cancellation completed while the active decoder exited;
            // otherwise it waits only until the shared shutdown deadline, then logs and detaches.
            preparation.finish_for_shutdown(deadline);
        }
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Enqueue(paths) => self.enqueue(paths),
            Command::PlayNext(tracks) => self.play_next(tracks),
            Command::ReplaceQueue(tracks) => self.replace_queue(tracks),
            Command::Previous => self.previous_track(),
            Command::Seek { key, position_ms } => {
                let (position_ms, next_command) = drain_same_key_seeks(&self.commands, &key, position_ms);
                self.pending_command = next_command;
                self.seek_to(key, position_ms);
            }
            Command::SelectOutput(id) => {
                self.selected_output = id.clone();
                self.settings.output_device_id = id;
                if self.active.is_some() {
                    self.emit(PlaybackEvent::Status(
                        "Output selection will apply when the next track starts.".into(),
                    ));
                } else {
                    self.emit(PlaybackEvent::Status("Output selection updated.".into()));
                    // Idle: the slider targets the selected output (`§4.5`), so a new selection
                    // must refresh it now rather than wait for the 1 s poll.
                    self.refresh_volume(true);
                    if !self.queue.is_empty() {
                        self.start_next();
                    }
                }
            }
            Command::SetVolume(level) => self.set_volume(level),
            Command::SetHogMode(enabled) => {
                self.settings.hog_mode_enabled = enabled;
                if self.active.is_some() {
                    self.emit(PlaybackEvent::Status(
                        "Hog mode preference will apply when the next track starts.".into(),
                    ));
                } else {
                    self.emit(PlaybackEvent::Status(if enabled {
                        "Hog mode is enabled for macOS playback.".into()
                    } else {
                        "Hog mode is disabled by user preference.".into()
                    }));
                    if !self.queue.is_empty() {
                        self.start_next();
                    }
                }
            }
            Command::TogglePlayback => self.toggle_playback(),
            Command::Next => self.next_track(),
            Command::Shutdown => self.shutdown = true,
        }
    }

    /// `tracks` were already probed by the library scanner (`§6` Stage 3); the worker no longer
    /// calls `probe_file` here (kept only for tests, via `fixture_track`).
    fn enqueue(&mut self, tracks: Vec<PreparedTrack>) {
        if tracks.is_empty() {
            self.emit_queue_snapshot();
            return;
        }
        if let Some(skipped) = recover_blocked_head_for_enqueue(
            &mut self.queue,
            &mut self.blocked_head,
            self.active.is_some(),
        ) {
            self.emit(PlaybackEvent::Status(format!(
                "Skipped previously failed track {}; continuing the queue.",
                display_name(&skipped.path)
            )));
        }
        self.queue.extend(tracks);
        self.emit_queue_snapshot();
        if self.active.is_none() {
            self.start_next();
        }
    }

    /// Inserts `tracks` in order at the front of the pending queue (`§3.7`); starts them if
    /// nothing is currently playing.
    fn play_next(&mut self, tracks: Vec<PreparedTrack>) {
        if tracks.is_empty() {
            return;
        }
        if let Some(skipped) =
            recover_blocked_head_for_enqueue(&mut self.queue, &mut self.blocked_head, self.active.is_some())
        {
            self.emit(PlaybackEvent::Status(format!(
                "Skipped previously failed track {}; continuing the queue.",
                display_name(&skipped.path)
            )));
        }
        for track in tracks.into_iter().rev() {
            self.queue.push_front(track);
        }
        self.emit_queue_snapshot();
        if self.active.is_none() {
            self.start_next();
        }
    }

    /// Stops the current track immediately (moving it to history), clears the pending queue,
    /// and starts `tracks` from the first item (`§3.7`, `§4.3`).
    fn replace_queue(&mut self, tracks: Vec<PreparedTrack>) {
        if tracks.is_empty() {
            return;
        }
        if self.active.is_some() && self.retiring_decoders_at_capacity() {
            self.emit(PlaybackEvent::Status(RETIRING_DECODERS_BUSY_MESSAGE.into()));
            return;
        }
        if let Some(current) = self.stop_active_now(true) {
            self.push_history(current);
        }
        self.queue.clear();
        self.queue.extend(tracks);
        self.blocked_head = false;
        self.failed_preparation_key = None;
        self.emit_queue_snapshot();
        self.start_next();
    }

    /// `pos < 3 s` (or an empty history) goes back to the previous track; otherwise the current
    /// track restarts from 0 (or, if it is unhealthy, is re-prepared from the queue) (`§4.3`).
    fn previous_track(&mut self) {
        if let Some(active) = self.active.as_ref() {
            let pos = self.current_position_ms(active);
            if pos >= 3_000 || self.history.is_empty() {
                let unhealthy = active.decoder_failed.load(Ordering::Acquire) || active.output_failed.load(Ordering::Acquire);
                if unhealthy {
                    self.restart_current_from_queue();
                } else {
                    let key = active.prepared.key.clone();
                    self.seek_to(key, 0); // position 0 needs no known duration
                }
                return;
            }
        }
        if self.active.is_some() && self.retiring_decoders_at_capacity() {
            self.emit(PlaybackEvent::Status(RETIRING_DECODERS_BUSY_MESSAGE.into()));
            return;
        }
        let Some(prev) = self.history.pop() else {
            self.emit(PlaybackEvent::Status("No previous track in this session.".into()));
            return;
        };
        if let Some(current) = self.stop_active_now(true) {
            self.queue.push_front(current);
        }
        self.queue.push_front(prev);
        self.blocked_head = false;
        self.emit_queue_snapshot();
        self.start_next();
    }

    /// Re-prepares the current (unhealthy) track from the queue head, from frame 0, with the
    /// current output/Hog selection — unlike a seek, which is pinned (`§4.3`).
    fn restart_current_from_queue(&mut self) {
        if self.retiring_decoders_at_capacity() {
            self.emit(PlaybackEvent::Status(RETIRING_DECODERS_BUSY_MESSAGE.into()));
            return;
        }
        if let Some(current) = self.stop_active_now(true) {
            self.queue.push_front(current);
        }
        self.blocked_head = false;
        self.emit_queue_snapshot();
        self.start_next();
    }

    fn current_position_ms(&self, active: &ActivePlayback) -> u64 {
        audible_position_for(active)
    }

    /// Whether `stop_active_now` would push `retiring_decoders` past its cap. Every immediate-stop
    /// caller (`seek_to`, `replace_queue`, `previous_track`, `restart_current_from_queue`) must
    /// check this first: a NAS read stuck in `lime-audio-decoder` is never joined inline, so a rapid
    /// run of stops would otherwise pile up blocked decoder threads with no limit (`§4.2`).
    fn retiring_decoders_at_capacity(&self) -> bool {
        self.retiring_decoders.len() >= MAX_RETIRING_DECODERS
    }

    /// Stops the active stream right now (cancel, pause, drop) and records it as the most
    /// recent stop for the rate-change guard. Its decoder is never joined inline (a NAS read
    /// may block); it moves to `retiring_decoders` for `reap_retiring_decoders` to reclaim.
    /// Returns the stopped track so the caller decides where it goes next (history, the queue
    /// head, or straight into a reseek).
    fn stop_active_now(&mut self, emit_playing: bool) -> Option<PreparedTrack> {
        let mut active = self.active.take()?;
        self.last_stop = Some(StoppedOutput {
            at: Instant::now(),
            device_id: active.output_id.clone(),
            rate: active.prepared.info.sample_rate,
            // Raw field, not `uses_strict_integer_route(&active.prepared.info)`: this call runs on
            // every platform (not `cfg(target_os = "macos")`), and `uses_strict_integer_route` must
            // keep exactly its 2 real call sites (`spawn_track_preparation`, `start_track_prepared`).
            // `Route::from_integer_output` reads this bool the same way either predicate would.
            integer_output: active.prepared.info.integer_pcm,
        });
        active.stop_output();
        if let Some(handle) = active.decoder.take() {
            self.retiring_decoders.push(handle);
        }
        // Flush any underrun growth the rate limiter is still holding back: an immediate stop
        // (Seek, Previous, ReplaceQueue, a restart) takes the track out of `self.active` right
        // here, and nothing else would ever report it otherwise (`§1.4`).
        if let Some((message, _)) = flush_underrun_report(
            active.reported_underrun_samples,
            active.diagnostics.underrun_samples.load(Ordering::Acquire),
            active.prepared.info.sample_rate,
        ) {
            self.emit(PlaybackEvent::Status(message));
        }
        if emit_playing {
            self.emit(PlaybackEvent::Playing(false));
        }
        Some(active.prepared.clone())
    }

    /// Cancels a preparation that will never start (a failed seek's attempt) and moves its
    /// decoder to `retiring_decoders`. Never assigned to `self.lookahead`: that slot may hold
    /// the live next-track preparation, which must not be dropped.
    fn retire_preparation(&mut self, mut prep: TrackPreparation) {
        prep.cancel();
        if let Some(handle) = prep.audio.decoder.take() {
            self.retiring_decoders.push(handle);
        }
    }

    /// A seek's guards were satisfied but the restart itself failed: puts the track back at the
    /// queue head, blocked, and reports it as a refusal, never a silent fallback (`§1.4`, `§4.2`).
    fn fail_seek(&mut self, prepared: PreparedTrack, error: String, prep: Option<TrackPreparation>) {
        let duration_ms = prepared.info.duration_ms;
        if let Some(prep) = prep {
            self.retire_preparation(prep);
        }
        // The queue head becomes the current track again, so the next-track lookahead no
        // longer matches. Cancel it but keep it in `self.lookahead`: the existing reap /
        // waiting_for_decoder_cancel logic then handles it exactly as any other mismatched head.
        if let Some(lookahead) = self.lookahead.as_mut() {
            lookahead.cancel();
        }
        self.queue.push_front(prepared);
        self.blocked_head = true;
        self.hog_lease.take();
        self.emit(PlaybackEvent::Playing(false));
        self.emit(PlaybackEvent::SeekRejected);
        self.emit(PlaybackEvent::Timeline { position_ms: 0, duration_ms });
        // No track is active once a seek fails (the Hog lease above is already released):
        // the UI must not keep showing the pre-failure route status or a seekable bar (`§1.4`).
        self.emit(PlaybackEvent::Inactive);
        self.emit_queue_snapshot();
        self.emit(PlaybackEvent::Status(format!(
            "Seek failed: {}. Press Play to retry from the start.",
            error.trim_end_matches('.')
        )));
    }

    /// A seek guard rejected the request outright, before ever touching the active stream.
    fn reject_seek(&mut self, message: impl Into<String>) {
        self.emit(PlaybackEvent::Status(message.into()));
        self.emit(PlaybackEvent::SeekRejected);
        if let Some(active) = self.active.as_ref() {
            self.emit(PlaybackEvent::Timeline {
                position_ms: self.current_position_ms(active),
                duration_ms: active.prepared.info.duration_ms,
            });
        }
    }

    /// Seeks the active track to `position_ms`, pinned to its current output device and Hog
    /// mode so the seek can never trigger a device change or a Hog acquire/release (`§4.2`).
    fn seek_to(&mut self, key: TrackKey, position_ms: u64) {
        let Some(active) = self.active.as_ref() else {
            self.reject_seek("Seek ignored: no track is active.");
            return;
        };
        if active.prepared.key != key {
            // Covers the race where `advance_if_drained` already moved on this tick.
            self.reject_seek("Seek ignored: the track changed.");
            return;
        }
        if active.cancel.load(Ordering::Acquire) {
            self.reject_seek("A skip is already pending.");
            return;
        }
        if active.decoder_failed.load(Ordering::Acquire) || active.output_failed.load(Ordering::Acquire) {
            self.reject_seek("Seeking is unavailable because playback failed.");
            return;
        }
        if position_ms > 0 && active.prepared.info.duration_ms.is_none() {
            self.reject_seek("This file does not report its length; seeking is unavailable.");
            return;
        }
        if self.retiring_decoders_at_capacity() {
            self.reject_seek(RETIRING_DECODERS_BUSY_MESSAGE);
            return;
        }

        // `info.duration_ms` is already the *sub-range* duration for a CUE track (overridden by
        // the scanner at expansion time), so `seek_target_frame`'s clamp is against the sub-range's
        // own length, and its result is already track-relative (0 at this track's own start) —
        // `key.start_frame` is added below to get the absolute physical-file decode frame.
        let relative_frame = if position_ms == 0 {
            0
        } else {
            seek_target_frame(position_ms, active.prepared.info.sample_rate, active.prepared.info.duration_ms.unwrap())
        };
        let start_frame = active.prepared.key.start_frame + relative_frame;
        let end_frame = active.prepared.key.end_frame;
        let was_playing = active.playing;
        let pinned = pinned_output(active);

        // Guards above already confirmed `self.active` is `Some`.
        let Some(prepared) = self.stop_active_now(false) else { return };

        match spawn_track_preparation(prepared.clone(), start_frame, end_frame, self.events.clone()) {
            Ok(preparation) => {
                let options = StartOptions {
                    start_frame,
                    start_playing: was_playing,
                    reason: StartReason::Seek,
                    pinned_output: Some(pinned),
                };
                if let Err((error, prep)) = self.start_track(prepared.clone(), preparation, None, options) {
                    self.fail_seek(prepared, error, Some(prep));
                }
            }
            Err(error) => self.fail_seek(prepared, error, None),
        }
    }

    fn start_next(&mut self) {
        self.start_next_from_drain(None);
    }

    fn start_next_from_drain(&mut self, worker_drain_observed_at: Option<Instant>) {
        if self.active.is_some() {
            return;
        }
        let Some(prepared) = self.queue.front().cloned() else {
            self.emit_queue_snapshot();
            self.hog_lease.take();
            self.emit(PlaybackEvent::Stopped);
            return;
        };
        let preparation = match self.preparation_for_start(&prepared) {
            Ok(preparation) => preparation,
            Err(error) => {
                if !self.waiting_for_decoder_cancel {
                    self.hog_lease.take();
                    self.blocked_head = true;
                    self.emit_queue_snapshot();
                    // `self.active` is already `None` here (checked above), but whatever ended the
                    // previous track — a natural drain, in particular — never told the UI the
                    // route is now dead; without this it would keep showing the old route status
                    // and a seekable bar for a track that finished (`§1.4`).
                    self.emit(PlaybackEvent::Inactive);
                    self.emit(PlaybackEvent::Status(error));
                }
                return;
            }
        };
        let options = StartOptions {
            // A CUE sub-range track's fresh (non-seek) start decodes from its own INDEX 01, not
            // absolute file frame 0; a whole-file track's `key.start_frame` is always 0.
            start_frame: prepared.key.start_frame,
            start_playing: true,
            reason: StartReason::NewTrack,
            pinned_output: None,
        };
        if let Err((error, mut preparation)) =
            self.start_track(prepared, preparation, worker_drain_observed_at, options)
        {
            self.hog_lease.take();
            self.blocked_head = true;
            if !preparation.audio.decoder_finished() {
                preparation.cancel();
                self.lookahead = Some(preparation);
            } else {
                preparation.audio.reap_finished_decoder();
            }
            // Keep the failed item at the queue head. A user can change the output/Hog
            // setting or press Play to retry it. A new open-file request skips this blocked
            // head only when no stream is active; Next remains an explicit skip as well.
            self.emit_queue_snapshot();
            // Same as above: nothing else signals that the route died when the failure happens
            // right here, on the very attempt to start the next track (`§1.4`).
            self.emit(PlaybackEvent::Inactive);
            self.emit(PlaybackEvent::Status(error));
            return;
        }
        self.queue.pop_front();
        self.blocked_head = false;
        self.waiting_for_decoder_cancel = false;
        self.emit_queue_snapshot();
        self.reconcile_lookahead();
    }

    fn preparation_for_start(&mut self, prepared: &PreparedTrack) -> Result<TrackPreparation, String> {
        if let Some(mut preparation) = self.lookahead.take() {
            if preparation.prepared.key == prepared.key
                && !preparation.audio.cancel.load(Ordering::Acquire)
            {
                return Ok(preparation);
            }
            preparation.cancel();
            if !preparation.audio.reap_finished_decoder() {
                self.lookahead = Some(preparation);
                self.blocked_head = false;
                if !self.waiting_for_decoder_cancel {
                    self.emit(PlaybackEvent::Status(
                        "Waiting for the previous next-track decoder to stop before opening the new queue head.".into(),
                    ));
                }
                self.waiting_for_decoder_cancel = true;
                return Err("A previous next-track decoder is still stopping.".into());
            }
        }
        self.failed_preparation_key = None;
        self.waiting_for_decoder_cancel = false;
        spawn_track_preparation(prepared.clone(), prepared.key.start_frame, prepared.key.end_frame, self.events.clone())
    }

    fn reconcile_lookahead(&mut self) {
        if self.shutdown {
            if let Some(preparation) = self.lookahead.as_mut() {
                preparation.cancel();
                preparation.audio.reap_finished_decoder();
            }
            return;
        }
        let Some(prepared) = self
            .active
            .as_ref()
            .and_then(|_| self.queue.front().cloned())
        else {
            let can_reap = if let Some(preparation) = self.lookahead.as_mut() {
                preparation.cancel();
                preparation.audio.reap_finished_decoder()
            } else {
                false
            };
            if can_reap {
                self.lookahead = None;
            }
            return;
        };
        if self.failed_preparation_key.as_ref().is_some_and(|key| key != &prepared.key) {
            self.failed_preparation_key = None;
        }

        let has_current_head = self.lookahead.as_ref().is_some_and(|preparation| {
            preparation.prepared.key == prepared.key
                && !preparation.audio.cancel.load(Ordering::Acquire)
        });
        if has_current_head {
            if let Some((elapsed, buffered)) = self
                .lookahead
                .as_mut()
                .and_then(TrackPreparation::mark_prebuffer_ready)
            {
                self.emit(PlaybackEvent::Diagnostic(format!(
                    "Next-track PCM prebuffer ready: {} ms · {buffered} samples.",
                    elapsed.as_millis()
                )));
            }
            return;
        }

        let can_reap = if let Some(preparation) = self.lookahead.as_mut() {
            preparation.cancel();
            preparation.audio.reap_finished_decoder()
        } else {
            true
        };
        if !can_reap {
            return;
        }
        if self.lookahead.is_some() {
            self.lookahead = None;
        }

        if self.failed_preparation_key.as_ref() == Some(&prepared.key) {
            return;
        }
        match spawn_track_preparation(prepared.clone(), prepared.key.start_frame, prepared.key.end_frame, self.events.clone()) {
            Ok(preparation) => {
                self.failed_preparation_key = None;
                self.lookahead = Some(preparation);
            }
            Err(error) => {
                self.failed_preparation_key = Some(prepared.key.clone());
                self.emit(PlaybackEvent::Status(format!(
                    "Could not prepare next track audio: {error}"
                )));
            }
        }
    }

    fn resume_start_after_decoder_cancel(&mut self) {
        if self.shutdown || !self.waiting_for_decoder_cancel {
            return;
        }
        if self
            .lookahead
            .as_ref()
            .is_some_and(|preparation| !preparation.audio.decoder_finished())
        {
            return;
        }
        if let Some(mut preparation) = self.lookahead.take() {
            preparation.audio.reap_finished_decoder();
        }
        self.waiting_for_decoder_cancel = false;
        if self.active.is_none() && !self.blocked_head && !self.queue.is_empty() {
            self.start_next();
        }
    }

    fn start_track(
        &mut self,
        prepared: PreparedTrack,
        mut preparation: TrackPreparation,
        worker_drain_observed_at: Option<Instant>,
        options: StartOptions,
    ) -> Result<(), (String, TrackPreparation)> {
        let result = self.start_track_prepared(&prepared, &mut preparation, worker_drain_observed_at, options);
        result.map_err(|error| (error, preparation))
    }

    fn start_track_prepared(
        &mut self,
        prepared: &PreparedTrack,
        preparation: &mut TrackPreparation,
        worker_drain_observed_at: Option<Instant>,
        options: StartOptions,
    ) -> Result<(), String> {
        let mut timings = TrackStartTimings::default();
        let selected_id = match &options.pinned_output {
            Some(pinned) => pinned.device_id.clone(),
            None => self
                .selected_output
                .as_deref()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| "Choose an output DAC before opening audio.".to_owned())?
                .to_owned(),
        };
        let hog_mode_enabled = options.pinned_output.as_ref().map_or(self.settings.hog_mode_enabled, |p| p.hog_mode);
        let device_id = DeviceId::from_str(&selected_id)
            .map_err(|e| format!("Saved output device is invalid: {e}"))?;
        let host = cpal::default_host();
        let device = host
            .device_by_id(&device_id)
            .ok_or_else(|| "Selected output device is no longer available.".to_owned())?;
        let description = device.description().map_err(|e| e.to_string())?;
        let output_label = description.name().to_owned();
        let integer_output = uses_strict_integer_route(&prepared.info);
        if integer_output && prepared.info.source_channels != 2 {
            return Err("Strict integer output requires stereo PCM; mono-to-stereo conversion is not allowed.".into());
        }
        let (sample_format, config) = exact_output_config(&device, prepared.info.sample_rate, integer_output)?;

        // Decode/prebuffer is speculative and does not touch CoreAudio or the existing Hog
        // lease. This wait occurs only once the prior active stream has fully drained.
        let prebuffer_wait = preparation.audio.wait_until_prebuffered()?;
        timings.prebuffer_wait = prebuffer_wait;
        let ready_observed_at = preparation
            .prebuffer_ready_observed_at
            .get_or_insert_with(Instant::now);
        timings.worker_observed_preparation_to_prebuffer_ready =
            ready_observed_at.duration_since(preparation.started_at);

        #[cfg(target_os = "macos")]
        {
            let hog_started_at = Instant::now();
            let uid = device_id.id();
            if hog_mode_enabled {
                let needs_lease = self.hog_lease.as_ref().is_none_or(|(current_uid, _)| current_uid != uid);
                if needs_lease {
                    self.hog_lease.take();
                    let lease = coreaudio::acquire_hog(uid)?;
                    self.hog_lease = Some((uid.to_owned(), lease));
                }
            } else {
                self.hog_lease.take();
            }
            timings.hog_setup = Some(hog_started_at.elapsed());
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.hog_lease.take();
            if hog_mode_enabled {
                self.emit(PlaybackEvent::Status("Hog mode is only available on macOS.".into()));
            }
        }

        #[cfg(target_os = "macos")]
        let (audio_device_id, pre_build_physical_format, did_dac_prime) = {
            let uid = device_id.id();
            // Captured before anything below touches the device, so it reflects whatever the
            // DAC was actually doing at the end of the *previous* start — independent of the
            // rate/format snapshot taken further down, which can already read "correct" even when
            // the hardware itself never actually reconfigured (see `needs_dac_prime`). A capture
            // failure (e.g. device not resolvable yet) must never block the track, so it just
            // disables the prime-need check for this start.
            let before_dac_state = coreaudio::capture_dac_state(uid).ok();
            let previous_route = self
                .last_stop
                .as_ref()
                .filter(|stopped| stopped.device_id == selected_id)
                .map(|stopped| Route::from_integer_output(stopped.integer_output));
            let new_route = Route::from_integer_output(integer_output);
            if let Some(wait) = rate_change_guard_wait(self.last_stop.as_ref(), uid, prepared.info.sample_rate, Instant::now()) {
                thread::sleep(wait);
            }
            let nominal_rate_started_at = Instant::now();
            let audio_device_id = coreaudio::set_and_verify_sample_rate(uid, prepared.info.sample_rate)?;
            // The nominal-rate readback above can report the new rate while the physical stream is
            // still mid-switch. Wait for the physical side to settle, then (strict route only)
            // select the exact integer physical format, BEFORE `build_stream` — never after: a CPAL
            // AudioUnit built against a stale physical format stays stale even once the format is
            // corrected afterward. That ordering (fix-after-build) is the root cause of the
            // "sampling config stays stuck" bug when switching between the strict integer and float
            // routes; see the bugfix memory saved under this change.
            coreaudio::wait_for_physical_sample_rate(
                audio_device_id,
                prepared.info.sample_rate,
                PHYSICAL_RATE_SETTLE_TIMEOUT,
            )?;
            if integer_output {
                coreaudio::ensure_exact_integer_output(audio_device_id, prepared.info.sample_rate)?;
            }
            timings.nominal_rate_setup = Some(nominal_rate_started_at.elapsed());
            let mut physical_format = coreaudio::get_physical_format_description(audio_device_id)
                .unwrap_or_else(|error| format!("unavailable ({error})"));
            self.emit_dac_format_diagnostic("before build", prepared.info.sample_rate, &physical_format);

            // DAC priming: even though the HAL property state read above already looks correct
            // (or already looked correct on a prior track's own before-build snapshot), the DAC
            // hardware itself sometimes keeps its old clock/route across a stream rebuild — the
            // known FiiO K11 bug where only a manual "press play again" (an IO stop+start at the
            // now-already-correct rate) fixes it. Reproduce that stop+start here, before the real
            // stream exists, whenever a rate/format/route change is detected.
            let settle_ms = dac_prime_settle_ms();
            // Computed once, from the exact same `needs_dac_prime` call used to decide whether to
            // run the priming-stream cycle below — reused verbatim (never re-evaluated) to also
            // decide whether the real stream needs a silent pre-roll (see `dac_preroll_frames`).
            // `settle_ms == 0` disables priming entirely, and therefore also disables pre-roll: a
            // disabled prime can never itself demute/relock the DAC, so there is nothing for a
            // pre-roll to cover in that configuration.
            let prime_reason = match (before_dac_state.as_ref(), settle_ms > 0) {
                (Some(before_state), true) => {
                    let after_state =
                        DacState { nominal_rate: prepared.info.sample_rate, physical_format: physical_format.clone() };
                    needs_dac_prime(before_state, &after_state, previous_route, new_route)
                }
                _ => None,
            };
            if let Some(reason) = prime_reason {
                let prime_started_at = Instant::now();
                match build_priming_stream(&device, config, sample_format).and_then(|priming_stream| {
                    priming_stream.play().map_err(|error| error.to_string())?;
                    Ok(priming_stream)
                }) {
                    Ok(priming_stream) => {
                        thread::sleep(Duration::from_millis(settle_ms));
                        let _ = priming_stream.pause();
                        drop(priming_stream);
                        thread::sleep(Duration::from_millis(settle_ms));
                        // Re-confirm the strict physical format now that the priming stream is
                        // gone, and refresh the snapshot used by the post-build drift check and
                        // the "before build" diagnostic below so it reflects post-priming state.
                        if integer_output
                            && let Err(error) =
                                coreaudio::ensure_exact_integer_output(audio_device_id, prepared.info.sample_rate)
                        {
                            self.emit(PlaybackEvent::Diagnostic(format!(
                                "DAC prime failed · reason={reason} · error={error}"
                            )));
                        }
                        physical_format = coreaudio::get_physical_format_description(audio_device_id)
                            .unwrap_or_else(|error| format!("unavailable ({error})"));
                        let took_ms = prime_started_at.elapsed().as_millis();
                        self.emit(PlaybackEvent::Diagnostic(format!(
                            "DAC prime · reason={reason} · settle_ms={settle_ms} · took_ms={took_ms}"
                        )));
                    }
                    Err(error) => {
                        self.emit(PlaybackEvent::Diagnostic(format!(
                            "DAC prime failed · reason={reason} · error={error}"
                        )));
                    }
                }
            }

            (audio_device_id, physical_format, prime_reason.is_some())
        };
        #[cfg(not(target_os = "macos"))]
        let audio_device_id = 0;
        #[cfg(not(target_os = "macos"))]
        let did_dac_prime = false;

        let output_failed = Arc::new(AtomicBool::new(false));
        let format_mismatch = Arc::new(AtomicU8::new(0));
        let consumed_output_samples = Arc::new(AtomicU64::new(options.start_frame * 2));
        let output_latency_nanos = Arc::new(AtomicU64::new(0));
        let error_sender = self.events.clone();
        // Only a start that actually primed the DAC (`did_dac_prime`) can leave it muted/relocking
        // once the real stream begins; a seek or an unchanged-rate/route start never primes and
        // therefore never pre-rolls.
        let preroll_frames = if did_dac_prime { dac_preroll_frames(dac_preroll_ms(), prepared.info.sample_rate) } else { 0 };
        if preroll_frames > 0 {
            self.emit(PlaybackEvent::Diagnostic(format!(
                "DAC preroll · ms={} · frames={preroll_frames}",
                dac_preroll_ms()
            )));
        }
        let stream_build_started_at = Instant::now();
        let stream_result = build_stream(
            &device,
            config,
            sample_format,
            preroll_frames,
            preparation.audio.consumer.take().ok_or_else(|| "The prepared audio buffer is unavailable.".to_owned())?,
            preparation.audio.buffered.clone(),
            preparation.audio.eof.clone(),
            preparation.audio.drained.clone(),
            output_failed.clone(),
            format_mismatch.clone(),
            preparation.audio.diagnostics.clone(),
            consumed_output_samples.clone(),
            output_latency_nanos.clone(),
            error_sender,
        );
        let stream = match stream_result {
            Ok(stream) => stream,
            Err(error) => {
                preparation.cancel();
                return Err(format!("Could not create audio output stream: {error}"));
            }
        };
        timings.stream_build = stream_build_started_at.elapsed();

        #[cfg(target_os = "macos")]
        let format_check = {
            let format_verify_started_at = Instant::now();
            let post_build_physical_format = coreaudio::get_physical_format_description(audio_device_id)
                .unwrap_or_else(|error| format!("unavailable ({error})"));
            self.emit_dac_format_diagnostic("after build", prepared.info.sample_rate, &post_build_physical_format);
            // CPAL only reports its virtual callback format and may itself change the device's
            // *physical* format while constructing the AudioUnit, even though it was already set
            // correctly right before `build_stream` above. When that happens the just-built
            // AudioUnit is initialized against whatever it settled on; restoring the hardware
            // format at this point cannot make an already-built stream pick it up (that is the
            // exact staleness this fix addresses), so a detected drift on the strict route is
            // refused outright instead of calling `play()` on a stream built before the final
            // format change. The float route only ever needs the physical *rate*, checked
            // separately since it does not pin an exact physical format.
            let result = if needs_rebuild_after_post_build_check(
                integer_output,
                &pre_build_physical_format,
                &post_build_physical_format,
            ) {
                Err(format!(
                    "DAC physical format changed while building the output stream (before: {pre_build_physical_format}; after: {post_build_physical_format}); refusing to play a stale stream."
                ))
            } else if !integer_output {
                coreaudio::verify_physical_sample_rate(audio_device_id, prepared.info.sample_rate)
            } else {
                Ok(())
            };
            timings.physical_format_setup = Some(format_verify_started_at.elapsed());
            result
        };
        #[cfg(not(target_os = "macos"))]
        let format_check: Result<(), String> = Ok(());
        if let Err(error) = format_check {
            preparation.cancel();
            drop(stream);
            return Err(error);
        }

        let stream_play_started_at = Instant::now();
        // CPAL may auto-start some backends regardless of an explicit pause, so a track that
        // should start paused (a seek that landed on a paused track) is explicitly paused right
        // after creation rather than never played.
        if options.start_playing {
            if let Err(error) = stream.play() {
                preparation.cancel();
                return Err(format!("Could not start DAC output stream: {error}"));
            }
        } else if let Err(error) = stream.pause() {
            preparation.cancel();
            return Err(format!("Could not pause DAC output stream: {error}"));
        }
        #[cfg(target_os = "macos")]
        {
            let physical_format = coreaudio::get_physical_format_description(audio_device_id)
                .unwrap_or_else(|error| format!("unavailable ({error})"));
            self.emit_dac_format_diagnostic("after play", prepared.info.sample_rate, &physical_format);
        }
        let stream_play_returned_at = Instant::now();
        timings.stream_play_call = stream_play_returned_at.duration_since(stream_play_started_at);
        let stream_play_returned_stream_time = stream.now();
        let stream_play_returned_unix_ms = unix_time_millis();

        match options.reason {
            StartReason::NewTrack => {
                let title = display_name(&prepared.key.path);
                let parent = prepared.key.path.parent().map(display_path).unwrap_or_else(|| "Audio file".into());
                self.emit(PlaybackEvent::Started {
                    key: prepared.key.clone(),
                    info: prepared.info.clone(),
                    title,
                    album: parent,
                    artist: "Unknown Artist".into(),
                    output: output_label,
                    hog_mode: cfg!(target_os = "macos") && hog_mode_enabled,
                    integer_output_verified: integer_output,
                });
                self.emit(PlaybackEvent::Timeline {
                    position_ms: 0,
                    duration_ms: prepared.info.duration_ms,
                });
                self.emit(PlaybackEvent::Playing(true));
                // A new track may have started on a different device than whatever the slider
                // last showed; refresh now rather than wait for the 1 s poll (`§4.5`). `self.active`
                // is not assigned yet, so this still resolves through `self.selected_output`,
                // which is exactly `selected_id` above for every `NewTrack` start (`pinned_output`
                // is always `None` here; only a `Seek` restart pins to a different device).
                self.refresh_volume(true);
                if let Some(worker_drain_observed_at) = worker_drain_observed_at {
                    let diagnostic = handoff_timing_diagnostic(
                        &prepared.key.path.display().to_string(),
                        &prepared.info.format,
                        prepared.info.sample_rate,
                        stream_play_returned_at.saturating_duration_since(worker_drain_observed_at),
                        &timings,
                    );
                    // This timestamp is captured when the worker first observes the callback's
                    // drained flag (and starts the drain guard), not at callback time or DAC
                    // presentation time. Keep the acoustic gap explicitly unmeasured.
                    // Developer-only: main.rs prints every Diagnostic line with a "DIAG" prefix
                    // and never shows it in the UI.
                    self.emit(PlaybackEvent::Diagnostic(diagnostic));
                }
            }
            StartReason::Seek => {
                // No `Started` and no handoff diagnostic: the track itself has not changed.
                // `consumed_samples_position_millis` gives the *absolute* file position; the
                // emitted Timeline must be track-relative (0 at this track's own `key.start_frame`),
                // subtracted here via `track_relative_millis` so it is never double-subtracted or
                // forgotten on this code path (`CLAUDE.md` CUE sheet support, §4).
                let absolute_ms =
                    consumed_samples_position_millis(options.start_frame * 2, prepared.info.sample_rate).unwrap_or(0);
                let position_ms = track_relative_millis(absolute_ms, prepared.key.start_frame, prepared.info.sample_rate);
                self.emit(PlaybackEvent::Timeline { position_ms, duration_ms: prepared.info.duration_ms });
                self.emit(PlaybackEvent::Playing(options.start_playing));
            }
        }
        self.active = Some(ActivePlayback {
            prepared: prepared.clone(),
            stream: Some(stream),
            decoder: preparation.audio.decoder.take(),
            cancel: preparation.audio.cancel.clone(),
            drained: preparation.audio.drained.clone(),
            decoder_failed: preparation.audio.decoder_failed.clone(),
            output_failed,
            format_mismatch,
            diagnostics: preparation.audio.diagnostics.clone(),
            reported_diagnostics: AudioDiagnosticsSnapshot::default(),
            reported_underrun_samples: 0,
            underrun_reported_at: Instant::now(),
            stream_play_returned_stream_time,
            stream_play_returned_unix_ms,
            first_output_pcm_callback_reported: false,
            consumed_output_samples,
            output_latency_nanos,
            start_frame: options.start_frame,
            timeline_reported_samples: options.start_frame * 2,
            timeline_reported_at: Instant::now(),
            drain_started: None,
            playing: options.start_playing,
            skip_requested: false,
            output_id: selected_id,
            hog_mode: hog_mode_enabled,
        });
        Ok(())
    }

    fn advance_if_drained(&mut self) {
        self.emit_audio_diagnostics_if_changed();
        let Some(active) = self.active.as_mut() else { return; };
        if active.output_failed.load(Ordering::Acquire) {
            let mismatch_code = active.format_mismatch.load(Ordering::Relaxed);
            let detail = format_mismatch_message(mismatch_code);
            self.finish_active_failure(PlaybackFailure::Output, detail);
            return;
        }
        if !active.drained.load(Ordering::Acquire) {
            return;
        }
        if !drain_guard_elapsed(
            active.drained.load(Ordering::Acquire),
            &mut active.drain_started,
            Instant::now(),
        ) {
            return;
        }
        let worker_drain_observed_at = active.drain_started.unwrap_or_else(Instant::now);
        if active.decoder_failed.load(Ordering::Acquire) && !active.skip_requested {
            self.finish_active_failure(PlaybackFailure::Decoder, None);
            return;
        }
        let active = self.active.take().unwrap();
        // Flush any underrun growth the rate limiter is still holding back: the track is
        // about to leave `self.active`, and nothing else would ever report it (`§1.4`).
        if let Some((message, _)) = flush_underrun_report(
            active.reported_underrun_samples,
            active.diagnostics.underrun_samples.load(Ordering::Acquire),
            active.prepared.info.sample_rate,
        ) {
            self.emit(PlaybackEvent::Status(message));
        }
        self.emit(PlaybackEvent::Timeline {
            position_ms: active_track_relative_position_ms(&active),
            duration_ms: active.prepared.info.duration_ms,
        });
        let naturally_completed = (!active.skip_requested).then(|| active.prepared.clone());
        // A track that finishes draining here — whether by natural completion or a completed
        // graceful skip — leaves for a reason other than Previous or a seek, so it always joins
        // history (`§3.7`).
        self.push_history(active.prepared.clone());
        active.finish();
        if let Some(prepared) = naturally_completed {
            self.last_completed = Some(prepared);
        }
        self.emit(PlaybackEvent::Playing(false));
        self.start_next_from_drain(Some(worker_drain_observed_at));
    }

    /// Signal-flag changes (decoded/callback nonzero) are developer diagnostics: emitted
    /// as `Diagnostic` whenever they change, never rate-limited. Underruns are a real
    /// degradation, so they stay a rate-limited, user-facing `Status`.
    fn emit_audio_diagnostics_if_changed(&mut self) {
        let signal_status = {
            let Some(active) = self.active.as_mut() else { return; };
            let snapshot = active.diagnostics.snapshot();
            let signal_state_changed = snapshot.decoder_nonzero_sample
                != active.reported_diagnostics.decoder_nonzero_sample
                || snapshot.callback_nonzero_sample
                    != active.reported_diagnostics.callback_nonzero_sample;
            active.reported_diagnostics = snapshot;
            signal_state_changed.then(|| snapshot.status())
        };
        if let Some(status) = signal_status {
            self.emit(PlaybackEvent::Diagnostic(status));
        }
        self.emit_underrun_status_if_due();
    }

    fn emit_underrun_status_if_due(&mut self) {
        let message = {
            let Some(active) = self.active.as_mut() else { return; };
            let current = active.diagnostics.underrun_samples.load(Ordering::Acquire);
            let Some((message, reported)) = underrun_report(
                active.reported_underrun_samples,
                current,
                active.underrun_reported_at,
                Instant::now(),
                active.prepared.info.sample_rate,
            ) else {
                return;
            };
            active.reported_underrun_samples = reported;
            active.underrun_reported_at = Instant::now();
            message
        };
        self.emit(PlaybackEvent::Status(message));
    }

    fn emit_first_output_pcm_callback_timing(&mut self) {
        let status = {
            let Some(active) = self.active.as_mut() else { return; };
            if active.first_output_pcm_callback_reported {
                return;
            }
            let Some(first_playback_time) = first_output_pcm_playback_time(&active.diagnostics) else {
                return;
            };
            active.first_output_pcm_callback_reported = true;
            let (playback_estimate, preceded_play_return) = playback_estimate_from_play_return(
                first_playback_time,
                active.stream_play_returned_stream_time,
            );
            first_output_pcm_callback_status(
                playback_estimate,
                preceded_play_return,
                active.stream_play_returned_unix_ms,
                unix_time_millis(),
            )
        };
        self.emit(PlaybackEvent::Diagnostic(status));
    }

    fn emit_playback_timeline_if_due(&mut self) {
        let (position_ms, duration_ms) = {
            let Some(active) = self.active.as_mut() else { return; };
            if active.timeline_reported_at.elapsed() < PLAYBACK_TIMELINE_REPORT_INTERVAL {
                return;
            }
            let consumed_samples = active.consumed_output_samples.load(Ordering::Acquire);
            if consumed_samples == active.timeline_reported_samples {
                return;
            }
            active.timeline_reported_samples = consumed_samples;
            active.timeline_reported_at = Instant::now();
            (audible_position_for(active), active.prepared.info.duration_ms)
        };
        self.emit(PlaybackEvent::Timeline { position_ms, duration_ms });
    }

    /// The cheap HAL re-read on the run loop (`§4.5`): a plain, unforced `refresh_volume`, so it
    /// only emits when the device level actually moved (someone turned a physical knob) or
    /// availability changed.
    fn emit_volume_if_due(&mut self) {
        if self.volume.polled_at.elapsed() < VOLUME_POLL_INTERVAL {
            return;
        }
        self.refresh_volume(false);
    }

    /// Re-reads the volume of the device the slider currently targets (`volume_target_uid`) and
    /// emits a `Volume` event when the change is meaningful (`volume_changed_enough`) or
    /// `force_emit` is set (`§4.5`). Always runs on the worker thread: RT-safety requires every
    /// HAL call stay off the CPAL data callback.
    fn refresh_volume(&mut self, force_emit: bool) {
        self.volume.polled_at = Instant::now();
        let uid = volume_target_uid(
            self.active.as_ref().map(|active| active.output_id.as_str()),
            self.selected_output.as_deref(),
        );
        self.volume.uid = uid.clone();
        let (available, level, detail) = match uid {
            None => (false, self.volume.level, VOLUME_DETAIL_NO_TARGET.to_owned()),
            Some(uid) => match coreaudio::device_volume(&uid) {
                Ok(Some(level)) => (true, level, VOLUME_DETAIL_AVAILABLE.to_owned()),
                Ok(None) => (false, self.volume.level, VOLUME_DETAIL_NO_CONTROL.to_owned()),
                Err(error) => (false, self.volume.level, error),
            },
        };
        let available_changed = available != self.volume.available;
        // A detail-only change (e.g. a device goes from "no volume control" to "no longer
        // available" while `available` stays false) must still reach the UI, or the tooltip/right
        // panel goes stale (`§4.5` rule 4, `§6` Stage 4 fix).
        let detail_changed = detail != self.volume.detail;
        let should_emit =
            volume_changed_enough(force_emit, available_changed || detail_changed, self.volume.level, level);
        self.volume.available = available;
        self.volume.level = level;
        self.volume.detail = detail.clone();
        if should_emit {
            self.emit(PlaybackEvent::Volume { available, level, detail });
        }
    }

    /// `Command::SetVolume` (`§4.5`). Re-resolves the current target before every set (`§1.2`):
    /// the worker is single-threaded, so `self.active`/`self.selected_output` are authoritative at
    /// command time and re-resolving here never races a track transition (`§6` Stage 4 fix — the
    /// previous cached-uid shortcut let a click land on a device that had stopped playing). A
    /// target change or an unavailable device gets a forced re-read instead of a blind set, which
    /// snaps the UI back to the truth; a click that only raced an availability flip is retried
    /// once the re-read confirms the device is settable again, so it is applied or visibly
    /// refused, never silently dropped.
    fn set_volume(&mut self, level: f32) {
        if !level.is_finite() {
            // Never hand a non-finite value to CoreAudio; treat it like any other invalid command
            // and just re-sync the UI with the truth (`§6` Stage 4 fix).
            self.refresh_volume(true);
            return;
        }
        let target = volume_target_uid(
            self.active.as_ref().map(|active| active.output_id.as_str()),
            self.selected_output.as_deref(),
        );
        if target != self.volume.uid {
            self.refresh_volume(true);
            return;
        }
        if !self.volume.available {
            self.refresh_volume(true);
            if !self.volume.available {
                return;
            }
        }
        let Some(uid) = self.volume.uid.clone() else {
            self.refresh_volume(true);
            return;
        };
        match coreaudio::set_device_volume(&uid, level) {
            Ok(readback) => {
                self.volume.level = readback;
                self.emit(PlaybackEvent::Volume {
                    available: true,
                    level: readback,
                    detail: self.volume.detail.clone(),
                });
            }
            Err(error) => {
                self.volume.available = false;
                self.volume.detail = error.clone();
                self.emit(PlaybackEvent::Status(error.clone()));
                // Without this, the UI keeps the optimistic pre-click level forever whenever the
                // next HAL read also fails, and a click that raced this failure gets silently
                // dropped once `available` is false (`§1.4`, `§6` Stage 4 fix).
                self.emit(PlaybackEvent::Volume { available: false, level: self.volume.level, detail: error });
            }
        }
    }

    fn toggle_playback(&mut self) {
        let Some(active) = self.active.as_mut() else {
            if self.queue.is_empty() {
                if !restore_last_completed_track(&mut self.queue, self.last_completed.as_ref()) {
                    self.emit(PlaybackEvent::Status("Open an audio file to start playback.".into()));
                    return;
                }
                self.start_next();
            } else {
                self.start_next();
            }
            return;
        };
        let Some(stream) = active.stream.as_ref() else { return; };
        // CPAL does not expose whether a stream is currently paused, so this UI action is
        // mirrored explicitly by the controller's playback state.
        if self_is_playing(active) {
            if let Err(error) = stream.pause() {
                self.emit(PlaybackEvent::Status(format!("Could not pause output: {error}")));
            } else {
                active.playing = false;
                let position_ms = audible_position_for(active);
                let duration_ms = active.prepared.info.duration_ms;
                self.emit(PlaybackEvent::Playing(false));
                self.emit(PlaybackEvent::Timeline { position_ms, duration_ms });
            }
        } else if let Err(error) = stream.play() {
            self.emit(PlaybackEvent::Status(format!("Could not resume output: {error}")));
        } else {
            active.playing = true;
            self.emit(PlaybackEvent::Playing(true));
        }
    }

    fn next_track(&mut self) {
        if let Some(active) = self.active.as_mut() {
            let active_failed = active.decoder_failed.load(Ordering::Acquire)
                || active.output_failed.load(Ordering::Acquire);
            if !can_next_skip(!self.queue.is_empty(), active_failed) {
                self.emit(PlaybackEvent::Status("The queue is empty.".into()));
                return;
            }
            if active.cancel.load(Ordering::Acquire) {
                self.emit(PlaybackEvent::Status("A safe skip is already pending.".into()));
                return;
            }
            let status = request_graceful_skip(&active.cancel, &mut active.skip_requested, active.playing);
            self.emit(PlaybackEvent::Status(status.into()));
            return;
        }
        if self.queue.is_empty() {
            self.emit(PlaybackEvent::Status("The queue is empty.".into()));
            return;
        }
        // With no active stream, Next explicitly skips a queue item left blocked by a
        // prior output/Hog/rate setup error.
        self.queue.pop_front();
        self.blocked_head = false;
        self.emit_queue_snapshot();
        self.start_next();
    }

    /// `detail`, when present, is the specific reason for the failure (for example a format
    /// mismatch message). It is folded into the single retained-track `Status` below, because
    /// `main.rs` keeps only one status line and a separate `Status` emitted first would be
    /// overwritten before the UI ever repaints it (`§3.2`).
    fn finish_active_failure(&mut self, failure: PlaybackFailure, detail: Option<&'static str>) {
        let Some(active) = self.active.take() else { return; };
        let status = retain_failed_track(&mut self.queue, active.prepared.clone(), failure, active.skip_requested);
        // Flush any underrun growth the rate limiter is still holding back before the track
        // leaves `self.active` (`§1.4`). In the same event batch as the failure Status below,
        // main.rs's single status line will show only the last one emitted; that pre-existing
        // one-line limitation is unchanged by this fix.
        let underrun_status = flush_underrun_report(
            active.reported_underrun_samples,
            active.diagnostics.underrun_samples.load(Ordering::Acquire),
            active.prepared.info.sample_rate,
        );
        self.emit(PlaybackEvent::Timeline {
            position_ms: active_track_relative_position_ms(&active),
            duration_ms: active.prepared.info.duration_ms,
        });
        if status.is_none() {
            // `status` is None only when the failure coincided with a completed graceful skip
            // (`retain_failed_track`): the track leaves for a reason other than Previous or a
            // seek, so it joins history exactly like the normal drain path does (`§3.7`).
            self.push_history(active.prepared.clone());
        }
        active.finish();
        self.blocked_head = status.is_some();
        if status.is_some() {
            // The track is retained at the queue head, blocked, with no further start attempt
            // in this call (unlike the `None` branch below, which immediately tries `start_next`):
            // the UI must not keep showing the pre-failure route status or a seekable bar (`§1.4`).
            self.emit(PlaybackEvent::Inactive);
        }
        self.emit(PlaybackEvent::Playing(false));
        self.emit_queue_snapshot();
        if let Some((underrun_message, _)) = underrun_status {
            self.emit(PlaybackEvent::Status(underrun_message));
        }
        if let Some(status) = status {
            let message = match detail {
                Some(detail) => format!("{detail} {status}"),
                None => status.to_string(),
            };
            self.emit(PlaybackEvent::Status(message));
        } else {
            // Advancing here is only allowed after an explicit user Next request.
            self.start_next();
        }
    }

    fn emit_queue_snapshot(&self) {
        self.emit(PlaybackEvent::QueueSnapshot(queue_snapshot(&self.queue)));
    }

    fn emit(&self, event: PlaybackEvent) {
        let _ = self.events.send(event);
    }

    /// Developer-only diagnostic (`main.rs` prints it with a `DIAG` prefix, never surfaced in the
    /// UI) reporting the nominal rate requested for this track and the DAC's physical stream
    /// format at a named point in the start sequence (before build, after build, after play).
    #[cfg(target_os = "macos")]
    fn emit_dac_format_diagnostic(&self, stage: &str, nominal_rate_hz: u32, physical_format: &str) {
        self.emit(PlaybackEvent::Diagnostic(format!(
            "DAC format · {stage} · nominal_rate_hz={nominal_rate_hz} · physical: {physical_format}"
        )));
    }
}

fn queue_snapshot(queue: &VecDeque<PreparedTrack>) -> Vec<QueueTrackSnapshot> {
    queue
        .iter()
        .map(|prepared| QueueTrackSnapshot {
            key: prepared.key.clone(),
            title: display_name(&prepared.key.path),
            parent_folder: prepared
                .key
                .path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .map(display_path)
                .unwrap_or_else(|| "Audio file".into()),
            format: prepared.info.format.clone(),
            sample_rate: prepared.info.sample_rate,
            bits_per_sample: prepared.info.bits_per_sample,
            is_float: prepared.info.is_float,
            duration_ms: prepared.info.duration_ms,
        })
        .collect()
}

fn request_graceful_skip(cancel: &AtomicBool, skip_requested: &mut bool, playing: bool) -> &'static str {
    cancel.store(true, Ordering::Release);
    *skip_requested = true;
    if playing {
        "Skipping after buffered audio drains."
    } else {
        "Resume playback to finish the safe skip."
    }
}

fn retain_failed_track(
    queue: &mut VecDeque<PreparedTrack>,
    prepared: PreparedTrack,
    failure: PlaybackFailure,
    skip_requested: bool,
) -> Option<&'static str> {
    if skip_requested {
        None
    } else {
        queue.push_front(prepared);
        Some(failure.status())
    }
}

fn can_next_skip(queue_has_successor: bool, active_failed: bool) -> bool {
    queue_has_successor || active_failed
}

fn recover_blocked_head_for_enqueue(
    queue: &mut VecDeque<PreparedTrack>,
    blocked_head: &mut bool,
    has_active_playback: bool,
) -> Option<TrackKey> {
    if has_active_playback || !*blocked_head {
        return None;
    }
    let failed_key = queue.pop_front().map(|track| track.key);
    *blocked_head = false;
    failed_key
}

fn restore_last_completed_track(
    queue: &mut VecDeque<PreparedTrack>,
    last_completed: Option<&PreparedTrack>,
) -> bool {
    if !queue.is_empty() {
        return false;
    }
    let Some(prepared) = last_completed else {
        return false;
    };
    queue.push_back(prepared.clone());
    true
}

fn drain_guard_elapsed(drained: bool, drain_started: &mut Option<Instant>, now: Instant) -> bool {
    if !drained {
        *drain_started = None;
        return false;
    }
    let started = drain_started.get_or_insert(now);
    now.duration_since(*started) >= OUTPUT_DRAIN_GUARD
}

fn self_is_playing(active: &ActivePlayback) -> bool {
    active.playing
}

fn exact_output_config(
    device: &Device,
    sample_rate: u32,
    integer_output: bool,
) -> Result<(SampleFormat, StreamConfig), String> {
    let ranges = device.supported_output_configs().map_err(|e| e.to_string())?;
    for range in ranges {
        if range.channels() != 2
            || range.min_sample_rate() > sample_rate
            || range.max_sample_rate() < sample_rate
        {
            continue;
        }
        // CoreAudio CPAL 0.18.2 reports the device's virtual callback format (usually
        // F32), not every physical format. For strict PCM, request an I32 callback
        // explicitly; physical support is separately verified after stream creation.
        let sample_format = if integer_output { SampleFormat::I32 } else { SampleFormat::F32 };
        let config = range.with_sample_rate(sample_rate).into();
        return Ok((sample_format, config));
    }
    Err(format!("The selected output does not support stereo at exactly {sample_rate} Hz."))
}

/// A throwaway output stream that only zero-fills its buffer, built on the same device/config/
/// sample format about to be used for the real stream. `start_track_prepared`'s DAC-priming cycle
/// plays this briefly then drops it, before building the real stream, to emulate the "press play
/// again" workaround that reliably fixes a DAC left on the previous track's rate/route: an IO
/// stop+start at the now-already-correct rate/format. Real-time safe like `build_stream`'s own
/// callbacks: no allocation, lock or shared state, since nothing here needs to communicate back.
#[cfg(target_os = "macos")]
fn build_priming_stream(device: &Device, config: StreamConfig, sample_format: SampleFormat) -> Result<Stream, String> {
    match sample_format {
        SampleFormat::I32 => device
            .build_output_stream::<i32, _, _>(
                config,
                |data: &mut [i32], _| data.iter_mut().for_each(|sample| *sample = 0),
                |_error| {},
                None,
            )
            .map_err(|error| error.to_string()),
        SampleFormat::F32 => device
            .build_output_stream::<f32, _, _>(
                config,
                |data: &mut [f32], _| data.iter_mut().for_each(|sample| *sample = 0.0),
                |_error| {},
                None,
            )
            .map_err(|error| error.to_string()),
        other => Err(format!("The selected output sample format {other:?} is not supported for DAC priming.")),
    }
}

fn build_stream(
    device: &Device,
    config: StreamConfig,
    sample_format: SampleFormat,
    preroll_frames: usize,
    mut consumer: Consumer<PcmSample>,
    buffered: Arc<AtomicUsize>,
    eof: Arc<AtomicBool>,
    drained: Arc<AtomicBool>,
    output_failed: Arc<AtomicBool>,
    format_mismatch: Arc<AtomicU8>,
    diagnostics: Arc<AudioDiagnostics>,
    consumed_output_samples: Arc<AtomicU64>,
    output_latency_nanos: Arc<AtomicU64>,
    events: Sender<PlaybackEvent>,
) -> Result<Stream, String> {
    match sample_format {
        SampleFormat::I32 => {
            let data_failure = output_failed.clone();
            let data_mismatch = format_mismatch;
            let data_diagnostics = diagnostics.clone();
            let timeline_samples = consumed_output_samples.clone();
            let timeline_latency = output_latency_nanos;
            let callback_failure = output_failed;
            // Persistent closure-local countdown (not shared/atomic): counts down once per output
            // sample across callback invocations, writing exact silence ahead of the first real PCM
            // sample so a just-primed DAC has finished unmuting/relocking before real audio reaches
            // it. Invisible to every downstream counter (`delivered_pcm_samples`,
            // `callback_found_nonzero_sample`, `underrun_samples`, `buffered`/`eof`/`drained`) since
            // it returns before any of those are touched.
            let mut preroll_remaining_samples = preroll_frames.saturating_mul(2); // stereo
            device
                .build_output_stream::<i32, _, _>(
                    config,
                    move |data, callback_info| {
                        let mut mismatch_reported = false;
                        let mut callback_found_nonzero_sample = false;
                        let mut underrun_samples = 0u64;
                        let mut delivered_pcm_samples = 0u64;
                        for target in data {
                            if preroll_remaining_samples > 0 {
                                *target = 0;
                                preroll_remaining_samples -= 1;
                                continue;
                            }
                            match consumer.pop() {
                                Ok(PcmSample::Integer(sample)) => {
                                    *target = sample;
                                    callback_found_nonzero_sample |= sample != 0;
                                    delivered_pcm_samples = delivered_pcm_samples.saturating_add(1);
                                    buffered.fetch_sub(1, Ordering::Release);
                                }
                                Ok(_) => {
                                    *target = 0;
                                    buffered.fetch_sub(1, Ordering::Release);
                                    if !mismatch_reported {
                                        mismatch_reported = true;
                                        data_mismatch.store(MISMATCH_FLOAT_IN_I32, Ordering::Relaxed);
                                        data_failure.store(true, Ordering::Release);
                                    }
                                }
                                Err(_) => {
                                    *target = 0;
                                    if eof.load(Ordering::Acquire) {
                                        drained.store(true, Ordering::Release);
                                    } else {
                                        underrun_samples += 1;
                                    }
                                }
                            }
                        }
                        if callback_found_nonzero_sample {
                            data_diagnostics.callback_nonzero_sample.store(true, Ordering::Release);
                        }
                        mark_first_output_pcm_playback_time(
                            &data_diagnostics.first_output_timestamp_state,
                            &data_diagnostics.first_output_playback_secs,
                            &data_diagnostics.first_output_playback_nanos,
                            callback_info.timestamp().playback,
                            delivered_pcm_samples,
                        );
                        record_output_latency(&timeline_latency, callback_info.timestamp());
                        record_output_samples_consumed(&timeline_samples, delivered_pcm_samples);
                        if underrun_samples > 0 {
                            data_diagnostics.underrun_samples.fetch_add(underrun_samples, Ordering::AcqRel);
                        }
                    },
                    move |error| {
                        callback_failure.store(true, Ordering::Release);
                        let _ = events.send(PlaybackEvent::Status(format!("Audio output error: {error}")));
                    },
                    None,
                )
                .map_err(|error| error.to_string())
        }
        SampleFormat::F32 => {
            let data_failure = output_failed.clone();
            let data_mismatch = format_mismatch;
            let data_diagnostics = diagnostics;
            let timeline_samples = consumed_output_samples;
            let timeline_latency = output_latency_nanos;
            let callback_failure = output_failed;
            // See the I32 arm above for why this is a plain closure-local counter, not shared state.
            let mut preroll_remaining_samples = preroll_frames.saturating_mul(2); // stereo
            device
                .build_output_stream::<f32, _, _>(
                    config,
                    move |data, callback_info| {
                        let mut mismatch_reported = false;
                        let mut callback_found_nonzero_sample = false;
                        let mut underrun_samples = 0u64;
                        let mut delivered_pcm_samples = 0u64;
                        for target in data {
                            if preroll_remaining_samples > 0 {
                                *target = 0.0;
                                preroll_remaining_samples -= 1;
                                continue;
                            }
                            match consumer.pop() {
                                Ok(PcmSample::Float(sample)) => {
                                    *target = sample;
                                    callback_found_nonzero_sample |= sample != 0.0;
                                    delivered_pcm_samples = delivered_pcm_samples.saturating_add(1);
                                    buffered.fetch_sub(1, Ordering::Release);
                                }
                                Ok(PcmSample::Integer(_)) => {
                                    *target = 0.0;
                                    buffered.fetch_sub(1, Ordering::Release);
                                    if !mismatch_reported {
                                        mismatch_reported = true;
                                        data_mismatch.store(MISMATCH_INTEGER_IN_F32, Ordering::Relaxed);
                                        data_failure.store(true, Ordering::Release);
                                    }
                                }
                                Err(_) => {
                                    *target = 0.0;
                                    if eof.load(Ordering::Acquire) {
                                        drained.store(true, Ordering::Release);
                                    } else {
                                        underrun_samples += 1;
                                    }
                                }
                            }
                        }
                        if callback_found_nonzero_sample {
                            data_diagnostics.callback_nonzero_sample.store(true, Ordering::Release);
                        }
                        mark_first_output_pcm_playback_time(
                            &data_diagnostics.first_output_timestamp_state,
                            &data_diagnostics.first_output_playback_secs,
                            &data_diagnostics.first_output_playback_nanos,
                            callback_info.timestamp().playback,
                            delivered_pcm_samples,
                        );
                        record_output_latency(&timeline_latency, callback_info.timestamp());
                        record_output_samples_consumed(&timeline_samples, delivered_pcm_samples);
                        if underrun_samples > 0 {
                            data_diagnostics.underrun_samples.fetch_add(underrun_samples, Ordering::AcqRel);
                        }
                    },
                    move |error| {
                        callback_failure.store(true, Ordering::Release);
                        let _ = events.send(PlaybackEvent::Status(format!("Audio output error: {error}")));
                    },
                    None,
                )
                .map_err(|error| error.to_string())
        }
        other => Err(format!("The selected output sample format {other:?} is not supported.")),
    }
}

fn sample_is_nonzero(sample: PcmSample) -> bool {
    match sample {
        PcmSample::Integer(value) => value != 0,
        PcmSample::Float(value) => value != 0.0,
    }
}

fn display_name(path: &Path) -> String {
    path.file_stem()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| path.display().to_string())
}

fn display_path(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::decoder::probe_file;
    use std::path::PathBuf;

    #[test]
    fn handoff_timing_diagnostic_labels_transition_stages_and_unmeasured_gap() {
        let timings = TrackStartTimings {
            prebuffer_wait: Duration::from_millis(3),
            hog_setup: Some(Duration::from_millis(4)),
            nominal_rate_setup: Some(Duration::from_millis(5)),
            physical_format_setup: Some(Duration::from_millis(6)),
            stream_build: Duration::from_millis(7),
            stream_play_call: Duration::from_millis(8),
            worker_observed_preparation_to_prebuffer_ready: Duration::from_millis(9),
        };

        let diagnostic = handoff_timing_diagnostic(
            "/nas/album/next.wv",
            "WavPack",
            192_000,
            Duration::from_millis(10),
            &timings,
        );

        assert_eq!(
            diagnostic,
            "audio_handoff_timing track=\"/nas/album/next.wv\" format=\"WavPack\" target_rate_hz=192000 worker_drain_observation_to_play_return_ms=10 worker_observed_prep_to_prebuffer_ready_ms=9 prebuffer_wait_ms=3 hog_setup_macos_ms=4 nominal_rate_setup_macos_ms=5 physical_format_setup_macos_ms=6 stream_build_ms=7 stream_play_call_ms=8 acoustic_gap=unmeasured"
        );
    }

    #[test]
    fn handoff_timing_diagnostic_marks_macos_only_stages_not_applicable_elsewhere() {
        let timings = TrackStartTimings::default();
        let diagnostic = handoff_timing_diagnostic(
            "/nas/album/next.flac",
            "FLAC",
            44_100,
            Duration::from_millis(10),
            &timings,
        );

        assert!(diagnostic.contains("nominal_rate_setup_macos_ms=n/a"));
        assert!(diagnostic.contains("physical_format_setup_macos_ms=n/a"));
        assert!(diagnostic.contains("hog_setup_macos_ms=n/a"));
    }

    #[test]
    fn manual_skip_waits_for_software_drain_and_guard() {
        let cancel = AtomicBool::new(false);
        let mut skip_requested = false;
        assert_eq!(
            request_graceful_skip(&cancel, &mut skip_requested, true),
            "Skipping after buffered audio drains."
        );
        assert!(cancel.load(Ordering::Acquire));
        assert!(skip_requested);

        let now = Instant::now();
        let mut guard_started = None;
        assert!(!drain_guard_elapsed(false, &mut guard_started, now));
        assert_eq!(guard_started, None);
        assert!(!drain_guard_elapsed(true, &mut guard_started, now));
        assert!(!drain_guard_elapsed(true, &mut guard_started, now + OUTPUT_DRAIN_GUARD - Duration::from_millis(1)));
        assert!(drain_guard_elapsed(true, &mut guard_started, now + OUTPUT_DRAIN_GUARD));
    }

    #[test]
    fn paused_manual_skip_waits_for_resume_to_drain() {
        let cancel = AtomicBool::new(false);
        let mut skip_requested = false;
        assert_eq!(
            request_graceful_skip(&cancel, &mut skip_requested, false),
            "Resume playback to finish the safe skip."
        );
        assert!(cancel.load(Ordering::Acquire));
        assert!(skip_requested);
    }

    fn test_track(name: &str) -> PreparedTrack {
        PreparedTrack {
            key: TrackKey::whole_file(PathBuf::from(name)),
            info: AudioInfo {
                sample_rate: 44_100,
                duration_ms: Some(1_000),
                source_channels: 2,
                bits_per_sample: 16,
                is_float: false,
                integer_pcm: true,
                format: "FLAC".into(),
            },
        }
    }

    #[test]
    fn queue_snapshot_preserves_pending_order_and_audio_metadata() {
        let mut first = test_track("/nas/album/first.flac");
        first.info.format = "FLAC".into();
        first.info.sample_rate = 96_000;
        first.info.duration_ms = Some(95_000);
        let mut second = test_track("/nas/other/second.mp3");
        second.info.format = "MPEG Audio".into();
        second.info.sample_rate = 44_100;
        second.info.duration_ms = None;
        // A float WavPack track queued with no library record yet must still snapshot as float, so
        // the Queue view's badge reads "WavPack 32f/..." rather than silently reporting integer.
        second.info.is_float = true;

        let queue = VecDeque::from([first, second]);
        let snapshot = queue_snapshot(&queue);

        assert_eq!(
            snapshot,
            vec![
                QueueTrackSnapshot {
                    key: TrackKey::whole_file(PathBuf::from("/nas/album/first.flac")),
                    title: "first".into(),
                    parent_folder: "album".into(),
                    format: "FLAC".into(),
                    sample_rate: 96_000,
                    bits_per_sample: 16,
                    is_float: false,
                    duration_ms: Some(95_000),
                },
                QueueTrackSnapshot {
                    key: TrackKey::whole_file(PathBuf::from("/nas/other/second.mp3")),
                    title: "second".into(),
                    parent_folder: "other".into(),
                    format: "MPEG Audio".into(),
                    sample_rate: 44_100,
                    bits_per_sample: 16,
                    is_float: true,
                    duration_ms: None,
                },
            ]
        );
    }

    fn fixture_track(name: &str) -> PreparedTrack {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
        let info = probe_file(&path).expect("decoder fixture should be probeable");
        PreparedTrack { key: TrackKey::whole_file(path), info }
    }

    /// A minimal, hardware-free `ActivePlayback` for a track identified by `key`, for tests that
    /// only need to exercise pure position/timeline math (`audible_position_for` and friends), not
    /// the full worker/queue machinery `worker_with_active_track_and_events` sets up.
    fn test_active_playback_with_key(key: TrackKey) -> ActivePlayback {
        let info = AudioInfo {
            sample_rate: 44_100,
            duration_ms: Some(10_000),
            source_channels: 2,
            bits_per_sample: 16,
            is_float: false,
            integer_pcm: true,
            format: "FLAC".into(),
        };
        ActivePlayback {
            prepared: PreparedTrack { key, info },
            stream: None,
            decoder: None,
            cancel: Arc::new(AtomicBool::new(false)),
            drained: Arc::new(AtomicBool::new(false)),
            decoder_failed: Arc::new(AtomicBool::new(false)),
            output_failed: Arc::new(AtomicBool::new(false)),
            format_mismatch: Arc::new(AtomicU8::new(0)),
            diagnostics: Arc::new(AudioDiagnostics::default()),
            reported_diagnostics: AudioDiagnosticsSnapshot::default(),
            reported_underrun_samples: 0,
            underrun_reported_at: Instant::now(),
            stream_play_returned_stream_time: StreamInstant::ZERO,
            stream_play_returned_unix_ms: unix_time_millis(),
            first_output_pcm_callback_reported: false,
            consumed_output_samples: Arc::new(AtomicU64::new(0)),
            output_latency_nanos: Arc::new(AtomicU64::new(0)),
            start_frame: 0,
            timeline_reported_samples: 0,
            timeline_reported_at: Instant::now(),
            drain_started: None,
            playing: true,
            skip_requested: false,
            output_id: String::new(),
            hog_mode: false,
        }
    }

    fn worker_with_active_track(queue: Vec<PreparedTrack>) -> PlaybackWorker {
        worker_with_active_track_and_events(queue).0
    }

    /// Same as `worker_with_active_track`, but also returns the event `Receiver` so a test can
    /// observe `PlaybackEvent` routing (Diagnostic vs Status) instead of discarding it.
    fn worker_with_active_track_and_events(
        queue: Vec<PreparedTrack>,
    ) -> (PlaybackWorker, Receiver<PlaybackEvent>) {
        let (_command_tx, commands) = unbounded();
        let (events, event_rx) = unbounded();
        let settings = PlayerSettings {
            output_device_id: Some("unchanged-output-id".into()),
            hog_mode_enabled: true,
        };
        let mut worker = PlaybackWorker::new(settings, commands, events);
        worker.active = Some(ActivePlayback {
            prepared: test_track("/music/current.flac"),
            stream: None,
            decoder: None,
            cancel: Arc::new(AtomicBool::new(false)),
            drained: Arc::new(AtomicBool::new(false)),
            decoder_failed: Arc::new(AtomicBool::new(false)),
            output_failed: Arc::new(AtomicBool::new(false)),
            format_mismatch: Arc::new(AtomicU8::new(0)),
            diagnostics: Arc::new(AudioDiagnostics::default()),
            reported_diagnostics: AudioDiagnosticsSnapshot::default(),
            reported_underrun_samples: 0,
            underrun_reported_at: Instant::now(),
            stream_play_returned_stream_time: StreamInstant::ZERO,
            stream_play_returned_unix_ms: unix_time_millis(),
            first_output_pcm_callback_reported: false,
            consumed_output_samples: Arc::new(AtomicU64::new(0)),
            output_latency_nanos: Arc::new(AtomicU64::new(0)),
            start_frame: 0,
            timeline_reported_samples: 0,
            timeline_reported_at: Instant::now(),
            drain_started: None,
            playing: true,
            skip_requested: false,
            // Deliberately invalid (fails `DeviceId::from_str` immediately, before any cpal
            // call): lets seek/restart tests exercise the real start path and observe a
            // deterministic, hardware-free failure instead of touching a real output device.
            output_id: String::new(),
            hog_mode: false,
        });
        worker.queue.extend(queue);
        (worker, event_rx)
    }

    fn cancel_and_reap_lookahead(worker: &mut PlaybackWorker) {
        let Some(mut preparation) = worker.lookahead.take() else { return; };
        preparation.cancel();
        for _ in 0..1_000 {
            if preparation.audio.decoder_finished() {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert!(preparation.audio.decoder_finished(), "cancelled fixture decoder should stop promptly");
        preparation.audio.reap_finished_decoder();
    }

    #[test]
    fn lookahead_prebuffer_is_bounded_by_pcm_ring_capacity() {
        assert_eq!(prebuffer_samples(44_100), 5_292);
        assert_eq!(prebuffer_samples(192_000), 23_040);
        assert_eq!(prebuffer_samples(768_000), RING_SAMPLES / 2);
        assert!(prebuffer_samples(768_000) <= RING_SAMPLES);
    }

    #[test]
    fn shutdown_cancels_lookahead_before_joining_active_decoder() {
        let mut worker = worker_with_active_track(Vec::new());
        let lookahead_cancel = Arc::new(AtomicBool::new(false));
        let lookahead_finished = Arc::new(AtomicBool::new(false));
        let active_cancel = Arc::new(AtomicBool::new(false));
        let active_observed_lookahead_cancel = Arc::new(AtomicBool::new(false));

        let decoder_cancel = lookahead_cancel.clone();
        let decoder_finished = lookahead_finished.clone();
        let lookahead_decoder = thread::spawn(move || {
            while !decoder_cancel.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1));
            }
            decoder_finished.store(true, Ordering::Release);
        });
        let decoder_cancel = active_cancel.clone();
        let observed_cancel = active_observed_lookahead_cancel.clone();
        let lookahead_cancel_for_active = lookahead_cancel.clone();
        let active_decoder = thread::spawn(move || {
            while !decoder_cancel.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1));
            }
            observed_cancel.store(lookahead_cancel_for_active.load(Ordering::Acquire), Ordering::Release);
        });

        let (_producer, consumer) = RingBuffer::<PcmSample>::new(RING_SAMPLES);
        let active = worker.active.as_mut().unwrap();
        active.cancel = active_cancel;
        active.decoder = Some(active_decoder);
        worker.lookahead = Some(TrackPreparation {
            prepared: test_track("/nas/next.flac"),
            audio: AudioPreparation {
                consumer: Some(consumer),
                decoder: Some(lookahead_decoder),
                cancel: lookahead_cancel,
                eof: Arc::new(AtomicBool::new(false)),
                drained: Arc::new(AtomicBool::new(false)),
                decoder_failed: Arc::new(AtomicBool::new(false)),
                buffered: Arc::new(AtomicUsize::new(0)),
                diagnostics: Arc::new(AudioDiagnostics::default()),
                decoder_error: Arc::new(Mutex::new(None)),
                prebuffer_samples: 256,
            },
            started_at: Instant::now(),
            prebuffer_ready_observed_at: None,
        });

        worker.shutdown_audio_workers();

        assert!(active_observed_lookahead_cancel.load(Ordering::Acquire));
        assert!(lookahead_finished.load(Ordering::Acquire));
        assert!(worker.active.is_none());
        assert!(worker.lookahead.is_none());
    }

    #[test]
    fn shutdown_detaches_uninterruptible_decoder_after_deadline() {
        let cancel = Arc::new(AtomicBool::new(false));
        let release_blocked_read = Arc::new(AtomicBool::new(false));
        let decoder_finished = Arc::new(AtomicBool::new(false));
        let thread_release = release_blocked_read.clone();
        let thread_finished = decoder_finished.clone();
        let decoder = thread::spawn(move || {
            while !thread_release.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1));
            }
            thread_finished.store(true, Ordering::Release);
        });

        cancel.store(true, Ordering::Release);
        let joined = join_decoder_until(
            decoder,
            Instant::now() + Duration::from_millis(10),
            Duration::from_millis(10),
            "test NAS decoder",
        );

        assert!(!joined);
        assert!(cancel.load(Ordering::Acquire));
        release_blocked_read.store(true, Ordering::Release);
        for _ in 0..1_000 {
            if decoder_finished.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert!(decoder_finished.load(Ordering::Acquire));
    }

    #[test]
    fn next_track_preparation_does_not_mutate_active_output_state() {
        let next = fixture_track("decoder-tone.flac");
        let next_path = next.key.path.clone();
        let mut worker = worker_with_active_track(vec![next]);
        let output_id = worker.selected_output.clone();
        let hog_mode = worker.settings.hog_mode_enabled;

        worker.reconcile_lookahead();

        assert_eq!(worker.active.as_ref().unwrap().prepared.key.path, PathBuf::from("/music/current.flac"));
        assert_eq!(worker.selected_output, output_id);
        assert_eq!(worker.settings.hog_mode_enabled, hog_mode);
        assert!(worker.hog_lease.is_none());
        assert_eq!(worker.queue.front().unwrap().key.path, next_path);
        assert_eq!(worker.lookahead.as_ref().unwrap().prepared.key.path, next_path);
        cancel_and_reap_lookahead(&mut worker);
    }

    #[test]
    fn replacing_queue_head_waits_for_cancelled_decoder_before_replacement() {
        let old = fixture_track("decoder-tone.flac");
        let new = fixture_track("decoder-tone.wv");
        let new_path = new.key.path.clone();
        let mut worker = worker_with_active_track(vec![old.clone()]);
        worker.reconcile_lookahead();
        let old_finished = worker.lookahead.as_ref().unwrap().audio.eof.clone();

        worker.queue.pop_front();
        worker.queue.push_front(new);
        worker.reconcile_lookahead();

        if worker.lookahead.as_ref().is_some_and(|prep| prep.prepared.key.path == old.key.path) {
            assert!(worker.lookahead.as_ref().unwrap().audio.cancel.load(Ordering::Acquire));
            for _ in 0..1_000 {
                if worker.lookahead.as_ref().unwrap().audio.decoder_finished() {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
            assert!(worker.lookahead.as_ref().unwrap().audio.decoder_finished());
            worker.reconcile_lookahead();
        }

        assert!(old_finished.load(Ordering::Acquire));
        assert_eq!(worker.lookahead.as_ref().unwrap().prepared.key.path, new_path);
        cancel_and_reap_lookahead(&mut worker);
    }

    #[test]
    fn speculative_decode_error_keeps_active_track_and_queue_head_intact() {
        let mut missing = test_track("/nas/missing-next-track.flac");
        missing.info = fixture_track("decoder-tone.flac").info;
        let missing_path = missing.key.path.clone();
        let mut worker = worker_with_active_track(vec![missing]);

        worker.reconcile_lookahead();
        for _ in 0..1_000 {
            if worker.lookahead.as_ref().unwrap().audio.eof.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }

        let preparation = worker.lookahead.as_ref().unwrap();
        assert!(preparation.audio.eof.load(Ordering::Acquire));
        assert!(preparation.audio.decoder_failed.load(Ordering::Acquire));
        assert_eq!(worker.active.as_ref().unwrap().prepared.key.path, PathBuf::from("/music/current.flac"));
        assert_eq!(worker.queue.front().unwrap().key.path, missing_path);
        assert_eq!(worker.selected_output.as_deref(), Some("unchanged-output-id"));
        assert!(worker.settings.hog_mode_enabled);
        cancel_and_reap_lookahead(&mut worker);
    }

    #[test]
    fn decoder_failure_retains_active_path_ahead_of_queued_tracks() {
        let failed = test_track("/music/failed.flac");
        let mut queue = VecDeque::from([test_track("/music/next.flac")]);

        let status = retain_failed_track(&mut queue, failed.clone(), PlaybackFailure::Decoder, false);

        assert_eq!(queue.front().unwrap().key.path, failed.key.path);
        assert_eq!(status, Some(PlaybackFailure::Decoder.status()));
    }

    #[test]
    fn output_failure_retains_active_path_without_auto_advance() {
        let failed = test_track("/music/output-failed.flac");
        let mut queue = VecDeque::from([test_track("/music/next.flac")]);

        let status = retain_failed_track(&mut queue, failed.clone(), PlaybackFailure::Output, false);

        assert_eq!(queue.front().unwrap().key.path, failed.key.path);
        assert_eq!(status, Some(PlaybackFailure::Output.status()));
    }

    #[test]
    fn explicit_skip_does_not_requeue_failed_active_path() {
        let failed = test_track("/music/skipped.flac");
        let mut queue = VecDeque::from([test_track("/music/next.flac")]);

        let status = retain_failed_track(&mut queue, failed, PlaybackFailure::Decoder, true);

        assert_eq!(queue.front().unwrap().key.path, PathBuf::from("/music/next.flac"));
        assert_eq!(status, None);
    }

    #[test]
    fn finish_active_failure_pushes_a_completed_graceful_skip_to_history() {
        let (mut worker, _events) = worker_with_active_track_and_events(Vec::new());
        let active = worker.active.as_mut().unwrap();
        active.output_failed.store(true, Ordering::Release);
        active.skip_requested = true;

        worker.finish_active_failure(PlaybackFailure::Output, None);

        assert!(worker.active.is_none());
        assert!(!worker.blocked_head, "a completed skip is not a retained failure");
        assert_eq!(
            worker.history.last().map(|t| t.key.path.clone()),
            Some(PathBuf::from("/music/current.flac")),
            "an output-failed track that finished a requested skip must still join history (§3.7), \
             so Previous can return to it"
        );
    }

    #[test]
    fn next_can_discard_a_failed_sole_active_track() {
        assert!(can_next_skip(false, true));
    }

    #[test]
    fn next_remains_noop_for_a_healthy_sole_active_track() {
        assert!(!can_next_skip(false, false));
    }

    #[test]
    fn new_enqueue_discards_a_blocked_failed_head_when_idle() {
        let failed = test_track("/music/failed.wv");
        let newly_opened = test_track("/music/new.flac");
        let mut queue = VecDeque::from([failed.clone()]);
        queue.push_back(newly_opened.clone());
        let mut blocked_head = true;

        let skipped = recover_blocked_head_for_enqueue(&mut queue, &mut blocked_head, false);

        assert_eq!(skipped, Some(failed.key.clone()));
        assert!(!blocked_head);
        assert_eq!(queue.front().unwrap().key.path, newly_opened.key.path);
    }

    #[test]
    fn enqueue_during_active_playback_does_not_discard_queue_head() {
        let queued = test_track("/music/queued.flac");
        let mut queue = VecDeque::from([queued.clone()]);
        let mut blocked_head = true;

        let skipped = recover_blocked_head_for_enqueue(&mut queue, &mut blocked_head, true);

        assert_eq!(skipped, None);
        assert!(blocked_head);
        assert_eq!(queue.front().unwrap().key.path, queued.key.path);
    }

    #[test]
    fn play_after_natural_eof_requeues_last_completed_track() {
        let completed = test_track("/nas/short-track.flac");
        let mut queue = VecDeque::new();

        assert!(restore_last_completed_track(&mut queue, Some(&completed)));

        assert_eq!(queue.len(), 1);
        assert_eq!(queue.front().unwrap().key.path, completed.key.path);
    }

    #[test]
    fn replay_does_not_replace_an_existing_queue() {
        let queued = test_track("/nas/queued-next.flac");
        let completed = test_track("/nas/last-completed.flac");
        let mut queue = VecDeque::from([queued.clone()]);

        assert!(!restore_last_completed_track(&mut queue, Some(&completed)));

        assert_eq!(queue.len(), 1);
        assert_eq!(queue.front().unwrap().key.path, queued.key.path);
    }

    #[test]
    fn format_mismatch_codes_map_to_messages() {
        assert_eq!(
            format_mismatch_message(MISMATCH_FLOAT_IN_I32),
            Some("Strict I32 output received floating-point PCM.")
        );
        assert_eq!(
            format_mismatch_message(MISMATCH_INTEGER_IN_F32),
            Some("F32 output received integer PCM without an explicit fallback conversion.")
        );
        assert_eq!(format_mismatch_message(0), None);
    }

    #[test]
    fn sample_signal_detection_handles_integer_and_float_pcm() {
        assert!(!sample_is_nonzero(PcmSample::Integer(0)));
        assert!(sample_is_nonzero(PcmSample::Integer(1)));
        assert!(!sample_is_nonzero(PcmSample::Float(0.0)));
        assert!(sample_is_nonzero(PcmSample::Float(0.25)));
    }

    #[test]
    fn playback_position_counts_only_complete_stereo_pcm_frames() {
        assert_eq!(consumed_samples_position_millis(88_200, 44_100), Some(1_000));
        assert_eq!(consumed_samples_position_millis(44_100, 44_100), Some(500));
        assert_eq!(consumed_samples_position_millis(44_101, 44_100), Some(500));
        assert_eq!(consumed_samples_position_millis(0, 0), None);
    }

    #[test]
    fn callback_accounting_adds_only_successfully_delivered_pcm_samples() {
        let consumed = AtomicU64::new(0);
        record_output_samples_consumed(&consumed, 6);
        record_output_samples_consumed(&consumed, 0);
        assert_eq!(consumed.load(Ordering::Acquire), 6);
    }

    #[test]
    fn diagnostics_snapshot_reports_nonzero_flow_and_underruns() {
        let diagnostics = AudioDiagnostics::default();
        diagnostics.decoder_nonzero_sample.store(true, Ordering::Release);
        diagnostics.callback_nonzero_sample.store(true, Ordering::Release);
        diagnostics.underrun_samples.store(64, Ordering::Release);

        assert_eq!(
            diagnostics.snapshot().status(),
            "Audio path · decoded nonzero: yes · output callback nonzero: yes · underrun samples: 64"
        );
    }

    #[test]
    fn first_output_pcm_timestamp_only_tracks_delivered_pcm_and_keeps_first_time() {
        let diagnostics = AudioDiagnostics::default();
        let first_time = StreamInstant::new(3, 40);
        mark_first_output_pcm_playback_time(
            &diagnostics.first_output_timestamp_state,
            &diagnostics.first_output_playback_secs,
            &diagnostics.first_output_playback_nanos,
            first_time,
            0,
        );
        assert_eq!(first_output_pcm_playback_time(&diagnostics), None);

        mark_first_output_pcm_playback_time(
            &diagnostics.first_output_timestamp_state,
            &diagnostics.first_output_playback_secs,
            &diagnostics.first_output_playback_nanos,
            first_time,
            2,
        );
        assert_eq!(first_output_pcm_playback_time(&diagnostics), Some(first_time));

        mark_first_output_pcm_playback_time(
            &diagnostics.first_output_timestamp_state,
            &diagnostics.first_output_playback_secs,
            &diagnostics.first_output_playback_nanos,
            StreamInstant::new(9, 0),
            4,
        );
        assert_eq!(first_output_pcm_playback_time(&diagnostics), Some(first_time));
    }

    #[test]
    fn first_output_pcm_playback_estimate_uses_same_stream_clock() {
        let (elapsed, preceded_play_return) = playback_estimate_from_play_return(
            StreamInstant::new(8, 750_000_000),
            StreamInstant::new(8, 25_000_000),
        );
        assert_eq!(elapsed, Duration::from_millis(725));
        assert!(!preceded_play_return);

        let (elapsed, preceded_play_return) = playback_estimate_from_play_return(
            StreamInstant::new(7, 800_000_000),
            StreamInstant::new(8, 25_000_000),
        );
        assert_eq!(elapsed, Duration::from_millis(225));
        assert!(preceded_play_return);
    }

    #[test]
    fn first_output_pcm_status_distinguishes_dac_estimate_from_wall_clock_correlation() {
        let status = first_output_pcm_callback_status(
            Duration::from_millis(713),
            false,
            1_800_000_000_123,
            1_800_000_000_836,
        );
        assert_eq!(
            status,
            "First output PCM DAC playback estimate · play-return-to-scheduled-playback +713 ms · wall-clock correlation: play return unix-ms 1800000000123 · worker observation unix-ms 1800000000836"
        );

        let preceded_status = first_output_pcm_callback_status(
            Duration::from_millis(5),
            true,
            1_800_000_000_123,
            1_800_000_000_836,
        );
        assert!(preceded_status.contains("play-return-to-scheduled-playback -5 ms"));
    }

    #[test]
    fn audible_position_subtracts_output_latency() {
        assert_eq!(audible_position_millis(88_200, 0, 10_000_000, 44_100), Some(990));
    }

    #[test]
    fn audible_position_never_precedes_seek_start() {
        assert_eq!(audible_position_millis(88_300, 44_100, 20_000_000, 44_100), Some(1_000));
    }

    #[test]
    fn audible_position_clamps_absurd_latency_and_zero_rate() {
        assert_eq!(audible_position_millis(88_200, 0, 0, 0), None);
        // 882_000 consumed samples is 10 s of audio at 44.1 kHz (frames = samples / 2). With
        // latency clamped to MAX_OUTPUT_LATENCY_NANOS (5 s), the audible position is 10 s - 5 s.
        // Without the clamp, u64::MAX of latency would swamp the 10 s of audio entirely and
        // saturate to 0, so this case would not distinguish a missing clamp from a present one.
        assert_eq!(audible_position_millis(882_000, 0, u64::MAX, 44_100), Some(5_000));
    }

    #[test]
    fn record_output_latency_stores_playback_minus_callback() {
        let latency = AtomicU64::new(999);
        record_output_latency(
            &latency,
            OutputStreamTimestamp { callback: StreamInstant::new(10, 0), playback: StreamInstant::new(9, 0) },
        );
        assert_eq!(latency.load(Ordering::Relaxed), 0, "playback before callback stores 0");

        record_output_latency(
            &latency,
            OutputStreamTimestamp {
                callback: StreamInstant::new(10, 0),
                playback: StreamInstant::new(10, 12_500_000),
            },
        );
        assert_eq!(latency.load(Ordering::Relaxed), 12_500_000, "an in-range gap is stored as-is");

        record_output_latency(
            &latency,
            OutputStreamTimestamp { callback: StreamInstant::new(10, 0), playback: StreamInstant::new(17, 0) },
        );
        assert_eq!(latency.load(Ordering::Relaxed), MAX_OUTPUT_LATENCY_NANOS, "a 7 s gap clamps to 5 s");
    }

    #[test]
    fn strict_route_predicate_matches_platform_and_integer_pcm() {
        let mut info = test_track("/music/route.flac").info;
        info.integer_pcm = true;
        assert_eq!(uses_strict_integer_route(&info), cfg!(target_os = "macos"));
        info.integer_pcm = false;
        assert!(!uses_strict_integer_route(&info));
    }

    #[test]
    fn post_build_check_flags_only_a_drifted_strict_route_build() {
        // Strict route: the DAC's physical format changed while `build_stream` ran, so the
        // already-built AudioUnit is stale and must be refused.
        assert!(needs_rebuild_after_post_build_check(true, "rate=96000 format=int32", "rate=44100 format=int32"));
        // Strict route: no drift, nothing to refuse.
        assert!(!needs_rebuild_after_post_build_check(true, "rate=96000 format=int32", "rate=96000 format=int32"));
        // Float route never pins an exact physical format, so a "drift" here is not a rebuild
        // signal; the float route's own rate check handles it separately.
        assert!(!needs_rebuild_after_post_build_check(false, "rate=96000 format=float32", "rate=44100 format=int32"));
    }

    fn dac_state(rate: u32, format: &str) -> DacState {
        DacState { nominal_rate: rate, physical_format: format.into() }
    }

    #[test]
    fn dac_prime_not_needed_when_seek_reuses_same_rate_and_route() {
        // A seek pins to the already-active device/rate/route (`pinned_output`), so `before` and
        // `after` are identical and `previous_route` is exactly `new_route`: nothing changed.
        let state = dac_state(96_000, "rate=96000 format=int32");
        assert_eq!(
            needs_dac_prime(&state, &state, Some(Route::Strict), Route::Strict),
            None,
            "a seek must never trigger DAC priming"
        );
    }

    #[test]
    fn dac_prime_not_needed_for_same_rate_same_route_next_track() {
        // Two consecutive FLAC tracks at the same rate: same route, same HAL state.
        let state = dac_state(44_100, "rate=44100 format=int32");
        assert_eq!(needs_dac_prime(&state, &state, Some(Route::Strict), Route::Strict), None);
    }

    #[test]
    fn dac_prime_needed_when_nominal_rate_changed() {
        let before = dac_state(44_100, "rate=44100 format=int32");
        let after = dac_state(96_000, "rate=96000 format=int32");
        assert_eq!(needs_dac_prime(&before, &after, Some(Route::Strict), Route::Strict), Some("rate_changed"));
    }

    #[test]
    fn dac_prime_needed_when_physical_format_changed_at_same_rate() {
        let before = dac_state(96_000, "rate=96000 format=int32");
        let after = dac_state(96_000, "rate=96000 format=float32");
        assert_eq!(needs_dac_prime(&before, &after, Some(Route::Strict), Route::Float), Some("format_changed"));
    }

    #[test]
    fn dac_prime_needed_when_route_changed_but_hal_state_reads_unchanged() {
        // The critical hardware-bug-reproducing case: HAL state already reads "correct"/unchanged
        // at snapshot time (float never pins an exact physical format the way strict does), but
        // the route actually differs from what was last playing on this device.
        let state = dac_state(44_100, "rate=44100 format=unavailable");
        assert_eq!(
            needs_dac_prime(&state, &state, Some(Route::Float), Route::Strict),
            Some("route_changed")
        );
        assert_eq!(
            needs_dac_prime(&state, &state, Some(Route::Strict), Route::Float),
            Some("route_changed")
        );
    }

    #[test]
    fn dac_prime_not_needed_on_first_start_with_no_previous_route() {
        // No prior stop recorded on this device (first-ever start): nothing to compare against.
        let state = dac_state(44_100, "rate=44100 format=int32");
        assert_eq!(needs_dac_prime(&state, &state, None, Route::Strict), None);
    }

    #[test]
    fn no_dac_prime_means_no_preroll_for_seek_and_first_start() {
        // `did_dac_prime` in `start_track_prepared` is derived directly from `needs_dac_prime`'s
        // result being `Some`; whenever it is `None`, the real stream's `preroll_frames` is forced
        // to 0 regardless of `dac_preroll_ms()`. A seek (pinned to the already-active rate/route)
        // and a from-fresh/no-previous-route start are exactly the two cases that must never
        // pre-roll, so this pins `needs_dac_prime`'s `None` result for both.
        let seek_state = dac_state(96_000, "rate=96000 format=int32");
        assert_eq!(
            needs_dac_prime(&seek_state, &seek_state, Some(Route::Strict), Route::Strict),
            None,
            "a seek must never trigger DAC priming, and therefore never a pre-roll"
        );
        let fresh_state = dac_state(44_100, "rate=44100 format=int32");
        assert_eq!(
            needs_dac_prime(&fresh_state, &fresh_state, None, Route::Strict),
            None,
            "a first start with no previous route must never trigger DAC priming, and therefore never a pre-roll"
        );
    }

    #[test]
    fn clamp_preroll_ms_defaults_and_clamps() {
        assert_eq!(clamp_preroll_ms(None), 1000, "unset LIME_DAC_PREROLL_MS defaults to 1000 ms");
        assert_eq!(clamp_preroll_ms(Some(500)), 500, "an in-range value passes through unchanged");
        assert_eq!(clamp_preroll_ms(Some(3000)), 3000, "exactly the upper bound stays unchanged");
        assert_eq!(clamp_preroll_ms(Some(5000)), 3000, "an over-range value clamps to the 3000 ms ceiling");
        assert_eq!(clamp_preroll_ms(Some(0)), 0, "0 stays 0, disabling pre-roll entirely");
    }

    #[test]
    fn dac_preroll_frames_converts_ms_to_frames_per_sample_rate() {
        assert_eq!(dac_preroll_frames(1000, 44_100), 44_100);
        assert_eq!(dac_preroll_frames(1000, 48_000), 48_000);
        assert_eq!(dac_preroll_frames(150, 48_000), 7_200);
        assert_eq!(dac_preroll_frames(0, 48_000), 0);
    }

    #[test]
    fn decoder_exit_guard_sets_eof_and_failed_on_panic() {
        let eof = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let error_slot = Arc::new(Mutex::new(None));
        let (guard_eof, guard_failed, guard_cancel, guard_error_slot) =
            (eof.clone(), failed.clone(), cancel.clone(), error_slot.clone());
        let handle = thread::spawn(move || {
            let _guard = DecoderExitGuard {
                eof: guard_eof,
                failed: guard_failed,
                cancel: guard_cancel,
                error_slot: guard_error_slot,
            };
            panic!("simulated decoder panic");
        });
        assert!(handle.join().is_err());

        assert!(eof.load(Ordering::Acquire));
        assert!(failed.load(Ordering::Acquire));
        assert!(error_slot.lock().unwrap().is_some());
    }

    #[test]
    fn decoder_exit_guard_sets_only_eof_on_normal_exit() {
        let eof = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let error_slot = Arc::new(Mutex::new(None));
        {
            let _guard = DecoderExitGuard {
                eof: eof.clone(),
                failed: failed.clone(),
                cancel: cancel.clone(),
                error_slot: error_slot.clone(),
            };
        }

        assert!(eof.load(Ordering::Acquire));
        assert!(!failed.load(Ordering::Acquire));
        assert!(error_slot.lock().unwrap().is_none());
    }

    fn empty_audio_preparation(decoder: Option<JoinHandle<()>>) -> AudioPreparation {
        let (_producer, consumer) = RingBuffer::<PcmSample>::new(RING_SAMPLES);
        AudioPreparation {
            consumer: Some(consumer),
            decoder,
            cancel: Arc::new(AtomicBool::new(false)),
            eof: Arc::new(AtomicBool::new(false)),
            drained: Arc::new(AtomicBool::new(false)),
            decoder_failed: Arc::new(AtomicBool::new(false)),
            buffered: Arc::new(AtomicUsize::new(0)),
            diagnostics: Arc::new(AudioDiagnostics::default()),
            decoder_error: Arc::new(Mutex::new(None)),
            prebuffer_samples: 256,
        }
    }

    #[test]
    fn dropping_unstarted_preparation_cancels_its_decoder() {
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut preparation = empty_audio_preparation(Some(thread::spawn(|| {})));
            preparation.cancel = cancel.clone();
        }
        assert!(cancel.load(Ordering::Acquire), "an unstarted preparation must cancel its own decoder on drop");
    }

    #[test]
    fn dropping_preparation_without_decoder_handle_keeps_cancel_clear() {
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut preparation = empty_audio_preparation(Some(thread::spawn(|| {})));
            preparation.cancel = cancel.clone();
            // Simulates a started track: `start_track_prepared` moves the decoder handle into
            // `ActivePlayback`, sharing the same `cancel` Arc, before this preparation drops.
            preparation.decoder.take();
        }
        assert!(!cancel.load(Ordering::Acquire), "a started track's shared cancel flag must survive the drop");
    }

    #[test]
    fn prebuffer_wait_times_out() {
        let preparation = empty_audio_preparation(None);
        let error = preparation
            .wait_until_prebuffered_within(Duration::from_millis(20))
            .expect_err("no decoder ever fills the buffer, so this must time out");
        assert!(error.contains("could not be read in time"), "unexpected message: {error}");
    }

    #[test]
    fn underrun_increase_produces_rate_limited_status() {
        let now = Instant::now();
        let one_second_ago = now - DIAGNOSTIC_REPORT_INTERVAL;

        let (message, reported) = underrun_report(100, 100 + 2 * 441, one_second_ago, now, 44_100)
            .expect("growth after the report interval should produce a status");
        assert_eq!(message, "Playback underrun: the file could not be read fast enough (\u{2248}10 ms of silence inserted).");
        assert_eq!(reported, 100 + 2 * 441);

        assert!(
            underrun_report(100, 200, now, now, 44_100).is_none(),
            "within the 1 s window must not report again"
        );
        assert!(underrun_report(100, 100, one_second_ago, now, 44_100).is_none(), "no growth must not report");
    }

    #[test]
    fn flush_underrun_report_ignores_the_interval_gate() {
        // A growth the rate limiter would still be holding back (well inside the 1 s window)
        // must still be flushed once the track is about to leave `self.active` (`§1.4`).
        let (message, reported) = flush_underrun_report(100, 100 + 2 * 441, 44_100)
            .expect("pending growth must be flushed even inside the report interval");
        assert_eq!(message, "Playback underrun: the file could not be read fast enough (\u{2248}10 ms of silence inserted).");
        assert_eq!(reported, 100 + 2 * 441);

        assert!(flush_underrun_report(100, 100, 44_100).is_none(), "no growth must not report");
    }

    #[test]
    fn diagnostics_and_underrun_route_to_different_event_kinds() {
        let (mut worker, events) = worker_with_active_track_and_events(Vec::new());

        worker.active.as_ref().unwrap().diagnostics.decoder_nonzero_sample.store(true, Ordering::Release);
        worker.emit_audio_diagnostics_if_changed();

        match events.try_recv().expect("a signal-flag change must emit an event") {
            PlaybackEvent::Diagnostic(line) => {
                assert!(line.contains("decoded nonzero: yes"), "unexpected diagnostic: {line}");
            }
            other => panic!("expected Diagnostic, got {other:?}"),
        }
        assert!(events.try_recv().is_err(), "no Status should follow a signal-flag-only change");

        worker.active.as_ref().unwrap().diagnostics.underrun_samples.store(200, Ordering::Release);
        worker.active.as_mut().unwrap().underrun_reported_at = Instant::now() - DIAGNOSTIC_REPORT_INTERVAL;
        worker.emit_audio_diagnostics_if_changed();

        match events.try_recv().expect("underrun growth past the report interval must emit a Status") {
            PlaybackEvent::Status(message) => {
                assert!(message.starts_with("Playback underrun"), "unexpected status: {message}");
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn format_mismatch_status_is_combined_and_track_is_retained() {
        let (mut worker, events) = worker_with_active_track_and_events(Vec::new());
        {
            let active = worker.active.as_ref().unwrap();
            active.format_mismatch.store(MISMATCH_FLOAT_IN_I32, Ordering::Relaxed);
            active.output_failed.store(true, Ordering::Release);
        }

        worker.advance_if_drained();

        assert!(worker.active.is_none(), "a failed output must clear the active slot");
        assert_eq!(
            worker.queue.front().map(|track| track.key.path.clone()),
            Some(PathBuf::from("/music/current.flac")),
            "a non-skip output failure keeps the track at the queue head"
        );

        let mut saw_combined_status = false;
        while let Ok(event) = events.try_recv() {
            if let PlaybackEvent::Status(message) = event {
                assert_eq!(
                    message,
                    "Strict I32 output received floating-point PCM. Audio output failed; the current track was retained. Choose an output or press Play to retry; use Next to skip."
                );
                saw_combined_status = true;
            }
        }
        assert!(saw_combined_status, "the mismatch reason must reach the single visible Status");
    }

    /// Joins every handle still in `retiring_decoders`, polling briefly so tests that leave a
    /// short-lived decoder thread behind do not leak it.
    fn drain_retiring_decoders(worker: &mut PlaybackWorker) {
        for _ in 0..2_000 {
            worker.reap_retiring_decoders();
            if worker.retiring_decoders.is_empty() {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn seek_target_frame_clamps_before_end() {
        assert_eq!(seek_target_frame(0, 44_100, 10_000), 0);
        assert_eq!(seek_target_frame(4_000, 44_100, 10_000), 4_000 * 44_100 / 1_000);
        // 10_000 ms requested against a 10_000 ms track clamps to duration - 500 ms, always
        // leaving audio to prebuffer.
        assert_eq!(seek_target_frame(10_000, 44_100, 10_000), 9_500 * 44_100 / 1_000);
        assert_eq!(seek_target_frame(50_000, 44_100, 10_000), 9_500 * 44_100 / 1_000);
    }

    /// The CUE sub-range seek composition (`CLAUDE.md` §4): given a `TrackKey` with a nonzero
    /// `start_frame`, a track-relative `position_ms` and a sample rate, the absolute decode frame
    /// `seek_to` hands to `spawn_track_preparation` is exactly `key.start_frame +
    /// seek_target_frame(position_ms, rate, sub_range_duration_ms)` — using the *sub-range's own*
    /// duration for the clamp, never the physical file's whole duration. This locks down the exact
    /// composition `seek_to` performs (see its own `relative_frame`/`start_frame` computation).
    #[test]
    fn cue_sub_range_seek_offset_composes_key_start_frame_with_track_relative_seek_target() {
        let key = TrackKey { path: PathBuf::from("/music/album.flac"), start_frame: 44_100 * 60, end_frame: Some(44_100 * 120) };
        let sample_rate = 44_100;
        // The sub-range spans 60 s (120s - 60s of the physical file), never the whole file's
        // duration, which could be much longer.
        let sub_range_duration_ms = 60_000;

        let relative_frame = seek_target_frame(30_000, sample_rate, sub_range_duration_ms);
        let absolute_frame = key.start_frame + relative_frame;
        assert_eq!(relative_frame, 30_000 * sample_rate as u64 / 1_000, "30 s into a 60 s sub-range, track-relative");
        assert_eq!(absolute_frame, key.start_frame + relative_frame);
        assert!(absolute_frame > key.start_frame, "an absolute seek frame must land inside the sub-range, past its own start");
        assert!(
            absolute_frame < key.end_frame.unwrap(),
            "an absolute seek frame must never land at or past this track's own end_frame"
        );

        // A seek near the end of the sub-range clamps against the *sub-range's* own duration
        // (60_000 ms), not the physical file's: requesting past it still lands within 500 ms of
        // this track's own end, never anywhere near the physical file's true end.
        let clamped_relative = seek_target_frame(120_000, sample_rate, sub_range_duration_ms);
        let clamped_absolute = key.start_frame + clamped_relative;
        assert_eq!(clamped_relative, (60_000 - 500) * sample_rate as u64 / 1_000);
        assert!(clamped_absolute < key.end_frame.unwrap(), "the clamp must stay strictly inside this track's own end_frame");
    }

    /// The Timeline track-relative subtraction (`CLAUDE.md` §4): a CUE track's `key.start_frame`
    /// equivalent in time must be subtracted from the absolute audible position before it is
    /// emitted, so the UI's timeline always reads 0 at this track's own start — `audible_position_for`
    /// is the single point that performs this subtraction (via `track_relative_millis`).
    #[test]
    fn audible_position_is_relative_to_the_tracks_own_key_start_frame() {
        let key = TrackKey { path: PathBuf::from("/music/album.flac"), start_frame: 44_100 * 10, end_frame: Some(44_100 * 20) };
        let mut active = test_active_playback_with_key(key.clone());
        // 12 s into the physical file: 2 s into this track's own 10 s sub-range.
        active.start_frame = key.start_frame;
        active.consumed_output_samples.store(44_100 * 12 * 2, Ordering::Release);

        let position_ms = audible_position_for(&active);

        assert_eq!(position_ms, 2_000, "2 s into the sub-range, not 12 s into the physical file");
    }

    /// A whole-file track's `key.start_frame` is always 0, so `track_relative_millis` must be a
    /// pure no-op for it — the pre-existing (non-CUE) behavior must be unchanged.
    #[test]
    fn track_relative_millis_is_a_no_op_for_a_whole_file_key() {
        assert_eq!(track_relative_millis(5_000, 0, 44_100), 5_000);
    }

    #[test]
    fn seek_with_stale_path_is_ignored() {
        let (mut worker, events) = worker_with_active_track_and_events(Vec::new());
        let queue_before: Vec<_> = worker.queue.iter().cloned().collect();

        worker.seek_to(TrackKey::whole_file(PathBuf::from("/music/other-track.flac")), 1_000);

        assert_eq!(worker.active.as_ref().unwrap().prepared.key.path, PathBuf::from("/music/current.flac"));
        assert_eq!(worker.queue.iter().cloned().collect::<Vec<_>>(), queue_before);

        let mut saw_status = false;
        let mut saw_rejected = false;
        while let Ok(event) = events.try_recv() {
            match event {
                PlaybackEvent::Status(_) => saw_status = true,
                PlaybackEvent::SeekRejected => saw_rejected = true,
                _ => {}
            }
        }
        assert!(saw_status, "expected a Status explaining the ignored seek");
        assert!(saw_rejected, "expected SeekRejected");
    }

    #[test]
    fn seek_without_duration_is_refused_but_restart_from_zero_is_allowed_past_the_guard() {
        let (mut worker, events) = worker_with_active_track_and_events(Vec::new());
        worker.active.as_mut().unwrap().prepared.info.duration_ms = None;

        worker.seek_to(TrackKey::whole_file(PathBuf::from("/music/current.flac")), 5_000);

        assert!(worker.active.is_some(), "a duration-guard refusal must not touch the active track");
        let mut duration_guard_status = None;
        while let Ok(event) = events.try_recv() {
            if let PlaybackEvent::Status(message) = event {
                duration_guard_status = Some(message);
            }
        }
        assert_eq!(
            duration_guard_status.as_deref(),
            Some("This file does not report its length; seeking is unavailable.")
        );

        // Position 0 needs no known duration, so it proceeds past this specific guard and
        // attempts a real restart, which then fails for an unrelated reason (no real output
        // device is configured in this hardware-free unit test).
        worker.seek_to(TrackKey::whole_file(PathBuf::from("/music/current.flac")), 0);

        assert!(worker.active.is_none(), "stop_active_now always clears the active slot before restarting");
        let mut saw_seek_failed_status = false;
        while let Ok(event) = events.try_recv() {
            if let PlaybackEvent::Status(message) = event
                && message.starts_with("Seek failed")
            {
                saw_seek_failed_status = true;
            }
        }
        assert!(saw_seek_failed_status, "a restart-from-zero must proceed past the duration guard");
        drain_retiring_decoders(&mut worker);
    }

    #[test]
    fn failed_seek_retains_track_at_queue_head_and_blocks() {
        let (mut worker, events) = worker_with_active_track_and_events(Vec::new());
        assert_eq!(worker.active.as_ref().unwrap().output_id, "", "test fixture: no real output device");

        worker.seek_to(TrackKey::whole_file(PathBuf::from("/music/current.flac")), 0);

        assert_eq!(worker.queue.front().map(|track| track.key.path.clone()), Some(PathBuf::from("/music/current.flac")));
        assert!(worker.blocked_head);

        let mut saw_rejected = false;
        let mut saw_failed_status = false;
        while let Ok(event) = events.try_recv() {
            match event {
                PlaybackEvent::SeekRejected => saw_rejected = true,
                PlaybackEvent::Status(message) if message.starts_with("Seek failed") => saw_failed_status = true,
                _ => {}
            }
        }
        assert!(saw_rejected, "expected SeekRejected");
        assert!(saw_failed_status, "expected a 'Seek failed' status");

        assert_eq!(worker.retiring_decoders.len(), 1, "the failed preparation's decoder must be retiring");
        drain_retiring_decoders(&mut worker);
        assert!(worker.retiring_decoders.is_empty(), "the retiring decoder must finish and be reaped promptly");
    }

    #[test]
    fn failed_seek_with_live_lookahead_leaves_no_uncancelled_preparation() {
        let next = fixture_track("decoder-tone.flac");
        let next_path = next.key.path.clone();
        let mut worker = worker_with_active_track(vec![next]);
        worker.reconcile_lookahead();
        assert_eq!(worker.lookahead.as_ref().unwrap().prepared.key.path, next_path);
        assert!(!worker.lookahead.as_ref().unwrap().audio.cancel.load(Ordering::Acquire));

        worker.seek_to(TrackKey::whole_file(PathBuf::from("/music/current.flac")), 0);

        let lookahead = worker.lookahead.as_ref().expect("the live lookahead must survive a failed seek");
        assert_eq!(lookahead.prepared.key.path, next_path);
        assert!(lookahead.audio.cancel.load(Ordering::Acquire), "fail_seek must cancel the mismatched lookahead");

        cancel_and_reap_lookahead(&mut worker);
        drain_retiring_decoders(&mut worker);
    }

    #[test]
    fn seek_start_is_pinned_to_active_output() {
        let (mut worker, _events) = worker_with_active_track_and_events(Vec::new());
        worker.active.as_mut().unwrap().output_id = "coreaudio:builtin-output".into();
        worker.active.as_mut().unwrap().hog_mode = true;
        worker.selected_output = Some("coreaudio:other-output".into());
        worker.settings.hog_mode_enabled = false;

        // Unit-test the option construction, not CoreAudio: `seek_to` must pin to the active
        // stream's own device/Hog, never the current selection.
        let pinned = pinned_output(worker.active.as_ref().unwrap());

        assert_eq!(pinned.device_id, "coreaudio:builtin-output");
        assert!(pinned.hog_mode);
        assert_ne!(Some(pinned.device_id), worker.selected_output);
    }

    #[test]
    fn seek_to_starts_from_the_pinned_output_not_the_current_selection() {
        // `seek_start_is_pinned_to_active_output` above only checks the `PinnedOutput` struct
        // construction; this drives `seek_to` itself through `SelectOutput` and inspects which
        // device id actually reached the start path, so an unpinned `seek_to` would fail this
        // test even though the struct-copy test still passes (`§1.4`, `§4.2`).
        let (mut worker, events) = worker_with_active_track_and_events(Vec::new());
        assert_eq!(worker.active.as_ref().unwrap().output_id, "", "test fixture: no real output device");
        worker.handle(Command::SelectOutput(Some("unchanged-output-id".into())));

        worker.seek_to(TrackKey::whole_file(PathBuf::from("/music/current.flac")), 0);

        let mut seek_failed_status = None;
        while let Ok(event) = events.try_recv() {
            if let PlaybackEvent::Status(message) = event
                && message.starts_with("Seek failed")
            {
                seek_failed_status = Some(message);
            }
        }
        let status = seek_failed_status.expect("expected a 'Seek failed' status");
        assert!(
            status.contains("failed to parse device ID \"\""),
            "seek must fail on the pinned (empty) active output id, got: {status}"
        );
        assert!(
            !status.contains("unchanged-output-id"),
            "seek must never fall back to the current output selection, got: {status}"
        );
        drain_retiring_decoders(&mut worker);
    }

    #[test]
    fn retire_preparation_cancels_the_decoder_before_retiring_it() {
        let mut worker = worker_with_active_track(Vec::new());
        let (events_tx, _events_rx) = unbounded();
        let prep = spawn_track_preparation(fixture_track("decoder-tone.flac"), 0, None, events_tx).unwrap();
        let cancel = prep.audio.cancel.clone();

        worker.retire_preparation(prep);

        assert!(cancel.load(Ordering::Acquire), "retire_preparation must cancel the preparation's decoder");
        assert_eq!(worker.retiring_decoders.len(), 1);
        drain_retiring_decoders(&mut worker);
    }

    #[test]
    fn queued_seeks_for_same_path_coalesce_and_other_commands_keep_order() {
        let (command_tx, command_rx) = unbounded::<Command>();
        let key = TrackKey::whole_file(PathBuf::from("/music/a.flac"));
        command_tx.send(Command::Seek { key: key.clone(), position_ms: 2_000 }).unwrap();
        command_tx.send(Command::TogglePlayback).unwrap();
        command_tx.send(Command::Seek { key: key.clone(), position_ms: 3_000 }).unwrap();

        let (target_ms, next_command) = drain_same_key_seeks(&command_rx, &key, 1_000);

        assert_eq!(target_ms, 2_000);
        assert!(matches!(next_command, Some(Command::TogglePlayback)));
        assert!(matches!(command_rx.try_recv(), Ok(Command::Seek { position_ms: 3_000, .. })));
    }

    #[test]
    fn handle_seek_stashes_the_next_command_behind_the_workers_own_queued_seeks() {
        // `drain_same_key_seeks` above only unit-tests the helper; this exercises the actual
        // wiring in `handle` (which stores `pending_command`) against the worker's own command
        // channel, which `run()` replays before the next `recv_timeout` (§4.2).
        let (mut worker, events) = worker_with_active_track_and_events(Vec::new());
        let (command_tx, command_rx) = unbounded();
        worker.commands = command_rx;
        // A key that never matches the active track: `seek_to` rejects it on the track guard,
        // so no hardware is ever reached.
        let key = TrackKey::whole_file(PathBuf::from("/music/a.flac"));
        command_tx.send(Command::Seek { key: key.clone(), position_ms: 2_000 }).unwrap();
        command_tx.send(Command::TogglePlayback).unwrap();
        command_tx.send(Command::Seek { key: key.clone(), position_ms: 3_000 }).unwrap();

        worker.handle(Command::Seek { key: key.clone(), position_ms: 1_000 });

        assert!(
            matches!(worker.pending_command, Some(Command::TogglePlayback)),
            "handle must stash the first non-seek command it drained behind the coalesced seeks"
        );
        assert!(
            matches!(worker.commands.try_recv(), Ok(Command::Seek { position_ms: 3_000, .. })),
            "the later Seek must stay queued for the next handle() call"
        );
        while events.try_recv().is_ok() {} // drain the rejection this stale-path seek produced
    }

    #[test]
    fn stop_active_now_without_emit_sends_no_playing_event() {
        let (mut worker, events) = worker_with_active_track_and_events(Vec::new());

        let prepared = worker.stop_active_now(false);

        assert_eq!(prepared.map(|p| p.key.path), Some(PathBuf::from("/music/current.flac")));
        assert!(worker.active.is_none());
        assert!(worker.last_stop.is_some(), "stop_active_now always records the stop for the rate-change guard");
        assert!(events.try_recv().is_err(), "no Playing event should be emitted when emit_playing is false");
    }

    #[test]
    fn stop_active_now_flushes_pending_underrun_growth_without_a_playing_event() {
        let (mut worker, events) = worker_with_active_track_and_events(Vec::new());
        // Underrun growth the rate limiter has not reported yet (`§1.4`) must not be dropped
        // just because the track is leaving `self.active` through an immediate stop.
        worker.active.as_ref().unwrap().diagnostics.underrun_samples.store(882, Ordering::Release);

        worker.stop_active_now(false);

        let mut saw_underrun_status = false;
        while let Ok(event) = events.try_recv() {
            match event {
                PlaybackEvent::Status(message) => {
                    assert!(message.starts_with("Playback underrun"), "unexpected status: {message}");
                    saw_underrun_status = true;
                }
                PlaybackEvent::Playing(_) => panic!("no Playing event should be emitted when emit_playing is false"),
                _ => {}
            }
        }
        assert!(saw_underrun_status, "pending underrun growth must still be reported on an immediate stop");
    }

    #[test]
    // The device ids below are `coreaudio:...`; cpal's `HostId::from_str("coreaudio")` only
    // resolves on macOS, matching the production call site (player.rs `start_track_prepared`),
    // which is itself `#[cfg(target_os = "macos")]`.
    #[cfg(target_os = "macos")]
    fn rate_change_guard_waits_only_for_same_device_new_rate_within_90ms() {
        let now = Instant::now();
        let last =
            StoppedOutput { at: now, device_id: "coreaudio:builtin-output".into(), rate: 44_100, integer_output: true };

        assert_eq!(
            rate_change_guard_wait(Some(&last), "builtin-output", 44_100, now),
            None,
            "same device, same rate: nothing changed"
        );
        assert_eq!(
            rate_change_guard_wait(Some(&last), "other-output", 96_000, now),
            None,
            "a different device was never using this rate window"
        );
        assert_eq!(rate_change_guard_wait(None, "builtin-output", 96_000, now), None, "nothing to guard against");

        let elapsed = Duration::from_millis(20);
        let wait = rate_change_guard_wait(Some(&last), "builtin-output", 96_000, now + elapsed)
            .expect("a same-device rate change within the guard window must wait");
        assert_eq!(wait, OUTPUT_DRAIN_GUARD - elapsed);

        assert_eq!(
            rate_change_guard_wait(Some(&last), "builtin-output", 96_000, now + OUTPUT_DRAIN_GUARD),
            None,
            "no wait once the guard window has fully elapsed"
        );
    }

    #[test]
    fn previous_on_failed_active_restarts_through_queue() {
        let mut worker = worker_with_active_track(Vec::new());
        worker.active.as_ref().unwrap().decoder_failed.store(true, Ordering::Release);

        worker.previous_track();

        assert!(worker.active.is_none(), "restart_current_from_queue stops the unhealthy active track");
        assert_eq!(
            worker.queue.front().map(|t| t.key.path.clone()),
            Some(PathBuf::from("/music/current.flac")),
            "the unhealthy track is requeued at the head for the restart attempt"
        );
        cancel_and_reap_lookahead(&mut worker);
        drain_retiring_decoders(&mut worker);
    }

    #[test]
    fn replace_queue_moves_current_to_history_and_resets_queue() {
        let stale_pending = fixture_track("decoder-tone.wv");
        let mut worker = worker_with_active_track(vec![stale_pending]);

        let first = fixture_track("decoder-tone.flac");
        let first_path = first.key.path.clone();
        let second = fixture_track("decoder-tone.wav");
        let second_path = second.key.path.clone();
        worker.replace_queue(vec![first, second]);

        assert_eq!(
            worker.history.last().map(|t| t.key.path.clone()),
            Some(PathBuf::from("/music/current.flac")),
            "the previously active track moves to history"
        );
        assert_eq!(
            worker.queue.iter().map(|t| t.key.path.clone()).collect::<Vec<_>>(),
            vec![first_path, second_path],
            "replace_queue discards the old pending queue and takes the new list verbatim, in order"
        );
        assert!(worker.active.is_none(), "stop_active_now clears the active slot before the new list starts");

        drain_retiring_decoders(&mut worker);
        cancel_and_reap_lookahead(&mut worker);
    }

    #[test]
    fn play_next_inserts_in_order_at_front() {
        let existing = test_track("/music/existing.flac");
        let mut worker = worker_with_active_track(vec![existing.clone()]);

        let first = test_track("/music/first.flac");
        let second = test_track("/music/second.flac");
        worker.play_next(vec![first.clone(), second.clone()]);

        assert_eq!(
            worker.queue.iter().map(|t| t.key.path.clone()).collect::<Vec<_>>(),
            vec![first.key.path, second.key.path, existing.key.path],
            "play_next inserts the new tracks in order ahead of the existing pending queue"
        );
    }

    #[test]
    fn previous_pops_history_and_requeues_current() {
        let queued = test_track("/music/queued.flac");
        let mut worker = worker_with_active_track(vec![queued.clone()]);
        let prior = test_track("/music/prior.flac");
        worker.history.push(prior.clone());

        worker.previous_track();

        assert!(worker.history.is_empty(), "the popped history entry is removed");
        assert_eq!(
            worker.queue.iter().map(|t| t.key.path.clone()).collect::<Vec<_>>(),
            vec![prior.key.path, PathBuf::from("/music/current.flac"), queued.key.path],
            "the previous track leads, the just-stopped current track follows, then the rest of the pending queue"
        );
        drain_retiring_decoders(&mut worker);
        cancel_and_reap_lookahead(&mut worker);
    }

    #[test]
    fn previous_after_three_seconds_restarts_current_even_with_history() {
        // The stage's manual check #4: `pos >= 3 s` restarts the current track through
        // `seek_to`, rather than popping history, even when history is non-empty.
        let (mut worker, events) = worker_with_active_track_and_events(Vec::new());
        let prior = test_track("/music/prior.flac");
        worker.history.push(prior.clone());
        // 3 s at 44.1 kHz stereo (`consumed_output_samples` counts interleaved samples).
        worker.active.as_ref().unwrap().consumed_output_samples.store(3 * 44_100 * 2, Ordering::Release);

        worker.previous_track();

        assert_eq!(worker.history.len(), 1, "history is untouched: the current track restarts instead of being popped");
        assert_eq!(worker.history.last().map(|t| t.key.path.clone()), Some(prior.key.path.clone()));
        assert_ne!(
            worker.queue.front().map(|t| t.key.path.clone()),
            Some(prior.key.path),
            "the queue head must not become the history track"
        );

        let mut saw_seek_failed_status = false;
        while let Ok(event) = events.try_recv() {
            if let PlaybackEvent::Status(message) = event
                && message.starts_with("Seek failed")
            {
                saw_seek_failed_status = true;
            }
        }
        assert!(
            saw_seek_failed_status,
            "past the 3 s threshold, Previous must go through seek_to, not the history pop path"
        );
        drain_retiring_decoders(&mut worker);
    }

    #[test]
    fn history_is_bounded() {
        let mut worker = worker_with_active_track(Vec::new());
        for i in 0..250 {
            worker.push_history(test_track(&format!("/music/track-{i}.flac")));
        }

        assert_eq!(worker.history.len(), HISTORY_LIMIT);
        assert_eq!(worker.history.first().unwrap().key.path, PathBuf::from("/music/track-50.flac"));
        assert_eq!(worker.history.last().unwrap().key.path, PathBuf::from("/music/track-249.flac"));
    }

    #[test]
    fn retiring_decoders_are_reaped_when_finished() {
        let mut worker = worker_with_active_track(Vec::new());
        let still_running_flag = Arc::new(AtomicBool::new(false));
        let thread_flag = still_running_flag.clone();
        let still_running = thread::spawn(move || {
            while !thread_flag.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1));
            }
        });
        let finished = thread::spawn(|| {});
        for _ in 0..1_000 {
            if finished.is_finished() {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert!(finished.is_finished());
        worker.retiring_decoders.push(finished);
        worker.retiring_decoders.push(still_running);

        worker.reap_retiring_decoders();

        assert_eq!(worker.retiring_decoders.len(), 1, "the still-running handle must be kept");

        still_running_flag.store(true, Ordering::Release);
        drain_retiring_decoders(&mut worker);
        assert!(worker.retiring_decoders.is_empty());
    }

    /// Fills `worker.retiring_decoders` to `MAX_RETIRING_DECODERS` with blocked handles, so a test
    /// can exercise the `retiring_decoders_at_capacity` refusal deterministically. Returns the flag
    /// that releases every handle; the caller must set it and drain before the test ends.
    fn fill_retiring_decoders_to_capacity(worker: &mut PlaybackWorker) -> Arc<AtomicBool> {
        let release = Arc::new(AtomicBool::new(false));
        for _ in 0..MAX_RETIRING_DECODERS {
            let thread_release = release.clone();
            worker.retiring_decoders.push(thread::spawn(move || {
                while !thread_release.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(1));
                }
            }));
        }
        release
    }

    #[test]
    fn replace_queue_is_refused_when_retiring_decoders_are_at_capacity() {
        let (mut worker, events) = worker_with_active_track_and_events(Vec::new());
        let release = fill_retiring_decoders_to_capacity(&mut worker);

        worker.replace_queue(vec![test_track("/music/new.flac")]);

        assert!(worker.active.is_some(), "the active track must not have been stopped");
        let mut saw_busy_status = false;
        while let Ok(event) = events.try_recv() {
            if let PlaybackEvent::Status(message) = event
                && message == RETIRING_DECODERS_BUSY_MESSAGE
            {
                saw_busy_status = true;
            }
        }
        assert!(saw_busy_status, "expected the retiring-decoders-busy refusal");

        release.store(true, Ordering::Release);
        drain_retiring_decoders(&mut worker);
    }

    #[test]
    fn previous_track_is_refused_when_retiring_decoders_are_at_capacity() {
        let (mut worker, events) = worker_with_active_track_and_events(Vec::new());
        worker.history.push(test_track("/music/before.flac"));
        let release = fill_retiring_decoders_to_capacity(&mut worker);

        worker.previous_track();

        assert!(worker.active.is_some(), "the active track must not have been stopped");
        assert_eq!(worker.history.len(), 1, "history must be untouched by a refused Previous");
        let mut saw_busy_status = false;
        while let Ok(event) = events.try_recv() {
            if let PlaybackEvent::Status(message) = event
                && message == RETIRING_DECODERS_BUSY_MESSAGE
            {
                saw_busy_status = true;
            }
        }
        assert!(saw_busy_status, "expected the retiring-decoders-busy refusal");

        release.store(true, Ordering::Release);
        drain_retiring_decoders(&mut worker);
    }

    #[test]
    fn restart_current_from_queue_is_refused_when_retiring_decoders_are_at_capacity() {
        let (mut worker, events) = worker_with_active_track_and_events(Vec::new());
        // No history and an unhealthy active track: `previous_track` takes the
        // `restart_current_from_queue` branch instead of going back in history.
        worker.active.as_ref().unwrap().decoder_failed.store(true, Ordering::Release);
        let release = fill_retiring_decoders_to_capacity(&mut worker);

        worker.previous_track();

        assert!(worker.active.is_some(), "the unhealthy active track must not have been stopped");
        let mut saw_busy_status = false;
        while let Ok(event) = events.try_recv() {
            if let PlaybackEvent::Status(message) = event
                && message == RETIRING_DECODERS_BUSY_MESSAGE
            {
                saw_busy_status = true;
            }
        }
        assert!(saw_busy_status, "expected the retiring-decoders-busy refusal");

        release.store(true, Ordering::Release);
        drain_retiring_decoders(&mut worker);
    }

    #[test]
    fn volume_state_emits_only_on_meaningful_change() {
        assert!(volume_changed_enough(true, false, 0.5, 0.5), "forced always emits, even with no real change");
        assert!(volume_changed_enough(false, true, 0.5, 0.5), "an availability flip always emits");
        assert!(volume_changed_enough(false, false, 0.5, 0.51), "a level move past the epsilon emits");
        assert!(
            !volume_changed_enough(false, false, 0.5, 0.502),
            "a level move within the epsilon is noise, not a real change"
        );
        assert!(!volume_changed_enough(false, false, 0.5, 0.5), "no change at all never emits");
    }

    #[test]
    fn volume_detail_messages_are_honest() {
        // `§1.4`: volume copy says "Device volume (CoreAudio)", never "hardware volume", and never
        // overstates what the strict route actually verifies. Checked against the real production
        // constants (`§6` Stage 4 fix), not a hand-copied duplicate that could drift from them.
        for detail in [VOLUME_DETAIL_NO_TARGET, VOLUME_DETAIL_AVAILABLE, VOLUME_DETAIL_NO_CONTROL] {
            assert!(!detail.to_lowercase().contains("hardware"), "dishonest detail message: {detail}");
            assert!(!detail.to_lowercase().contains("bit-perfect"), "unverified claim in detail message: {detail}");
        }
        assert!(VOLUME_DETAIL_AVAILABLE.contains("Device volume (CoreAudio)"));
    }

    #[test]
    // The device ids below are `coreaudio:...`; like `rate_change_guard_wait_...` above, cpal's
    // `HostId::from_str("coreaudio")` only resolves on macOS, matching the production call sites.
    // The platform-neutral assertions live in the sibling test below.
    #[cfg(target_os = "macos")]
    fn volume_uid_prefers_active_output_over_selection() {
        // While a track plays, the slider always controls the device that is actually playing,
        // never a pending selection change (`§4.5`).
        assert_eq!(
            volume_target_uid(Some("coreaudio:playing-output"), Some("coreaudio:other-output")),
            Some("playing-output".to_owned())
        );
        assert_eq!(
            volume_target_uid(None, Some("coreaudio:selected-output")),
            Some("selected-output".to_owned()),
            "idle falls back to the selected output"
        );
    }

    #[test]
    fn volume_uid_resolves_to_none_without_a_parseable_device_id() {
        assert_eq!(volume_target_uid(None, None), None, "nothing selected and nothing playing resolves to no target");
        assert_eq!(
            volume_target_uid(Some("not-a-device-id"), None),
            None,
            "an unparseable device id string is treated as no target, never passed on to CoreAudio"
        );
    }

    #[test]
    fn set_volume_when_unavailable_snaps_back() {
        let (mut worker, events) = worker_with_active_track_and_events(Vec::new());
        // The fixture's active output id is empty (see `worker_with_active_track_and_events`), so
        // `refresh_volume` cannot resolve a real CoreAudio target; volume starts unavailable.
        worker.volume.available = false;
        while events.try_recv().is_ok() {}

        worker.set_volume(0.5);

        let mut saw_unavailable_volume = false;
        while let Ok(event) = events.try_recv() {
            if let PlaybackEvent::Volume { available: false, .. } = event {
                saw_unavailable_volume = true;
            }
        }
        assert!(saw_unavailable_volume, "a SetVolume with no settable target must snap the UI back with available: false");
    }

    #[test]
    fn set_volume_snaps_back_instead_of_setting_a_stale_target() {
        let (mut worker, events) = worker_with_active_track_and_events(Vec::new());
        // Simulate a stale cached target (`§6` Stage 4 fix): the worker still believes volume
        // control targets a device from a previous refresh, but the live target (the fixture's
        // active `output_id` is empty, so it resolves to no target at all) has since changed.
        worker.volume.uid = Some("stale-device".into());
        worker.volume.available = true;
        while events.try_recv().is_ok() {}

        worker.set_volume(0.4);

        let mut saw_forced_volume = false;
        while let Ok(event) = events.try_recv() {
            if let PlaybackEvent::Volume { .. } = event {
                saw_forced_volume = true;
            }
        }
        assert!(
            saw_forced_volume,
            "a SetVolume after the target changed must force a re-read, never blindly set the stale uid"
        );
        assert_eq!(worker.volume.uid, None, "the cached uid must be re-resolved to the live target");
    }

    #[test]
    fn set_volume_rejects_a_non_finite_level() {
        let (mut worker, events) = worker_with_active_track_and_events(Vec::new());
        while events.try_recv().is_ok() {}

        worker.set_volume(f32::NAN);

        let mut saw_volume_event = false;
        while let Ok(event) = events.try_recv() {
            if let PlaybackEvent::Volume { level, .. } = event {
                assert!(level.is_finite(), "a NaN level must never reach the UI");
                saw_volume_event = true;
            }
        }
        assert!(saw_volume_event, "a non-finite level must still snap the UI back with a real value, never be ignored");
    }
}

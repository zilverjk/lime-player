//! The OS media session (`CLAUDE.md` "Media controls"): keyboard media keys (F7/F8/F9), the Control
//! Center / menu-bar Now Playing widget and its transport buttons and scrubber.
//!
//! macOS only does real work here, through MediaPlayer's `MPRemoteCommandCenter` (commands in) and
//! `MPNowPlayingInfoCenter` (metadata out). Every other platform gets a no-op `MediaSession` with the
//! same API, so the rest of the app never branches on the platform.
//!
//! Two layers, split so the decisions can be unit-tested on any OS without a live session:
//!
//! - The parent module: pure, platform-independent values and logic — `MediaCommand`,
//!   `NowPlayingState`, how a command maps to an app action (`plan_media_command`), what a publish
//!   should say (`plan_transport_publish`, `timeline_needs_publish`) and the hold-to-seek stepping
//!   rules (`HoldSeek`). None of it touches MediaPlayer.
//! - `platform`: the FFI bridge. MediaPlayer's handlers only ever push a `MediaCommand` into a
//!   channel; `main.rs` drains it on the UI thread, where the queue/timeline/pending-seek state lives.
//!
//! No test constructs a `MediaSession` or touches a real command/now-playing center: doing so would
//! take over the developer's real media keys and Now Playing state.

use std::time::{Duration, Instant};

use crossbeam_channel::Sender;

/// Which way a held rewind/fast-forward key moves the position.
// Constructed only by the macOS bridge (and by tests), so other platforms would flag the variants.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekDirection {
    Backward,
    Forward,
}

/// A request from the OS media session, already reduced to what Lime Player can act on. Sent by
/// MediaPlayer's handler blocks (which may run on any thread) and drained on the UI thread.
// Constructed only by the macOS bridge (and by tests), so other platforms would flag the variants.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaCommand {
    TogglePlayPause,
    Play,
    Pause,
    NextTrack,
    PreviousTrack,
    /// A held rewind/fast-forward key (or Control Center button) went down. macOS sends no repeats:
    /// the app steps the position itself until `SeekEnd` (`HoldSeek`).
    SeekBegin(SeekDirection),
    SeekEnd(SeekDirection),
    /// A scrub of the Control Center progress bar, track-relative.
    SetPosition { position_ms: u64 },
}

/// The cover for the Now Playing widget: the picture the UI already shows for the playing track's
/// album, and its `AppState::artwork_revision_of` revision so the session converts it once per
/// picture instead of on every publish.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Clone)]
pub struct NowPlayingArtwork {
    pub revision: u64,
    pub image: slint::Image,
}

/// What the OS Now Playing widget should show, mirroring the app's own now-playing panel.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Clone)]
pub struct NowPlayingState {
    pub title: String,
    pub artist: String,
    pub album: String,
    /// `None` while the file has not reported its length: no progress bar, no scrubbing.
    pub duration_ms: Option<u64>,
    pub elapsed_ms: u64,
    /// Whether the app actually shows playing. The widget extrapolates elapsed time from this (rate
    /// 1.0 / 0.0) between publishes, so a paused track must say so.
    pub playing: bool,
    pub artwork: Option<NowPlayingArtwork>,
}

/// `MPChangePlaybackPositionCommandEvent.positionTime` (seconds, an arbitrary `f64`) as a
/// millisecond position. Float-to-integer `as` casts saturate, so NaN and negative values are 0 and
/// an absurdly large one is `u64::MAX` — the seek path then clamps it to the duration.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn position_seconds_to_ms(seconds: f64) -> u64 {
    (seconds * 1_000.0).round() as u64
}

// ---------------------------------------------------------------------------------------------
// Command mapping
// ---------------------------------------------------------------------------------------------

/// What the UI thread does for a `MediaCommand`, decided from the UI's current play state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportAction {
    /// The same path as the on-screen play/pause button (`AudioPlayer::toggle_playback`).
    TogglePlayback,
    /// `AudioPlayer::next` — the controller does the rest, including Next while paused.
    Next,
    /// `AudioPlayer::previous` — the controller already applies Apple Music's rule (restart the
    /// track after 3 s, otherwise go back), so it is deliberately not repeated here.
    Previous,
    /// The same seek path as the on-screen scrubber.
    SeekTo { position_ms: u64 },
    BeginHoldSeek(SeekDirection),
    EndHoldSeek(SeekDirection),
    /// Nothing to do (an idempotent Play while already playing, ...).
    Ignore,
}

/// Maps a `MediaCommand` to the app action. `ui_playing` is what the UI shows right now: the
/// controller only has a toggle, so a dedicated Play/Pause (a headset button, Control Center) becomes
/// a toggle only when it would actually change state, and is dropped otherwise.
pub fn plan_media_command(command: MediaCommand, ui_playing: bool) -> TransportAction {
    match command {
        MediaCommand::TogglePlayPause => TransportAction::TogglePlayback,
        MediaCommand::Play if !ui_playing => TransportAction::TogglePlayback,
        MediaCommand::Pause if ui_playing => TransportAction::TogglePlayback,
        MediaCommand::Play | MediaCommand::Pause => TransportAction::Ignore,
        MediaCommand::NextTrack => TransportAction::Next,
        MediaCommand::PreviousTrack => TransportAction::Previous,
        MediaCommand::SetPosition { position_ms } => TransportAction::SeekTo { position_ms },
        MediaCommand::SeekBegin(direction) => TransportAction::BeginHoldSeek(direction),
        MediaCommand::SeekEnd(direction) => TransportAction::EndHoldSeek(direction),
    }
}

// ---------------------------------------------------------------------------------------------
// Hold-to-seek
// ---------------------------------------------------------------------------------------------

/// How far one hold-to-seek step moves the position.
pub const HOLD_SEEK_STEP_MS: u64 = 5_000;
/// Delay before the first step after the key goes down, so a quick Begin/End pair (a Control Center
/// click may deliver one next to its next/previous command) is a no-op.
pub const HOLD_SEEK_INITIAL_DELAY: Duration = Duration::from_millis(250);
/// The least time between two steps. Every seek tears the output stream down and rebuilds it, so
/// stepping faster than the controller can finish only queues restarts.
pub const HOLD_SEEK_INTERVAL: Duration = Duration::from_millis(400);
/// A hold never lasts longer than this: an End can be lost (the OS drops the client's continuous
/// controls when it unregisters, the app can miss the release), and stepping must not run forever.
pub const HOLD_SEEK_FAILSAFE: Duration = Duration::from_secs(60);
/// The controller (`seek_target_frame`) never seeks closer than this to the end of a track; the
/// hold clamps the same way, so it stops where the controller would.
const SEEK_TAIL_MS: u64 = 500;

/// The position one hold step from `position_ms` moves to: 5 s in `direction`, clamped to
/// `[0, duration - 500 ms]`. `None` means nothing is left to do — the start or the end was reached
/// (a hold never skips to another track) — or the length is unknown, since the controller refuses a
/// nonzero seek without one and a forward step cannot be clamped.
pub fn hold_seek_target(position_ms: u64, direction: SeekDirection, duration_ms: Option<u64>) -> Option<u64> {
    let duration_ms = duration_ms.filter(|duration| *duration > 0)?;
    let max_ms = duration_ms.saturating_sub(SEEK_TAIL_MS);
    let target = match direction {
        // Already at (or, from the tail, past) the last seekable position: never step backwards to it.
        SeekDirection::Forward if position_ms >= max_ms => return None,
        SeekDirection::Forward => position_ms.saturating_add(HOLD_SEEK_STEP_MS).min(max_ms),
        SeekDirection::Backward => position_ms.saturating_sub(HOLD_SEEK_STEP_MS).min(max_ms),
    };
    (target != position_ms).then_some(target)
}

/// What `HoldSeek::tick` saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldSeekTick {
    /// Not armed.
    Idle,
    /// Armed, but no step is due (initial delay, interval, or a seek still pending).
    Wait,
    /// Seek the playing track to this track-relative position.
    Step { target_ms: u64 },
    /// The hold ended by itself (start/end reached, fail-safe): it is disarmed now.
    Finished,
}

/// The UI facts a step is computed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoldSeekInput {
    /// A seek is still in flight (`pending_seek.is_some()`): the position is not trustworthy yet.
    pub seek_pending: bool,
    /// The pending target if any, else the last applied timeline position.
    pub position_ms: u64,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct ArmedHold {
    direction: SeekDirection,
    armed_at: Instant,
    next_step_at: Instant,
    /// The target of the step last issued. A step that clamps to the same target again means the
    /// hold is pinned at the start or the end: comparing with the current position would not notice,
    /// because a playing track keeps advancing after the step lands (so a hold at the start would
    /// otherwise rewind to 0 again, restarting the output stream, every `HOLD_SEEK_INTERVAL`).
    last_target_ms: Option<u64>,
}

/// Hold-to-seek stepping: while a rewind/fast-forward key is held, move the position in
/// `HOLD_SEEK_STEP_MS` steps, the first after `HOLD_SEEK_INITIAL_DELAY`, then at most every
/// `HOLD_SEEK_INTERVAL` and only once the previous seek has completed, until the start or the end of
/// the track is reached. Pure state driven by `main.rs`'s UI timer through `tick`; it never touches
/// the player itself.
#[derive(Debug, Default)]
pub struct HoldSeek {
    armed: Option<ArmedHold>,
}

impl HoldSeek {
    /// The key went down. A repeated Begin for the direction already armed is ignored (it must not
    /// restart the delay); a Begin for the other direction replaces the hold.
    pub fn begin(&mut self, direction: SeekDirection, now: Instant) {
        if self.armed.is_some_and(|armed| armed.direction == direction) {
            return;
        }
        self.armed = Some(ArmedHold { direction, armed_at: now, next_step_at: now + HOLD_SEEK_INITIAL_DELAY, last_target_ms: None });
    }

    /// The key came up. Only the direction that is armed disarms: a late End for the direction a
    /// newer Begin already replaced must not cancel the hold that is really down.
    pub fn end(&mut self, direction: SeekDirection) {
        if self.armed.is_some_and(|armed| armed.direction == direction) {
            self.armed = None;
        }
    }

    /// Unconditionally stops the hold: a track change (`Started`), `Stopped`, `Inactive`, a rejected
    /// seek — anything after which stepping from the old position would be wrong.
    pub fn disarm(&mut self) {
        self.armed = None;
    }

    pub fn is_armed(&self) -> bool {
        self.armed.is_some()
    }

    /// Advances the hold to `now`. Cheap: called on every UI tick.
    pub fn tick(&mut self, now: Instant, input: HoldSeekInput) -> HoldSeekTick {
        let Some(armed) = self.armed.as_mut() else {
            return HoldSeekTick::Idle;
        };
        if now.saturating_duration_since(armed.armed_at) >= HOLD_SEEK_FAILSAFE {
            self.armed = None;
            return HoldSeekTick::Finished;
        }
        if now < armed.next_step_at || input.seek_pending {
            return HoldSeekTick::Wait;
        }
        match hold_seek_target(input.position_ms, armed.direction, input.duration_ms) {
            // A target equal to the previous step's is the clamp again: the start or the end is
            // reached, even though a playing track's position has moved on since the step landed.
            Some(target_ms) if armed.last_target_ms != Some(target_ms) => {
                armed.last_target_ms = Some(target_ms);
                armed.next_step_at = now + HOLD_SEEK_INTERVAL;
                HoldSeekTick::Step { target_ms }
            }
            _ => {
                self.armed = None;
                HoldSeekTick::Finished
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// What to publish to the OS
// ---------------------------------------------------------------------------------------------

/// The UI facts a Now Playing publish is derived from (`plan_transport_publish`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportSnapshot {
    /// The UI has an active now-playing track (`Started` seen, no `Stopped`/`Inactive` since).
    pub has_track: bool,
    /// The on-screen play button would do something (`main.rs` `play_button_enabled`): a track has
    /// played this session, or the queue is not empty. With no active track this is what keeps the
    /// session: the controller keeps a track that failed to (re)start at the queue head and asks the
    /// user to press Play, and the button can replay the last finished one, so the media key must
    /// reach that same retry instead of launching another app.
    pub can_resume: bool,
    /// What the play/pause button shows.
    pub ui_playing: bool,
    pub duration_ms: Option<u64>,
    /// The last timeline position the UI applied.
    pub last_applied_ms: u64,
    /// The target of a seek still in flight, which the UI already shows.
    pub pending_seek_ms: Option<u64>,
}

/// The transport half of a `NowPlayingState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportPlan {
    pub playing: bool,
    pub elapsed_ms: u64,
    pub duration_ms: Option<u64>,
}

/// What to publish for `snapshot`, or `None` to clear the session (nothing is loaded and the play
/// button would do nothing, so an idle Lime must not hold the media keys). The widget shows what the
/// app shows: the playing flag as the UI has it, and the elapsed time of a seek in flight as its
/// target — otherwise a Control Center scrub would jump back to the old position until the seek
/// completed. A zero duration is unknown.
///
/// With no active track but a play button that still works (a blocked queue head awaiting a retry,
/// the last finished track that can be replayed), the session stays as a PAUSED one, never cleared:
/// macOS sends the media keys to another app (Music.app) as soon as playback state is `Stopped`, and
/// a retry would then take the player-bar button and the keyboard to different places. Such a
/// session reports position 0 and no length: whatever Play starts starts from the beginning, and
/// there is nothing to scrub.
pub fn plan_transport_publish(snapshot: &TransportSnapshot) -> Option<TransportPlan> {
    if !snapshot.has_track {
        return snapshot.can_resume.then_some(TransportPlan { playing: false, elapsed_ms: 0, duration_ms: None });
    }
    let duration_ms = snapshot.duration_ms.filter(|duration| *duration > 0);
    let elapsed_ms = snapshot.pending_seek_ms.unwrap_or(snapshot.last_applied_ms);
    Some(TransportPlan { playing: snapshot.ui_playing, elapsed_ms: duration_ms.map_or(elapsed_ms, |duration| elapsed_ms.min(duration)), duration_ms })
}

/// A timeline position further than this from where the OS would extrapolate to is a discontinuity
/// (a seek landed, playback stalled, the silent DAC pre-roll before the first sample) that needs a
/// fresh publish. Far above the ~50 ms jitter of ordinary `Timeline` ticks, far below any seek.
pub const TIMELINE_DRIFT_TOLERANCE_MS: u64 = 750;

/// What the OS was last told, kept so accepted timeline positions can be compared with what the
/// widget extrapolates on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishedTransport {
    position_ms: u64,
    at: Instant,
    playing: bool,
    duration_ms: Option<u64>,
}

impl PublishedTransport {
    pub fn new(plan: &TransportPlan, at: Instant) -> Self {
        Self { position_ms: plan.elapsed_ms, at, playing: plan.playing, duration_ms: plan.duration_ms }
    }

    /// Where the widget's progress bar is at `now`: elapsed time only advances at rate 1.0.
    fn extrapolated_ms(&self, now: Instant) -> u64 {
        if self.playing {
            self.position_ms.saturating_add(now.saturating_duration_since(self.at).as_millis() as u64)
        } else {
            self.position_ms
        }
    }
}

/// Whether an accepted `Timeline` (`position_ms`, `duration_ms`) needs a new Now Playing publish.
/// The widget extrapolates from the last publish, so the ~50 ms timeline ticks are deliberately NOT
/// forwarded: only nothing published yet, a late-discovered duration, or a position that has
/// drifted from the extrapolation (a seek from anywhere — scrubber, hold step, Previous restarting
/// the track — or a stall) marks the session dirty.
pub fn timeline_needs_publish(published: Option<&PublishedTransport>, position_ms: u64, duration_ms: Option<u64>, now: Instant) -> bool {
    let Some(published) = published else {
        return true;
    };
    published.duration_ms != duration_ms.filter(|duration| *duration > 0)
        || position_ms.abs_diff(published.extrapolated_ms(now)) > TIMELINE_DRIFT_TOLERANCE_MS
}

// ---------------------------------------------------------------------------------------------
// The session
// ---------------------------------------------------------------------------------------------

/// The OS media session. On macOS: media-key and Control Center commands arrive on the channel given
/// to `new`, and `publish`/`clear` drive the Now Playing widget. Elsewhere: inert.
///
/// Create it once, on the UI thread, from `main()` only — never from code `MainWindow::new()` can
/// reach (the offscreen snapshot harness builds one on a software platform). It is `!Send` on macOS
/// (it owns Objective-C objects) and registers process-wide handlers, so a second one is inert.
pub struct MediaSession {
    session: Option<platform::Session>,
}

impl MediaSession {
    /// Registers the OS command handlers, which only ever `send` into `sender`. Until the first
    /// `publish` they are disabled, so an idle Lime does not take the media keys from another player.
    pub fn new(sender: Sender<MediaCommand>) -> Self {
        Self { session: platform::Session::new(sender) }
    }

    /// Shows `state` in the Now Playing widget, enabling the transport commands. Call it once per UI
    /// tick at most, from the final state of the tick — see `main.rs`.
    pub fn publish(&mut self, state: &NowPlayingState) {
        if let Some(session) = self.session.as_mut() {
            session.publish(state);
        }
    }

    /// Nothing is loaded: empties the widget and disables the transport commands so the media keys go
    /// back to whichever app had them. Idempotent.
    pub fn clear(&mut self) {
        if let Some(session) = self.session.as_mut() {
            session.clear();
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use crossbeam_channel::Sender;

    use super::{MediaCommand, NowPlayingState};

    /// No OS media session outside macOS.
    pub struct Session;

    impl Session {
        pub fn new(_sender: Sender<MediaCommand>) -> Option<Self> {
            Some(Session)
        }

        pub fn publish(&mut self, _state: &NowPlayingState) {}

        pub fn clear(&mut self) {}
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::ptr::{self, NonNull};
    use std::sync::atomic::{AtomicBool, Ordering};

    use block2::RcBlock;
    use crossbeam_channel::Sender;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{AnyThread, MainThreadMarker, Message};
    use objc2_app_kit::{NSBitmapImageRep, NSDeviceRGBColorSpace, NSImage};
    use objc2_core_foundation::CGSize;
    use objc2_foundation::{NSDictionary, NSNumber, NSString};
    use objc2_media_player::{
        MPChangePlaybackPositionCommandEvent, MPMediaItemArtwork, MPMediaItemPropertyAlbumTitle, MPMediaItemPropertyArtist,
        MPMediaItemPropertyArtwork, MPMediaItemPropertyPlaybackDuration, MPMediaItemPropertyTitle, MPNowPlayingInfoCenter,
        MPNowPlayingInfoMediaType, MPNowPlayingInfoPropertyDefaultPlaybackRate, MPNowPlayingInfoPropertyElapsedPlaybackTime,
        MPNowPlayingInfoPropertyMediaType, MPNowPlayingInfoPropertyPlaybackRate, MPNowPlayingPlaybackState, MPRemoteCommand,
        MPRemoteCommandCenter, MPRemoteCommandEvent, MPRemoteCommandHandlerStatus, MPSeekCommandEvent, MPSeekCommandEventType,
    };

    use super::{MediaCommand, NowPlayingArtwork, NowPlayingState, SeekDirection, position_seconds_to_ms};

    /// MediaPlayer's command center is process-wide: registering the handlers twice stacks them and
    /// every key fires twice. Only the first `Session` is real.
    static REGISTERED: AtomicBool = AtomicBool::new(false);

    /// One command with the handler registered on it. `token` is the opaque target
    /// `addTargetWithHandler:` returned, needed for `removeTarget:`; the command center itself keeps
    /// the handler block alive until then.
    struct Registered {
        command: Retained<MPRemoteCommand>,
        token: Retained<AnyObject>,
    }

    /// The cover as MediaPlayer wants it, kept while its revision is the one being shown. `None`
    /// records a picture that could not be converted, so it is not retried on every publish.
    struct CachedArtwork {
        revision: u64,
        artwork: Option<Retained<MPMediaItemArtwork>>,
    }

    pub struct Session {
        /// Everything but the scrubber: play, pause, toggle, next, previous, seek forward/backward.
        transport: Vec<Registered>,
        /// `changePlaybackPositionCommand`, enabled only while the length is known (the controller
        /// refuses a nonzero seek without one).
        position: Registered,
        transport_enabled: bool,
        position_enabled: bool,
        /// Something is published (so `clear` has work to do).
        active: bool,
        artwork: Option<CachedArtwork>,
    }

    /// Registers `f` as the handler of `command`, initially disabled.
    fn add_handler<F>(command: Retained<MPRemoteCommand>, f: F) -> Registered
    where
        F: Fn(&MPRemoteCommandEvent) -> MPRemoteCommandHandlerStatus + 'static,
    {
        let block = RcBlock::new(move |event: NonNull<MPRemoteCommandEvent>| -> MPRemoteCommandHandlerStatus {
            // SAFETY: MediaPlayer passes a valid, live event for the duration of the call.
            f(unsafe { event.as_ref() })
        });
        // SAFETY: plain Objective-C calls on live objects; the command copies the block, so dropping
        // ours is fine.
        let token = unsafe {
            command.setEnabled(false);
            command.addTargetWithHandler(&block)
        };
        Registered { command, token }
    }

    impl Session {
        /// `None` off the main thread (MediaPlayer expects the AppKit main thread, which is the Slint
        /// UI thread on macOS) or when a session already exists in this process.
        pub fn new(sender: Sender<MediaCommand>) -> Option<Self> {
            let _main_thread = MainThreadMarker::new()?;
            if REGISTERED.swap(true, Ordering::AcqRel) {
                return None;
            }
            // SAFETY: Objective-C calls on the shared command center, from the main thread.
            unsafe {
                let center = MPRemoteCommandCenter::sharedCommandCenter();
                // Handlers run on a thread MediaPlayer picks: they only send and return immediately.
                let simple = |command: Retained<MPRemoteCommand>, message: MediaCommand| {
                    let sender = sender.clone();
                    add_handler(command, move |_event| {
                        let _ = sender.send(message);
                        MPRemoteCommandHandlerStatus::Success
                    })
                };
                let mut transport = vec![
                    simple(center.playCommand(), MediaCommand::Play),
                    simple(center.pauseCommand(), MediaCommand::Pause),
                    simple(center.togglePlayPauseCommand(), MediaCommand::TogglePlayPause),
                    simple(center.nextTrackCommand(), MediaCommand::NextTrack),
                    simple(center.previousTrackCommand(), MediaCommand::PreviousTrack),
                ];
                // Holding F7/F9 (or a Control Center rewind/forward press) arrives as a
                // Begin/EndSeeking pair on these two commands, with no repeats in between.
                for (direction, command) in [
                    (SeekDirection::Forward, center.seekForwardCommand()),
                    (SeekDirection::Backward, center.seekBackwardCommand()),
                ] {
                    let sender = sender.clone();
                    transport.push(add_handler(command, move |event| {
                        let message = match event.downcast_ref::<MPSeekCommandEvent>() {
                            Some(seek) if seek.r#type() == MPSeekCommandEventType::EndSeeking => MediaCommand::SeekEnd(direction),
                            // BeginSeeking, or defensively anything unexpected.
                            _ => MediaCommand::SeekBegin(direction),
                        };
                        let _ = sender.send(message);
                        MPRemoteCommandHandlerStatus::Success
                    }));
                }
                let position = {
                    let sender = sender.clone();
                    add_handler(Retained::into_super(center.changePlaybackPositionCommand()), move |event| {
                        match event.downcast_ref::<MPChangePlaybackPositionCommandEvent>() {
                            Some(change) => {
                                let position_ms = position_seconds_to_ms(change.positionTime());
                                let _ = sender.send(MediaCommand::SetPosition { position_ms });
                                MPRemoteCommandHandlerStatus::Success
                            }
                            None => MPRemoteCommandHandlerStatus::CommandFailed,
                        }
                    })
                };
                // Everything Lime does not implement stays disabled for good, so no dead buttons are
                // drawn and skip-by-interval never replaces the seek commands the hold relies on.
                center.stopCommand().setEnabled(false);
                Retained::into_super(center.skipForwardCommand()).setEnabled(false);
                Retained::into_super(center.skipBackwardCommand()).setEnabled(false);
                Retained::into_super(center.changePlaybackRateCommand()).setEnabled(false);
                Retained::into_super(center.changeRepeatModeCommand()).setEnabled(false);
                Retained::into_super(center.changeShuffleModeCommand()).setEnabled(false);
                Retained::into_super(center.ratingCommand()).setEnabled(false);
                Retained::into_super(center.likeCommand()).setEnabled(false);
                Retained::into_super(center.dislikeCommand()).setEnabled(false);
                Retained::into_super(center.bookmarkCommand()).setEnabled(false);
                center.enableLanguageOptionCommand().setEnabled(false);
                center.disableLanguageOptionCommand().setEnabled(false);
                Some(Self {
                    transport,
                    position,
                    transport_enabled: false,
                    position_enabled: false,
                    active: false,
                    artwork: None,
                })
            }
        }

        fn set_transport_enabled(&mut self, enabled: bool) {
            if self.transport_enabled != enabled {
                for registered in &self.transport {
                    // SAFETY: a plain property write on a live command.
                    unsafe { registered.command.setEnabled(enabled) };
                }
                self.transport_enabled = enabled;
            }
        }

        fn set_position_enabled(&mut self, enabled: bool) {
            if self.position_enabled != enabled {
                // SAFETY: as above.
                unsafe { self.position.command.setEnabled(enabled) };
                self.position_enabled = enabled;
            }
        }

        pub fn publish(&mut self, state: &NowPlayingState) {
            self.set_transport_enabled(true);
            self.set_position_enabled(state.duration_ms.is_some());
            let artwork = self.artwork_for(state.artwork.as_ref());
            let info = build_info(state, artwork.as_ref());
            // SAFETY: Objective-C calls on the default center. `playbackState` must be set on every
            // publish on macOS (MPNowPlayingInfoCenter.h): without it the media keys keep going to
            // whichever other app last played.
            unsafe {
                let center = MPNowPlayingInfoCenter::defaultCenter();
                center.setNowPlayingInfo(Some(&info));
                center.setPlaybackState(if state.playing { MPNowPlayingPlaybackState::Playing } else { MPNowPlayingPlaybackState::Paused });
            }
            self.active = true;
        }

        pub fn clear(&mut self) {
            if !self.active {
                return;
            }
            // SAFETY: as in `publish`.
            unsafe {
                let center = MPNowPlayingInfoCenter::defaultCenter();
                center.setNowPlayingInfo(None);
                center.setPlaybackState(MPNowPlayingPlaybackState::Stopped);
            }
            self.set_transport_enabled(false);
            self.set_position_enabled(false);
            // A decoded cover is up to ~480 KB; idle Lime holds none.
            self.artwork = None;
            self.active = false;
        }

        /// The MediaPlayer artwork for `wanted`, converted only when its revision changes. Every
        /// `nowPlayingInfo` dictionary must carry the artwork key or the widget drops the cover.
        fn artwork_for(&mut self, wanted: Option<&NowPlayingArtwork>) -> Option<Retained<MPMediaItemArtwork>> {
            let Some(wanted) = wanted else {
                self.artwork = None;
                return None;
            };
            if let Some(cached) = self.artwork.as_ref().filter(|cached| cached.revision == wanted.revision) {
                return cached.artwork.clone();
            }
            let artwork = artwork_from_image(&wanted.image);
            self.artwork = Some(CachedArtwork { revision: wanted.revision, artwork: artwork.clone() });
            artwork
        }
    }

    impl Drop for Session {
        fn drop(&mut self) {
            // SAFETY: as in `publish`. Runs when `main()` unwinds its UI state at exit: leaves no
            // handler pointing at a dead channel and no stale widget behind.
            unsafe {
                for registered in self.transport.iter().chain(std::iter::once(&self.position)) {
                    registered.command.removeTarget(Some(&registered.token));
                    registered.command.setEnabled(false);
                }
                if self.active {
                    let center = MPNowPlayingInfoCenter::defaultCenter();
                    center.setNowPlayingInfo(None);
                    center.setPlaybackState(MPNowPlayingPlaybackState::Stopped);
                }
            }
        }
    }

    /// Upcast to `AnyObject` for storage in an `NSDictionary<NSString, AnyObject>`.
    fn any<T: Message>(object: Retained<T>) -> Retained<AnyObject> {
        // SAFETY: every Objective-C object is an `AnyObject`; this only forgets the static type.
        unsafe { Retained::cast_unchecked(object) }
    }

    /// The `nowPlayingInfo` dictionary for `state`. Always a fresh, complete dictionary: the property
    /// is a copy and setting it is asynchronous, so it is never read back and merged.
    fn build_info(state: &NowPlayingState, artwork: Option<&Retained<MPMediaItemArtwork>>) -> Retained<NSDictionary<NSString, AnyObject>> {
        // SAFETY: the `MP*Property*` constants are immutable `NSString` globals of MediaPlayer.
        unsafe {
            let mut keys: Vec<&NSString> = vec![
                MPMediaItemPropertyTitle,
                MPMediaItemPropertyArtist,
                MPMediaItemPropertyAlbumTitle,
                MPNowPlayingInfoPropertyElapsedPlaybackTime,
                // Rate 0.0 while paused, or the widget keeps its bar running (or shows 0:00); the
                // system extrapolates elapsed time from this between publishes.
                MPNowPlayingInfoPropertyPlaybackRate,
                MPNowPlayingInfoPropertyDefaultPlaybackRate,
                MPNowPlayingInfoPropertyMediaType,
            ];
            let mut values: Vec<Retained<AnyObject>> = vec![
                any(NSString::from_str(&state.title)),
                any(NSString::from_str(&state.artist)),
                any(NSString::from_str(&state.album)),
                any(NSNumber::new_f64(state.elapsed_ms as f64 / 1_000.0)),
                any(NSNumber::new_f64(if state.playing { 1.0 } else { 0.0 })),
                any(NSNumber::new_f64(1.0)),
                any(NSNumber::new_u64(MPNowPlayingInfoMediaType::Audio.0 as u64)),
            ];
            if let Some(duration_ms) = state.duration_ms {
                keys.push(MPMediaItemPropertyPlaybackDuration);
                values.push(any(NSNumber::new_f64(duration_ms as f64 / 1_000.0)));
            }
            if let Some(artwork) = artwork {
                keys.push(MPMediaItemPropertyArtwork);
                values.push(any(artwork.clone()));
            }
            NSDictionary::from_retained_objects(&keys, &values)
        }
    }

    /// `MPMediaItemArtwork` for the picture the UI shows (the scanner's RGB8 thumbnail, longest side
    /// capped at 400 px), without an encode/decode round trip.
    fn artwork_from_image(image: &slint::Image) -> Option<Retained<MPMediaItemArtwork>> {
        let buffer = image.to_rgb8()?;
        artwork_from_rgb(buffer.as_bytes(), buffer.width() as usize, buffer.height() as usize)
    }

    /// `rgb` is tightly packed RGB8, `width * height * 3` bytes.
    fn artwork_from_rgb(rgb: &[u8], width: usize, height: usize) -> Option<Retained<MPMediaItemArtwork>> {
        if width == 0 || height == 0 || rgb.len() < width * height * 3 {
            return None;
        }
        // SAFETY: AppKit allocates the bitmap (null planes); rows may be padded, so each row is
        // copied to its own `bytesPerRow` stride, staying inside the buffer AppKit owns.
        unsafe {
            let representation = NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
                NSBitmapImageRep::alloc(),
                ptr::null_mut(),
                width as isize,
                height as isize,
                8,
                3,
                false,
                false,
                NSDeviceRGBColorSpace,
                0,
                0,
            )?;
            let stride = representation.bytesPerRow() as usize;
            let destination = representation.bitmapData();
            if destination.is_null() || stride < width * 3 {
                return None;
            }
            for row in 0..height {
                ptr::copy_nonoverlapping(rgb.as_ptr().add(row * width * 3), destination.add(row * stride), width * 3);
            }
            let size = CGSize { width: width as f64, height: height as f64 };
            let image = NSImage::initWithSize(NSImage::alloc(), size);
            image.addRepresentation(&representation);
            // MediaPlayer calls this (possibly off the main thread) whenever it needs a size: the
            // block owns the image and hands back the same one every time.
            let handler = RcBlock::new(move |_requested: CGSize| -> NonNull<NSImage> { NonNull::from(&*image) });
            Some(MPMediaItemArtwork::initWithBoundsSize_requestHandler(MPMediaItemArtwork::alloc(), size, &handler))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Builds the objects a publish hands MediaPlayer without ever touching a command center or
        /// the now-playing center, so nothing here registers with the OS or moves the media keys.
        #[test]
        fn now_playing_dictionary_and_artwork_build_without_registering_a_session() {
            let (width, height) = (5usize, 3usize);
            let rgb: Vec<u8> = (0..width * height * 3).map(|byte| byte as u8).collect();
            let artwork = artwork_from_rgb(&rgb, width, height).expect("a small RGB8 picture converts");
            // SAFETY: reads the artwork's own bounds and asks its handler for an image.
            let (bounds, handed_back) = unsafe { (artwork.bounds(), artwork.imageWithSize(CGSize { width: 2.0, height: 2.0 })) };
            assert_eq!((bounds.size.width, bounds.size.height), (width as f64, height as f64));
            assert!(handed_back.is_some(), "the request handler returns the cached image");
            assert!(artwork_from_rgb(&rgb[..10], width, height).is_none(), "a short buffer is refused, not read past its end");
            assert!(artwork_from_rgb(&rgb, 0, height).is_none());

            let mut state = NowPlayingState {
                title: "Title".into(),
                artist: "Artist".into(),
                album: "Album".into(),
                duration_ms: Some(200_000),
                elapsed_ms: 12_500,
                playing: true,
                artwork: None,
            };
            // title, artist, album, elapsed, rate, default rate, media type, duration
            assert_eq!(build_info(&state, None).len(), 8);
            assert_eq!(build_info(&state, Some(&artwork)).len(), 9, "the artwork key rides in every dictionary that has a cover");
            state.duration_ms = None;
            state.playing = false;
            assert_eq!(build_info(&state, None).len(), 7, "no duration key while the length is unknown");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, millis: u64) -> Instant {
        base + Duration::from_millis(millis)
    }

    fn input(position_ms: u64, duration_ms: Option<u64>) -> HoldSeekInput {
        HoldSeekInput { seek_pending: false, position_ms, duration_ms }
    }

    // -- position conversion ------------------------------------------------------------------

    #[test]
    fn position_seconds_convert_to_rounded_milliseconds() {
        assert_eq!(position_seconds_to_ms(0.0), 0);
        assert_eq!(position_seconds_to_ms(42.5), 42_500);
        assert_eq!(position_seconds_to_ms(0.0004), 0);
        assert_eq!(position_seconds_to_ms(0.0006), 1);
    }

    #[test]
    fn hostile_position_values_never_wrap_or_panic() {
        assert_eq!(position_seconds_to_ms(f64::NAN), 0);
        assert_eq!(position_seconds_to_ms(-3.0), 0);
        assert_eq!(position_seconds_to_ms(f64::NEG_INFINITY), 0);
        assert_eq!(position_seconds_to_ms(f64::INFINITY), u64::MAX);
        assert_eq!(position_seconds_to_ms(1e30), u64::MAX);
    }

    // -- command mapping ----------------------------------------------------------------------

    #[test]
    fn toggle_next_previous_and_position_map_straight_through() {
        for playing in [true, false] {
            assert_eq!(plan_media_command(MediaCommand::TogglePlayPause, playing), TransportAction::TogglePlayback);
            assert_eq!(plan_media_command(MediaCommand::NextTrack, playing), TransportAction::Next);
            assert_eq!(plan_media_command(MediaCommand::PreviousTrack, playing), TransportAction::Previous);
            assert_eq!(
                plan_media_command(MediaCommand::SetPosition { position_ms: 42_500 }, playing),
                TransportAction::SeekTo { position_ms: 42_500 }
            );
        }
    }

    #[test]
    fn play_and_pause_toggle_only_when_they_change_the_state() {
        assert_eq!(plan_media_command(MediaCommand::Play, false), TransportAction::TogglePlayback);
        assert_eq!(plan_media_command(MediaCommand::Play, true), TransportAction::Ignore);
        assert_eq!(plan_media_command(MediaCommand::Pause, true), TransportAction::TogglePlayback);
        assert_eq!(plan_media_command(MediaCommand::Pause, false), TransportAction::Ignore);
    }

    #[test]
    fn seek_begin_and_end_map_to_the_hold_controller_with_their_direction() {
        for direction in [SeekDirection::Backward, SeekDirection::Forward] {
            assert_eq!(plan_media_command(MediaCommand::SeekBegin(direction), false), TransportAction::BeginHoldSeek(direction));
            assert_eq!(plan_media_command(MediaCommand::SeekEnd(direction), true), TransportAction::EndHoldSeek(direction));
        }
    }

    // -- hold-to-seek step math ---------------------------------------------------------------

    #[test]
    fn a_step_moves_five_seconds_in_the_held_direction() {
        assert_eq!(hold_seek_target(30_000, SeekDirection::Forward, Some(200_000)), Some(35_000));
        assert_eq!(hold_seek_target(30_000, SeekDirection::Backward, Some(200_000)), Some(25_000));
    }

    #[test]
    fn steps_clamp_to_the_start_and_to_the_controllers_end_margin() {
        assert_eq!(hold_seek_target(3_000, SeekDirection::Backward, Some(200_000)), Some(0));
        // duration - 500 ms is where the controller clamps every seek.
        assert_eq!(hold_seek_target(197_000, SeekDirection::Forward, Some(200_000)), Some(199_500));
        assert_eq!(hold_seek_target(195_000, SeekDirection::Forward, Some(200_000)), Some(199_500));
    }

    #[test]
    fn a_hold_stops_at_the_ends_instead_of_stepping_in_place_or_backwards() {
        assert_eq!(hold_seek_target(0, SeekDirection::Backward, Some(200_000)), None, "already at the start");
        assert_eq!(hold_seek_target(199_500, SeekDirection::Forward, Some(200_000)), None, "already at the last seekable position");
        assert_eq!(
            hold_seek_target(199_900, SeekDirection::Forward, Some(200_000)),
            None,
            "past the margin: never step backwards to reach it"
        );
        assert_eq!(hold_seek_target(199_900, SeekDirection::Backward, Some(200_000)), Some(194_900));
        assert_eq!(hold_seek_target(0, SeekDirection::Forward, Some(300)), None, "a track shorter than the margin has no room");
    }

    #[test]
    fn steps_need_a_known_duration() {
        assert_eq!(hold_seek_target(30_000, SeekDirection::Forward, None), None);
        assert_eq!(hold_seek_target(30_000, SeekDirection::Backward, None), None);
        assert_eq!(hold_seek_target(30_000, SeekDirection::Forward, Some(0)), None, "a zero duration is unknown");
    }

    // -- hold-to-seek controller --------------------------------------------------------------

    #[test]
    fn a_hold_waits_out_the_initial_delay_then_steps_once() {
        let t0 = Instant::now();
        let mut hold = HoldSeek::default();
        assert_eq!(hold.tick(t0, input(30_000, Some(200_000))), HoldSeekTick::Idle);

        hold.begin(SeekDirection::Forward, t0);
        assert!(hold.is_armed());
        assert_eq!(hold.tick(t0, input(30_000, Some(200_000))), HoldSeekTick::Wait);
        assert_eq!(hold.tick(at(t0, 249), input(30_000, Some(200_000))), HoldSeekTick::Wait);
        assert_eq!(hold.tick(at(t0, 250), input(30_000, Some(200_000))), HoldSeekTick::Step { target_ms: 35_000 });
    }

    #[test]
    fn a_quick_begin_end_pair_never_steps() {
        let t0 = Instant::now();
        let mut hold = HoldSeek::default();
        hold.begin(SeekDirection::Forward, t0);
        assert_eq!(hold.tick(at(t0, 100), input(30_000, Some(200_000))), HoldSeekTick::Wait);
        hold.end(SeekDirection::Forward);
        assert!(!hold.is_armed());
        assert_eq!(hold.tick(at(t0, 400), input(30_000, Some(200_000))), HoldSeekTick::Idle);
    }

    #[test]
    fn steps_repeat_no_faster_than_the_interval() {
        let t0 = Instant::now();
        let mut hold = HoldSeek::default();
        hold.begin(SeekDirection::Forward, t0);
        assert_eq!(hold.tick(at(t0, 250), input(30_000, Some(200_000))), HoldSeekTick::Step { target_ms: 35_000 });
        assert_eq!(hold.tick(at(t0, 250 + 399), input(35_000, Some(200_000))), HoldSeekTick::Wait);
        assert_eq!(hold.tick(at(t0, 250 + 400), input(35_000, Some(200_000))), HoldSeekTick::Step { target_ms: 40_000 });
    }

    #[test]
    fn no_step_is_taken_while_a_seek_is_still_pending() {
        let t0 = Instant::now();
        let mut hold = HoldSeek::default();
        hold.begin(SeekDirection::Backward, t0);
        let pending = HoldSeekInput { seek_pending: true, position_ms: 30_000, duration_ms: Some(200_000) };
        assert_eq!(hold.tick(at(t0, 250), pending), HoldSeekTick::Wait);
        assert_eq!(hold.tick(at(t0, 2_000), pending), HoldSeekTick::Wait);
        assert!(hold.is_armed(), "waiting on a seek does not disarm");
        // The step is due the moment the pending seek clears.
        assert_eq!(hold.tick(at(t0, 2_040), input(30_000, Some(200_000))), HoldSeekTick::Step { target_ms: 25_000 });
    }

    #[test]
    fn reaching_the_start_or_the_end_finishes_the_hold_without_changing_track() {
        let t0 = Instant::now();
        let mut back = HoldSeek::default();
        back.begin(SeekDirection::Backward, t0);
        assert_eq!(back.tick(at(t0, 250), input(3_000, Some(200_000))), HoldSeekTick::Step { target_ms: 0 });
        assert_eq!(back.tick(at(t0, 650), input(0, Some(200_000))), HoldSeekTick::Finished);
        assert!(!back.is_armed());

        let mut forward = HoldSeek::default();
        forward.begin(SeekDirection::Forward, t0);
        assert_eq!(forward.tick(at(t0, 250), input(199_500, Some(200_000))), HoldSeekTick::Finished);
        assert!(!forward.is_armed());
    }

    /// While playing, the position keeps advancing after a step to 0 lands, so it never reads exactly
    /// 0 again: the hold must still end there instead of rewinding to 0 (a full stream rebuild) every
    /// interval until the key is released.
    #[test]
    fn a_rewind_hold_pinned_at_the_start_of_a_playing_track_finishes_instead_of_restarting_in_place() {
        let t0 = Instant::now();
        let mut hold = HoldSeek::default();
        hold.begin(SeekDirection::Backward, t0);
        assert_eq!(hold.tick(at(t0, 250), input(3_000, Some(200_000))), HoldSeekTick::Step { target_ms: 0 });
        // The step landed and playback moved on: 300 ms in, still inside the next step's reach.
        assert_eq!(hold.tick(at(t0, 650), input(300, Some(200_000))), HoldSeekTick::Finished);
        assert!(!hold.is_armed());
        assert_eq!(hold.tick(at(t0, 1_050), input(700, Some(200_000))), HoldSeekTick::Idle, "and it stays finished");
    }

    /// The pending-seek gate accepts any timeline within 1.5 s of the target, so the old stream's own
    /// pre-seek position can read as the landing: a fast-forward pinned at the end must not repeat
    /// its clamped step from there either.
    #[test]
    fn a_fast_forward_hold_pinned_at_the_end_finishes_instead_of_repeating_the_clamped_step() {
        let t0 = Instant::now();
        let mut hold = HoldSeek::default();
        hold.begin(SeekDirection::Forward, t0);
        assert_eq!(hold.tick(at(t0, 250), input(197_000, Some(200_000))), HoldSeekTick::Step { target_ms: 199_500 });
        assert_eq!(hold.tick(at(t0, 650), input(197_600, Some(200_000))), HoldSeekTick::Finished);
        assert!(!hold.is_armed());
    }

    /// The pinned check compares with the step just issued, not with any earlier one: a hold keeps
    /// going while each step moves somewhere new, and a fresh Begin starts with no memory of the last.
    #[test]
    fn only_a_repeated_target_ends_a_hold_and_a_new_begin_forgets_it() {
        let t0 = Instant::now();
        let mut hold = HoldSeek::default();
        hold.begin(SeekDirection::Backward, t0);
        assert_eq!(hold.tick(at(t0, 250), input(30_000, Some(200_000))), HoldSeekTick::Step { target_ms: 25_000 });
        assert_eq!(hold.tick(at(t0, 650), input(25_100, Some(200_000))), HoldSeekTick::Step { target_ms: 20_100 });
        assert_eq!(hold.tick(at(t0, 1_050), input(20_300, Some(200_000))), HoldSeekTick::Step { target_ms: 15_300 });

        let mut pinned = HoldSeek::default();
        pinned.begin(SeekDirection::Backward, t0);
        assert_eq!(pinned.tick(at(t0, 250), input(2_000, Some(200_000))), HoldSeekTick::Step { target_ms: 0 });
        assert_eq!(pinned.tick(at(t0, 650), input(400, Some(200_000))), HoldSeekTick::Finished);
        // The key goes down again on a track that has moved on: a fresh hold, free to step to 0 once more.
        pinned.begin(SeekDirection::Backward, at(t0, 2_000));
        assert_eq!(pinned.tick(at(t0, 2_250), input(1_500, Some(200_000))), HoldSeekTick::Step { target_ms: 0 });
    }

    #[test]
    fn a_forward_hold_without_a_known_duration_finishes_instead_of_seeking() {
        let t0 = Instant::now();
        let mut hold = HoldSeek::default();
        hold.begin(SeekDirection::Forward, t0);
        assert_eq!(hold.tick(at(t0, 250), input(30_000, None)), HoldSeekTick::Finished);
        assert!(!hold.is_armed());
    }

    #[test]
    fn a_repeated_begin_for_the_same_direction_keeps_the_original_delay() {
        let t0 = Instant::now();
        let mut hold = HoldSeek::default();
        hold.begin(SeekDirection::Forward, t0);
        hold.begin(SeekDirection::Forward, at(t0, 200));
        assert_eq!(
            hold.tick(at(t0, 250), input(30_000, Some(200_000))),
            HoldSeekTick::Step { target_ms: 35_000 },
            "the duplicate Begin must not restart the initial delay"
        );
    }

    #[test]
    fn a_begin_for_the_other_direction_replaces_the_hold() {
        let t0 = Instant::now();
        let mut hold = HoldSeek::default();
        hold.begin(SeekDirection::Forward, t0);
        hold.begin(SeekDirection::Backward, at(t0, 200));
        assert_eq!(hold.tick(at(t0, 250), input(30_000, Some(200_000))), HoldSeekTick::Wait, "the new direction has its own delay");
        assert_eq!(hold.tick(at(t0, 450), input(30_000, Some(200_000))), HoldSeekTick::Step { target_ms: 25_000 });
    }

    #[test]
    fn only_the_armed_directions_end_disarms() {
        let t0 = Instant::now();
        let mut hold = HoldSeek::default();
        hold.begin(SeekDirection::Forward, t0);
        hold.begin(SeekDirection::Backward, at(t0, 10));
        hold.end(SeekDirection::Forward); // the late End of the replaced hold
        assert!(hold.is_armed(), "a stale End for the other direction must not cancel the live hold");
        hold.end(SeekDirection::Backward);
        assert!(!hold.is_armed());
        hold.end(SeekDirection::Backward); // an End with nothing armed is harmless
        assert!(!hold.is_armed());
    }

    #[test]
    fn disarm_stops_the_hold_unconditionally() {
        let t0 = Instant::now();
        let mut hold = HoldSeek::default();
        hold.begin(SeekDirection::Forward, t0);
        hold.disarm();
        assert!(!hold.is_armed());
        assert_eq!(hold.tick(at(t0, 1_000), input(30_000, Some(200_000))), HoldSeekTick::Idle);
        hold.disarm(); // and again with nothing armed
    }

    #[test]
    fn a_lost_end_is_cut_off_by_the_failsafe() {
        let t0 = Instant::now();
        let mut hold = HoldSeek::default();
        hold.begin(SeekDirection::Forward, t0);
        // Still stepping well into the hold...
        assert_eq!(hold.tick(at(t0, 59_000), input(30_000, Some(600_000))), HoldSeekTick::Step { target_ms: 35_000 });
        // ...until the fail-safe, even though a step would otherwise be due and nothing is pending.
        assert_eq!(hold.tick(at(t0, 60_000), input(30_000, Some(600_000))), HoldSeekTick::Finished);
        assert!(!hold.is_armed());
    }

    // -- publish planning ---------------------------------------------------------------------

    fn snapshot() -> TransportSnapshot {
        TransportSnapshot {
            has_track: true,
            can_resume: true,
            ui_playing: true,
            duration_ms: Some(200_000),
            last_applied_ms: 42_000,
            pending_seek_ms: None,
        }
    }

    #[test]
    fn nothing_loaded_and_nothing_to_resume_clears_the_session() {
        assert_eq!(plan_transport_publish(&TransportSnapshot { has_track: false, can_resume: false, ..snapshot() }), None);
    }

    /// The blocked queue head (a failed seek or start, "Press Play to retry") and the last finished
    /// track leave no active track, but the play button still retries: F8 must reach it, so the
    /// session stays, paused, instead of falling back to Music.app.
    #[test]
    fn no_active_track_but_a_working_play_button_keeps_a_paused_session() {
        let idle = TransportSnapshot { has_track: false, can_resume: true, ..snapshot() };
        assert_eq!(plan_transport_publish(&idle), Some(TransportPlan { playing: false, elapsed_ms: 0, duration_ms: None }));
        // Whatever the UI last showed (a seek target, the old length, the playing flag) is stale by now.
        let stale = TransportSnapshot { ui_playing: true, pending_seek_ms: Some(90_000), ..idle };
        assert_eq!(plan_transport_publish(&stale), Some(TransportPlan { playing: false, elapsed_ms: 0, duration_ms: None }));
    }

    #[test]
    fn a_loaded_track_publishes_what_the_ui_shows() {
        assert_eq!(
            plan_transport_publish(&snapshot()),
            Some(TransportPlan { playing: true, elapsed_ms: 42_000, duration_ms: Some(200_000) })
        );
        assert_eq!(
            plan_transport_publish(&TransportSnapshot { ui_playing: false, ..snapshot() }),
            Some(TransportPlan { playing: false, elapsed_ms: 42_000, duration_ms: Some(200_000) }),
            "rate follows the UI: a paused track is published paused"
        );
    }

    #[test]
    fn a_pending_seek_publishes_its_target_not_the_stale_position() {
        let plan = plan_transport_publish(&TransportSnapshot { pending_seek_ms: Some(90_000), ..snapshot() }).unwrap();
        assert_eq!(plan.elapsed_ms, 90_000);
    }

    #[test]
    fn an_unknown_or_zero_duration_publishes_no_length() {
        for duration_ms in [None, Some(0)] {
            let plan = plan_transport_publish(&TransportSnapshot { duration_ms, ..snapshot() }).unwrap();
            assert_eq!(plan.duration_ms, None);
            assert_eq!(plan.elapsed_ms, 42_000);
        }
    }

    #[test]
    fn elapsed_never_exceeds_the_duration() {
        let plan = plan_transport_publish(&TransportSnapshot { pending_seek_ms: Some(999_000), ..snapshot() }).unwrap();
        assert_eq!(plan.elapsed_ms, 200_000);
    }

    // -- republish detection ------------------------------------------------------------------

    fn published(playing: bool, at: Instant) -> PublishedTransport {
        PublishedTransport::new(&TransportPlan { playing, elapsed_ms: 10_000, duration_ms: Some(200_000) }, at)
    }

    #[test]
    fn a_timeline_before_anything_was_published_needs_a_publish() {
        assert!(timeline_needs_publish(None, 0, Some(200_000), Instant::now()));
    }

    #[test]
    fn ordinary_timeline_ticks_that_match_the_extrapolation_do_not_republish() {
        let t0 = Instant::now();
        let published = published(true, t0);
        // 10 s published, 3 s later the widget shows 13 s; a timeline at 13.1 s is just playback.
        assert!(!timeline_needs_publish(Some(&published), 13_100, Some(200_000), at(t0, 3_000)));
        // Within the tolerance in either direction.
        assert!(!timeline_needs_publish(Some(&published), 13_750, Some(200_000), at(t0, 3_000)));
        assert!(!timeline_needs_publish(Some(&published), 12_250, Some(200_000), at(t0, 3_000)));
    }

    #[test]
    fn a_seek_or_stall_is_a_drift_that_republishes() {
        let t0 = Instant::now();
        let published = published(true, t0);
        assert!(timeline_needs_publish(Some(&published), 60_000, Some(200_000), at(t0, 3_000)), "a seek forward");
        assert!(timeline_needs_publish(Some(&published), 0, Some(200_000), at(t0, 3_000)), "a restart");
        assert!(timeline_needs_publish(Some(&published), 11_000, Some(200_000), at(t0, 3_000)), "playback stalled 2 s");
        assert!(timeline_needs_publish(Some(&published), 12_200, Some(200_000), at(t0, 3_000)), "just past the tolerance");
    }

    #[test]
    fn a_paused_publish_does_not_extrapolate() {
        let t0 = Instant::now();
        let published = published(false, t0);
        assert!(!timeline_needs_publish(Some(&published), 10_000, Some(200_000), at(t0, 30_000)));
        assert!(timeline_needs_publish(Some(&published), 40_000, Some(200_000), at(t0, 30_000)));
    }

    #[test]
    fn a_duration_that_appears_late_republishes() {
        let t0 = Instant::now();
        let plan = TransportPlan { playing: true, elapsed_ms: 10_000, duration_ms: None };
        let published = PublishedTransport::new(&plan, t0);
        assert!(timeline_needs_publish(Some(&published), 10_100, Some(200_000), at(t0, 100)));
        assert!(!timeline_needs_publish(Some(&published), 10_100, None, at(t0, 100)));
        assert!(!timeline_needs_publish(Some(&published), 10_100, Some(0), at(t0, 100)), "a zero duration is still unknown");
    }
}

mod app_state;
mod audio;
mod library;
mod media_controls;
mod settings;
mod view_model;
#[cfg(test)]
mod ui_snapshot;

slint::include_modules!();

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use app_state::{
    AppState, Navigation, PendingVolume, expire_pending_volume, project_now_playing, resolve_volume_event, should_send_volume,
    unavailable_label,
};
use audio::{AudioInfo, AudioPlayer, OutputDevice, PlaybackEvent, PlayerSettings, PreparedTrack, QueueTrackSnapshot, enumerate_outputs};
use library::{TrackKey, TrackRecord};
use library::format::{format_clock, format_scan_failures_summary, playback_progress};
use library::repair::{RateRepairConfig, RepairNotes};
use library::scanner::{LibraryEvent, LibraryScanner, ScanFailurePhase};
use library::store::LibrarySources;
use library::walker::AUDIO_EXTENSIONS;
use media_controls::{
    HoldSeek, HoldSeekInput, HoldSeekTick, MediaCommand, MediaSession, NowPlayingArtwork, NowPlayingState, PublishedTransport,
    TransportAction, TransportSnapshot, plan_media_command, plan_transport_publish, timeline_needs_publish,
};
use settings::Preferences;
use slint::{ModelRc, SharedString, Timer, TimerMode, VecModel};
use view_model::{perform_album_action, prepared_suffix, project_library, project_queue_rows};

/// UI-thread state for an in-flight seek (`§5.10`). `app_state.rs`/`AppState` exists as of Stage 3
/// (the session library and its artwork cache), but pending-seek/volume state stays here until
/// Stage 4/5 actually needs to share it with more than `main.rs`.
struct PendingSeek {
    // Not yet consulted by `accept_timeline` (`Timeline` events carry no key to compare
    // against); kept because `Started`/a future per-track staleness check will need it once
    // this moves into `app_state.rs` (`§5.10`).
    #[allow(dead_code)]
    key: TrackKey,
    target_ms: u64,
    requested_at: Instant,
}

/// Whether a `Timeline` position should be applied while a seek is pending: either it landed
/// close enough to the target, or the pending seek has been waiting long enough that it should
/// stop holding the UI back (`§5.10`).
fn accept_timeline(pending: &PendingSeek, position_ms: u64, now: Instant) -> bool {
    position_ms.abs_diff(pending.target_ms) <= 1_500 || now.duration_since(pending.requested_at) >= Duration::from_secs(3)
}

/// On `SeekRejected` (or a new `Started`), the pending seek is cleared immediately so the next
/// `Timeline` applies normally (`§5.10`).
fn clear_pending_seek(pending_seek: &mut Option<PendingSeek>) {
    *pending_seek = None;
}

/// What the UI shows the instant a seek to `target_ms` is requested: the target clamped to the
/// track's length and the matching progress-bar fraction. The controller clamps the real seek the same
/// way (`seek_target_frame`), so a target past the end can never leave the bar beyond it.
fn seek_ui_target(target_ms: u64, duration_ms: u64) -> (u64, f32) {
    let target_ms = target_ms.min(duration_ms);
    (target_ms, playback_progress(target_ms, Some(duration_ms)))
}

/// The one way the UI asks for a seek (`§5.10`): the scrubber, a Control Center scrub
/// (`MediaCommand::SetPosition`) and every hold-to-seek step all go through `request`, so they share
/// the same guards, optimistic UI and `PendingSeek` bookkeeping. The state is `Rc`-shared with the
/// event timer that reads it back (`accept_timeline`, `SeekRejected`).
struct SeekRequester {
    player: Arc<AudioPlayer>,
    now_playing_key: Rc<RefCell<Option<TrackKey>>>,
    now_playing_duration_ms: Rc<RefCell<Option<u64>>>,
    pending_seek: Rc<RefCell<Option<PendingSeek>>>,
    /// Set so the OS Now Playing widget shows the new position right away (`sync_media_session`).
    media_dirty: Rc<Cell<bool>>,
}

impl SeekRequester {
    /// Seeks the playing track to `target_ms` (track-relative). Returns whether a seek was actually
    /// sent: there must be a seekable playing track with a known length.
    fn request(&self, window: &MainWindow, target_ms: u64) -> bool {
        if !window.get_can_seek() {
            return false;
        }
        let Some(key) = self.now_playing_key.borrow().clone() else { return false; };
        let Some(duration_ms) = *self.now_playing_duration_ms.borrow() else { return false; };
        let (target_ms, progress) = seek_ui_target(target_ms, duration_ms);
        // Update immediately (optimistic UI); the worker's next Timeline confirms or the
        // pending-seek tolerance window (`accept_timeline`) holds the value until it does.
        window.set_playback_progress(progress);
        window.set_playback_elapsed(format_clock(target_ms).into());
        *self.pending_seek.borrow_mut() = Some(PendingSeek { key: key.clone(), target_ms, requested_at: Instant::now() });
        self.media_dirty.set(true);
        self.player.seek(key, target_ms);
        true
    }

    /// The scrubber's entry point: `fraction` of the track's length.
    fn request_fraction(&self, window: &MainWindow, fraction: f32) -> bool {
        let Some(duration_ms) = *self.now_playing_duration_ms.borrow() else { return false; };
        self.request(window, (f64::from(fraction) * duration_ms as f64).round() as u64)
    }
}

/// Which track table the user can see and click right now, from the window's own navigation state:
/// the Search view's songs table while a search is active (it covers every other view), else the
/// table of the current view. `None` for views with no track table (Home, Albums, Artists, ...).
/// A selection made in a table that is not on screen resolves to nothing (`visible_selection`).
fn visible_track_list(view: View, search_active: bool) -> Option<TrackListKind> {
    if search_active {
        return Some(TrackListKind::Search);
    }
    match view {
        View::Songs => Some(TrackListKind::Songs),
        View::RecentlyAdded => Some(TrackListKind::RecentlyAdded),
        View::AlbumDetail => Some(TrackListKind::Album),
        View::Queue => Some(TrackListKind::Queue),
        _ => None,
    }
}

/// The rows the window currently renders in `kind`'s table.
fn track_table_model(window: &MainWindow, kind: TrackListKind) -> ModelRc<TrackRowData> {
    match kind {
        TrackListKind::Songs => window.get_songs(),
        TrackListKind::RecentlyAdded => window.get_recent_songs(),
        TrackListKind::Album => window.get_album_tracks(),
        TrackListKind::Queue => window.get_queue_rows(),
        TrackListKind::Search => window.get_search_songs(),
    }
}

/// The position of the row showing the track `selected_key` (a `TrackRowData.key`), if there is one.
/// The selection is a key, not a position (`MainWindow.selected-track-key`): the rows are replaced by
/// every library re-projection, so the key is looked up again each time it is needed.
fn row_of_key(rows: &impl slint::Model<Data = TrackRowData>, selected_key: &str) -> Option<usize> {
    if selected_key.is_empty() {
        return None;
    }
    rows.iter().position(|row| row.key == selected_key)
}

/// The selected track's row in the table the user can see right now, resolved at the moment Space is
/// pressed: `None` when nothing is selected, when the selection was made in a table that is not on
/// screen (`visible_track_list`), or when the selected track is no longer in the visible list (it
/// left the library, or a search or another album no longer shows it) — the selection is dropped, not
/// moved to whatever row now sits at its old position.
fn visible_selection(window: &MainWindow) -> Option<(TrackListKind, i32)> {
    let kind = visible_track_list(window.get_view(), window.get_search_active())?;
    let row = row_of_key(&track_table_model(window, kind), window.get_selected_track_key().as_str())?;
    Some((kind, i32::try_from(row).ok()?))
}

/// Forgets the row the user last clicked. Called on every navigation (`sync_navigation`) and on every
/// search edit that changes the list on screen (`apply_search_edit`): a selection belongs to the page
/// it was made on.
fn clear_track_selection(window: &MainWindow) {
    window.set_selected_track_key(SharedString::default());
}

/// What Space does (`CLAUDE.md` "Media controls" -> "Spacebar").
// No `Eq`: Slint's generated `TrackListKind` derives `PartialEq` only.
#[derive(Debug, Clone, Copy, PartialEq)]
enum SpaceAction {
    /// The on-screen play button's own action (`AudioPlayer::toggle_playback`): pause or resume the
    /// playing track, start the queue head, or restore the last finished track — and, with none of
    /// those, its "Open an audio file" status.
    TogglePlayback,
    /// Play the selected row exactly like a double click on it (`track-activated`).
    ActivateSelection { kind: TrackListKind, index: i32 },
}

/// Whether the player bar's play button does anything: the enabling expression of `PlayerBar`'s
/// button in `ui/app.slint` (a title is shown, or the queue is not empty), which a contract test pins
/// next to this helper's truth table. Space (`plan_space_action`) and the OS media session's decision
/// to keep the media keys (`TransportSnapshot::can_resume`) both ask it, so the keyboard, the media
/// keys and the button can never disagree about whether there is something to play or retry. The
/// title is never cleared once a track has played, so this is false only before anything has played
/// this session and while the queue is empty.
fn play_button_enabled(now_playing_title: &str, queue_count: i32) -> bool {
    !now_playing_title.is_empty() || queue_count > 0
}

/// `play_button_enabled` for what `window` shows right now.
fn window_play_button_enabled(window: &MainWindow) -> bool {
    play_button_enabled(window.get_now_playing_title().as_str(), window.get_queue_count())
}

/// Decides what Space does. `play_button_enabled` is whether the player bar's play button would do
/// something (the helper of the same name). While it is true, Space is that button — including with a
/// row selected, since a track that is playing, waiting or restorable wins over a row that merely has
/// the highlight. Only in the library-only state does a selected row get played; with no selection
/// either, Space falls back to the button so the user still gets its status message.
fn plan_space_action(play_button_enabled: bool, selection: Option<(TrackListKind, i32)>) -> SpaceAction {
    match selection {
        Some((kind, index)) if !play_button_enabled => SpaceAction::ActivateSelection { kind, index },
        _ => SpaceAction::TogglePlayback,
    }
}

/// Status shown once a real output device is selected, whether that happens at startup (a saved
/// device is present in the enumerated list) or through `on_output_chosen` (the user picks one) —
/// kept as one constant so both call sites show identical text.
const OUTPUT_SELECTED_STATUS: &str = "Output selected. Open an audio file to play.";

/// `ui/app.slint`'s own default `playback-status` value, duplicated here so the startup
/// library-load-error path (which must always call `set_playback_status` at least once to let its
/// `settings_save_error` prefix show, even when no output device was ever saved) can restore it
/// exactly rather than leaving a stale value.
const CHOOSE_OUTPUT_STATUS: &str = "Choose an output DAC, then open audio.";

/// Whether choosing `selected_id` from the output combo box is a no-op against `current_id`, the
/// device already selected/saved. `selected(value)` (`ui/app.slint`) fires from `ComboBoxBase`
/// on any user click or arrow-key move — including picking the row that is already selected, or
/// pressing Up at index 0 / Down at the last index, where the index does not actually change — so
/// `on_output_chosen` must skip `Command::SelectOutput`, the settings write, and the status
/// overwrite in that case, or it can stomp a verified route status while playing with "Output
/// selection will apply when the next track starts." for a selection that never changed.
fn is_output_reselection_noop(selected_id: Option<&str>, current_id: Option<&str>) -> bool {
    selected_id == current_id
}

/// Failures collected for one scan batch, split by `ScanFailurePhase` (`§1.4`): `probe` files never
/// reached the queue or the library at all, while `metadata` files were already probed, enqueued
/// and added — only their tags/artwork could not be read. Kept apart because
/// `format_scan_failures_summary` must never count a `metadata` failure as "not added".
#[derive(Default)]
struct BatchFailures {
    probe: Vec<(String, String)>,
    metadata: Vec<(String, String)>,
}

impl BatchFailures {
    fn is_empty(&self) -> bool {
        self.probe.is_empty() && self.metadata.is_empty()
    }
}

/// Why a `LibraryScanner` batch was requested, recorded per batch id (`batch_kinds`) the moment
/// `scan`/`scan_folder` returns its id — before any of its events can arrive — so the event loop
/// never has to guess whether a `Probed` should enqueue-and-play or stay library-only.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BatchKind {
    /// "Open Files…": every probed track is enqueued (and started, if nothing else is already
    /// playing) — the one path that keeps today's existing behavior.
    OpenFiles,
    /// "Open Folder…": added to the library only, never enqueued or played. The user plays from
    /// the views.
    OpenFolder,
    /// The startup library restore: fills the library from the persisted sources
    /// (`library::store::LibrarySources`), never enqueues or plays anything, and reports
    /// unreadable/missing paths as one aggregate status instead of per-file spam.
    StartupRestore,
}

/// Whether a `Probed` batch of this `kind` should be enqueued (and possibly started) — only "Open
/// Files…" does; "Open Folder…" and the startup restore only ever fill the library (`main.rs`'s own
/// `BatchKind`/enqueue contract, kept as a pure, directly testable function).
fn batch_should_enqueue(kind: BatchKind) -> bool {
    kind == BatchKind::OpenFiles
}

/// Whether a per-file `Failed` event for a batch of this `kind` should show its own inline status
/// line immediately. The startup restore stays quiet per-file (a NAS with many stale entries would
/// otherwise spam the status line before the user has done anything) and instead reports one
/// aggregate count at `BatchDone`; "Open Files…" and "Open Folder…" both still want immediate
/// per-file feedback.
fn batch_reports_failures_inline(kind: BatchKind) -> bool {
    kind != BatchKind::StartupRestore
}

/// Drops every `track` whose `TrackKey` is in `sources.excluded_tracks` (`CLAUDE.md` "Library
/// exclusions") — applied to a `LibraryEvent::Probed` batch before it can be enqueued or added to
/// the library, so a rescan of an already-persisted folder/file (chiefly the startup restore) never
/// silently brings back a track the user explicitly removed. A pure, directly testable filter, kept
/// separate from `LibrarySources::is_excluded` itself so a test can assert on the whole-batch
/// behavior (order preserved, non-excluded tracks untouched) in one call.
fn exclude_removed_tracks(tracks: Vec<PreparedTrack>, sources: &LibrarySources) -> Vec<PreparedTrack> {
    tracks.into_iter().filter(|track| !sources.is_excluded(&track.key)).collect()
}

/// A file's display name for a status line: its last path component, or the whole path when it has none.
fn file_name_of(path: &std::path::Path) -> String {
    path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
}

/// Prints a developer-only `DIAG` line to stderr, never surfaced in the UI — the same convention as
/// `PlaybackEvent::Diagnostic`, and just as unable to panic on a closed stderr pipe.
fn write_diagnostic(line: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr().lock(), "DIAG {line}");
}

fn main() -> Result<(), slint::PlatformError> {
    // Unified title bar (`§6` Stage 5b): must run before any window is created (`MainWindow::new()`
    // below), on macOS only. `with_titlebar_transparent`/`with_title_hidden` hide the native
    // "Lime Player" title bar strip; `with_fullsize_content_view` lets the content view (and so
    // `Sidebar`/`TopBar`) extend under where the title bar used to be, so only the traffic lights
    // float over it. `MainWindow.title` stays "Lime Player" (still used for Mission Control/the
    // Window menu); it is only hidden in the titlebar itself. Every other platform keeps Slint's
    // normal decorated window untouched.
    #[cfg(target_os = "macos")]
    {
        use slint::winit_030::winit::platform::macos::WindowAttributesExtMacOS;
        slint::BackendSelector::new()
            .with_winit_window_attributes_hook(|attributes| {
                attributes.with_titlebar_transparent(true).with_title_hidden(true).with_fullsize_content_view(true)
            })
            .select()?;
    }

    let preferences = Rc::new(RefCell::new(Preferences::load()));
    let settings_save_error = Rc::new(RefCell::new(None::<String>));
    // Persisted library sources (`library.json`, next to `settings.json`): loaded here, restored
    // through the scanner further down once `library_scanner`/`window` exist. A load failure is
    // reported (via `settings_save_error`'s persistent status prefix) but never overwrites the
    // file — `library_sources` just starts empty for this run instead.
    let library_sources = match LibrarySources::load() {
        Ok(sources) => sources,
        Err(error) => {
            *settings_save_error.borrow_mut() = Some(format!("Could not load library.json: {error}"));
            LibrarySources::default()
        }
    };
    let library_sources = Rc::new(RefCell::new(library_sources));
    let output_devices = Rc::new(RefCell::new(enumerate_outputs().unwrap_or_else(|error| {
        eprintln!("Could not enumerate audio outputs: {error}");
        Vec::new()
    })));
    let audio_player = Arc::new(AudioPlayer::new(PlayerSettings {
        output_device_id: preferences.borrow().output_device_id.clone(),
        hog_mode_enabled: preferences.borrow().hog_mode_enabled,
    }));
    // The scanner probes opened files and reads their tags/artwork on its own thread, keeping that
    // work off both the UI thread and the audio controller thread (`§6` Stage 3); `app_state`
    // holds the session library it fills and the decoded-artwork cache built from it.
    // With the sample-rate repair on (`Preferences::repair_nonstandard_sample_rates`, default), the
    // scanner also converts FLAC files no output device can play (`CLAUDE.md` "Sample-rate repair").
    let rate_repair_backup_dir = settings::rate_repair_backup_dir().filter(|_| preferences.borrow().repair_nonstandard_sample_rates);
    let library_scanner = Arc::new(match rate_repair_backup_dir {
        Some(backup_dir) => LibraryScanner::with_rate_repair(RateRepairConfig { backup_dir }),
        None => LibraryScanner::new(),
    });
    let app_state = Rc::new(RefCell::new(AppState::new()));
    // Rust-owned navigation (`§3.6`, Stage 5): current view, sidebar highlight and back stack.
    let navigation = Rc::new(RefCell::new(Navigation::new()));
    // The now-playing path/duration come from the last `Started`/`Timeline` events (`§6` Stage 2);
    // `on_seek_requested` needs both to turn a fraction into a `player.seek(path, ms)` call.
    let now_playing_key = Rc::new(RefCell::new(None::<TrackKey>));
    let now_playing_duration_ms = Rc::new(RefCell::new(None::<u64>));
    // The last `Started` event's own fallback title/album and the `AudioInfo` of what is actually
    // playing, kept so a later `Scanned` record for the same path can re-project the now-playing
    // fields without waiting for another `Started` (`§5.10` "Now-playing projection").
    let now_playing_fallback = Rc::new(RefCell::new((String::new(), String::new())));
    let now_playing_info = Rc::new(RefCell::new(None::<AudioInfo>));
    let pending_seek = Rc::new(RefCell::new(None::<PendingSeek>));
    // The last Timeline the worker actually applied to the UI (never the optimistic seek
    // target). `SeekRejected` restores this instead of leaving a fake position on the bar when
    // nothing is active to answer with a fresh Timeline of its own (`§1.4`).
    let last_timeline = Rc::new(RefCell::new((0u64, None::<u64>)));
    // Optimistic volume UI state (`§5.10` "Pending volume"): the level a click/drag just set,
    // held until the worker's own `Volume` echo confirms it (`accept_volume`).
    let pending_volume = Rc::new(RefCell::new(None::<PendingVolume>));
    // The last time a `SetVolume` command was actually sent, so `should_send_volume` can throttle
    // a drag gesture to at most one command per 50 ms without ever holding back its final value.
    let last_volume_sent_at = Rc::new(RefCell::new(None::<Instant>));
    // The level of the most recent `Volume` event, whether or not it was applied to the UI
    // (`resolve_volume_event`). Lets the 40 ms timer snap a rejected pending value back to the
    // truth once it has been held for 500 ms, instead of leaving a level the device never reached
    // on the slider forever (`§6` Stage 4 fix).
    let last_worker_volume = Rc::new(Cell::new(None::<f32>));
    // The last route status a `Started` event set ("Playing · ..."), and `None` once `Stopped`
    // clears it. A scan started while a track is active must not leave "Reading {n} files…" on
    // the status line forever when no later `Started`/`Failed` happens to overwrite it (`§6`
    // Stage 3 fix: opening more files during playback must not strand the route status).
    let active_route_status = Rc::new(RefCell::new(None::<String>));
    // Failures collected per scan batch (`LibraryEvent::Failed`'s `batch`), split by phase
    // (`BatchFailures`): `Probed` shows the phase-1 refusal summary as soon as phase 1 finishes,
    // instead of waiting for `BatchDone` — otherwise a large batch's phase 2 (tags/artwork) could
    // delay a refusal by minutes on a slow NAS share while the individual per-file "Could not open"
    // status lines get overwritten by later events. `BatchDone` then reports metadata (phase-2)
    // failures too, still never counting them as "not added" (`§1.4`, "refuse rather than silently
    // degrade").
    let batch_failures = Rc::new(RefCell::new(std::collections::HashMap::<u64, BatchFailures>::new()));
    // What the sample-rate repair did per batch, reported once when the batch finishes.
    let batch_repairs = Rc::new(RefCell::new(std::collections::HashMap::<u64, RepairNotes>::new()));
    // What every in-flight (or not-yet-reported) scan batch is for, keyed by the id `scan`/
    // `scan_folder` returned synchronously at call time (`BatchKind`). Never guessed from an
    // event: `Probed`/`Failed`/`BatchDone` only carry the batch id, not why it was requested.
    let batch_kinds = Rc::new(RefCell::new(std::collections::HashMap::<u64, BatchKind>::new()));
    // The worker's latest pending-queue snapshot and the paths behind the Queue view's rendered
    // rows, in the same order (`§3.4`, `§3.7`). `on_track_activated` maps a clicked row back to a
    // path through the latter; both are recomputed together by `project_and_set_queue`.
    let queue_pending = Rc::new(RefCell::new(Vec::<QueueTrackSnapshot>::new()));
    let queue_paths = Rc::new(RefCell::new(Vec::<TrackKey>::new()));
    // Set by anything that can change a library projection's *inputs* in a way too frequent to
    // reproject inline (typing in the search box, a batch of scanner events): the 40 ms timer
    // reprojects once per tick while this is set, then clears it (`§3.4`). Navigation events
    // (`nav-selected`, `album-opened`, `artist-opened`, `back-requested`) are discrete clicks, so
    // they reproject immediately instead of waiting a tick.
    let library_dirty = Rc::new(Cell::new(false));
    // How many `scan()` requests are still in flight (incremented when `Open Files…` calls
    // `LibraryScanner::scan`, decremented on each `LibraryEvent::BatchDone`). While this is above
    // zero, library reprojection is throttled (`library_last_projected_at` below): a big NAS scan
    // otherwise marks the library dirty on every `Scanned` event and reprojects Albums/Artists/
    // Songs (up to ~10k rows, several lowercased-string allocations per sort comparison) on every
    // 40 ms tick, stuttering the progress bar and elapsed time that share the same tick (`§3.4`,
    // R1 "progress bar ... out of sync").
    let scanning_batches = Rc::new(Cell::new(0u32));
    // The last time `project_and_set_library` actually ran; `None` projects immediately the first
    // time regardless of `scanning_batches`.
    let library_last_projected_at = Rc::new(Cell::new(None::<Instant>));
    // Library reprojection is throttled to at most once per this interval while a scan is in
    // flight; the first tick after the last batch finishes always reprojects (a "final pass"),
    // since `scanning_batches` reaching zero makes `due` unconditional below.
    const LIBRARY_REPROJECT_THROTTLE: Duration = Duration::from_millis(250);
    // The paths behind the Songs/Recently Added/album-detail tables' currently rendered rows, in
    // the same order as those rows, so `on_track_activated` can map a clicked row back to
    // `library.prepared(path)` (`§3.4`, mirrors `queue_paths` above).
    let songs_paths = Rc::new(RefCell::new(Vec::<TrackKey>::new()));
    let recent_paths = Rc::new(RefCell::new(Vec::<TrackKey>::new()));
    let album_paths = Rc::new(RefCell::new(Vec::<TrackKey>::new()));
    // The paths behind the dedicated Search view's (capped) Songs section rows, same idea as
    // `songs_paths` above (`§3.4` "Search").
    let search_paths = Rc::new(RefCell::new(Vec::<TrackKey>::new()));

    let window = MainWindow::new()?;
    // Drives `Sidebar`'s reserved 52 px traffic-light strip and `TopBar`'s title text (`ui/app.slint`
    // `MainWindow.unified-title-bar`); matches the `BackendSelector` hook installed above.
    #[cfg(target_os = "macos")]
    window.set_unified_title_bar(true);
    // `window-drag-requested`/`window-zoom-requested` only fire from the `TouchArea`s `ui/app.slint`
    // shows when `unified-title-bar` is true, i.e. only on macOS, so the handlers are macOS-only
    // too. `drag_window()`/`set_maximized()` are winit 0.30 APIs reached through
    // `slint::winit_030::WinitWindowAccessor` (`§6` Stage 5b).
    #[cfg(target_os = "macos")]
    {
        use slint::winit_030::WinitWindowAccessor;

        let drag_window = window.as_weak();
        window.on_window_drag_requested(move || {
            if let Some(window) = drag_window.upgrade() {
                window.window().with_winit_window(|winit_window| {
                    let _ = winit_window.drag_window();
                });
            }
        });

        let zoom_window = window.as_weak();
        window.on_window_zoom_requested(move || {
            if let Some(window) = zoom_window.upgrade() {
                window.window().with_winit_window(|winit_window| {
                    winit_window.set_maximized(!winit_window.is_maximized());
                });
            }
        });
    }
    window.set_hog_mode_enabled(preferences.borrow().hog_mode_enabled);
    set_device_model(&window, &output_devices.borrow());
    sync_navigation(&window, &navigation.borrow());
    project_and_set_library(
        &window,
        &app_state.borrow(),
        &navigation.borrow(),
        navigation.borrow().search_query(),
        LibraryViewPaths { songs: &songs_paths, recent: &recent_paths, album: &album_paths, search: &search_paths },
    );
    let initial_index = preferences
        .borrow()
        .output_device_id
        .as_ref()
        .and_then(|selected| output_devices.borrow().iter().position(|device| &device.id == selected))
        .map(|index| index as i32 + 1)
        .unwrap_or(0);
    window.set_selected_output_index(initial_index);
    window.set_output_name(
        output_devices
            .borrow()
            .iter()
            .find(|device| device.id == preferences.borrow().output_device_id.as_deref().unwrap_or_default())
            .map(|device| SharedString::from(device.label.as_str()))
            .unwrap_or_else(|| SharedString::from("Not selected")),
    );
    // `set_selected_output_index` above is a programmatic write, so it never fires `output-chosen`
    // (`ui/app.slint`'s `selected(value)` only reacts to a real user click/arrow-key move) — unlike
    // before both output `ComboBox`es switched to that callback, nothing else sets the status line
    // for a returning user whose saved device is still present. A missing saved device is left
    // alone: `PlaybackEvent::Devices`, emitted first thing by the controller thread, already shows
    // "Saved output device is unavailable; choose another DAC." for that case, and nothing is saved
    // keeps the default `playback-status` from `ui/app.slint`.
    if initial_index > 0 {
        set_playback_status(&window, OUTPUT_SELECTED_STATUS, &settings_save_error);
    } else if settings_save_error.borrow().is_some() {
        // No output-selected status would otherwise ever be set at startup in this branch, and a
        // library-load error must still show through `set_playback_status`'s prefix (`§...`).
        set_playback_status(&window, CHOOSE_OUTPUT_STATUS, &settings_save_error);
    }

    let scanner_for_open = Arc::clone(&library_scanner);
    let open_window = window.as_weak();
    let picker_parent_window = window.as_weak();
    let open_save_error = Rc::clone(&settings_save_error);
    let open_scanning_batches = Rc::clone(&scanning_batches);
    let open_batch_kinds = Rc::clone(&batch_kinds);
    let open_library_sources = Rc::clone(&library_sources);
    window.on_open_files(move || {
        let Some(parent) = picker_parent_window.upgrade() else { return; };
        let scanner = Arc::clone(&scanner_for_open);
        let parent_window = parent.window().window_handle();
        let selection = pick_audio_files(parent_window);
        let status_window = open_window.clone();
        let status_save_error = Rc::clone(&open_save_error);
        let scanning_batches = Rc::clone(&open_scanning_batches);
        let batch_kinds = Rc::clone(&open_batch_kinds);
        let sources = Rc::clone(&open_library_sources);
        if let Err(error) = slint::spawn_local(async move {
            if let Some(paths) = selection.await
                && !paths.is_empty()
            {
                if let Some(window) = status_window.upgrade() {
                    set_playback_status(&window, format!("Reading {} files…", paths.len()), &status_save_error);
                }
                scanning_batches.set(scanning_batches.get() + 1);
                // Explicitly re-opening a file overrides any earlier "Remove from Library" on it
                // (`CLAUDE.md` "Library exclusions"): lifted before `scan()` is even dispatched, so
                // the `Probed`/`Scanned` events this batch produces are never filtered back out. A
                // re-opened `.cue` sheet needs its own path, since a CUE-derived `TrackKey.path` is
                // the underlying audio file the sheet resolves to, never the `.cue` path itself
                // (`LibrarySources::include_cue_sheet`'s doc comment).
                {
                    let mut sources = sources.borrow_mut();
                    for path in &paths {
                        if library::walker::has_cue_extension(path) {
                            sources.include_cue_sheet(path);
                        } else {
                            sources.include_path(path);
                        }
                    }
                }
                // `explicit_selection: true` — this is the user's own complete "Open Files…" pick,
                // so cue expansion must not silently claim sibling files they never selected
                // (`CLAUDE.md` "Over-broad CUE claim on Open Files"). The exclusions are snapshotted
                // here, after the lift above, so the scanner never counts a track the user just
                // re-opened as one whose record gets dropped (`ScanRequest::excluded`).
                let excluded = sources.borrow().excluded_tracks.clone();
                let batch = scanner.scan(paths.clone(), true, excluded);
                batch_kinds.borrow_mut().insert(batch, BatchKind::OpenFiles);
                // Persisted so these files are re-scanned (never re-enqueued) at the next startup
                // (`CLAUDE.md` "Persistent library"). A save failure is reported but never blocks
                // opening/playing the files that were just selected. A directly-opened `.cue` sheet
                // is tracked separately (`cue_files`) so its own folder's cue expansion runs again
                // on restore, the same way a saved `files`/`folders` entry does.
                let mut sources = sources.borrow_mut();
                for path in &paths {
                    if library::walker::has_cue_extension(path) {
                        sources.add_cue_file(path.clone());
                    } else {
                        sources.add_file(path.clone());
                    }
                }
                if let Err(error) = sources.save() {
                    *status_save_error.borrow_mut() = Some(format!("Could not save library.json: {error}"));
                }
            }
        }) && let Some(window) = open_window.upgrade()
        {
            set_playback_status(
                &window,
                format!("Could not start the file picker: {error}"),
                &open_save_error,
            );
        }
    });

    let scanner_for_open_folder = Arc::clone(&library_scanner);
    let open_folder_window = window.as_weak();
    let folder_picker_parent_window = window.as_weak();
    let open_folder_save_error = Rc::clone(&settings_save_error);
    let open_folder_scanning_batches = Rc::clone(&scanning_batches);
    let open_folder_batch_kinds = Rc::clone(&batch_kinds);
    let open_folder_sources = Rc::clone(&library_sources);
    window.on_open_folder(move || {
        let Some(parent) = folder_picker_parent_window.upgrade() else { return; };
        let scanner = Arc::clone(&scanner_for_open_folder);
        let parent_window = parent.window().window_handle();
        let selection = pick_audio_folders(parent_window);
        let status_window = open_folder_window.clone();
        let status_save_error = Rc::clone(&open_folder_save_error);
        let scanning_batches = Rc::clone(&open_folder_scanning_batches);
        let batch_kinds = Rc::clone(&open_folder_batch_kinds);
        let sources = Rc::clone(&open_folder_sources);
        if let Err(error) = slint::spawn_local(async move {
            if let Some(folders) = selection.await
                && !folders.is_empty()
            {
                if let Some(window) = status_window.upgrade() {
                    set_playback_status(&window, folder_scan_status(&folders), &status_save_error);
                }
                // One `scan_folder` batch per chosen folder: each walks on its own thread
                // (`LibraryScanner::scan_folder`) and reports its own `BatchDone`, which is what
                // `scanning_batches` counts.
                for folder in &folders {
                    scanning_batches.set(scanning_batches.get() + 1);
                    // Same override rule as "Open Files…" above, for every track under the re-added
                    // folder (`CLAUDE.md` "Library exclusions").
                    sources.borrow_mut().include_folder(folder);
                    let excluded = sources.borrow().excluded_tracks.clone();
                    let batch = scanner.scan_folder(folder.clone(), excluded);
                    batch_kinds.borrow_mut().insert(batch, BatchKind::OpenFolder);
                }
                // Added to the library only (`batch_should_enqueue(BatchKind::OpenFolder)` is
                // false) and persisted so they are re-walked at every startup, picking up files
                // added to them since (`CLAUDE.md` "Persistent library").
                let mut sources = sources.borrow_mut();
                for folder in folders {
                    sources.add_folder(folder);
                }
                if let Err(error) = sources.save() {
                    *status_save_error.borrow_mut() = Some(format!("Could not save library.json: {error}"));
                }
            }
        }) && let Some(window) = open_folder_window.upgrade()
        {
            set_playback_status(
                &window,
                format!("Could not start the folder picker: {error}"),
                &open_folder_save_error,
            );
        }
    });

    // Startup library restore (`CLAUDE.md` "Persistent library"): re-scans every persisted source
    // through the same scanner thread "Open Files…"/"Open Folder…" use, tagged `StartupRestore` so
    // `Probed` (below) never enqueues or plays anything — it only ever fills the library. Missing
    // paths (an unmounted NAS share) are kept in `library_sources` regardless of whether this
    // scan succeeds; nothing here ever removes an entry.
    {
        let sources = library_sources.borrow();
        if !sources.files.is_empty() {
            // `explicit_selection: false` — a restore behaves like the whole directory was already
            // requested, keeping full cue expansion exactly like `OpenFolder` (`CLAUDE.md` "Over-
            // broad CUE claim on Open Files"); nothing here is enqueued or played regardless.
            let batch = library_scanner.scan(sources.files.clone(), false, sources.excluded_tracks.clone());
            batch_kinds.borrow_mut().insert(batch, BatchKind::StartupRestore);
            scanning_batches.set(scanning_batches.get() + 1);
        }
        // A directly-opened `.cue` sheet re-scans through the same `scan()` request as ordinary
        // files: the scanner's per-directory cue expansion (`library::scanner`) detects it from its
        // own path exactly like a freshly opened one.
        if !sources.cue_files.is_empty() {
            let batch = library_scanner.scan(sources.cue_files.clone(), false, sources.excluded_tracks.clone());
            batch_kinds.borrow_mut().insert(batch, BatchKind::StartupRestore);
            scanning_batches.set(scanning_batches.get() + 1);
        }
        for folder in &sources.folders {
            let batch = library_scanner.scan_folder(folder.clone(), sources.excluded_tracks.clone());
            batch_kinds.borrow_mut().insert(batch, BatchKind::StartupRestore);
            scanning_batches.set(scanning_batches.get() + 1);
        }
    }

    let player_for_output = Arc::clone(&audio_player);
    let preferences_for_output = Rc::clone(&preferences);
    let save_error_for_output = Rc::clone(&settings_save_error);
    let devices_for_output = Rc::clone(&output_devices);
    let output_window = window.as_weak();
    window.on_output_chosen(move |index| {
        let selected = if index > 0 {
            devices_for_output.borrow().get(index as usize - 1).cloned()
        } else {
            None
        };
        let selected_id = selected.as_ref().map(|device| device.id.clone());
        if is_output_reselection_noop(selected_id.as_deref(), preferences_for_output.borrow().output_device_id.as_deref()) {
            return;
        }
        player_for_output.select_output(selected_id.clone());
        preferences_for_output.borrow_mut().output_device_id = selected_id;
        let save_status = match preferences_for_output.borrow().save() {
            Ok(()) => {
                *save_error_for_output.borrow_mut() = None;
                "Output preference saved."
            }
            Err(error) => {
                *save_error_for_output.borrow_mut() = Some(format!("Could not save settings: {error}"));
                "Output changed, but the preference could not be saved."
            }
        };
        if let Some(window) = output_window.upgrade() {
            window.set_output_name(
                selected
                    .map(|device| SharedString::from(device.label.as_str()))
                    .unwrap_or_else(|| SharedString::from("Not selected")),
            );
            let status = if save_error_for_output.borrow().is_some() {
                save_status
            } else if index > 0 {
                OUTPUT_SELECTED_STATUS
            } else {
                "Choose an output DAC, then open audio."
            };
            set_playback_status(&window, status, &save_error_for_output);
        }
    });

    let player_for_hog = Arc::clone(&audio_player);
    let preferences_for_hog = Rc::clone(&preferences);
    let save_error_for_hog = Rc::clone(&settings_save_error);
    let hog_window = window.as_weak();
    window.on_hog_mode_changed(move |enabled| {
        player_for_hog.set_hog_mode(enabled);
        preferences_for_hog.borrow_mut().hog_mode_enabled = enabled;
        let status = match preferences_for_hog.borrow().save() {
            Ok(()) => {
                *save_error_for_hog.borrow_mut() = None;
                "Hog mode preference saved."
            }
            Err(error) => {
                *save_error_for_hog.borrow_mut() = Some(format!("Could not save Hog mode setting: {error}"));
                "Hog mode changed, but the preference could not be saved."
            }
        };
        if let Some(window) = hog_window.upgrade() {
            set_playback_status(&window, status, &save_error_for_hog);
        }
    });

    let player_for_toggle = Arc::clone(&audio_player);
    window.on_play_pause_requested(move || player_for_toggle.toggle_playback());
    let player_for_next = Arc::clone(&audio_player);
    window.on_next_track_requested(move || player_for_next.next());
    let player_for_previous = Arc::clone(&audio_player);
    window.on_previous_track_requested(move || player_for_previous.previous());

    // Marked by everything that changes what the OS Now Playing widget should show; the event timer
    // publishes at most once per tick from the final UI state (`sync_media_session`).
    let media_dirty = Rc::new(Cell::new(false));
    let seek_requester = Rc::new(SeekRequester {
        player: Arc::clone(&audio_player),
        now_playing_key: Rc::clone(&now_playing_key),
        now_playing_duration_ms: Rc::clone(&now_playing_duration_ms),
        pending_seek: Rc::clone(&pending_seek),
        media_dirty: Rc::clone(&media_dirty),
    });
    let seek_window = window.as_weak();
    let scrubber_seek = Rc::clone(&seek_requester);
    window.on_seek_requested(move |fraction| {
        let Some(window) = seek_window.upgrade() else { return; };
        scrubber_seek.request_fraction(&window, fraction);
    });

    // The Slint handler already set `volume-level` optimistically (`§5.10` "Pending volume")
    // before this callback runs; `is_final` is true for a click and for the last tick of a drag.
    let player_for_volume = Arc::clone(&audio_player);
    let volume_pending = Rc::clone(&pending_volume);
    let volume_last_sent_at = Rc::clone(&last_volume_sent_at);
    window.on_volume_requested(move |level, is_final| {
        let now = Instant::now();
        *volume_pending.borrow_mut() = Some(PendingVolume { level, changed_at: now });
        if should_send_volume(*volume_last_sent_at.borrow(), now, is_final) {
            *volume_last_sent_at.borrow_mut() = Some(now);
            player_for_volume.set_volume(level);
        }
    });

    // Navigation (`§3.6`): every callback mutates the Rust-owned `Navigation`, re-syncs the handful
    // of Slint properties it drives (`view`, `nav-view`, `can-go-back`, `artist-filter`,
    // `unavailable-title`), then reprojects the library views: `nav-selected` clears the artist
    // filter (`§3.6`), a projection input, so it needs the same reprojection as `album-opened`/
    // `artist-opened`/`back-requested` — otherwise a view switched away from an artist-filtered
    // Albums grid would keep showing that artist's albums under the plain "Albums" hero.
    let navigation_for_nav_selected = Rc::clone(&navigation);
    let nav_selected_window = window.as_weak();
    let nav_selected_app_state = Rc::clone(&app_state);
    let nav_selected_songs_paths = Rc::clone(&songs_paths);
    let nav_selected_recent_paths = Rc::clone(&recent_paths);
    let nav_selected_album_paths = Rc::clone(&album_paths);
    let nav_selected_search_paths = Rc::clone(&search_paths);
    window.on_nav_selected(move |view| {
        navigation_for_nav_selected.borrow_mut().nav_selected(view);
        if let Some(window) = nav_selected_window.upgrade() {
            sync_navigation(&window, &navigation_for_nav_selected.borrow());
            project_and_set_library(
                &window,
                &nav_selected_app_state.borrow(),
                &navigation_for_nav_selected.borrow(),
                navigation_for_nav_selected.borrow().search_query(),
                LibraryViewPaths {
                    songs: &nav_selected_songs_paths,
                    recent: &nav_selected_recent_paths,
                    album: &nav_selected_album_paths,
                    search: &nav_selected_search_paths,
                },
            );
        }
    });

    let navigation_for_back = Rc::clone(&navigation);
    let back_window = window.as_weak();
    let back_app_state = Rc::clone(&app_state);
    let back_songs_paths = Rc::clone(&songs_paths);
    let back_recent_paths = Rc::clone(&recent_paths);
    let back_album_paths = Rc::clone(&album_paths);
    let back_search_paths = Rc::clone(&search_paths);
    window.on_back_requested(move || {
        navigation_for_back.borrow_mut().back_requested();
        if let Some(window) = back_window.upgrade() {
            sync_navigation(&window, &navigation_for_back.borrow());
            project_and_set_library(
                &window,
                &back_app_state.borrow(),
                &navigation_for_back.borrow(),
                navigation_for_back.borrow().search_query(),
                LibraryViewPaths {
                    songs: &back_songs_paths,
                    recent: &back_recent_paths,
                    album: &back_album_paths,
                    search: &back_search_paths,
                },
            );
        }
    });

    let navigation_for_album_opened = Rc::clone(&navigation);
    let album_opened_window = window.as_weak();
    let album_opened_app_state = Rc::clone(&app_state);
    let album_opened_songs_paths = Rc::clone(&songs_paths);
    let album_opened_recent_paths = Rc::clone(&recent_paths);
    let album_opened_album_paths = Rc::clone(&album_paths);
    let album_opened_search_paths = Rc::clone(&search_paths);
    window.on_album_opened(move |key| {
        navigation_for_album_opened.borrow_mut().album_opened(&key);
        if let Some(window) = album_opened_window.upgrade() {
            sync_navigation(&window, &navigation_for_album_opened.borrow());
            project_and_set_library(
                &window,
                &album_opened_app_state.borrow(),
                &navigation_for_album_opened.borrow(),
                navigation_for_album_opened.borrow().search_query(),
                LibraryViewPaths {
                    songs: &album_opened_songs_paths,
                    recent: &album_opened_recent_paths,
                    album: &album_opened_album_paths,
                    search: &album_opened_search_paths,
                },
            );
        }
    });

    let navigation_for_artist_opened = Rc::clone(&navigation);
    let artist_opened_window = window.as_weak();
    let artist_opened_app_state = Rc::clone(&app_state);
    let artist_opened_songs_paths = Rc::clone(&songs_paths);
    let artist_opened_recent_paths = Rc::clone(&recent_paths);
    let artist_opened_album_paths = Rc::clone(&album_paths);
    let artist_opened_search_paths = Rc::clone(&search_paths);
    window.on_artist_opened(move |name| {
        navigation_for_artist_opened.borrow_mut().artist_opened(&name);
        if let Some(window) = artist_opened_window.upgrade() {
            sync_navigation(&window, &navigation_for_artist_opened.borrow());
            project_and_set_library(
                &window,
                &artist_opened_app_state.borrow(),
                &navigation_for_artist_opened.borrow(),
                navigation_for_artist_opened.borrow().search_query(),
                LibraryViewPaths {
                    songs: &artist_opened_songs_paths,
                    recent: &artist_opened_recent_paths,
                    album: &artist_opened_album_paths,
                    search: &artist_opened_search_paths,
                },
            );
        }
    });

    // The player-bar artwork click (`§5.6` item 1): same as `album-opened`, with the now-playing
    // path's own album key when the library knows one (`§3.6`).
    let navigation_for_now_playing_album = Rc::clone(&navigation);
    let now_playing_album_window = window.as_weak();
    let now_playing_album_app_state = Rc::clone(&app_state);
    let now_playing_album_key = Rc::clone(&now_playing_key);
    let now_playing_album_songs_paths = Rc::clone(&songs_paths);
    let now_playing_album_recent_paths = Rc::clone(&recent_paths);
    let now_playing_album_album_paths = Rc::clone(&album_paths);
    let now_playing_album_search_paths = Rc::clone(&search_paths);
    window.on_now_playing_album_requested(move || {
        let key = now_playing_album_key
            .borrow()
            .as_ref()
            .and_then(|path| now_playing_album_app_state.borrow().library.get(path).map(library::album_key));
        let Some(key) = key else { return; };
        navigation_for_now_playing_album.borrow_mut().album_opened(&key);
        if let Some(window) = now_playing_album_window.upgrade() {
            sync_navigation(&window, &navigation_for_now_playing_album.borrow());
            project_and_set_library(
                &window,
                &now_playing_album_app_state.borrow(),
                &navigation_for_now_playing_album.borrow(),
                navigation_for_now_playing_album.borrow().search_query(),
                LibraryViewPaths {
                    songs: &now_playing_album_songs_paths,
                    recent: &now_playing_album_recent_paths,
                    album: &now_playing_album_album_paths,
                    search: &now_playing_album_search_paths,
                },
            );
        }
    });

    // `search-edited` (`§3.4`): stores the query in `Navigation` and marks the library dirty; the
    // 40 ms timer reprojects at most once per tick, batching fast typing the same way a scan batch
    // is batched. `search-active` is pushed to the window immediately, not left for that tick,
    // so the Search view swaps in/out on every keystroke instead of trailing it by up to 40 ms
    // (`apply_search_edit`, which also forgets the row selection when the list on screen changes).
    let navigation_for_search_edited = Rc::clone(&navigation);
    let search_edited_window = window.as_weak();
    let library_dirty_for_search = Rc::clone(&library_dirty);
    window.on_search_edited(move |query| {
        library_dirty_for_search.set(true);
        if let Some(window) = search_edited_window.upgrade() {
            apply_search_edit(&window, &mut navigation_for_search_edited.borrow_mut(), query.to_string());
        }
    });

    // `shuffle-all` (Home, `§3.7`): shuffles the whole library, not just one album.
    let player_for_shuffle_all = Arc::clone(&audio_player);
    let app_state_for_shuffle_all = Rc::clone(&app_state);
    window.on_shuffle_all(move || {
        let tracks: Vec<PreparedTrack> = app_state_for_shuffle_all.borrow().library.songs("").into_iter().map(prepared_track).collect();
        perform_album_action(AlbumAction::Shuffle, tracks, shuffle_seed(), player_for_shuffle_all.as_ref());
    });

    // `album-action` (album detail's Play/Shuffle/Add to Queue/Play Next buttons, `§3.7`).
    let player_for_album_action = Arc::clone(&audio_player);
    let app_state_for_album_action = Rc::clone(&app_state);
    window.on_album_action(move |key, action| {
        let tracks: Vec<PreparedTrack> =
            app_state_for_album_action.borrow().library.album_tracks(&key).into_iter().map(prepared_track).collect();
        perform_album_action(action, tracks, shuffle_seed(), player_for_album_action.as_ref());
    });

    // "Remove from Library" (album card's three-dot menu, every grid — Home, Albums, Search,
    // artist albums, `CLAUDE.md` "Library exclusions"). Non-destructive and library-only: this
    // drops the album's tracks from the *session library* (`Library::remove_album`) and persists an
    // exclusion so the startup restore does not bring them back, but it deliberately never touches
    // `AudioPlayer` — the queue and the active stream are separate, session-only state from the
    // library (`CLAUDE.md` "Session library and view layer"), so a track from the removed album
    // that is currently playing or queued keeps playing/queued untouched; only the browsable
    // catalog loses it.
    let navigation_for_remove_album = Rc::clone(&navigation);
    let remove_album_window = window.as_weak();
    let app_state_for_remove_album = Rc::clone(&app_state);
    let library_sources_for_remove_album = Rc::clone(&library_sources);
    let save_error_for_remove_album = Rc::clone(&settings_save_error);
    let remove_album_songs_paths = Rc::clone(&songs_paths);
    let remove_album_recent_paths = Rc::clone(&recent_paths);
    let remove_album_album_paths = Rc::clone(&album_paths);
    let remove_album_search_paths = Rc::clone(&search_paths);
    window.on_remove_album_requested(move |key| {
        let Some(album) = app_state_for_remove_album.borrow().library.album(&key) else { return; };
        let title = album.title.clone();
        let removed_keys = app_state_for_remove_album.borrow_mut().remove_album(&key);
        if removed_keys.is_empty() {
            return;
        }
        {
            let mut sources = library_sources_for_remove_album.borrow_mut();
            sources.exclude_tracks(removed_keys);
            if let Err(error) = sources.save() {
                *save_error_for_remove_album.borrow_mut() = Some(format!("Could not save library.json: {error}"));
            }
        }
        // If the album-detail page for the just-removed album is currently open, it has nothing
        // left to show — navigate back automatically instead of leaving it stranded on an empty
        // header.
        navigation_for_remove_album.borrow_mut().remove_album(&key);
        if let Some(window) = remove_album_window.upgrade() {
            sync_navigation(&window, &navigation_for_remove_album.borrow());
            project_and_set_library(
                &window,
                &app_state_for_remove_album.borrow(),
                &navigation_for_remove_album.borrow(),
                navigation_for_remove_album.borrow().search_query(),
                LibraryViewPaths {
                    songs: &remove_album_songs_paths,
                    recent: &remove_album_recent_paths,
                    album: &remove_album_album_paths,
                    search: &remove_album_search_paths,
                },
            );
            set_playback_status(&window, format!("Removed \u{201c}{title}\u{201d} from the library."), &save_error_for_remove_album);
        }
    });

    // `track-activated` (`§3.7`): every kind maps a clicked row to the suffix of prepared tracks
    // starting there, through whichever path list backs that table (`songs_paths`/`recent_paths`/
    // `album_paths`/`queue_paths`/`search_paths`), then replaces the queue with it.
    let player_for_track_activated = Arc::clone(&audio_player);
    let app_state_for_track_activated = Rc::clone(&app_state);
    let queue_paths_for_activated = Rc::clone(&queue_paths);
    let songs_paths_for_activated = Rc::clone(&songs_paths);
    let recent_paths_for_activated = Rc::clone(&recent_paths);
    let album_paths_for_activated = Rc::clone(&album_paths);
    let search_paths_for_activated = Rc::clone(&search_paths);
    window.on_track_activated(move |kind, index| {
        let Ok(index) = usize::try_from(index) else { return; };
        let paths = match kind {
            TrackListKind::Queue => &queue_paths_for_activated,
            TrackListKind::Songs => &songs_paths_for_activated,
            TrackListKind::RecentlyAdded => &recent_paths_for_activated,
            TrackListKind::Album => &album_paths_for_activated,
            TrackListKind::Search => &search_paths_for_activated,
        };
        let tracks = prepared_suffix(&app_state_for_track_activated.borrow().library, &paths.borrow(), index);
        // The UI never sends an empty `ReplaceQueue`: the worker would just ignore it, but a path
        // that has left the library between render and click should not send a stop for nothing.
        if !tracks.is_empty() {
            player_for_track_activated.replace_queue(tracks);
        }
    });

    // Space, from `MainWindow`'s window-wide `key-root` (plain Space only: never a key repeat, never
    // typed into the search field). The row last clicked is `MainWindow.selected-track-key`, a track
    // key that `visible_selection` resolves to a row of the visible table only now, so a re-projection
    // that moved or removed the track in between cannot make Space play another one. A selected row
    // plays through `track-activated`, the same handler a double click reaches, so the two can never
    // disagree about which rows a click queues.
    let player_for_space = Arc::clone(&audio_player);
    let space_window = window.as_weak();
    window.on_space_pressed(move || {
        let Some(window) = space_window.upgrade() else { return; };
        match plan_space_action(window_play_button_enabled(&window), visible_selection(&window)) {
            SpaceAction::TogglePlayback => player_for_space.toggle_playback(),
            SpaceAction::ActivateSelection { kind, index } => window.invoke_track_activated(kind, index),
        }
    });

    let events = audio_player.events();
    let library_events = library_scanner.events();
    let event_window = window.as_weak();
    let event_devices = Rc::clone(&output_devices);
    let event_preferences = Rc::clone(&preferences);
    let event_save_error = Rc::clone(&settings_save_error);
    let event_now_playing_key = Rc::clone(&now_playing_key);
    let event_duration = Rc::clone(&now_playing_duration_ms);
    let event_fallback = Rc::clone(&now_playing_fallback);
    let event_info = Rc::clone(&now_playing_info);
    let event_pending_seek = Rc::clone(&pending_seek);
    let event_last_timeline = Rc::clone(&last_timeline);
    let event_pending_volume = Rc::clone(&pending_volume);
    let event_last_worker_volume = Rc::clone(&last_worker_volume);
    let event_active_route_status = Rc::clone(&active_route_status);
    let event_batch_failures = Rc::clone(&batch_failures);
    let event_batch_repairs = Rc::clone(&batch_repairs);
    let event_batch_kinds = Rc::clone(&batch_kinds);
    let event_library_sources = Rc::clone(&library_sources);
    let event_scanning_batches = Rc::clone(&scanning_batches);
    let event_library_last_projected = Rc::clone(&library_last_projected_at);
    let event_app_state = Rc::clone(&app_state);
    let event_player_for_library = Arc::clone(&audio_player);
    let event_queue_pending = Rc::clone(&queue_pending);
    let event_queue_paths = Rc::clone(&queue_paths);
    let event_navigation = Rc::clone(&navigation);
    let event_library_dirty = Rc::clone(&library_dirty);
    let event_songs_paths = Rc::clone(&songs_paths);
    let event_recent_paths = Rc::clone(&recent_paths);
    let event_album_paths = Rc::clone(&album_paths);
    let event_search_paths = Rc::clone(&search_paths);
    // The OS media session (macOS media keys, Control Center; inert elsewhere). Registered once, here
    // in `main()` and never from anything `MainWindow::new()` reaches. Its handlers only push into
    // this channel, which the event timer drains on the UI thread; the session and the hold-to-seek
    // state below live inside that timer closure, the only thing that uses them.
    let (media_tx, media_rx) = crossbeam_channel::unbounded::<MediaCommand>();
    let mut media_session = MediaSession::new(media_tx);
    let mut hold_seek = HoldSeek::default();
    // What the OS was last told, to notice a timeline that drifted from the widget's extrapolation.
    let mut media_published = None::<PublishedTransport>;
    // What the now-playing panel last showed (`PanelNowPlaying`): the widget mirrors it, and it
    // outlives `Stopped`/`Inactive`, like the panel's own title and cover do.
    let mut panel_now_playing = None::<PanelNowPlaying>;
    let event_media_dirty = Rc::clone(&media_dirty);
    let event_seek_requester = Rc::clone(&seek_requester);
    let event_player_for_media = Arc::clone(&audio_player);
    let _event_timer = Timer::default();
    _event_timer.start(TimerMode::Repeated, Duration::from_millis(40), move || {
        // `§3.4`: "projections run once per dirty flag, so large scans are batched." Every event
        // below that can change the Queue view's rows only marks this, instead of calling
        // `project_and_set_queue` inline; a batch of N `Scanned` events used to rebuild the queue N
        // times (a `TrackRowData` plus a fresh `VecModel` per rebuild) against a queue of up to N
        // pending tracks, an O(n^2) cost on the UI thread for one open of N files.
        let mut queue_dirty = false;
        // OS media commands first, so the playback events they cause are seen this tick or the next.
        // Every command is a `DIAG` line: whether a physical hold arrives as Begin/End, and whether
        // a tap also sends a seek pair, can only be settled on real hardware (`CLAUDE.md`).
        let mut media_ui_playing = None::<bool>;
        while let Ok(command) = media_rx.try_recv() {
            let Some(window) = event_window.upgrade() else { return; };
            write_diagnostic(&format!("media command {command:?}"));
            // Two toggles in one drain must not both read the same stale button state.
            let ui_playing = *media_ui_playing.get_or_insert_with(|| window.get_is_playing());
            match plan_media_command(command, ui_playing) {
                TransportAction::TogglePlayback => {
                    event_player_for_media.toggle_playback();
                    media_ui_playing = Some(!ui_playing);
                }
                TransportAction::Next => event_player_for_media.next(),
                // The controller applies the restart-after-3 s rule itself.
                TransportAction::Previous => event_player_for_media.previous(),
                TransportAction::SeekTo { position_ms } => {
                    event_seek_requester.request(&window, position_ms);
                }
                TransportAction::BeginHoldSeek(direction) => hold_seek.begin(direction, Instant::now()),
                TransportAction::EndHoldSeek(direction) => hold_seek.end(direction),
                TransportAction::Ignore => {}
            }
        }
        while let Ok(event) = events.try_recv() {
            let Some(window) = event_window.upgrade() else { return; };
            match event {
                PlaybackEvent::Devices(devices) => {
                    *event_devices.borrow_mut() = devices;
                    set_device_model(&window, &event_devices.borrow());
                    let selected_id = event_preferences.borrow().output_device_id.clone();
                    let index = selected_id
                        .as_ref()
                        .and_then(|selected| event_devices.borrow().iter().position(|device| &device.id == selected))
                        .map(|index| index as i32 + 1)
                        .unwrap_or(0);
                    window.set_selected_output_index(index);
                    if index == 0 && selected_id.is_some() {
                        set_playback_status(
                            &window,
                            "Saved output device is unavailable; choose another DAC.",
                            &event_save_error,
                        );
                    }
                }
                PlaybackEvent::Started {
                    key,
                    info,
                    title,
                    album,
                    artist: _artist,
                    output,
                    hog_mode,
                    integer_output_verified,
                } => {
                    *event_now_playing_key.borrow_mut() = Some(key.clone());
                    *event_fallback.borrow_mut() = (title, album);
                    *event_info.borrow_mut() = Some(info.clone());
                    clear_pending_seek(&mut event_pending_seek.borrow_mut());
                    // A new track: stepping on from the old one's position would be wrong.
                    hold_seek.disarm();
                    event_media_dirty.set(true);
                    // A single `borrow_mut()` call, not a split lookup-then-mutate: edition 2024's
                    // `if let` rescoping keeps a `Ref` from the scrutinee alive through the whole
                    // then-block, so `event_app_state.borrow().library.get(...)` followed by
                    // `event_app_state.borrow_mut()` in the same block panicked with "already
                    // borrowed" on every `Started` (`§6` Stage 5 fix).
                    event_app_state.borrow_mut().note_played_key(&key);
                    panel_now_playing =
                        Some(apply_now_playing_projection(&window, &event_app_state.borrow(), &key, &info, &event_fallback.borrow()));
                    queue_dirty = true;
                    // `note_played_key` may have pushed a new album to the front of Home's "Jump
                    // back in" shelf (`§5.10` "Recently played").
                    event_library_dirty.set(true);
                    // `active-output-name`, not `output-name`: the latter is the *selected* device
                    // (set at startup and by `on_output_chosen` alone, before anything necessarily
                    // plays there), while this is the device the stream is actually on (`§6`).
                    window.set_active_output_name(output.into());
                    let status = match (integer_output_verified, hog_mode) {
                        (true, true) => "Playing · 32-bit integer output verified · Hog mode active",
                        (true, false) => "Playing · 32-bit integer output verified",
                        (false, true) => "Playing · Float output path (not bit-perfect) · Hog mode active",
                        (false, false) => "Playing · Float output path (not bit-perfect)",
                    };
                    *event_active_route_status.borrow_mut() = Some(status.to_owned());
                    set_playback_status(&window, status, &event_save_error);
                    window.set_is_playing(true);
                }
                PlaybackEvent::Timeline { position_ms, duration_ms } => {
                    *event_duration.borrow_mut() = duration_ms;
                    let apply = {
                        let mut pending = event_pending_seek.borrow_mut();
                        match pending.take() {
                            Some(seek) if accept_timeline(&seek, position_ms, Instant::now()) => true,
                            Some(seek) => {
                                *pending = Some(seek);
                                false
                            }
                            None => true,
                        }
                    };
                    if apply {
                        window.set_playback_elapsed(format_clock(position_ms).into());
                        window.set_playback_duration(
                            duration_ms
                                .map(format_clock)
                                .unwrap_or_else(|| "—:—".into())
                                .into(),
                        );
                        window.set_playback_progress(playback_progress(position_ms, duration_ms));
                        *event_last_timeline.borrow_mut() = (position_ms, duration_ms);
                        // Not every ~50 ms tick: the OS widget extrapolates from the last publish, so
                        // only a position that drifted from that (a seek landed, a stall) or a
                        // late-discovered duration republishes.
                        if timeline_needs_publish(media_published.as_ref(), position_ms, duration_ms, Instant::now()) {
                            event_media_dirty.set(true);
                        }
                    }
                    window.set_can_seek(duration_ms.is_some_and(|duration_ms| duration_ms > 0));
                    window.set_duration_ms(duration_ms.unwrap_or(0) as i32);
                }
                PlaybackEvent::Playing(playing) => {
                    window.set_is_playing(playing);
                    event_media_dirty.set(true);
                }
                PlaybackEvent::SeekRejected => {
                    clear_pending_seek(&mut event_pending_seek.borrow_mut());
                    // A rejected seek ends a hold (its next step would be refused the same way), and
                    // the widget must go back from the requested target to the true position.
                    hold_seek.disarm();
                    event_media_dirty.set(true);
                    // Nothing guarantees a fresh Timeline follows a rejection (a reject with no
                    // active track sends none): restore the last truthful position ourselves so
                    // the bar never keeps showing the clicked-but-never-applied target (`§1.4`).
                    let (elapsed, duration, progress) = timeline_after_seek_rejected(*event_last_timeline.borrow());
                    window.set_playback_elapsed(elapsed.into());
                    window.set_playback_duration(duration.into());
                    window.set_playback_progress(progress);
                }
                PlaybackEvent::QueueSnapshot(tracks) => {
                    window.set_queue_count(tracks.len() as i32);
                    *event_queue_pending.borrow_mut() = tracks;
                    queue_dirty = true;
                }
                PlaybackEvent::Volume { available, level, detail } => {
                    // `available` and `detail` always apply; `level` only through
                    // `resolve_volume_event`, so a stale echo never overwrites a value the user
                    // just set with their own hand (`§5.10` "Pending volume"). Recorded regardless
                    // of acceptance so a rejected echo can still be snapped to later (`§6` Stage 4
                    // fix, the 40 ms timer's `expire_pending_volume` check below).
                    window.set_volume_available(available);
                    window.set_volume_detail(detail.into());
                    event_last_worker_volume.set(Some(level));
                    let now = Instant::now();
                    let apply_level = {
                        let mut pending = event_pending_volume.borrow_mut();
                        resolve_volume_event(&mut pending, available, level, now)
                    };
                    if apply_level {
                        window.set_volume_level(level);
                    }
                }
                PlaybackEvent::Stopped => {
                    hold_seek.disarm();
                    event_media_dirty.set(true);
                    window.set_is_playing(false);
                    // No track is active once Stopped is emitted, so seeking must not stay
                    // enabled on a bar with nothing to seek (`§1.4`).
                    window.set_can_seek(false);
                    *event_active_route_status.borrow_mut() = None;
                    // Nothing is active anymore (unlike a pause, which only sends `Playing(false)`):
                    // the Queue view's "Now Playing" section must not keep showing a stopped track.
                    *event_now_playing_key.borrow_mut() = None;
                    *event_info.borrow_mut() = None;
                    // Clears the "now playing" accent marker in Songs/Albums/Queue rows, which
                    // otherwise stays on a track that is no longer active (`§5.10`). The bar's
                    // title/art are left as-is so Play-after-EOF can still show what will replay.
                    window.set_now_playing_key("".into());
                    // Nothing is actually routing audio once Stopped fires; the panel's "Playing on
                    // X" must not keep claiming otherwise (`§6`).
                    window.set_active_output_name("".into());
                    queue_dirty = true;
                    set_playback_status(&window, "Stopped", &event_save_error);
                }
                PlaybackEvent::Inactive => {
                    // The worker's active track ended without a new one starting and without the
                    // queue itself being cleared (a failed seek, a retained playback failure, or a
                    // failed automatic advance) — unlike `Stopped`, its own `Status` explains why,
                    // so this must not touch the status line (`§1.4`).
                    hold_seek.disarm();
                    event_media_dirty.set(true);
                    window.set_is_playing(false);
                    window.set_can_seek(false);
                    *event_active_route_status.borrow_mut() = None;
                    *event_now_playing_key.borrow_mut() = None;
                    *event_info.borrow_mut() = None;
                    window.set_now_playing_key("".into());
                    window.set_active_output_name("".into());
                    queue_dirty = true;
                }
                PlaybackEvent::Status(status) => set_playback_status(&window, &status, &event_save_error),
                PlaybackEvent::Diagnostic(line) => {
                    // Use a write that cannot panic: `eprintln!` unwinds the UI-thread timer
                    // callback (and takes the event loop with it) if stderr is a closed pipe.
                    use std::io::Write;
                    let _ = writeln!(std::io::stderr().lock(), "DIAG {line}");
                }
            }
        }
        // Hold-to-seek steps, after the playback events above so the pending-seek and timeline state
        // they read is this tick's final one.
        if hold_seek.is_armed() && let Some(window) = event_window.upgrade() {
            let input = {
                let pending = event_pending_seek.borrow();
                HoldSeekInput {
                    seek_pending: pending.is_some(),
                    position_ms: pending.as_ref().map_or_else(|| event_last_timeline.borrow().0, |seek| seek.target_ms),
                    duration_ms: *event_duration.borrow(),
                }
            };
            if let HoldSeekTick::Step { target_ms } = hold_seek.tick(Instant::now(), input) {
                event_seek_requester.request(&window, target_ms);
            }
        }
        while let Ok(event) = library_events.try_recv() {
            let Some(window) = event_window.upgrade() else { return; };
            match event {
                LibraryEvent::Probed { batch, tracks } => {
                    let kind = event_batch_kinds.borrow().get(&batch).copied().unwrap_or(BatchKind::OpenFiles);
                    // Drops any track "Remove from Library" excluded (`CLAUDE.md` "Library
                    // exclusions") before it can be enqueued or added back to the library — chiefly
                    // matters for the startup restore rescanning a persisted folder/file; an
                    // "Open Files…"/"Open Folder…" batch never actually contains an excluded track
                    // in the first place, since re-opening a path lifts its exclusion synchronously
                    // before `scan()`/`scan_folder()` is even dispatched (see the callbacks above).
                    let tracks = exclude_removed_tracks(tracks, &event_library_sources.borrow());
                    if batch_should_enqueue(kind) {
                        event_player_for_library.enqueue(tracks.clone());
                    }
                    event_app_state.borrow_mut().apply_probed(&tracks);
                    // `library_empty` and every library view are projected together, batched below
                    // (`§3.4`) rather than set inline here: a batch of N files would otherwise
                    // recompute Albums/Artists/Songs up to N times over.
                    event_library_dirty.set(true);
                    // The status-line reaction to a batch's own phase-1 failures below is specific
                    // to "Open Files…" (whose tracks are already enqueued/playing by the time this
                    // fires): "Open Folder…" reports its own "Scanning…"/"Added N tracks" via
                    // `FolderScanProgress`/`BatchDone`, and the startup restore stays quiet here —
                    // see `batch_reports_failures_inline`.
                    if kind == BatchKind::OpenFiles {
                        // Phase 1 is fully finished by the time `Probed` arrives — every phase-1
                        // `Failed` for this batch was already sent (and processed in this same tick,
                        // ahead of `Probed` on the same channel) before phase 2 even starts queuing
                        // this batch's items. Show that phase-1 refusal summary right now instead of
                        // waiting for `BatchDone`, which only fires once phase 2 (tags/artwork)
                        // finishes too — possibly minutes later on a slow NAS share (`§1.4`, "refuse
                        // rather than silently degrade"). A batch with no phase-1 failures just
                        // restores the ordinary route status, since the tracks are already enqueued.
                        let probe_failures =
                            event_batch_failures.borrow().get(&batch).map(|failures| failures.probe.clone()).unwrap_or_default();
                        if probe_failures.is_empty() {
                            restore_active_route_status(&window, &event_active_route_status, &event_save_error);
                        } else {
                            let requested = tracks.len() + probe_failures.len();
                            set_playback_status(&window, format_scan_failures_summary(requested, &probe_failures, &[]), &event_save_error);
                        }
                    }
                }
                LibraryEvent::Scanned(record) => {
                    // Same exclusion as `Probed` above: a phase-2 record for an excluded track must
                    // never reach `AppState` either, or it would resurrect the track that `Probed`
                    // already correctly skipped (`CLAUDE.md` "Library exclusions").
                    if event_library_sources.borrow().is_excluded(&record.key) {
                        continue;
                    }
                    let key = record.key.clone();
                    // The playing track's album picture as it is before this record lands, to tell
                    // below whether this scan replaced it (`AppState::artwork_revision_of`).
                    let now_playing_key = event_now_playing_key.borrow().clone();
                    let art_before = now_playing_key.as_ref().and_then(|now_playing| event_app_state.borrow().artwork_revision_of(now_playing));
                    // `apply_scanned` reports every album-key transition this scan caused — not only
                    // this record's own phase-1 `dir:`-to-tagged move (`§6` Stage 6 fix), but also any
                    // sibling's key silently shifted by group re-resolution (`CLAUDE.md` "Album
                    // grouping"): rewrite `Navigation` for each one immediately so an open
                    // album-detail page or a back-stack entry stays resolvable.
                    for (old_key, new_key) in event_app_state.borrow_mut().apply_scanned(*record) {
                        event_navigation.borrow_mut().rekey_album(&old_key, &new_key);
                    }
                    // Re-project now-playing for its own record (real tags replace the filename
                    // fallback) and when this scan replaced the picture of ITS album: a SIBLING
                    // track's higher-tier picture (e.g. `cover.jpg` in `CD2/` replacing the embedded
                    // picture `CD1/` showed) must reach the panel that is already displaying the older
                    // one (`CLAUDE.md` "Album grouping" -> "Artwork"). Never for another album's
                    // picture, nor for a playing track that is no longer in the library (removed while
                    // it plays): its projection would fall back to the filename and drop the art and
                    // lyrics the panel is showing.
                    if let Some(now_playing_key) = now_playing_key
                        && (now_playing_key == key
                            || event_app_state.borrow().artwork_revision_of(&now_playing_key) != art_before)
                        && let Some(info) = event_info.borrow().clone()
                    {
                        panel_now_playing = Some(apply_now_playing_projection(
                            &window,
                            &event_app_state.borrow(),
                            &now_playing_key,
                            &info,
                            &event_fallback.borrow(),
                        ));
                        // Real tags or a better cover for the playing track: the OS widget mirrors the panel.
                        event_media_dirty.set(true);
                    }
                    // A scanned record can belong to a track already sitting in the Queue view or
                    // any library view (or be the now-playing one, handled above): re-project so
                    // its row picks up real tags instead of the scanner's phase-1 fallback (`§3.4`).
                    // Marked dirty, not projected inline: a batch scan emits one `Scanned` per file,
                    // so this can fire many times inside one 40 ms tick.
                    queue_dirty = true;
                    event_library_dirty.set(true);
                }
                LibraryEvent::Failed { batch, path, error, phase } => {
                    let kind = event_batch_kinds.borrow().get(&batch).copied().unwrap_or(BatchKind::OpenFiles);
                    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
                    let show_inline = batch_reports_failures_inline(kind);
                    let mut failures = event_batch_failures.borrow_mut();
                    let entry = failures.entry(batch).or_default();
                    match phase {
                        // Phase 1: the file never reached the queue or the library at all.
                        ScanFailurePhase::Probe => {
                            if show_inline {
                                set_playback_status(&window, format!("Could not open {name}: {error}"), &event_save_error);
                            }
                            entry.probe.push((name, error));
                        }
                        // Phase 2: the file was already probed, enqueued and added — only its tags
                        // could not be read, so this must never be reported (or counted) as if the
                        // track itself had been refused (`§1.4`).
                        ScanFailurePhase::Metadata => {
                            if show_inline {
                                set_playback_status(&window, format!("Could not read tags for {name}: {error}"), &event_save_error);
                            }
                            entry.metadata.push((name, error));
                        }
                        // A `.cue` sheet failed to parse or resolve: like `Probe`, nothing it would
                        // have claimed ever reached the queue or the library — its files fall
                        // through to ordinary whole-file scanning instead (`library::scanner`).
                        ScanFailurePhase::Cue => {
                            if show_inline {
                                set_playback_status(&window, format!("Could not read cue sheet {name}: {error}"), &event_save_error);
                            }
                            entry.probe.push((name, error));
                        }
                    }
                }
                LibraryEvent::BatchDone { batch, requested, count } => {
                    // One fewer request in flight; reaching zero lifts the reprojection throttle
                    // below immediately, so the library always gets a final, up-to-date pass right
                    // after a scan finishes instead of waiting out the rest of the interval.
                    event_scanning_batches.set(event_scanning_batches.get().saturating_sub(1));
                    let kind = event_batch_kinds.borrow_mut().remove(&batch).unwrap_or(BatchKind::OpenFiles);
                    let had_scan_failures = event_batch_failures.borrow().get(&batch).is_some_and(|failures| !failures.is_empty());
                    match event_batch_failures.borrow_mut().remove(&batch) {
                        Some(failures) if !failures.is_empty() => {
                            let message = if kind == BatchKind::StartupRestore {
                                // One aggregate line instead of the generic per-file summary: a
                                // quiet startup must not spam "Could not open …" for every stale
                                // saved path, but a missing NAS share still needs to be visible.
                                let count = failures.probe.len();
                                let noun = if count == 1 { "track is" } else { "tracks are" };
                                format!("{count} saved {noun} unavailable (is the NAS mounted?)")
                            } else {
                                // A sticky summary instead of the ordinary route-status restore: a
                                // refused file must be reported, never silently dropped from the
                                // selection (`§1.4`, R4 "added tracks show up nowhere"). Metadata
                                // failures are reported here too (the phase-1 summary at `Probed`
                                // cannot know about them yet, since phase 2 for this batch runs
                                // afterward).
                                format_scan_failures_summary(requested, &failures.probe, &failures.metadata)
                            };
                            set_playback_status(&window, message, &event_save_error);
                        }
                        _ if kind == BatchKind::OpenFolder => {
                            // `count` is the real resulting track count (post-cue-expansion), not
                            // `requested` (the pre-expansion file count the walker found) — a cue
                            // sheet that expands one physical file into several tracks, or claims
                            // several files into fewer tracks than files, must be reported
                            // accurately (`CLAUDE.md` "Wrong track count in the Added N tracks
                            // status").
                            set_playback_status(&window, format!("Added {count} tracks"), &event_save_error);
                        }
                        // `StartupRestore` with no failures stays silent (`active_route_status` is
                        // still `None` at startup, so this is a no-op there too); "Open Files…"
                        // keeps restoring whatever route status `Started` last set.
                        _ => restore_active_route_status(&window, &event_active_route_status, &event_save_error),
                    }
                    // A file the sample-rate repair rewrote (or could not) is reported last, and
                    // only when the batch had no scan failures to show — those take the status
                    // line. The quiet startup restore still says what it rewrote on disk, but
                    // leaves repair failures to the stderr diagnostic.
                    let repair_notes = event_batch_repairs.borrow_mut().remove(&batch);
                    if !had_scan_failures
                        && let Some(message) = repair_notes.and_then(|notes| notes.summary(kind != BatchKind::StartupRestore))
                    {
                        set_playback_status(&window, message, &event_save_error);
                    }
                }
                LibraryEvent::FolderScanProgress { root, found } => {
                    set_playback_status(&window, format!("Scanning {}: {found} tracks found", root.display()), &event_save_error);
                }
                LibraryEvent::RateRepaired { batch, path, from_rate, to_rate, backup } => {
                    write_diagnostic(&format!(
                        "rate repair: converted {} from {from_rate} Hz to {to_rate} Hz (original saved at {})",
                        path.display(),
                        backup.display()
                    ));
                    event_batch_repairs.borrow_mut().entry(batch).or_default().record_repaired(file_name_of(&path), from_rate, to_rate);
                }
                LibraryEvent::RateRepairFailed { batch, path, error } => {
                    write_diagnostic(&format!("rate repair: left {} unchanged: {error}", path.display()));
                    event_batch_repairs.borrow_mut().entry(batch).or_default().record_failed(file_name_of(&path), error);
                }
            }
        }
        // One projection per tick, however many events marked it dirty above (`§3.4`).
        if queue_dirty && let Some(window) = event_window.upgrade() {
            project_and_set_queue(
                &window,
                &event_app_state.borrow(),
                event_now_playing_key.borrow().as_ref(),
                event_info.borrow().as_ref(),
                &event_fallback.borrow(),
                &event_queue_pending.borrow(),
                &event_queue_paths,
            );
        }
        // Throttled only while a scan is in flight (`scanning_batches > 0`): once it drops back to
        // zero this is unconditionally due again, guaranteeing a final, fully up-to-date pass right
        // after the scan finishes rather than leaving the last throttled interval's stale view up.
        let library_reproject_due = match event_library_last_projected.get() {
            Some(last) if event_scanning_batches.get() > 0 => last.elapsed() >= LIBRARY_REPROJECT_THROTTLE,
            _ => true,
        };
        if event_library_dirty.get() && library_reproject_due && let Some(window) = event_window.upgrade() {
            project_and_set_library(
                &window,
                &event_app_state.borrow(),
                &event_navigation.borrow(),
                event_navigation.borrow().search_query(),
                LibraryViewPaths {
                    songs: &event_songs_paths,
                    recent: &event_recent_paths,
                    album: &event_album_paths,
                    search: &event_search_paths,
                },
            );
            event_library_dirty.set(false);
            event_library_last_projected.set(Some(Instant::now()));
        }
        // The OS Now Playing widget, once per tick from the final UI state, not per event (several
        // events of one tick — `Started`, `Playing`, a `Scanned` re-projection — describe one change).
        if event_media_dirty.replace(false) && let Some(window) = event_window.upgrade() {
            let snapshot = TransportSnapshot {
                has_track: event_now_playing_key.borrow().is_some() && event_info.borrow().is_some(),
                // The play button's own rule: with no active track, the session stays (paused) for as
                // long as the button could still retry or replay something (`plan_transport_publish`).
                can_resume: window_play_button_enabled(&window),
                ui_playing: window.get_is_playing(),
                duration_ms: *event_duration.borrow(),
                last_applied_ms: event_last_timeline.borrow().0,
                pending_seek_ms: event_pending_seek.borrow().as_ref().map(|seek| seek.target_ms),
            };
            sync_media_session(&mut media_session, &mut media_published, &snapshot, panel_now_playing.as_ref());
        }
        // A rejected `Volume` echo otherwise never gets corrected: the worker only emits again on
        // a real change, so without this the slider could show a level the device never reached
        // indefinitely (`§6` Stage 4 fix).
        if let Some(level) = expire_pending_volume(
            &mut event_pending_volume.borrow_mut(),
            event_last_worker_volume.get(),
            Instant::now(),
        ) && let Some(window) = event_window.upgrade()
        {
            window.set_volume_level(level);
        }
    });

    window.run()
}

/// Sets every now-playing property from `app_state::project_now_playing` (`§5.8`, `§5.10`
/// "Now-playing projection"). Called on `Started` for whatever track started, in the library or not
/// (a queued track of an album removed from the library still starts, and then projects only the
/// filename fallback with no art); again on a `Scanned` record for the currently playing track, so
/// real tags/art/lyrics replace the filename fallback as soon as the scanner's second pass reaches
/// it; and again when a `Scanned` record of a sibling replaced the picture of the playing track's own
/// album, so the panel picks up the better one. That `Scanned` path never re-projects a playing track
/// that is no longer in the library (`AppState::artwork_revision_of` is `None` for it), since the
/// filename fallback would blank the art, artist and lyrics the panel is showing.
///
/// Returns what it put on the panel that the OS Now Playing widget mirrors (`PanelNowPlaying`); the
/// caller keeps it, so the widget shows the panel's own last projection instead of projecting again.
fn apply_now_playing_projection(
    window: &MainWindow,
    app_state: &AppState,
    key: &TrackKey,
    info: &AudioInfo,
    fallback: &(String, String),
) -> PanelNowPlaying {
    let projection = project_now_playing(app_state, key, info, fallback);
    let shown = PanelNowPlaying {
        title: projection.title.clone(),
        artist: projection.artist.clone(),
        album: projection.album.clone(),
        // The revision of the picture this projection resolved, taken at the same moment: the two
        // come from the same `shown_artwork`, so they are both present or both absent.
        artwork: app_state
            .artwork_revision_of(key)
            .zip(projection.art.clone())
            .map(|(revision, image)| NowPlayingArtwork { revision, image }),
    };
    window.set_now_playing_key(projection.key.into());
    window.set_now_playing_title(projection.title.into());
    window.set_now_playing_artist(projection.artist.into());
    window.set_now_playing_album(projection.album.into());
    window.set_now_playing_year(projection.year.into());
    window.set_now_playing_genre(projection.genre.into());
    window.set_now_playing_badge(projection.badge.into());
    window.set_format_line(projection.format_line.into());
    window.set_file_size_line(projection.file_size_line.into());
    window.set_now_playing_has_art(projection.art.is_some());
    if let Some(art) = projection.art {
        window.set_now_playing_art(art);
    }
    window.set_lyrics(projection.lyrics.into());
    window.set_has_lyrics(projection.has_lyrics);
    shown
}

/// What the now-playing panel last showed for the track (`apply_now_playing_projection`), and so what
/// the OS Now Playing widget shows. Kept as the panel's own snapshot rather than re-derived on every
/// publish: the panel deliberately keeps its tags and cover for a playing track that "Remove from
/// Library" took out of the library (and after `Stopped`/`Inactive`), where a fresh projection would
/// fall back to the file name, "Unknown Artist" and no cover and the widget would stop matching the
/// app.
struct PanelNowPlaying {
    title: String,
    artist: String,
    album: String,
    /// The cover with the `AppState::artwork_revision_of` it had when projected, so the session
    /// converts it once per picture, not per publish.
    artwork: Option<NowPlayingArtwork>,
}

/// Shows the panel's track in the OS Now Playing widget, or clears the session when there is nothing
/// to show or resume (`plan_transport_publish`; and nothing to show also means no session before a
/// first track has ever started, whatever the queue holds): a fresh, complete state every time. Title, artist,
/// album and cover are what the now-playing panel shows (`PanelNowPlaying`), so the widget cannot
/// disagree with the app; the transport half follows the UI (`TransportSnapshot`). Records what was
/// published in `published`.
fn sync_media_session(
    session: &mut MediaSession,
    published: &mut Option<PublishedTransport>,
    snapshot: &TransportSnapshot,
    panel: Option<&PanelNowPlaying>,
) {
    let (Some(plan), Some(panel)) = (plan_transport_publish(snapshot), panel) else {
        session.clear();
        *published = None;
        return;
    };
    session.publish(&NowPlayingState {
        title: panel.title.clone(),
        artist: panel.artist.clone(),
        album: panel.album.clone(),
        duration_ms: plan.duration_ms,
        elapsed_ms: plan.elapsed_ms,
        playing: plan.playing,
        artwork: panel.artwork.clone(),
    });
    *published = Some(PublishedTransport::new(&plan, Instant::now()));
}

/// Re-projects the Queue view (`§5.7`, `§3.7`) from the worker's latest pending snapshot plus
/// whatever is currently playing, and records the pending rows' paths in `queue_paths` so
/// `on_track_activated` can map a clicked row back to a `replace_queue` call. Called after every
/// event that can change either input: a fresh `QueueSnapshot`, a `Started`/`Stopped` change of
/// what is playing, or a `Scanned` record landing on a queued path.
fn project_and_set_queue(
    window: &MainWindow,
    app_state: &AppState,
    now_playing_key: Option<&TrackKey>,
    now_playing_info: Option<&AudioInfo>,
    now_playing_fallback: &(String, String),
    pending: &[QueueTrackSnapshot],
    queue_paths: &Rc<RefCell<Vec<TrackKey>>>,
) {
    let now_playing = match (now_playing_key, now_playing_info) {
        (Some(key), Some(info)) => Some((key, info, now_playing_fallback.0.as_str(), now_playing_fallback.1.as_str())),
        _ => None,
    };
    let has_current = now_playing.is_some();
    let (current, rows, paths) = project_queue_rows(now_playing, pending, &app_state.library);
    window.set_has_queue_current(has_current);
    window.set_queue_current(current);
    window.set_queue_rows(ModelRc::new(VecModel::from_iter(rows)));
    *queue_paths.borrow_mut() = paths;
}

/// The path lists behind every library-driven table's currently rendered rows, in the same order
/// as those rows, so `on_track_activated` can map a clicked row back to `library.prepared(path)`
/// (`§3.4`/`§3.7`). Bundled into one struct, not four separate parameters, so
/// `project_and_set_library` stays under clippy's `too_many_arguments` threshold now that the
/// dedicated Search view added a fourth list alongside `songs`/`recent`/`album`.
struct LibraryViewPaths<'a> {
    songs: &'a Rc<RefCell<Vec<TrackKey>>>,
    recent: &'a Rc<RefCell<Vec<TrackKey>>>,
    album: &'a Rc<RefCell<Vec<TrackKey>>>,
    search: &'a Rc<RefCell<Vec<TrackKey>>>,
}

/// Re-projects every library-driven view (Home, Albums, Artists, Songs, Recently Added, album
/// detail and the dedicated Search view) from `app_state`, `navigation`'s current artist
/// filter/album key/search query, and `query` (`§6` Stage 6, `view_model::project_library`), and
/// records each table's visible-path list so `on_track_activated` can map a clicked row back to a
/// path. Called once at startup, immediately after every navigation mutation that can change
/// `artist_filter`/`album_key`/the search query, and once per 40 ms tick while `library_dirty` is
/// set (search edits and scanner events, `§3.4`).
fn project_and_set_library(window: &MainWindow, app_state: &AppState, navigation: &Navigation, query: &str, paths: LibraryViewPaths) {
    let projection = project_library(app_state, query, navigation.artist_filter(), navigation.album_key());
    window.set_library_empty(projection.library_empty);
    window.set_jump_back_albums(ModelRc::new(VecModel::from_iter(projection.jump_back_albums)));
    window.set_recent_albums(ModelRc::new(VecModel::from_iter(projection.recent_albums)));
    window.set_albums(ModelRc::new(VecModel::from_iter(projection.albums)));
    window.set_artists(ModelRc::new(VecModel::from_iter(projection.artists)));
    window.set_songs(ModelRc::new(VecModel::from_iter(projection.songs)));
    window.set_recent_songs(ModelRc::new(VecModel::from_iter(projection.recent_songs)));
    window.set_songs_summary(projection.songs_summary.into());
    window.set_recent_summary(projection.recent_summary.into());
    window.set_album_header(projection.album_header);
    window.set_album_tracks(ModelRc::new(VecModel::from_iter(projection.album_tracks)));
    window.set_search_songs(ModelRc::new(VecModel::from_iter(projection.search.songs)));
    window.set_search_songs_header(projection.search.songs_header.into());
    window.set_search_albums(ModelRc::new(VecModel::from_iter(projection.search.albums)));
    window.set_search_albums_header(projection.search.albums_header.into());
    window.set_search_artists_header(projection.search.artists_header.into());
    window.set_search_has_results(projection.search.has_results);
    *paths.songs.borrow_mut() = projection.songs_paths;
    *paths.recent.borrow_mut() = projection.recent_paths;
    *paths.album.borrow_mut() = projection.album_paths;
    *paths.search.borrow_mut() = projection.search.songs_paths;
}

/// `TrackRecord` -> `PreparedTrack`, for the album/library-wide actions (`§3.7` Album Play/Shuffle/
/// Add to Queue/Play Next, Home "Shuffle all") that already hold `&TrackRecord`s from
/// `Library::album_tracks`/`songs` instead of paths to look up through `Library::prepared`.
fn prepared_track(record: &TrackRecord) -> PreparedTrack {
    PreparedTrack { key: record.key.clone(), info: record.info.clone() }
}

/// A fresh shuffle seed from the wall clock (`§3.7` "seed from SystemTime nanos"), so Shuffle/
/// "Shuffle all" does not replay the same order every time it is pressed.
fn shuffle_seed() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_nanos() as u64).unwrap_or(0)
}

/// A `search-edited` from the top bar: stores the query, pushes `search-active` to the window at once
/// (the Search view swaps in and out on every keystroke) and forgets the row selection when the edit
/// changes the list on screen — a new query is a new list. An edit that leaves the effective query as
/// it was (`Navigation::effective_search_query`: Esc in an already-empty field, a lone Space typed
/// into it, a trailing space, another letter case) shows the same table, so the row the user clicked
/// keeps its highlight and Space keeps its target; Return, which never edits, already behaved that way.
fn apply_search_edit(window: &MainWindow, navigation: &mut Navigation, query: String) {
    let before = navigation.effective_search_query();
    navigation.set_search_query(query);
    window.set_search_active(navigation.search_active());
    if navigation.effective_search_query() != before {
        clear_track_selection(window);
    }
}

/// Applies the `§3.6` navigation state to the handful of Slint properties it drives, and forgets the
/// row selection (`clear_track_selection`). Called after every `Navigation` mutation, including the
/// ones that clear the search query (`nav_selected`, `album_opened`, `artist_opened`, `§3.4`
/// "Search") — pushing `search-query`/`search-active` here too keeps the search box and the dedicated
/// Search view in lockstep with `Navigation` itself, instead of `main.rs` having to remember to clear
/// them separately at each call site.
fn sync_navigation(window: &MainWindow, navigation: &Navigation) {
    // A navigation leaves the page a row was clicked on: its selection goes with it. (A re-projection
    // of the same page does not come through here, and keeps it.)
    clear_track_selection(window);
    window.set_view(navigation.view());
    window.set_nav_view(navigation.nav_view());
    window.set_can_go_back(navigation.can_go_back());
    window.set_artist_filter(navigation.artist_filter().unwrap_or_default().into());
    window.set_unavailable_title(unavailable_label(navigation.view()).into());
    window.set_search_query(navigation.search_query().into());
    window.set_search_active(navigation.search_active());
}

fn set_playback_status(
    window: &MainWindow,
    status: impl AsRef<str>,
    settings_save_error: &Rc<RefCell<Option<String>>>,
) {
    let status = status.as_ref();
    let visible = match settings_save_error.borrow().as_deref() {
        Some(error) => format!("{error} · {status}"),
        None => status.to_owned(),
    };
    window.set_playback_status(visible.into());
}

/// Re-applies the last route status `Started` set, but only while a track is still active
/// (`active_route_status` holds `None` once `Stopped` clears it). Called after a scan batch so
/// "Reading {n} files…" never strands the status line indefinitely when opening more files during
/// playback (`§6` Stage 3 fix).
fn restore_active_route_status(
    window: &MainWindow,
    active_route_status: &Rc<RefCell<Option<String>>>,
    settings_save_error: &Rc<RefCell<Option<String>>>,
) {
    if let Some(status) = active_route_status.borrow().clone() {
        set_playback_status(window, status, settings_save_error);
    }
}

fn set_device_model(window: &MainWindow, devices: &[OutputDevice]) {
    let mut names = vec![SharedString::from("Select output device…")];
    names.extend(devices.iter().map(|device| SharedString::from(device.label.as_str())));
    window.set_output_device_options(ModelRc::new(VecModel::from_iter(names)));
}

async fn pick_audio_files(parent_window: slint::WindowHandle) -> Option<Vec<PathBuf>> {
    // `.cue` is not itself a playable audio format (`walker::AUDIO_EXTENSIONS`), but a directly
    // opened cue sheet must trigger the same expansion its folder gets from a folder scan
    // (`library::scanner`'s cue detection), so the picker must allow selecting one.
    let mut extensions: Vec<&str> = AUDIO_EXTENSIONS.to_vec();
    extensions.push(library::walker::CUE_EXTENSION);
    rfd::AsyncFileDialog::new()
        .set_parent(&parent_window)
        .add_filter("Audio files", &extensions)
        .pick_files()
        .await
        .map(|files| {
            files
                .into_iter()
                .map(|file| file.path().to_path_buf())
                .collect()
        })
}

/// "Open Folder…": RFD's async folder picker (multiple selection allowed), scheduled on Slint's
/// event loop exactly like `pick_audio_files` above — never a synchronous `runModal()` inside a
/// callback (`CLAUDE.md`). Each chosen folder is walked recursively by `library::walker`
/// (`LibraryScanner::scan_folder`), off the UI thread, not here.
async fn pick_audio_folders(parent_window: slint::WindowHandle) -> Option<Vec<PathBuf>> {
    rfd::AsyncFileDialog::new()
        .set_parent(&parent_window)
        .pick_folders()
        .await
        .map(|folders| {
            folders
                .into_iter()
                .map(|folder| folder.path().to_path_buf())
                .collect()
        })
}

/// Status line shown while "Open Folder…" starts scanning `folders` (never empty).
fn folder_scan_status(folders: &[PathBuf]) -> String {
    match folders {
        [folder] => format!("Scanning {}…", folder.display()),
        _ => format!("Scanning {} folders…", folders.len()),
    }
}

/// Recomputes the elapsed/duration/progress strings from `last`, the last `Timeline` the worker
/// actually applied to the UI. Used on `SeekRejected` so a refusal restores the truthful
/// position instead of leaving the optimistic seek target on the bar (`§1.4`).
fn timeline_after_seek_rejected(last: (u64, Option<u64>)) -> (String, String, f32) {
    let (position_ms, duration_ms) = last;
    (
        format_clock(position_ms),
        duration_ms.map(format_clock).unwrap_or_else(|| "—:—".into()),
        playback_progress(position_ms, duration_ms),
    )
}

#[cfg(test)]
mod timeline_tests {
    use super::*;

    #[test]
    fn accept_timeline_during_pending_seek() {
        let pending = PendingSeek {
            key: TrackKey::whole_file(PathBuf::from("/nas/album/track.flac")),
            target_ms: 30_000,
            requested_at: Instant::now(),
        };

        assert!(accept_timeline(&pending, 30_000, pending.requested_at), "an exact match is accepted");
        assert!(
            accept_timeline(&pending, 31_500, pending.requested_at),
            "within the 1.5 s tolerance is accepted"
        );
        assert!(
            !accept_timeline(&pending, 32_000, pending.requested_at),
            "outside the tolerance, before the timeout, is held back"
        );
        assert!(
            accept_timeline(&pending, 0, pending.requested_at + Duration::from_secs(3)),
            "far outside the tolerance is still accepted once the 3 s timeout elapses"
        );
    }

    #[test]
    fn timeline_after_seek_rejected_restores_the_last_applied_position() {
        assert_eq!(
            timeline_after_seek_rejected((30_000, Some(120_000))),
            ("0:30".to_string(), "2:00".to_string(), 0.25)
        );
        assert_eq!(
            timeline_after_seek_rejected((0, None)),
            ("0:00".to_string(), "—:—".to_string(), 0.0)
        );
    }

    #[test]
    fn seek_ui_target_clamps_to_the_duration_and_reports_the_matching_progress() {
        assert_eq!(seek_ui_target(30_000, 120_000), (30_000, 0.25));
        assert_eq!(seek_ui_target(0, 120_000), (0, 0.0));
        assert_eq!(
            seek_ui_target(u64::MAX, 120_000),
            (120_000, 1.0),
            "a Control Center scrub past the end must not leave the bar or the pending target beyond the track"
        );
    }

    #[test]
    fn seek_rejected_clears_pending_seek() {
        let mut pending = Some(PendingSeek {
            key: TrackKey::whole_file(PathBuf::from("/nas/album/track.flac")),
            target_ms: 5_000,
            requested_at: Instant::now(),
        });

        clear_pending_seek(&mut pending);

        assert!(pending.is_none());
    }
}

#[cfg(test)]
mod space_tests {
    use super::*;

    fn model_of(keys: &[&str]) -> slint::VecModel<TrackRowData> {
        slint::VecModel::from(keys.iter().map(|key| TrackRowData { key: (*key).into(), ..Default::default() }).collect::<Vec<_>>())
    }

    #[test]
    fn the_selection_is_looked_up_by_track_key_in_whatever_rows_are_shown_now() {
        let before = model_of(&["a", "b", "c"]);
        assert_eq!(row_of_key(&before, "a"), Some(0));
        // The library was re-projected with two tracks ahead of it: same key, another position.
        let after = model_of(&["x", "y", "a", "b"]);
        assert_eq!(row_of_key(&after, "a"), Some(2), "the key follows its track to its new row");
        // And the track is gone: nothing takes over the old position.
        let without = model_of(&["x", "y", "b"]);
        assert_eq!(row_of_key(&without, "a"), None, "a key that left the list resolves to nothing, not to row 0");
    }

    #[test]
    fn no_selection_and_unknown_keys_resolve_to_no_row() {
        let rows = model_of(&["a", "", "b"]);
        assert_eq!(row_of_key(&rows, ""), None, "an empty selected key means nothing is selected, even next to a row with an empty key");
        assert_eq!(row_of_key(&rows, "zzz"), None);
        assert_eq!(row_of_key(&model_of(&[]), "a"), None);
    }

    #[test]
    fn the_visible_track_table_follows_the_view_and_the_search() {
        assert_eq!(visible_track_list(View::Songs, false), Some(TrackListKind::Songs));
        assert_eq!(visible_track_list(View::RecentlyAdded, false), Some(TrackListKind::RecentlyAdded));
        assert_eq!(visible_track_list(View::AlbumDetail, false), Some(TrackListKind::Album));
        assert_eq!(visible_track_list(View::Queue, false), Some(TrackListKind::Queue));
        for view in [View::Home, View::Albums, View::Artists, View::Browser, View::Liked] {
            assert_eq!(visible_track_list(view, false), None, "{view:?} has no track table");
        }
        // The Search view covers whichever view is underneath, tables included.
        for view in [View::Songs, View::Home, View::Queue, View::AlbumDetail] {
            assert_eq!(visible_track_list(view, true), Some(TrackListKind::Search));
        }
    }

    #[test]
    fn space_is_the_play_button_while_a_track_is_loaded_or_queued() {
        let selection = Some((TrackListKind::Songs, 2));
        assert_eq!(plan_space_action(true, selection), SpaceAction::TogglePlayback, "a loaded track wins over a highlighted row");
        assert_eq!(plan_space_action(true, None), SpaceAction::TogglePlayback);
    }

    #[test]
    fn space_plays_the_selected_row_only_when_nothing_is_loaded() {
        assert_eq!(
            plan_space_action(false, Some((TrackListKind::Album, 4))),
            SpaceAction::ActivateSelection { kind: TrackListKind::Album, index: 4 }
        );
        assert_eq!(
            plan_space_action(false, Some((TrackListKind::Search, 0))),
            SpaceAction::ActivateSelection { kind: TrackListKind::Search, index: 0 },
            "row 0 is a real row"
        );
    }

    #[test]
    fn space_with_nothing_loaded_and_nothing_selected_falls_back_to_the_play_button() {
        // The controller answers with "Open an audio file to start playback."
        assert_eq!(plan_space_action(false, None), SpaceAction::TogglePlayback);
    }
}

#[cfg(test)]
mod media_publish_tests {
    /// The OS Now Playing widget must show what the panel shows. The panel deliberately keeps the tags
    /// and cover of a playing track that left the library (and after `Stopped`), where a fresh
    /// projection would be the file-name fallback with no cover, so a publish that projects again
    /// makes the widget disagree with the app. `sync_media_session` only ever reads the panel's own
    /// snapshot (`PanelNowPlaying`, returned by `apply_now_playing_projection`).
    #[test]
    fn the_media_session_publishes_the_panels_snapshot_and_never_projects_again() {
        let source = include_str!("main.rs");
        let start = source.find("fn sync_media_session(").expect("main.rs defines sync_media_session");
        let body = &source[start..];
        let body = &body[..body.find("\n}\n").expect("sync_media_session ends at a closing brace in column 0")];
        assert!(
            !body.contains("project_now_playing") && !body.contains("artwork_revision_of") && !body.contains("AppState"),
            "sync_media_session must publish the panel's snapshot, not derive metadata from the library again:\n{body}"
        );
        for field in ["panel.title.clone()", "panel.artist.clone()", "panel.album.clone()", "panel.artwork.clone()"] {
            assert!(body.contains(field), "the widget's `{field}` must come from the panel's snapshot");
        }
    }
}

#[cfg(test)]
mod output_selection_tests {
    use super::*;

    #[test]
    fn reselecting_the_same_saved_device_is_a_noop() {
        assert!(is_output_reselection_noop(Some("coreaudio:builtin-output"), Some("coreaudio:builtin-output")));
    }

    #[test]
    fn reselecting_not_selected_when_already_unselected_is_a_noop() {
        assert!(is_output_reselection_noop(None, None));
    }

    #[test]
    fn choosing_a_different_device_is_not_a_noop() {
        assert!(!is_output_reselection_noop(Some("coreaudio:other-output"), Some("coreaudio:builtin-output")));
    }

    #[test]
    fn choosing_not_selected_when_a_device_was_saved_is_not_a_noop() {
        assert!(!is_output_reselection_noop(None, Some("coreaudio:builtin-output")));
    }

    #[test]
    fn choosing_a_device_when_none_was_saved_is_not_a_noop() {
        assert!(!is_output_reselection_noop(Some("coreaudio:builtin-output"), None));
    }
}

#[cfg(test)]
mod batch_kind_tests {
    use super::*;

    /// The core contract behind "Open Folder…"/startup restore never enqueuing or playing
    /// anything (`CLAUDE.md` "Persistent library"): only `BatchKind::OpenFiles` does.
    #[test]
    fn only_open_files_enqueues() {
        assert!(batch_should_enqueue(BatchKind::OpenFiles));
        assert!(!batch_should_enqueue(BatchKind::OpenFolder));
        assert!(!batch_should_enqueue(BatchKind::StartupRestore));
    }

    #[test]
    fn folder_scan_status_names_a_single_folder_and_counts_several() {
        assert_eq!(folder_scan_status(&[PathBuf::from("/music/Album")]), "Scanning /music/Album…");
        assert_eq!(
            folder_scan_status(&[PathBuf::from("/music/A"), PathBuf::from("/music/B")]),
            "Scanning 2 folders…"
        );
    }

    #[test]
    fn only_startup_restore_suppresses_inline_failure_status() {
        assert!(batch_reports_failures_inline(BatchKind::OpenFiles));
        assert!(batch_reports_failures_inline(BatchKind::OpenFolder));
        assert!(!batch_reports_failures_inline(BatchKind::StartupRestore));
    }

    fn prepared(path: &str) -> PreparedTrack {
        PreparedTrack {
            key: TrackKey::whole_file(PathBuf::from(path)),
            info: AudioInfo {
                sample_rate: 44_100,
                duration_ms: Some(60_000),
                source_channels: 2,
                bits_per_sample: 16,
                is_float: false,
                integer_pcm: true,
                format: "FLAC".into(),
            },
        }
    }

    /// "Remove from Library" (`CLAUDE.md` "Library exclusions"): a rescan (chiefly the startup
    /// restore) must never bring an excluded track back — `exclude_removed_tracks` is exactly the
    /// filter `LibraryEvent::Probed` runs through before a batch can be enqueued or added to the
    /// library, so this doubles as the "rescanning a folder respects exclusions" contract.
    #[test]
    fn exclude_removed_tracks_drops_only_excluded_keys_and_keeps_order() {
        let mut sources = LibrarySources::default();
        sources.exclude_tracks([TrackKey::whole_file(PathBuf::from("/music/removed.flac"))]);
        let tracks = vec![prepared("/music/kept-a.flac"), prepared("/music/removed.flac"), prepared("/music/kept-b.flac")];

        let filtered = exclude_removed_tracks(tracks, &sources);

        assert_eq!(
            filtered.into_iter().map(|track| track.key.path).collect::<Vec<_>>(),
            vec![PathBuf::from("/music/kept-a.flac"), PathBuf::from("/music/kept-b.flac")]
        );
    }

    #[test]
    fn exclude_removed_tracks_is_a_no_op_with_no_exclusions() {
        let sources = LibrarySources::default();
        let tracks = vec![prepared("/music/a.flac"), prepared("/music/b.flac")];

        let filtered = exclude_removed_tracks(tracks.clone(), &sources);

        assert_eq!(filtered.len(), tracks.len());
    }
}

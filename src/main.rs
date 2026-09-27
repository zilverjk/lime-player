mod app_state;
mod audio;
mod library;
mod settings;
mod view_model;
#[cfg(test)]
mod ui_snapshot;

slint::include_modules!();

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use app_state::{
    AppState, Navigation, PendingVolume, expire_pending_volume, project_now_playing, resolve_volume_event, should_send_volume,
    unavailable_label,
};
use audio::{AudioInfo, AudioPlayer, OutputDevice, PlaybackEvent, PlayerSettings, PreparedTrack, QueueTrackSnapshot, enumerate_outputs};
use library::TrackRecord;
use library::format::{format_clock, format_scan_failures_summary, playback_progress};
use library::scanner::{LibraryEvent, LibraryScanner, ScanFailurePhase};
use library::store::LibrarySources;
use library::walker::AUDIO_EXTENSIONS;
use settings::Preferences;
use slint::{ModelRc, SharedString, Timer, TimerMode, VecModel};
use view_model::{perform_album_action, prepared_suffix, project_library, project_queue_rows};

/// UI-thread state for an in-flight seek (`§5.10`). `app_state.rs`/`AppState` exists as of Stage 3
/// (the session library and its artwork cache), but pending-seek/volume state stays here until
/// Stage 4/5 actually needs to share it with more than `main.rs`.
struct PendingSeek {
    // Not yet consulted by `accept_timeline` (`Timeline` events carry no path to compare
    // against); kept because `Started`/a future per-track staleness check will need it once
    // this moves into `app_state.rs` (`§5.10`).
    #[allow(dead_code)]
    path: PathBuf,
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
    let library_scanner = Arc::new(LibraryScanner::new());
    let app_state = Rc::new(RefCell::new(AppState::new()));
    // Rust-owned navigation (`§3.6`, Stage 5): current view, sidebar highlight and back stack.
    let navigation = Rc::new(RefCell::new(Navigation::new()));
    // The now-playing path/duration come from the last `Started`/`Timeline` events (`§6` Stage 2);
    // `on_seek_requested` needs both to turn a fraction into a `player.seek(path, ms)` call.
    let now_playing_path = Rc::new(RefCell::new(None::<PathBuf>));
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
    // What every in-flight (or not-yet-reported) scan batch is for, keyed by the id `scan`/
    // `scan_folder` returned synchronously at call time (`BatchKind`). Never guessed from an
    // event: `Probed`/`Failed`/`BatchDone` only carry the batch id, not why it was requested.
    let batch_kinds = Rc::new(RefCell::new(std::collections::HashMap::<u64, BatchKind>::new()));
    // The worker's latest pending-queue snapshot and the paths behind the Queue view's rendered
    // rows, in the same order (`§3.4`, `§3.7`). `on_track_activated` maps a clicked row back to a
    // path through the latter; both are recomputed together by `project_and_set_queue`.
    let queue_pending = Rc::new(RefCell::new(Vec::<QueueTrackSnapshot>::new()));
    let queue_paths = Rc::new(RefCell::new(Vec::<PathBuf>::new()));
    // The current search box text (`§3.4` "search-edited... stores the query and marks the library
    // dirty"). Read by every library projection alongside `Navigation`'s artist filter/album key.
    let search_query = Rc::new(RefCell::new(String::new()));
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
    let songs_paths = Rc::new(RefCell::new(Vec::<PathBuf>::new()));
    let recent_paths = Rc::new(RefCell::new(Vec::<PathBuf>::new()));
    let album_paths = Rc::new(RefCell::new(Vec::<PathBuf>::new()));

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
    project_and_set_library(&window, &app_state.borrow(), &navigation.borrow(), &search_query.borrow(), &songs_paths, &recent_paths, &album_paths);
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
                let batch = scanner.scan(paths.clone());
                batch_kinds.borrow_mut().insert(batch, BatchKind::OpenFiles);
                // Persisted so these files are re-scanned (never re-enqueued) at the next startup
                // (`CLAUDE.md` "Persistent library"). A save failure is reported but never blocks
                // opening/playing the files that were just selected.
                let mut sources = sources.borrow_mut();
                for path in &paths {
                    sources.add_file(path.clone());
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
        let selection = pick_audio_folder(parent_window);
        let status_window = open_folder_window.clone();
        let status_save_error = Rc::clone(&open_folder_save_error);
        let scanning_batches = Rc::clone(&open_folder_scanning_batches);
        let batch_kinds = Rc::clone(&open_folder_batch_kinds);
        let sources = Rc::clone(&open_folder_sources);
        if let Err(error) = slint::spawn_local(async move {
            if let Some(folder) = selection.await {
                if let Some(window) = status_window.upgrade() {
                    set_playback_status(&window, format!("Scanning {}…", folder.display()), &status_save_error);
                }
                scanning_batches.set(scanning_batches.get() + 1);
                let batch = scanner.scan_folder(folder.clone());
                batch_kinds.borrow_mut().insert(batch, BatchKind::OpenFolder);
                // Added to the library only (`batch_should_enqueue(BatchKind::OpenFolder)` is
                // false) and persisted so it is re-walked at every startup, picking up files added
                // to it since (`CLAUDE.md` "Persistent library").
                let mut sources = sources.borrow_mut();
                sources.add_folder(folder);
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
            let batch = library_scanner.scan(sources.files.clone());
            batch_kinds.borrow_mut().insert(batch, BatchKind::StartupRestore);
            scanning_batches.set(scanning_batches.get() + 1);
        }
        for folder in &sources.folders {
            let batch = library_scanner.scan_folder(folder.clone());
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

    let player_for_seek = Arc::clone(&audio_player);
    let seek_window = window.as_weak();
    let seek_now_playing_path = Rc::clone(&now_playing_path);
    let seek_duration = Rc::clone(&now_playing_duration_ms);
    let seek_pending = Rc::clone(&pending_seek);
    window.on_seek_requested(move |fraction| {
        let Some(window) = seek_window.upgrade() else { return; };
        if !window.get_can_seek() {
            return;
        }
        let Some(path) = seek_now_playing_path.borrow().clone() else { return; };
        let Some(duration_ms) = *seek_duration.borrow() else { return; };
        let target_ms = (f64::from(fraction) * duration_ms as f64).round() as u64;
        // Update immediately (optimistic UI); the worker's next Timeline confirms or the
        // pending-seek tolerance window (`accept_timeline`) holds the value until it does.
        window.set_playback_progress(fraction);
        window.set_playback_elapsed(format_clock(target_ms).into());
        *seek_pending.borrow_mut() = Some(PendingSeek { path: path.clone(), target_ms, requested_at: Instant::now() });
        player_for_seek.seek(path, target_ms);
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
    let nav_selected_search_query = Rc::clone(&search_query);
    let nav_selected_songs_paths = Rc::clone(&songs_paths);
    let nav_selected_recent_paths = Rc::clone(&recent_paths);
    let nav_selected_album_paths = Rc::clone(&album_paths);
    window.on_nav_selected(move |view| {
        navigation_for_nav_selected.borrow_mut().nav_selected(view);
        if let Some(window) = nav_selected_window.upgrade() {
            sync_navigation(&window, &navigation_for_nav_selected.borrow());
            project_and_set_library(
                &window,
                &nav_selected_app_state.borrow(),
                &navigation_for_nav_selected.borrow(),
                &nav_selected_search_query.borrow(),
                &nav_selected_songs_paths,
                &nav_selected_recent_paths,
                &nav_selected_album_paths,
            );
        }
    });

    let navigation_for_back = Rc::clone(&navigation);
    let back_window = window.as_weak();
    let back_app_state = Rc::clone(&app_state);
    let back_search_query = Rc::clone(&search_query);
    let back_songs_paths = Rc::clone(&songs_paths);
    let back_recent_paths = Rc::clone(&recent_paths);
    let back_album_paths = Rc::clone(&album_paths);
    window.on_back_requested(move || {
        navigation_for_back.borrow_mut().back_requested();
        if let Some(window) = back_window.upgrade() {
            sync_navigation(&window, &navigation_for_back.borrow());
            project_and_set_library(
                &window,
                &back_app_state.borrow(),
                &navigation_for_back.borrow(),
                &back_search_query.borrow(),
                &back_songs_paths,
                &back_recent_paths,
                &back_album_paths,
            );
        }
    });

    let navigation_for_album_opened = Rc::clone(&navigation);
    let album_opened_window = window.as_weak();
    let album_opened_app_state = Rc::clone(&app_state);
    let album_opened_search_query = Rc::clone(&search_query);
    let album_opened_songs_paths = Rc::clone(&songs_paths);
    let album_opened_recent_paths = Rc::clone(&recent_paths);
    let album_opened_album_paths = Rc::clone(&album_paths);
    window.on_album_opened(move |key| {
        navigation_for_album_opened.borrow_mut().album_opened(&key);
        if let Some(window) = album_opened_window.upgrade() {
            sync_navigation(&window, &navigation_for_album_opened.borrow());
            project_and_set_library(
                &window,
                &album_opened_app_state.borrow(),
                &navigation_for_album_opened.borrow(),
                &album_opened_search_query.borrow(),
                &album_opened_songs_paths,
                &album_opened_recent_paths,
                &album_opened_album_paths,
            );
        }
    });

    let navigation_for_artist_opened = Rc::clone(&navigation);
    let artist_opened_window = window.as_weak();
    let artist_opened_app_state = Rc::clone(&app_state);
    let artist_opened_search_query = Rc::clone(&search_query);
    let artist_opened_songs_paths = Rc::clone(&songs_paths);
    let artist_opened_recent_paths = Rc::clone(&recent_paths);
    let artist_opened_album_paths = Rc::clone(&album_paths);
    window.on_artist_opened(move |name| {
        navigation_for_artist_opened.borrow_mut().artist_opened(&name);
        if let Some(window) = artist_opened_window.upgrade() {
            sync_navigation(&window, &navigation_for_artist_opened.borrow());
            project_and_set_library(
                &window,
                &artist_opened_app_state.borrow(),
                &navigation_for_artist_opened.borrow(),
                &artist_opened_search_query.borrow(),
                &artist_opened_songs_paths,
                &artist_opened_recent_paths,
                &artist_opened_album_paths,
            );
        }
    });

    // The player-bar artwork click (`§5.6` item 1): same as `album-opened`, with the now-playing
    // path's own album key when the library knows one (`§3.6`).
    let navigation_for_now_playing_album = Rc::clone(&navigation);
    let now_playing_album_window = window.as_weak();
    let now_playing_album_app_state = Rc::clone(&app_state);
    let now_playing_album_path = Rc::clone(&now_playing_path);
    let now_playing_album_search_query = Rc::clone(&search_query);
    let now_playing_album_songs_paths = Rc::clone(&songs_paths);
    let now_playing_album_recent_paths = Rc::clone(&recent_paths);
    let now_playing_album_album_paths = Rc::clone(&album_paths);
    window.on_now_playing_album_requested(move || {
        let key = now_playing_album_path
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
                &now_playing_album_search_query.borrow(),
                &now_playing_album_songs_paths,
                &now_playing_album_recent_paths,
                &now_playing_album_album_paths,
            );
        }
    });

    // `search-edited` (`§3.4`): stores the query and marks the library dirty; the 40 ms timer
    // reprojects at most once per tick, batching fast typing the same way a scan batch is batched.
    let search_query_for_edit = Rc::clone(&search_query);
    let library_dirty_for_search = Rc::clone(&library_dirty);
    window.on_search_edited(move |query| {
        *search_query_for_edit.borrow_mut() = query.to_string();
        library_dirty_for_search.set(true);
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

    // `track-activated` (`§3.7`): every kind maps a clicked row to the suffix of prepared tracks
    // starting there, through whichever path list backs that table (`songs_paths`/`recent_paths`/
    // `album_paths`/`queue_paths`), then replaces the queue with it.
    let player_for_track_activated = Arc::clone(&audio_player);
    let app_state_for_track_activated = Rc::clone(&app_state);
    let queue_paths_for_activated = Rc::clone(&queue_paths);
    let songs_paths_for_activated = Rc::clone(&songs_paths);
    let recent_paths_for_activated = Rc::clone(&recent_paths);
    let album_paths_for_activated = Rc::clone(&album_paths);
    window.on_track_activated(move |kind, index| {
        let Ok(index) = usize::try_from(index) else { return; };
        let paths = match kind {
            TrackListKind::Queue => &queue_paths_for_activated,
            TrackListKind::Songs => &songs_paths_for_activated,
            TrackListKind::RecentlyAdded => &recent_paths_for_activated,
            TrackListKind::Album => &album_paths_for_activated,
        };
        let tracks = prepared_suffix(&app_state_for_track_activated.borrow().library, &paths.borrow(), index);
        // The UI never sends an empty `ReplaceQueue`: the worker would just ignore it, but a path
        // that has left the library between render and click should not send a stop for nothing.
        if !tracks.is_empty() {
            player_for_track_activated.replace_queue(tracks);
        }
    });

    let events = audio_player.events();
    let library_events = library_scanner.events();
    let event_window = window.as_weak();
    let event_devices = Rc::clone(&output_devices);
    let event_preferences = Rc::clone(&preferences);
    let event_save_error = Rc::clone(&settings_save_error);
    let event_now_playing_path = Rc::clone(&now_playing_path);
    let event_duration = Rc::clone(&now_playing_duration_ms);
    let event_fallback = Rc::clone(&now_playing_fallback);
    let event_info = Rc::clone(&now_playing_info);
    let event_pending_seek = Rc::clone(&pending_seek);
    let event_last_timeline = Rc::clone(&last_timeline);
    let event_pending_volume = Rc::clone(&pending_volume);
    let event_last_worker_volume = Rc::clone(&last_worker_volume);
    let event_active_route_status = Rc::clone(&active_route_status);
    let event_batch_failures = Rc::clone(&batch_failures);
    let event_batch_kinds = Rc::clone(&batch_kinds);
    let event_scanning_batches = Rc::clone(&scanning_batches);
    let event_library_last_projected = Rc::clone(&library_last_projected_at);
    let event_app_state = Rc::clone(&app_state);
    let event_player_for_library = Arc::clone(&audio_player);
    let event_queue_pending = Rc::clone(&queue_pending);
    let event_queue_paths = Rc::clone(&queue_paths);
    let event_navigation = Rc::clone(&navigation);
    let event_search_query = Rc::clone(&search_query);
    let event_library_dirty = Rc::clone(&library_dirty);
    let event_songs_paths = Rc::clone(&songs_paths);
    let event_recent_paths = Rc::clone(&recent_paths);
    let event_album_paths = Rc::clone(&album_paths);
    let _event_timer = Timer::default();
    _event_timer.start(TimerMode::Repeated, Duration::from_millis(40), move || {
        // `§3.4`: "projections run once per dirty flag, so large scans are batched." Every event
        // below that can change the Queue view's rows only marks this, instead of calling
        // `project_and_set_queue` inline; a batch of N `Scanned` events used to rebuild the queue N
        // times (a `TrackRowData` plus a fresh `VecModel` per rebuild) against a queue of up to N
        // pending tracks, an O(n^2) cost on the UI thread for one open of N files.
        let mut queue_dirty = false;
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
                    path,
                    info,
                    title,
                    album,
                    artist: _artist,
                    output,
                    hog_mode,
                    integer_output_verified,
                } => {
                    *event_now_playing_path.borrow_mut() = Some(path.clone());
                    *event_fallback.borrow_mut() = (title, album);
                    *event_info.borrow_mut() = Some(info.clone());
                    clear_pending_seek(&mut event_pending_seek.borrow_mut());
                    // A single `borrow_mut()` call, not a split lookup-then-mutate: edition 2024's
                    // `if let` rescoping keeps a `Ref` from the scrutinee alive through the whole
                    // then-block, so `event_app_state.borrow().library.get(...)` followed by
                    // `event_app_state.borrow_mut()` in the same block panicked with "already
                    // borrowed" on every `Started` (`§6` Stage 5 fix).
                    event_app_state.borrow_mut().note_played_path(&path);
                    apply_now_playing_projection(&window, &event_app_state.borrow(), &path, &info, &event_fallback.borrow());
                    queue_dirty = true;
                    // `note_played_path` may have pushed a new album to the front of Home's "Jump
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
                    }
                    window.set_can_seek(duration_ms.is_some_and(|duration_ms| duration_ms > 0));
                    window.set_duration_ms(duration_ms.unwrap_or(0) as i32);
                }
                PlaybackEvent::Playing(playing) => window.set_is_playing(playing),
                PlaybackEvent::SeekRejected => {
                    clear_pending_seek(&mut event_pending_seek.borrow_mut());
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
                    window.set_is_playing(false);
                    // No track is active once Stopped is emitted, so seeking must not stay
                    // enabled on a bar with nothing to seek (`§1.4`).
                    window.set_can_seek(false);
                    *event_active_route_status.borrow_mut() = None;
                    // Nothing is active anymore (unlike a pause, which only sends `Playing(false)`):
                    // the Queue view's "Now Playing" section must not keep showing a stopped track.
                    *event_now_playing_path.borrow_mut() = None;
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
                    window.set_is_playing(false);
                    window.set_can_seek(false);
                    *event_active_route_status.borrow_mut() = None;
                    *event_now_playing_path.borrow_mut() = None;
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
        while let Ok(event) = library_events.try_recv() {
            let Some(window) = event_window.upgrade() else { return; };
            match event {
                LibraryEvent::Probed { batch, tracks } => {
                    let kind = event_batch_kinds.borrow().get(&batch).copied().unwrap_or(BatchKind::OpenFiles);
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
                    let path = record.path.clone();
                    // `apply_scanned` reports a rekey when phase 2's tags moved this path off its
                    // phase-1 `dir:` album key (`§6` Stage 6 fix): rewrite `Navigation` immediately
                    // so an open album-detail page or a back-stack entry stays resolvable.
                    if let Some((old_key, new_key)) = event_app_state.borrow_mut().apply_scanned(*record) {
                        event_navigation.borrow_mut().rekey_album(&old_key, &new_key);
                    }
                    let is_now_playing = event_now_playing_path.borrow().as_deref() == Some(path.as_path());
                    if is_now_playing
                        && let Some(info) = event_info.borrow().clone()
                    {
                        apply_now_playing_projection(&window, &event_app_state.borrow(), &path, &info, &event_fallback.borrow());
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
                    }
                }
                LibraryEvent::BatchDone { batch, requested, .. } => {
                    // One fewer request in flight; reaching zero lifts the reprojection throttle
                    // below immediately, so the library always gets a final, up-to-date pass right
                    // after a scan finishes instead of waiting out the rest of the interval.
                    event_scanning_batches.set(event_scanning_batches.get().saturating_sub(1));
                    let kind = event_batch_kinds.borrow_mut().remove(&batch).unwrap_or(BatchKind::OpenFiles);
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
                            set_playback_status(&window, format!("Added {requested} tracks"), &event_save_error);
                        }
                        // `StartupRestore` with no failures stays silent (`active_route_status` is
                        // still `None` at startup, so this is a no-op there too); "Open Files…"
                        // keeps restoring whatever route status `Started` last set.
                        _ => restore_active_route_status(&window, &event_active_route_status, &event_save_error),
                    }
                }
                LibraryEvent::FolderScanProgress { root, found } => {
                    set_playback_status(&window, format!("Scanning {}: {found} tracks found", root.display()), &event_save_error);
                }
            }
        }
        // One projection per tick, however many events marked it dirty above (`§3.4`).
        if queue_dirty && let Some(window) = event_window.upgrade() {
            project_and_set_queue(
                &window,
                &event_app_state.borrow(),
                event_now_playing_path.borrow().as_deref(),
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
                &event_search_query.borrow(),
                &event_songs_paths,
                &event_recent_paths,
                &event_album_paths,
            );
            event_library_dirty.set(false);
            event_library_last_projected.set(Some(Instant::now()));
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
/// "Now-playing projection"). Called on `Started` and again on a `Scanned` record for the
/// currently playing path, so real tags/art/lyrics replace the filename fallback as soon as the
/// scanner's second pass reaches it.
fn apply_now_playing_projection(window: &MainWindow, app_state: &AppState, path: &Path, info: &AudioInfo, fallback: &(String, String)) {
    let projection = project_now_playing(app_state, path, info, fallback);
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
}

/// Re-projects the Queue view (`§5.7`, `§3.7`) from the worker's latest pending snapshot plus
/// whatever is currently playing, and records the pending rows' paths in `queue_paths` so
/// `on_track_activated` can map a clicked row back to a `replace_queue` call. Called after every
/// event that can change either input: a fresh `QueueSnapshot`, a `Started`/`Stopped` change of
/// what is playing, or a `Scanned` record landing on a queued path.
fn project_and_set_queue(
    window: &MainWindow,
    app_state: &AppState,
    now_playing_path: Option<&Path>,
    now_playing_info: Option<&AudioInfo>,
    now_playing_fallback: &(String, String),
    pending: &[QueueTrackSnapshot],
    queue_paths: &Rc<RefCell<Vec<PathBuf>>>,
) {
    let now_playing = match (now_playing_path, now_playing_info) {
        (Some(path), Some(info)) => Some((path, info, now_playing_fallback.0.as_str(), now_playing_fallback.1.as_str())),
        _ => None,
    };
    let has_current = now_playing.is_some();
    let (current, rows, paths) = project_queue_rows(now_playing, pending, &app_state.library);
    window.set_has_queue_current(has_current);
    window.set_queue_current(current);
    window.set_queue_rows(ModelRc::new(VecModel::from_iter(rows)));
    *queue_paths.borrow_mut() = paths;
}

/// Re-projects every library-driven view (Home, Albums, Artists, Songs, Recently Added and album
/// detail) from `app_state`, `navigation`'s current artist filter/album key, and `query` (`§6`
/// Stage 6, `view_model::project_library`), and records each table's visible-path list so
/// `on_track_activated` can map a clicked row back to a path. Called once at startup, immediately
/// after every navigation mutation that can change `artist_filter`/`album_key`, and once per 40 ms
/// tick while `library_dirty` is set (search edits and scanner events, `§3.4`).
fn project_and_set_library(
    window: &MainWindow,
    app_state: &AppState,
    navigation: &Navigation,
    query: &str,
    songs_paths: &Rc<RefCell<Vec<PathBuf>>>,
    recent_paths: &Rc<RefCell<Vec<PathBuf>>>,
    album_paths: &Rc<RefCell<Vec<PathBuf>>>,
) {
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
    *songs_paths.borrow_mut() = projection.songs_paths;
    *recent_paths.borrow_mut() = projection.recent_paths;
    *album_paths.borrow_mut() = projection.album_paths;
}

/// `TrackRecord` -> `PreparedTrack`, for the album/library-wide actions (`§3.7` Album Play/Shuffle/
/// Add to Queue/Play Next, Home "Shuffle all") that already hold `&TrackRecord`s from
/// `Library::album_tracks`/`songs` instead of paths to look up through `Library::prepared`.
fn prepared_track(record: &TrackRecord) -> PreparedTrack {
    PreparedTrack { path: record.path.clone(), info: record.info.clone() }
}

/// A fresh shuffle seed from the wall clock (`§3.7` "seed from SystemTime nanos"), so Shuffle/
/// "Shuffle all" does not replay the same order every time it is pressed.
fn shuffle_seed() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_nanos() as u64).unwrap_or(0)
}

/// Applies the `§3.6` navigation state to the handful of Slint properties it drives. Called after
/// every `Navigation` mutation.
fn sync_navigation(window: &MainWindow, navigation: &Navigation) {
    window.set_view(navigation.view());
    window.set_nav_view(navigation.nav_view());
    window.set_can_go_back(navigation.can_go_back());
    window.set_artist_filter(navigation.artist_filter().unwrap_or_default().into());
    window.set_unavailable_title(unavailable_label(navigation.view()).into());
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
    rfd::AsyncFileDialog::new()
        .set_parent(&parent_window)
        .add_filter("Audio files", &AUDIO_EXTENSIONS)
        .pick_files()
        .await
        .map(|files| {
            files
                .into_iter()
                .map(|file| file.path().to_path_buf())
                .collect()
        })
}

/// "Open Folder…": RFD's async folder picker, scheduled on Slint's event loop exactly like
/// `pick_audio_files` above — never a synchronous `runModal()` inside a callback (`CLAUDE.md`).
/// The chosen folder is walked recursively by `library::walker` (`LibraryScanner::scan_folder`),
/// off the UI thread, not here.
async fn pick_audio_folder(parent_window: slint::WindowHandle) -> Option<PathBuf> {
    rfd::AsyncFileDialog::new()
        .set_parent(&parent_window)
        .pick_folder()
        .await
        .map(|folder| folder.path().to_path_buf())
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
            path: PathBuf::from("/nas/album/track.flac"),
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
    fn seek_rejected_clears_pending_seek() {
        let mut pending = Some(PendingSeek {
            path: PathBuf::from("/nas/album/track.flac"),
            target_ms: 5_000,
            requested_at: Instant::now(),
        });

        clear_pending_seek(&mut pending);

        assert!(pending.is_none());
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
    fn only_startup_restore_suppresses_inline_failure_status() {
        assert!(batch_reports_failures_inline(BatchKind::OpenFiles));
        assert!(batch_reports_failures_inline(BatchKind::OpenFolder));
        assert!(!batch_reports_failures_inline(BatchKind::StartupRestore));
    }
}

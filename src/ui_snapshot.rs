//! Dev-only offscreen snapshot harness (`§6` Stage 5): renders `MainWindow` with Slint's software
//! renderer to PNG files, so a reviewer can see the UI shell without launching the app, playing
//! audio or touching CoreAudio. This module never constructs `AudioPlayer` and never opens a
//! device. All the sample data below lives only here; `ui/*.slint` never sees it (R3).
//!
//! Run: `LIME_SNAPSHOT_DIR=<dir> cargo test --bin lime-player render_snapshots -- --ignored`.
//! Skips, with a message, when `LIME_SNAPSHOT_DIR` is unset — so a normal `cargo test` run never
//! needs a writable directory or touches the filesystem for this.
//!
//! The same offscreen platform also hosts `keyboard_tests` (Space, focus return, table selection),
//! which drive `MainWindow` with real Slint key and pointer events and, unlike the snapshots, run on
//! every `cargo test`.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, PlatformError, WindowAdapter};
use slint::{ComponentHandle, Image, ModelRc, PhysicalSize, Rgb8Pixel, SharedPixelBuffer, VecModel};

use crate::{AlbumCardData, AlbumHeaderData, ArtistRowData, MainWindow, TrackRowData, View};

thread_local! {
    /// The window `create_window_adapter` hands out next. Set by `new_window` right before each
    /// `MainWindow::new()`, so every scenario gets its own independent software window instead of
    /// several component trees fighting over one adapter (the same pattern Slint's own
    /// `tests/popups.rs` uses for a platform that serves more than one test window).
    static PENDING_WINDOW: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
}

struct SnapshotPlatform;

impl Platform for SnapshotPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        PENDING_WINDOW.with(|pending| {
            pending
                .borrow_mut()
                .take()
                .map(|window| window as Rc<dyn WindowAdapter>)
                .ok_or_else(|| PlatformError::Other("render_snapshots: no window was queued via new_window()".into()))
        })
    }
}

/// Queues a fresh software window and returns it; the next `MainWindow::new()` picks it up.
fn new_window() -> Rc<MinimalSoftwareWindow> {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    PENDING_WINDOW.with(|pending| *pending.borrow_mut() = Some(window.clone()));
    window
}

/// Resizes `window` to `width`x`height`, renders one frame and writes it as a PNG at `path`.
fn render_png(window: &MinimalSoftwareWindow, width: u32, height: u32, path: &Path) {
    window.set_size(PhysicalSize::new(width, height));
    window.request_redraw();
    let mut buffer = SharedPixelBuffer::<Rgb8Pixel>::new(width, height);
    let stride = width as usize;
    let rendered = window.draw_if_needed(|renderer| {
        renderer.render(buffer.make_mut_slice(), stride);
    });
    assert!(rendered, "expected a redraw after resizing to {width}x{height}");
    let image = image::RgbImage::from_raw(width, height, buffer.as_bytes().to_vec())
        .expect("the pixel buffer's size must match width * height * 3 bytes");
    image.save(path).unwrap_or_else(|error| panic!("could not write {}: {error}", path.display()));
}

fn render_scenario(window: &MinimalSoftwareWindow, dir: &Path, name: &str) {
    render_png(window, 1100, 700, &dir.join(format!("{name}-1100x700.png")));
    render_png(window, 1440, 900, &dir.join(format!("{name}-1440x900.png")));
}

/// Resizes and forces one full layout/paint pass without saving anything, discarding the pixels.
/// Used only to settle geometry before toggling `AlbumTile.force-menu-open` (via
/// `MainWindow.debug-force-album-menu-open-index`): that property's `changed` handler calls
/// `menu-popup.show()`, and the popup's position is anchored off `artist-row.y`, which only holds
/// its real value once a layout pass has actually run — `render_scenario` alone would open the
/// popup during the very first layout, at its stale pre-layout position.
fn warmup_layout(window: &MinimalSoftwareWindow, width: u32, height: u32) {
    window.set_size(PhysicalSize::new(width, height));
    window.request_redraw();
    let mut buffer = SharedPixelBuffer::<Rgb8Pixel>::new(width, height);
    let stride = width as usize;
    window.draw_if_needed(|renderer| {
        renderer.render(buffer.make_mut_slice(), stride);
    });
}

/// A neutral, obviously-fake `TrackRowData` for the Queue/Songs/album-detail snapshots. Names are
/// placeholders like "Sample Album One", never anything resembling the deleted mock catalog (R3).
/// `format_label`/`format_variant` feed the per-track `FormatPill` next to the title and (where
/// `show-format` is set) the Format column's own colored pill — see `sample_album`'s doc comment
/// for the exact `AlbumFormat::label()`/`variant()` pairs to pass.
#[allow(clippy::too_many_arguments)]
fn sample_row(key: &str, number: &str, title: &str, album: &str, badge: &str, format_label: &str, format_variant: &str, duration: &str) -> TrackRowData {
    TrackRowData {
        key: key.into(),
        number: number.into(),
        title: title.into(),
        artist: "Sample Artist".into(),
        album: album.into(),
        year: "2024".into(),
        duration: duration.into(),
        badge: badge.into(),
        format_label: format_label.into(),
        format_variant: format_variant.into(),
    }
}

/// A solid-color RGB8 pixel buffer, the same shape the real scanner hands the UI thread
/// (`AppState::cache_artwork`, `§3.3` "Artwork cache"): a flat `Vec<u8>` of `width * height * 3`
/// bytes through `SharedPixelBuffer::clone_from_slice`. Distinct colors per sample album make it
/// obvious at a glance which tiles have art and which fall back to the disc placeholder.
fn generated_artwork(width: u32, height: u32, rgb: [u8; 3]) -> Image {
    let mut bytes = Vec::with_capacity((width * height) as usize * 3);
    for _ in 0..(width * height) {
        bytes.extend_from_slice(&rgb);
    }
    Image::from_rgb8(SharedPixelBuffer::<Rgb8Pixel>::clone_from_slice(&bytes, width, height))
}

/// A sample `AlbumCardData`/`AlbumHeaderData` art pair: `Some((w, h, rgb))` renders a generated
/// color swatch with `has-art: true`; `None` renders `Image::default()` with `has-art: false`, so
/// `ArtworkView` falls back to the neutral disc placeholder — exactly the two states a real,
/// partially-scanned library shows side by side.
fn sample_art(art: Option<(u32, u32, [u8; 3])>) -> (Image, bool) {
    match art {
        Some((width, height, rgb)) => (generated_artwork(width, height, rgb), true),
        None => (Image::default(), false),
    }
}

/// `format_label`/`format_variant` are passed through verbatim to `AlbumCardData` — pass `("", "")`
/// for a scenario where the format pill doesn't matter (omitted entirely, per `FormatPill`'s
/// `AlbumTile` usage in `ui/widgets.slint`), or one of the `AlbumFormat::label()`/`variant()` pairs
/// from `src/library/format.rs` (`"MP3"`/`"mp3"`, `"FLAC"`/`"flac"`, `"WavPack"`/`"wavpack"`,
/// `"Mix Formats"`/`"mix"`, `"WAV"`/`"wav"`) to render one.
#[allow(clippy::too_many_arguments)]
fn sample_album(
    key: &str,
    title: &str,
    artist: &str,
    subtitle: &str,
    art: Option<(u32, u32, [u8; 3])>,
    format_label: &str,
    format_variant: &str,
) -> AlbumCardData {
    let (art, has_art) = sample_art(art);
    AlbumCardData {
        key: key.into(),
        title: title.into(),
        artist: artist.into(),
        subtitle: subtitle.into(),
        art,
        has_art,
        format_label: format_label.into(),
        format_variant: format_variant.into(),
    }
}

/// See `sample_album` above for `format_label`/`format_variant`.
#[allow(clippy::too_many_arguments)]
fn sample_album_header(
    key: &str,
    title: &str,
    artist: &str,
    meta: &str,
    art: Option<(u32, u32, [u8; 3])>,
    format_label: &str,
    format_variant: &str,
) -> AlbumHeaderData {
    let (art, has_art) = sample_art(art);
    AlbumHeaderData {
        key: key.into(),
        title: title.into(),
        artist: artist.into(),
        meta: meta.into(),
        art,
        has_art,
        format_label: format_label.into(),
        format_variant: format_variant.into(),
    }
}

fn sample_artist(name: &str, subtitle: &str) -> ArtistRowData {
    ArtistRowData { name: name.into(), subtitle: subtitle.into() }
}

fn build_queue_window(volume_available: bool) -> (Rc<MinimalSoftwareWindow>, MainWindow) {
    let window = new_window();
    let ui = MainWindow::new().expect("MainWindow::new() must not require a real platform beyond the one installed above");

    ui.set_view(View::Queue);
    ui.set_nav_view(View::Queue);

    ui.set_now_playing_key("sample-1".into());
    ui.set_now_playing_title("Sample Track One".into());
    ui.set_now_playing_artist("Sample Artist".into());
    ui.set_now_playing_album("Sample Album One".into());
    ui.set_now_playing_year("2024".into());
    ui.set_now_playing_genre("Ambient".into());
    ui.set_now_playing_badge("FLAC 24/96".into());
    ui.set_now_playing_badge_variant("hires".into());
    ui.set_now_playing_album_available(true);
    // Matches `library::format::format_line` (`§7`): an explicit `\n` before the channels/kbps
    // segment is a hard line break the renderer always honors, so "2,304" and "kbps" always stay
    // on the same (second) line instead of the word-wrap-only U+00A0 approach that Slint 1.18.1's
    // renderer does not respect.
    ui.set_format_line("FLAC · 24-bit / 96\u{00A0}kHz\nStereo · 2,304\u{00A0}kbps".into());
    ui.set_file_size_line("42.3 MB".into());
    ui.set_has_lyrics(false);

    ui.set_is_playing(true);
    ui.set_playback_elapsed("1:38".into());
    ui.set_playback_duration("4:05".into());
    ui.set_playback_progress(0.4);
    ui.set_can_seek(true);
    ui.set_duration_ms(245_000);

    ui.set_queue_current(sample_row("sample-1", "1", "Sample Track One", "Sample Album One", "FLAC 24/96", "FLAC", "hires", "4:05"));
    ui.set_has_queue_current(true);
    ui.set_queue_rows(ModelRc::new(VecModel::from(vec![
        sample_row("sample-2", "2", "Sample Track Two", "Sample Album Two", "FLAC 16/44.1", "FLAC", "flac", "3:12"),
        sample_row("sample-3", "3", "Sample Track Three", "Sample Album Two", "MP3", "MP3", "mp3", "3:47"),
        sample_row("sample-4", "4", "Sample Track Four", "Sample Album Three", "WavPack 24/96", "WavPack", "hires", "5:01"),
    ])));
    ui.set_queue_count(3);

    ui.set_output_device_options(ModelRc::new(VecModel::from(vec![slint::SharedString::from("Sample DAC")])));
    ui.set_selected_output_index(1);
    ui.set_output_name("Sample DAC".into());
    // This scenario is actively playing (`set_is_playing(true)` below): `active-output-name`, not
    // just the selection, is what the panel's "Playing on X" line actually reads (`§6`).
    ui.set_active_output_name("Sample DAC".into());
    ui.set_hog_mode_enabled(false);
    ui.set_playback_status("Playing · 32-bit integer output verified".into());

    if volume_available {
        ui.set_volume_available(true);
        ui.set_volume_level(0.6);
        ui.set_volume_detail("Device volume (CoreAudio)".into());
    } else {
        ui.set_volume_available(false);
        ui.set_volume_level(0.0);
        ui.set_volume_detail("This output has no software-controllable volume; play at its fixed level or adjust it on the device.".into());
    }

    (window, ui)
}

#[test]
#[ignore]
fn render_snapshots() {
    let Ok(dir) = std::env::var("LIME_SNAPSHOT_DIR") else {
        eprintln!("LIME_SNAPSHOT_DIR is not set; skipping render_snapshots (see src/ui_snapshot.rs)");
        return;
    };
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir).expect("could not create LIME_SNAPSHOT_DIR");

    slint::platform::set_platform(Box::new(SnapshotPlatform))
        .expect("set_platform must succeed: Slint keeps its platform per thread, and this test runs on its own");

    // Scenario 1: empty-library Home.
    {
        let window = new_window();
        let ui = MainWindow::new().unwrap();
        ui.set_view(View::Home);
        ui.set_nav_view(View::Home);
        ui.window().show().unwrap();
        render_scenario(&window, &dir, "home-empty");
        ui.window().hide().unwrap();
    }

    // Scenario 1b: Home with files already opened (`library-empty: false`) — must not repeat the
    // "your library is empty" copy (`§6` Stage 5 fix).
    {
        let window = new_window();
        let ui = MainWindow::new().unwrap();
        ui.set_view(View::Home);
        ui.set_nav_view(View::Home);
        ui.set_library_empty(false);
        ui.window().show().unwrap();
        render_scenario(&window, &dir, "home-not-empty");
        ui.window().hide().unwrap();
    }

    // Scenario 1c: Home populated (`§6` Stage 6): "Jump back in" (max-rows 1) and "Recently Added"
    // (max-rows 2) shelves, each mixing an album with generated artwork and one with none, so the
    // disc placeholder and real art render side by side.
    {
        let window = new_window();
        let ui = MainWindow::new().unwrap();
        ui.set_view(View::Home);
        ui.set_nav_view(View::Home);
        ui.set_library_empty(false);
        ui.set_jump_back_albums(ModelRc::new(VecModel::from(vec![
            sample_album("album-warm-colors", "Warm Colors", "Nina Path", "2021 · 12 tracks", Some((64, 64, [196, 120, 68])), "", ""),
            sample_album("album-cold-shapes", "Cold Shapes", "Other Artist", "2019 · 9 tracks", None, "", ""),
        ])));
        ui.set_recent_albums(ModelRc::new(VecModel::from(vec![
            sample_album("album-night-drive", "Night Drive", "The Collective", "2024 · 8 tracks", Some((64, 64, [80, 140, 190])), "", ""),
            sample_album("album-paper-trail", "Paper Trail", "Solo Artist", "2023 · 5 tracks", None, "", ""),
            sample_album("album-wide-open", "Wide Open", "Nina Path", "2022 · 10 tracks", Some((64, 64, [150, 90, 200])), "", ""),
            sample_album("album-low-tide", "Low Tide", "Other Artist", "2021 · 7 tracks", None, "", ""),
            sample_album("album-fault-lines", "Fault Lines", "The Collective", "2020 · 11 tracks", Some((64, 64, [90, 170, 120])), "", ""),
        ])));
        ui.window().show().unwrap();
        render_scenario(&window, &dir, "home-populated");
        ui.window().hide().unwrap();
    }

    // Scenario 1c-bis: the same populated Home, with `unified-title-bar: true` (`§6` Stage 5b) —
    // never set by `main.rs` outside macOS, but rendered here (the software renderer draws no
    // native traffic lights) so a reviewer can check that `Sidebar` reserves its top 52 px and
    // `TopBar` aligns its title/search row against that same strip.
    {
        let window = new_window();
        let ui = MainWindow::new().unwrap();
        ui.set_view(View::Home);
        ui.set_nav_view(View::Home);
        ui.set_library_empty(false);
        ui.set_unified_title_bar(true);
        ui.set_jump_back_albums(ModelRc::new(VecModel::from(vec![
            sample_album("album-warm-colors", "Warm Colors", "Nina Path", "2021 · 12 tracks", Some((64, 64, [196, 120, 68])), "", ""),
            sample_album("album-cold-shapes", "Cold Shapes", "Other Artist", "2019 · 9 tracks", None, "", ""),
        ])));
        ui.set_recent_albums(ModelRc::new(VecModel::from(vec![
            sample_album("album-night-drive", "Night Drive", "The Collective", "2024 · 8 tracks", Some((64, 64, [80, 140, 190])), "", ""),
            sample_album("album-paper-trail", "Paper Trail", "Solo Artist", "2023 · 5 tracks", None, "", ""),
            sample_album("album-wide-open", "Wide Open", "Nina Path", "2022 · 10 tracks", Some((64, 64, [150, 90, 200])), "", ""),
            sample_album("album-low-tide", "Low Tide", "Other Artist", "2021 · 7 tracks", None, "", ""),
            sample_album("album-fault-lines", "Fault Lines", "The Collective", "2020 · 11 tracks", Some((64, 64, [90, 170, 120])), "", ""),
        ])));
        ui.window().show().unwrap();
        render_scenario(&window, &dir, "home-unified-titlebar");
        ui.window().hide().unwrap();
    }

    // Scenario 1d: the Albums grid (`§6` Stage 6), same art-mix rule as Home's shelves. Also
    // exercises every `FormatPill` color/variant (`§5.5`) — one card per `AlbumFormat` value
    // (`src/library/format.rs`), including "Mix Formats" for an album whose tracks aren't all the
    // same format, plus one card with no pill at all (empty label omits it entirely).
    {
        let window = new_window();
        let ui = MainWindow::new().unwrap();
        ui.set_view(View::Albums);
        ui.set_nav_view(View::Albums);
        ui.set_library_empty(false);
        ui.set_albums(ModelRc::new(VecModel::from(vec![
            sample_album("album-warm-colors", "Warm Colors", "Nina Path", "2021 · 12 tracks", Some((64, 64, [196, 120, 68])), "MP3", "mp3"),
            sample_album("album-cold-shapes", "Cold Shapes", "Other Artist", "2019 · 9 tracks", None, "FLAC", "flac"),
            sample_album("album-night-drive", "Night Drive", "The Collective", "2024 · 8 tracks", Some((64, 64, [80, 140, 190])), "WavPack", "wavpack"),
            sample_album("album-gold-standard", "Gold Standard", "Nina Path", "2024 · 9 tracks", None, "FLAC", "hires"),
            sample_album("album-paper-trail", "Paper Trail", "Solo Artist", "2023 · 5 tracks", None, "Mix Formats", "mix"),
            sample_album("album-wide-open", "Wide Open", "Nina Path", "2022 · 10 tracks", Some((64, 64, [150, 90, 200])), "WAV", "wav"),
            sample_album("album-low-tide", "Low Tide", "Other Artist", "2021 · 7 tracks", None, "", ""),
        ])));
        ui.window().show().unwrap();
        render_scenario(&window, &dir, "albums-grid");
        ui.window().hide().unwrap();
    }

    // Scenario 1d-bis: the Albums grid again, dedicated scenario name for the new three-dot "more"
    // button (Task 2 "Remove from Library"). The button is part of `AlbumTile` itself now, so every
    // scenario above that renders an `AlbumGrid` already shows it too — this one exists so a
    // reviewer has an explicitly-named snapshot for the new control on its own.
    {
        let window = new_window();
        let ui = MainWindow::new().unwrap();
        ui.set_view(View::Albums);
        ui.set_nav_view(View::Albums);
        ui.set_library_empty(false);
        ui.set_albums(ModelRc::new(VecModel::from(vec![
            sample_album("album-warm-colors", "Warm Colors", "Nina Path", "2021 · 12 tracks", Some((64, 64, [196, 120, 68])), "MP3", "mp3"),
            sample_album("album-cold-shapes", "Cold Shapes", "Other Artist", "2019 · 9 tracks", None, "FLAC", "flac"),
            sample_album("album-night-drive", "Night Drive", "The Collective", "2024 · 8 tracks", Some((64, 64, [80, 140, 190])), "WavPack", "wavpack"),
        ])));
        ui.window().show().unwrap();
        render_scenario(&window, &dir, "albums-grid-with-menu");
        ui.window().hide().unwrap();
    }

    // Scenario 1d-ter: the same grid with the first card's "more" popup forced open
    // (`debug-force-album-menu-open-index`, a snapshot-harness-only knob threaded through
    // `MainWindow` -> `AlbumsView` -> `AlbumGrid` -> `AlbumTile.force-menu-open`), so its anchored
    // position/placement is visible for review.
    {
        let window = new_window();
        let ui = MainWindow::new().unwrap();
        ui.set_view(View::Albums);
        ui.set_nav_view(View::Albums);
        ui.set_library_empty(false);
        ui.set_albums(ModelRc::new(VecModel::from(vec![
            sample_album("album-warm-colors", "Warm Colors", "Nina Path", "2021 · 12 tracks", Some((64, 64, [196, 120, 68])), "MP3", "mp3"),
            sample_album("album-cold-shapes", "Cold Shapes", "Other Artist", "2019 · 9 tracks", None, "FLAC", "flac"),
            sample_album("album-night-drive", "Night Drive", "The Collective", "2024 · 8 tracks", Some((64, 64, [80, 140, 190])), "WavPack", "wavpack"),
        ])));
        ui.window().show().unwrap();
        // Settle layout at the first snapshot size before opening the popup (see
        // `warmup_layout`'s doc comment) — `render_scenario` below then re-renders that same
        // 1100x700 size for the actual saved PNG, plus 1440x900, with the popup already open by
        // the time either one runs.
        warmup_layout(&window, 1100, 700);
        ui.set_debug_force_album_menu_open_index(0);
        render_scenario(&window, &dir, "albums-grid-menu-open");
        ui.window().hide().unwrap();
    }

    // Scenario 1e: the Artists list (`§6` Stage 6).
    {
        let window = new_window();
        let ui = MainWindow::new().unwrap();
        ui.set_view(View::Artists);
        ui.set_nav_view(View::Artists);
        ui.set_library_empty(false);
        ui.set_artists(ModelRc::new(VecModel::from(vec![
            sample_artist("Nina Path", "2 albums · 22 tracks"),
            sample_artist("Other Artist", "2 albums · 16 tracks"),
            sample_artist("Solo Artist", "1 album · 5 tracks"),
            sample_artist("The Collective", "2 albums · 19 tracks"),
        ])));
        ui.window().show().unwrap();
        render_scenario(&window, &dir, "artists-list");
        ui.window().hide().unwrap();
    }

    // Scenario 1f: the Songs table with its summary line (`§6` Stage 6), one row per format family
    // so the per-track `FormatPill` and the Format column's colored pill can be compared side by
    // side.
    {
        let window = new_window();
        let ui = MainWindow::new().unwrap();
        ui.set_view(View::Songs);
        ui.set_nav_view(View::Songs);
        ui.set_library_empty(false);
        ui.set_songs_summary("128 songs · 9 h 12 min".into());
        ui.set_songs(ModelRc::new(VecModel::from(vec![
            sample_row("song-1", "1", "Sunrise", "Warm Colors", "FLAC 24/96", "FLAC", "hires", "4:12"),
            sample_row("song-2", "2", "Nightfall", "Cold Shapes", "MP3", "MP3", "mp3", "3:47"),
            sample_row("song-3", "3", "Turnaround", "Night Drive", "WavPack 24/96", "WavPack", "hires", "5:01"),
            sample_row("song-4", "4", "Outro", "Warm Colors", "WAV 16/44.1", "WAV", "wav", "2:58"),
            sample_row("song-5", "5", "Daybreak", "Cold Shapes", "FLAC 16/44.1", "FLAC", "flac", "3:30"),
            sample_row("song-6", "6", "Low Tide", "Night Drive", "WavPack 24/48", "WavPack", "wavpack", "4:02"),
        ])));
        ui.window().show().unwrap();
        render_scenario(&window, &dir, "songs-table");
        ui.window().hide().unwrap();
    }

    // Scenario 1g: album detail (`§6` Stage 6): the header band with its four large buttons, a
    // multi-disc album (disc-dash-track numbering), a currently playing row, the back chevron
    // (`can-go-back`), a long, classical-style artist credit (eliding instead of overflowing the
    // page, `§6` Stage 6 fix) — and, per track, a different format family (MP3/FLAC/WavPack/WAV) so
    // every `FormatPill` color renders at once next to the title (`§5.5` per-track format pills).
    // The header pill itself shows "Mix Formats" (`AlbumFormat::Mix`), matching a real album whose
    // tracks disagree on format this way.
    {
        let window = new_window();
        let ui = MainWindow::new().unwrap();
        ui.set_view(View::AlbumDetail);
        ui.set_nav_view(View::Albums);
        ui.set_library_empty(false);
        ui.set_can_go_back(true);
        ui.set_album_header(sample_album_header(
            "album-double-feature",
            "Double Feature",
            "Berliner Philharmoniker, Herbert von Karajan, Anne-Sophie Mutter, Mstislav Rostropovich, Nina Path",
            "2021 · 4 tracks · 18:32",
            Some((200, 200, [196, 120, 68])),
            "Mix Formats",
            "mix",
        ));
        ui.set_now_playing_key("track-3".into());
        ui.set_album_tracks(ModelRc::new(VecModel::from(vec![
            sample_row("track-1", "1-01", "Intro", "Double Feature", "MP3", "MP3", "mp3", "3:02"),
            sample_row("track-2", "1-02", "Drifting", "Double Feature", "FLAC 24/96", "FLAC", "hires", "5:47"),
            sample_row("track-3", "2-01", "Turnaround", "Double Feature", "WavPack 24/96", "WavPack", "hires", "4:18"),
            sample_row("track-4", "2-02", "Outro", "Double Feature", "WAV 16/44.1", "WAV", "wav", "5:25"),
        ])));
        ui.window().show().unwrap();
        render_scenario(&window, &dir, "album-detail");
        ui.window().hide().unwrap();
    }

    // Scenario 1g-bis: a CUE-derived album detail — a single physical WavPack file split into
    // sequential tracks by a cue sheet (`TrackKey`, decoder end-frame truncation, scanner CUE
    // expansion): plain "1".."6" track numbers (no disc-dash prefix, unlike the multi-disc scenario
    // above), a shared "WavPack 24/96" badge on every row (the whole album is one decoded file, so
    // every track shares the same format/rate/bit depth), and the header itself showing the WavPack
    // format pill (`format-label`/`format-variant`, `§5.5`/`§5.7`).
    {
        let window = new_window();
        let ui = MainWindow::new().unwrap();
        ui.set_view(View::AlbumDetail);
        ui.set_nav_view(View::Albums);
        ui.set_library_empty(false);
        ui.set_can_go_back(true);
        ui.set_album_header(sample_album_header(
            "album-live-at-the-grotto",
            "Live at the Grotto",
            "Nina Path",
            "2018 · 6 tracks · 27:41",
            Some((200, 200, [110, 150, 95])),
            "WavPack",
            "hires",
        ));
        ui.set_now_playing_key("cue-track-3".into());
        ui.set_album_tracks(ModelRc::new(VecModel::from(vec![
            sample_row("cue-track-1", "1", "Walking In", "Live at the Grotto", "WavPack 24/96", "WavPack", "hires", "4:12"),
            sample_row("cue-track-2", "2", "Slow Burn", "Live at the Grotto", "WavPack 24/96", "WavPack", "hires", "3:58"),
            sample_row("cue-track-3", "3", "Turnstile", "Live at the Grotto", "WavPack 24/96", "WavPack", "hires", "5:20"),
            sample_row("cue-track-4", "4", "Low Light", "Live at the Grotto", "WavPack 24/96", "WavPack", "hires", "4:47"),
            sample_row("cue-track-5", "5", "Undertow", "Live at the Grotto", "WavPack 24/96", "WavPack", "hires", "3:31"),
            sample_row("cue-track-6", "6", "Walking Out", "Live at the Grotto", "WavPack 24/96", "WavPack", "hires", "5:53"),
        ])));
        ui.window().show().unwrap();
        render_scenario(&window, &dir, "cue-album");
        ui.window().hide().unwrap();
    }

    // Scenario 1h: search filtering (`§6` Stage 6) — a query with no matches, so Albums renders its
    // "No matches" `EmptyState` instead of an empty grid, and the top bar shows the typed query.
    {
        let window = new_window();
        let ui = MainWindow::new().unwrap();
        ui.set_view(View::Albums);
        ui.set_nav_view(View::Albums);
        ui.set_library_empty(false);
        ui.set_search_query("vinyl only".into());
        ui.set_albums(ModelRc::new(VecModel::from(Vec::<AlbumCardData>::new())));
        ui.window().show().unwrap();
        render_scenario(&window, &dir, "albums-no-search-matches");
        ui.window().hide().unwrap();
    }

    // Scenario 1i: the dedicated Search view (`§3.4` "Search") with matches in every section —
    // Songs (a plain, non-virtualized `TrackTable`), Albums (`AlbumGrid`) and Artists
    // (`ArtistListRow`), each headed with its own count. The underlying view (Home) is left
    // showing underneath: `search-active` alone decides what the content area renders, so this
    // also doubles as a check that a view switch is never required to reach the Search view.
    {
        let window = new_window();
        let ui = MainWindow::new().unwrap();
        ui.set_view(View::Home);
        ui.set_nav_view(View::Home);
        ui.set_library_empty(false);
        ui.set_search_query("nina".into());
        ui.set_search_active(true);
        ui.set_search_songs_header("Songs (4)".into());
        ui.set_search_songs(ModelRc::new(VecModel::from(vec![
            sample_row("song-1", "1", "Sunrise", "Warm Colors", "FLAC 24/96", "FLAC", "flac", "4:12"),
            sample_row("song-2", "2", "Wide Open", "Wide Open", "FLAC 16/44.1", "FLAC", "flac", "3:58"),
            sample_row("song-3", "3", "Nina's Theme", "Night Drive", "WavPack 24/96", "WavPack", "hires", "5:01"),
            sample_row("song-4", "4", "Outro", "Warm Colors", "FLAC 16/44.1", "FLAC", "flac", "2:58"),
        ])));
        ui.set_search_albums_header("Albums (2)".into());
        ui.set_search_albums(ModelRc::new(VecModel::from(vec![
            sample_album("album-warm-colors", "Warm Colors", "Nina Path", "2021 · 12 tracks", Some((64, 64, [196, 120, 68])), "", ""),
            sample_album("album-wide-open", "Wide Open", "Nina Path", "2022 · 10 tracks", Some((64, 64, [150, 90, 200])), "", ""),
        ])));
        ui.set_artists(ModelRc::new(VecModel::from(vec![sample_artist("Nina Path", "2 albums · 22 tracks")])));
        ui.set_search_artists_header("Artists (1)".into());
        ui.set_search_has_results(true);
        ui.window().show().unwrap();
        render_scenario(&window, &dir, "search-results");
        ui.window().hide().unwrap();
    }

    // Scenario 1j: the Search view with no matches anywhere — the "No results for ..." `EmptyState`
    // (distinct copy from a single unfiltered view's "No matches", `§3.4` "Search"), no section
    // rendered at all.
    {
        let window = new_window();
        let ui = MainWindow::new().unwrap();
        ui.set_view(View::Home);
        ui.set_nav_view(View::Home);
        ui.set_library_empty(false);
        ui.set_search_query("vinyl only".into());
        ui.set_search_active(true);
        ui.set_search_songs_header("Songs (0)".into());
        ui.set_search_albums_header("Albums (0)".into());
        ui.set_search_artists_header("Artists (0)".into());
        ui.set_search_has_results(false);
        ui.window().show().unwrap();
        render_scenario(&window, &dir, "search-empty");
        ui.window().hide().unwrap();
    }

    // Scenario 2: Queue with 3 rows and a playing track (player bar + right panel), progress
    // ~40%, volume ~60%.
    {
        let (window, ui) = build_queue_window(true);
        ui.window().show().unwrap();
        render_scenario(&window, &dir, "queue-playing");
        ui.window().hide().unwrap();
    }

    // Scenario 2b: the same, but volume unavailable (disabled scrubber + visible explanation).
    {
        let (window, ui) = build_queue_window(false);
        ui.window().show().unwrap();
        render_scenario(&window, &dir, "queue-playing-volume-unavailable");
        ui.window().hide().unwrap();
    }

    // Scenario 2c: a long genre tag and a long tag-derived title, to confirm the right panel elides
    // instead of widening (genre, `§6` Stage 5 fix) or growing past two lines (title, same fix).
    {
        let (window, ui) = build_queue_window(true);
        ui.set_now_playing_title("A Genuinely Overlong Track Title Pulled Straight From A Tag".into());
        ui.set_now_playing_genre("Progressive Instrumental Post-Rock Ambient Electronica".into());
        ui.set_output_name("Sample DAC".into());
        ui.window().show().unwrap();
        render_scenario(&window, &dir, "queue-playing-long-tags");
        ui.window().hide().unwrap();
    }

    // Scenario 2d: lyrics present alongside a long one-line status (`§7`/Gap coverage: "Lyrics only
    // when present" needs a with-lyrics render to compare against every other queue-playing
    // scenario above, all of which leave `has-lyrics` false; "fully visible ... with lyrics and a
    // long status" needs the 72 px bar to stay on screen and the panel to scroll instead of
    // clipping). The status chains a seek failure onto an underrun report, the way `player.rs`'s
    // `finish_active_failure`/`flush_underrun_report` can combine two messages in one tick.
    {
        let (window, ui) = build_queue_window(true);
        ui.set_has_lyrics(true);
        ui.set_lyrics(
            (1..=40).map(|line| format!("Sample lyric line {line} of the verse, repeating a placeholder phrase")).collect::<Vec<_>>().join("\n").into(),
        );
        ui.set_playback_status(
            "Seek failed: the device rejected the requested sample rate. Press Play to retry from the start. \u{b7} \
             Buffer underrun: 2,048 samples dropped at 96 kHz; check the NAS connection or choose a different output device."
                .into(),
        );
        ui.window().show().unwrap();
        render_scenario(&window, &dir, "queue-playing-lyrics-long-status");
        ui.window().hide().unwrap();
    }

    // Scenario 3: an Unavailable placeholder view.
    {
        let window = new_window();
        let ui = MainWindow::new().unwrap();
        ui.set_view(View::Browser);
        ui.set_nav_view(View::Browser);
        ui.set_unavailable_title("Browser".into());
        ui.window().show().unwrap();
        render_scenario(&window, &dir, "unavailable-browser");
        ui.window().hide().unwrap();
    }
}

/// Keyboard and focus behavior of `MainWindow`'s window-wide `key-root` (`CLAUDE.md` "Media controls"
/// -> "Spacebar"), driven through real Slint key and pointer events on the same offscreen software
/// window the snapshots use. Nothing here builds an `AudioPlayer`: the `space-pressed` callback is
/// counted instead of handled, and the row selection is read back from `selected-track-key`. Slint
/// keeps its platform per thread and every test runs on its own, so each installs its own.
#[cfg(test)]
mod keyboard_tests {
    use std::cell::Cell;

    use slint::platform::{Key, PointerEventButton, WindowEvent};
    use slint::{LogicalPosition, SharedString};

    use super::*;
    use crate::TrackListKind;

    const WIDTH: u32 = 1440;
    const HEIGHT: u32 = 900;

    /// A shown, active `MainWindow` with `space-pressed` counted (the row selection lives in the
    /// window's own `selected-track-key`, which the tests read back).
    struct Window {
        ui: MainWindow,
        window: Rc<MinimalSoftwareWindow>,
        spaces: Rc<Cell<u32>>,
    }

    impl Window {
        fn new() -> Self {
            // The result is ignored on purpose: it only fails when this thread already has a platform.
            let _ = slint::platform::set_platform(Box::new(SnapshotPlatform));
            let window = new_window();
            let ui = MainWindow::new().unwrap();
            let spaces = Rc::new(Cell::new(0));
            let counted = Rc::clone(&spaces);
            ui.on_space_pressed(move || counted.set(counted.get() + 1));
            ui.window().show().unwrap();
            ui.window().dispatch_event(WindowEvent::WindowActiveChanged(true));
            let this = Self { ui, window, spaces };
            this.frame();
            this
        }

        /// One layout and paint pass, which is also what instantiates the conditional views. It runs
        /// Slint's timers and `changed` handlers first, as the event loop does between events.
        fn frame(&self) {
            self.frame_pixels();
        }

        /// `frame`, returning what it painted.
        fn frame_pixels(&self) -> SharedPixelBuffer<Rgb8Pixel> {
            slint::platform::update_timers_and_animations();
            self.window.set_size(PhysicalSize::new(WIDTH, HEIGHT));
            self.window.request_redraw();
            let mut buffer = SharedPixelBuffer::<Rgb8Pixel>::new(WIDTH, HEIGHT);
            self.window.draw_if_needed(|renderer| {
                renderer.render(buffer.make_mut_slice(), WIDTH as usize);
            });
            buffer
        }

        /// Two passes: a model replaced under a virtualized list is laid out by the pass after the one
        /// that noticed it.
        fn settled_frame(&self) -> SharedPixelBuffer<Rgb8Pixel> {
            self.frame();
            self.frame_pixels()
        }

        fn press(&self, text: impl Into<SharedString>) {
            let text = text.into();
            self.ui.window().dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
            self.ui.window().dispatch_event(WindowEvent::KeyReleased { text });
        }

        fn type_text(&self, text: &str) {
            for character in text.chars() {
                self.press(character.to_string());
            }
        }

        fn click(&self, x: f32, y: f32) {
            let position = LogicalPosition::new(x, y);
            self.ui.window().dispatch_event(WindowEvent::PointerMoved { position });
            self.ui.window().dispatch_event(WindowEvent::PointerPressed { position, button: PointerEventButton::Left });
            self.ui.window().dispatch_event(WindowEvent::PointerReleased { position, button: PointerEventButton::Left });
        }

        /// Tab from `key-root` reaches the search field, the first focusable control after it.
        fn focus_search_with_tab(&self) {
            self.press(Key::Tab);
            self.type_text("q");
            assert_eq!(self.ui.get_search_query(), "q", "Tab from the window scope must land in the search field");
            self.press(Key::Backspace);
            assert_eq!(self.ui.get_search_query(), "");
        }

        fn spaces(&self) -> u32 {
            self.spaces.get()
        }
    }

    fn rows(count: usize) -> ModelRc<TrackRowData> {
        ModelRc::new(VecModel::from(
            (0..count)
                .map(|i| sample_row(&format!("row-{i}"), &format!("{}", i + 1), &format!("Sample Track {}", i + 1), "Sample Album", "FLAC 24/96", "FLAC", "flac", "3:00"))
                .collect::<Vec<_>>(),
        ))
    }

    #[test]
    fn space_reaches_the_window_scope_with_nothing_clicked_first() {
        let window = Window::new();
        window.press(" ");
        assert_eq!(window.spaces(), 1, "`forward-focus` must give key-root the keyboard before any click");
    }

    #[test]
    fn only_a_plain_first_press_of_space_toggles() {
        let window = Window::new();
        window.ui.window().dispatch_event(WindowEvent::KeyPressRepeated { text: " ".into() });
        window.ui.window().dispatch_event(WindowEvent::KeyPressRepeated { text: " ".into() });
        assert_eq!(window.spaces(), 0, "holding Space must not flip play/pause on every auto-repeat");
        for modifier in [Key::Control, Key::Meta, Key::Alt] {
            window.ui.window().dispatch_event(WindowEvent::KeyPressed { text: modifier.into() });
            window.press(" ");
            window.ui.window().dispatch_event(WindowEvent::KeyReleased { text: modifier.into() });
        }
        assert_eq!(window.spaces(), 0, "Control/Command/Option+Space belong to the OS, not to playback");
        window.press(" ");
        assert_eq!(window.spaces(), 1);
    }

    #[test]
    fn space_typed_into_the_search_field_is_text_not_a_toggle() {
        let window = Window::new();
        window.focus_search_with_tab();
        window.type_text("a b");
        assert_eq!(window.ui.get_search_query(), "a b", "a Space typed into the focused search field must still insert a space");
        assert_eq!(window.spaces(), 0, "the search field consumes its own spaces; the window scope never sees them");
    }

    #[test]
    fn escape_in_the_search_field_clears_it_and_hands_the_keyboard_back() {
        let window = Window::new();
        window.focus_search_with_tab();
        window.type_text("abc");
        window.press(Key::Escape);
        assert_eq!(window.ui.get_search_query(), "", "Esc still clears the query");
        window.press(" ");
        assert_eq!(window.spaces(), 1, "after Esc, Space toggles playback instead of typing into the search box");
        assert_eq!(window.ui.get_search_query(), "");
    }

    #[test]
    fn return_in_the_search_field_hands_the_keyboard_back_and_keeps_the_query() {
        let window = Window::new();
        window.focus_search_with_tab();
        window.type_text("abc");
        window.press(Key::Return);
        assert_eq!(window.ui.get_search_query(), "abc", "Return submits the query; it does not clear it");
        window.press(" ");
        assert_eq!(window.spaces(), 1);
        assert_eq!(window.ui.get_search_query(), "abc", "the Space after Return must not be typed into the query");
    }

    /// A table created while the user is typing (a search that starts matching songs) neither selects
    /// a row nor takes the keyboard from the search field.
    #[test]
    fn a_new_table_selects_nothing_and_does_not_steal_focus_from_the_search_field() {
        let window = Window::new();
        window.ui.set_library_empty(false);
        window.frame();
        window.focus_search_with_tab();
        window.type_text("sa");
        window.ui.set_search_active(true);
        window.ui.set_search_songs(rows(3));
        window.frame();
        assert_eq!(window.ui.get_selected_track_key(), "", "the Search table appears with no row selected");
        window.type_text(" x");
        assert_eq!(window.ui.get_search_query(), "sa x", "the table appearing must not pull the keyboard out of the search field");
        assert_eq!(window.spaces(), 0);
    }

    /// Finds the row whose track key is `key` by clicking down a column until the window reports it
    /// selected. Probing beats a hard-coded position: the row moves whenever the page header does.
    fn probe_row(window: &Window, key: &str) -> (f32, f32) {
        let x = 700.0;
        let mut y = 120.0;
        while y < HEIGHT as f32 - 100.0 {
            window.ui.set_selected_track_key("".into());
            window.click(x, y);
            if window.ui.get_selected_track_key() == key {
                return (x, y);
            }
            y += 4.0;
        }
        panic!("no click down the page selected the row `{key}`");
    }

    #[test]
    fn clicking_a_row_selects_it_and_returns_the_keyboard_to_the_window_scope() {
        let window = Window::new();
        window.ui.set_library_empty(false);
        window.ui.set_view(View::Songs);
        window.ui.set_nav_view(View::Songs);
        window.ui.set_songs(rows(5));
        window.frame();
        let (x, y) = probe_row(&window, "row-0");
        assert_eq!(window.spaces(), 0);

        window.frame();
        window.focus_search_with_tab();
        window.type_text("a");
        window.ui.set_selected_track_key("".into());
        window.click(x, y);
        assert_eq!(window.ui.get_selected_track_key(), "row-0", "the click selects the row, by its track key");
        window.press(" ");
        assert_eq!(window.spaces(), 1, "a click on a row must take the keyboard back from the search field");
        assert_eq!(window.ui.get_search_query(), "a", "the Space must not have been typed into the query");
    }

    /// The queue view's "Now Playing" row is a table of its own; a click on it is a selection too, and
    /// takes the keyboard back from the search field like every other row.
    #[test]
    fn a_click_on_the_queue_views_now_playing_row_selects_it_and_takes_the_keyboard_back() {
        let window = Window::new();
        window.ui.set_library_empty(false);
        window.ui.set_view(View::Queue);
        window.ui.set_nav_view(View::Queue);
        window.ui.set_queue_current(sample_row("row-now", "1", "Sample Track Now", "Sample Album", "FLAC 24/96", "FLAC", "flac", "3:00"));
        window.ui.set_has_queue_current(true);
        window.ui.set_queue_rows(rows(3));
        window.ui.set_queue_count(3);
        window.frame();
        probe_row(&window, "row-now");
        let (x, y) = probe_row(&window, "row-1");
        window.focus_search_with_tab();
        window.type_text("a");
        window.click(x, y);
        assert_eq!(window.ui.get_selected_track_key(), "row-1", "an Up Next row selects too");
        window.press(" ");
        assert_eq!(window.spaces(), 1);
        assert_eq!(window.ui.get_search_query(), "a");
    }

    // -- the now-playing click zones ---------------------------------------------------------------

    /// A window showing a playing track, with `now-playing-album-requested` counted.
    fn window_with_a_playing_track(album_available: bool) -> (Window, Rc<Cell<u32>>) {
        let window = Window::new();
        window.ui.set_library_empty(false);
        window.ui.set_now_playing_key("sample-1".into());
        window.ui.set_now_playing_title("Sample Track One".into());
        window.ui.set_now_playing_artist("Sample Artist".into());
        window.ui.set_now_playing_badge("FLAC 24/96".into());
        window.ui.set_now_playing_badge_variant("hires".into());
        window.ui.set_now_playing_album_available(album_available);
        let requested = Rc::new(Cell::new(0));
        let counted = Rc::clone(&requested);
        window.ui.on_now_playing_album_requested(move || counted.set(counted.get() + 1));
        window.frame();
        (window, requested)
    }

    /// Vertical middle of the player bar (`Theme.playerbar-h` is 72 px, docked at the bottom).
    const BAR_Y: f32 = HEIGHT as f32 - 36.0;

    #[test]
    fn clicking_the_player_bars_now_playing_zone_requests_the_album() {
        let (window, requested) = window_with_a_playing_track(true);

        window.click(40.0, BAR_Y);
        assert_eq!(requested.get(), 1, "the cover thumbnail is part of the zone");
        window.click(110.0, BAR_Y - 8.0);
        assert_eq!(requested.get(), 2, "so is the title text");
        window.click(110.0, BAR_Y + 10.0);
        assert_eq!(requested.get(), 3, "so is the artist/badge line");
    }

    #[test]
    fn the_player_bars_other_controls_do_not_request_the_album() {
        let (window, requested) = window_with_a_playing_track(true);

        // Everything right of the zone (the zone ends at 16 + 48 + 12 + 240 px at most): heart,
        // shuffle, the transport buttons, scrubber and volume. Clicking across all of it must never
        // reach the zone's touch area.
        let mut x = 330.0;
        while x < WIDTH as f32 - 10.0 {
            window.click(x, BAR_Y);
            x += 6.0;
        }
        assert_eq!(requested.get(), 0, "the heart button and the rest of the bar are outside the now-playing zone");
    }

    #[test]
    fn the_now_playing_zone_does_nothing_while_no_album_is_available() {
        let (window, requested) = window_with_a_playing_track(false);

        window.click(40.0, BAR_Y);
        window.click(110.0, BAR_Y - 8.0);

        assert_eq!(requested.get(), 0, "a track outside the library has no album page to open");
    }

    #[test]
    fn clicking_the_panels_cover_requests_the_album_and_returns_the_keyboard() {
        let (window, requested) = window_with_a_playing_track(true);
        window.focus_search_with_tab();
        window.type_text("a");

        // The cover sits at the panel's top, 20 px in, `Theme.panel-content-w` (260 px) square.
        window.click(WIDTH as f32 - 150.0, 120.0);

        assert_eq!(requested.get(), 1);
        window.press(" ");
        assert_eq!(window.spaces(), 1, "the forwarder hands the keyboard back from the search field");
        assert_eq!(window.ui.get_search_query(), "a");
    }

    #[test]
    fn clicking_the_player_bar_zone_returns_the_keyboard_from_the_search_field() {
        let (window, requested) = window_with_a_playing_track(true);
        window.focus_search_with_tab();
        window.type_text("a");

        window.click(40.0, BAR_Y);

        assert_eq!(requested.get(), 1);
        window.press(" ");
        assert_eq!(window.spaces(), 1);
        assert_eq!(window.ui.get_search_query(), "a");
    }

    // -- rows replaced under a table that stays on screen ---------------------------------------
    //
    // Slint keeps an `if` instance alive while its condition stays true, so a table whose `rows` are
    // replaced (a scan batch or a `Started` re-projects the library) is the same table with different
    // content. The selection is the track's key, so it follows the track: highlighted where the track
    // sits now, and gone once the track is gone. What these tests replace rows for is a re-projection
    // that is neither a navigation nor a search edit: those two forget the selection on purpose,
    // before any rows change (`sync_navigation`, and `apply_search_edit` whenever the effective query
    // changes, so a refined query or Back from one album page to another clears it; see
    // `navigating_forgets_the_selection` and `a_search_edit_forgets_the_selection_only_when_the_list_on_screen_can_change`).

    /// `bg-selected` (`ui/theme.slint`), the fill of a highlighted track row. Hover (`#1b1d1b`) and the
    /// plain row (`#0e0e0e`) are different colors.
    const SELECTED_ROW: Rgb8Pixel = Rgb8Pixel { r: 0x22, g: 0x25, b: 0x22 };
    const ROW_HEIGHT: u32 = 34;

    /// The top edge of every band of the middle column painted in the selected-row color, found by
    /// scanning for horizontal runs of it (a highlighted row is filled edge to edge; text covers only a
    /// sliver of it).
    fn highlighted_rows(pixels: &SharedPixelBuffer<Rgb8Pixel>) -> Vec<u32> {
        let width = pixels.width() as usize;
        let slice = pixels.as_slice();
        let mut tops = Vec::new();
        let mut in_band = false;
        for y in 0..pixels.height() as usize {
            let line = &slice[y * width + 400..y * width + 1000];
            let highlighted = line.iter().filter(|pixel| **pixel == SELECTED_ROW).count() > 300;
            if highlighted && !in_band {
                tops.push(y as u32);
            }
            in_band = highlighted;
        }
        tops
    }

    fn rows_with_keys(keys: &[&str]) -> ModelRc<TrackRowData> {
        ModelRc::new(VecModel::from(
            keys.iter()
                .enumerate()
                .map(|(i, key)| sample_row(key, &format!("{}", i + 1), &format!("Sample Track {key}"), "Sample Album", "FLAC 24/96", "FLAC", "flac", "3:00"))
                .collect::<Vec<_>>(),
        ))
    }

    /// Clicks `row-0` of the table `kind` shows (rows `row-0..row-4`), replaces the rows the way a
    /// library re-projection does — a new model — first with one that still holds the track, further
    /// down, and then with one that does not, and checks what the highlight and Space's resolution do.
    fn assert_selection_follows_its_track(window: &Window, kind: TrackListKind, set_rows: impl Fn(&MainWindow, ModelRc<TrackRowData>)) {
        probe_row(window, "row-0");
        window.ui.window().dispatch_event(WindowEvent::PointerExited);
        let before = highlighted_rows(&window.settled_frame());
        assert_eq!(before.len(), 1, "{kind:?}: exactly the clicked row is highlighted, got bands at {before:?}");
        assert_eq!(crate::visible_selection(&window.ui), Some((kind, 0)));

        // Two tracks now sort ahead of it (a scan added them).
        set_rows(&window.ui, rows_with_keys(&["new-a", "new-b", "row-0", "row-1", "row-2"]));
        let after = highlighted_rows(&window.settled_frame());
        assert_eq!(
            after,
            vec![before[0] + 2 * ROW_HEIGHT],
            "{kind:?}: the highlight must follow the track to its new position, not stay at index 0"
        );
        assert_eq!(window.ui.get_selected_track_key(), "row-0", "a re-projection that still holds the track keeps the selection");
        assert_eq!(
            crate::visible_selection(&window.ui),
            Some((kind, 2)),
            "{kind:?}: Space resolves the key to the row it is at now, not the one it was clicked at"
        );

        // The track leaves the list (removed from the library, no longer matching the search).
        set_rows(&window.ui, rows_with_keys(&["new-a", "new-b", "row-1"]));
        assert!(highlighted_rows(&window.settled_frame()).is_empty(), "{kind:?}: no row shows the selection once its track is gone");
        assert_eq!(
            crate::visible_selection(&window.ui),
            None,
            "{kind:?}: Space must not play whichever track now sits where the selection was"
        );
    }

    #[test]
    fn the_selection_of_the_songs_table_survives_a_library_growth_and_is_dropped_with_its_track() {
        let window = Window::new();
        window.ui.set_library_empty(false);
        window.ui.set_view(View::Songs);
        window.ui.set_nav_view(View::Songs);
        window.ui.set_songs(rows(5));
        window.frame();
        assert_selection_follows_its_track(&window, TrackListKind::Songs, |ui, model| ui.set_songs(model));
    }

    /// Sets `search-songs` directly, so this is a re-projection with the query unchanged (a scan batch or
    /// a `Started` landing while the Search view is open), not a query edit: a refined query goes through
    /// `apply_search_edit` and clears the selection.
    #[test]
    fn the_selection_of_the_search_table_survives_a_rescan_that_still_matches_it() {
        let window = Window::new();
        window.ui.set_library_empty(false);
        window.ui.set_search_active(true);
        window.ui.set_search_songs(rows(5));
        window.frame();
        assert_selection_follows_its_track(&window, TrackListKind::Search, |ui, model| ui.set_search_songs(model));
    }

    #[test]
    fn the_selection_of_the_album_table_survives_replaced_rows_of_the_same_album() {
        let window = Window::new();
        window.ui.set_library_empty(false);
        window.ui.set_view(View::AlbumDetail);
        window.ui.set_album_tracks(rows(5));
        window.frame();
        assert_selection_follows_its_track(&window, TrackListKind::Album, |ui, model| ui.set_album_tracks(model));
    }

    /// The selection is resolved against the table on screen only: a key that names a row of a table
    /// that is not the visible one resolves to nothing.
    #[test]
    fn a_selection_made_in_another_view_does_not_resolve_in_the_visible_table() {
        let window = Window::new();
        window.ui.set_library_empty(false);
        window.ui.set_view(View::Songs);
        window.ui.set_nav_view(View::Songs);
        window.ui.set_songs(rows(5));
        window.frame();
        window.ui.set_selected_track_key("row-3".into());
        assert_eq!(crate::visible_selection(&window.ui), Some((TrackListKind::Songs, 3)));
        window.ui.set_view(View::Home);
        assert_eq!(crate::visible_selection(&window.ui), None, "Home has no track table");
        window.ui.set_view(View::RecentlyAdded);
        assert_eq!(crate::visible_selection(&window.ui), None, "the Recently Added table does not hold the key");
    }

    #[test]
    fn navigating_forgets_the_selection() {
        let window = Window::new();
        window.ui.set_library_empty(false);
        window.ui.set_view(View::Songs);
        window.ui.set_nav_view(View::Songs);
        window.ui.set_songs(rows(5));
        window.frame();
        probe_row(&window, "row-0");
        window.ui.window().dispatch_event(WindowEvent::PointerExited);
        assert_eq!(highlighted_rows(&window.settled_frame()).len(), 1, "precondition: the clicked row is highlighted");

        // Every navigation callback of `main()` ends in `sync_navigation`.
        crate::sync_navigation(&window.ui, &crate::app_state::Navigation::new());

        assert_eq!(window.ui.get_selected_track_key(), "");
        window.ui.set_view(View::Songs);
        assert!(highlighted_rows(&window.settled_frame()).is_empty(), "the highlight is gone with the selection");
    }

    #[test]
    fn editing_the_search_query_forgets_the_selection() {
        let window = Window::new();
        window.ui.set_selected_track_key("row-0".into());
        let mut navigation = crate::app_state::Navigation::new();

        crate::apply_search_edit(&window.ui, &mut navigation, "ab".into());

        assert_eq!(window.ui.get_selected_track_key(), "");
        assert!(window.ui.get_search_active(), "the edit still turns the Search view on at once");
        assert_eq!(navigation.search_query(), "ab");
    }

    /// The selection goes with the list it was made in, so only an edit that changes the list on screen
    /// forgets it: a refined query does, an edit that leaves the effective query (trimmed, case- and
    /// accent-folded, what `Library` matches on) as it was does not.
    #[test]
    fn a_search_edit_forgets_the_selection_only_when_the_list_on_screen_can_change() {
        let window = Window::new();
        let mut navigation = crate::app_state::Navigation::new();
        let select = |window: &Window| window.ui.set_selected_track_key("row-0".into());

        select(&window);
        for no_op in ["", " ", "   ", ""] {
            crate::apply_search_edit(&window.ui, &mut navigation, no_op.into());
            assert_eq!(window.ui.get_selected_track_key(), "row-0", "{no_op:?} in an empty field changes nothing on screen");
            assert!(!window.ui.get_search_active());
        }

        crate::apply_search_edit(&window.ui, &mut navigation, "ab".into());
        assert_eq!(window.ui.get_selected_track_key(), "", "the first real query is a new list");

        select(&window);
        for same_list in ["ab ", " ab", "AB", "ab"] {
            crate::apply_search_edit(&window.ui, &mut navigation, same_list.into());
            assert_eq!(window.ui.get_selected_track_key(), "row-0", "{same_list:?} matches exactly what `ab` matches");
            assert!(window.ui.get_search_active());
        }

        crate::apply_search_edit(&window.ui, &mut navigation, "abc".into());
        assert_eq!(window.ui.get_selected_track_key(), "", "a refined query clears the selection, even when the table stays");

        select(&window);
        crate::apply_search_edit(&window.ui, &mut navigation, "".into());
        assert_eq!(window.ui.get_selected_track_key(), "", "clearing a real query swaps the Search view for the one underneath");
        assert!(!window.ui.get_search_active());
    }

    /// Wires `search-edited` to `apply_search_edit` the way `main()` does, with a `Navigation` of its own.
    fn wire_search_edits(window: &Window) {
        let navigation = Rc::new(std::cell::RefCell::new(crate::app_state::Navigation::new()));
        let weak = window.ui.as_weak();
        window.ui.on_search_edited(move |query| {
            if let Some(ui) = weak.upgrade() {
                crate::apply_search_edit(&ui, &mut navigation.borrow_mut(), query.to_string());
            }
        });
    }

    /// The Songs table with `row-0` selected and the (empty) search field focused, as after clicking a
    /// row and Tabbing into the field: the library-only state where Space plays the selection.
    fn songs_with_a_selected_row_and_the_search_field_focused() -> Window {
        let window = Window::new();
        window.ui.set_library_empty(false);
        window.ui.set_view(View::Songs);
        window.ui.set_nav_view(View::Songs);
        window.ui.set_songs(rows(5));
        window.frame();
        wire_search_edits(&window);
        // Tab, "q", Backspace: the field ends up focused and empty. The edits are real, so this
        // happens before the selection is made.
        window.focus_search_with_tab();
        window.ui.set_selected_track_key("row-0".into());
        assert_eq!(highlighted_rows(&window.settled_frame()).len(), 1, "precondition: the selected row is highlighted");
        window
    }

    /// README: "Esc ... gives Space back to playback". Esc in a field with nothing to clear must only
    /// do that, exactly like Return: the Songs list on screen did not change, so neither may the row
    /// the user clicked in it.
    #[test]
    fn esc_in_an_empty_search_field_keeps_the_selected_row() {
        let window = songs_with_a_selected_row_and_the_search_field_focused();

        window.press(Key::Escape);

        assert_eq!(window.ui.get_selected_track_key(), "row-0");
        assert_eq!(highlighted_rows(&window.settled_frame()).len(), 1, "the highlight stays");
        window.press(" ");
        assert_eq!(window.spaces(), 1, "Esc handed the keyboard back to the window scope");
    }

    #[test]
    fn a_lone_space_typed_into_the_empty_search_field_keeps_the_selected_row() {
        let window = songs_with_a_selected_row_and_the_search_field_focused();

        window.press(" ");

        assert_eq!(window.ui.get_search_query(), " ", "the space is typed into the field");
        assert!(!window.ui.get_search_active(), "a whitespace-only query does not switch views");
        assert_eq!(window.ui.get_selected_track_key(), "row-0");
        assert_eq!(highlighted_rows(&window.settled_frame()).len(), 1);
        assert_eq!(window.spaces(), 0);
    }

    #[test]
    fn esc_that_clears_a_real_query_still_forgets_the_selected_row() {
        let window = songs_with_a_selected_row_and_the_search_field_focused();
        window.type_text("ab");
        window.ui.set_selected_track_key("row-0".into());

        window.press(Key::Escape);

        assert_eq!(window.ui.get_search_query(), "");
        assert_eq!(window.ui.get_selected_track_key(), "", "clearing a query swaps the list, so the selection goes");
    }

    // -- the Output combo box must not keep the keyboard -----------------------------------------

    /// The window with the details panel's Output `ComboBox` focused, and how many times an
    /// `output-chosen` was seen. Found by clicking down the panel column until an arrow key (which
    /// only a focused `ComboBox` turns into a selection) produces one.
    fn window_with_the_output_combo_focused() -> (Window, Rc<Cell<u32>>) {
        let window = Window::new();
        window.ui.set_library_empty(false);
        let chosen = Rc::new(Cell::new(0));
        let counted = Rc::clone(&chosen);
        window.ui.on_output_chosen(move |_| counted.set(counted.get() + 1));
        window.frame();
        let x = WIDTH as f32 - 150.0;
        let mut y = 60.0;
        while y < HEIGHT as f32 - 100.0 {
            window.click(x, y);
            window.press(Key::DownArrow);
            if chosen.get() > 0 {
                // The click opened the dropdown; close it the way a user does, leaving the keyboard on the box.
                window.press(Key::Escape);
                return (window, chosen);
            }
            y += 4.0;
        }
        panic!("no click down the details panel reached the Output combo box");
    }

    /// Lets the hand-back timer (`MainWindow.focus-handback`) run, as the event loop would.
    fn wait_for_the_focus_handback(window: &Window) {
        std::thread::sleep(std::time::Duration::from_millis(120));
        window.frame();
    }

    #[test]
    fn space_still_toggles_after_the_details_panel_is_hidden_with_the_output_combo_focused() {
        let (window, _chosen) = window_with_the_output_combo_focused();
        window.ui.set_right_panel_visible(false);
        window.frame();
        window.press(" ");
        assert_eq!(
            window.spaces(),
            1,
            "an invisible focus item is dropped at the key press and Space would reach nobody; the panel must give the keyboard back as it hides"
        );
    }

    #[test]
    fn hiding_the_details_panel_does_not_take_the_keyboard_from_the_search_field() {
        let window = Window::new();
        window.ui.set_library_empty(false);
        window.frame();
        window.focus_search_with_tab();
        window.type_text("a");
        window.ui.set_right_panel_visible(false);
        window.frame();
        window.type_text(" b");
        assert_eq!(window.ui.get_search_query(), "a b", "the user is still typing in the search field");
        assert_eq!(window.spaces(), 0);
    }

    #[test]
    fn choosing_an_output_hands_the_keyboard_back_to_the_window_scope() {
        let (window, chosen) = window_with_the_output_combo_focused();
        // The box still holds the keyboard: another arrow key is another selection.
        window.press(Key::DownArrow);
        let while_focused = chosen.get();
        window.press(Key::DownArrow);
        assert!(chosen.get() > while_focused, "precondition: the focused combo box turns arrow keys into selections");

        wait_for_the_focus_handback(&window);
        let after_handback = chosen.get();
        window.press(Key::DownArrow);
        assert_eq!(chosen.get(), after_handback, "the combo box no longer holds the keyboard once the hand-back ran");
        window.press(" ");
        assert_eq!(window.spaces(), 1, "and Space reaches the window scope");
    }
}

/// What the OS Now Playing widget mirrors is the snapshot `apply_now_playing_projection` returns
/// (`PanelNowPlaying`), not a fresh projection: the panel keeps the tags and cover of a playing track
/// that "Remove from Library" took out of the library, and the widget has to keep matching it. Uses
/// the same offscreen window as the snapshots; no `MediaSession` is ever built here.
#[cfg(test)]
mod now_playing_panel_tests {
    use super::*;
    use crate::app_state::{AppState, project_now_playing};
    use crate::audio::AudioInfo;
    use crate::library::{ArtworkPixels, ArtworkSource, TrackKey, TrackRecord, album_key};

    #[test]
    fn the_widget_snapshot_is_what_the_panel_shows_and_outlives_the_album_leaving_the_library() {
        let _ = slint::platform::set_platform(Box::new(SnapshotPlatform));
        let _window = new_window();
        let ui = MainWindow::new().unwrap();
        let mut state = AppState::new();
        let key = TrackKey::whole_file(PathBuf::from("/nas/album/track.flac"));
        let info = AudioInfo {
            sample_rate: 44_100,
            duration_ms: Some(200_000),
            source_channels: 2,
            bits_per_sample: 16,
            is_float: false,
            integer_pcm: true,
            format: "FLAC".into(),
        };
        let mut record = TrackRecord::minimal(key.clone(), info.clone());
        record.tags.title = Some("Real Title".into());
        record.tags.artist = Some("Real Artist".into());
        record.tags.album = Some("Real Album".into());
        state.library.upsert(record);
        let album = album_key(state.library.get(&key).unwrap());
        state.cache_artwork(&album, &ArtworkPixels { width: 2, height: 2, rgb: vec![200; 12] }, ArtworkSource::Embedded, None);
        let fallback = ("track.flac".to_owned(), "album".to_owned());

        let shown = crate::apply_now_playing_projection(&ui, &state, &key, &info, &fallback);

        assert_eq!((shown.title.as_str(), shown.artist.as_str(), shown.album.as_str()), ("Real Title", "Real Artist", "Real Album"));
        assert_eq!(ui.get_now_playing_title(), shown.title.as_str(), "the widget's title is the panel's");
        assert_eq!(ui.get_now_playing_artist(), shown.artist.as_str());
        assert_eq!(ui.get_now_playing_album(), shown.album.as_str());
        let artwork = shown.artwork.as_ref().expect("the panel shows a cover, so the widget gets it too");
        assert!(ui.get_now_playing_has_art());
        assert_eq!(Some(artwork.revision), state.artwork_revision_of(&key), "the cover carries the revision it had when projected");

        // The album leaves the library while its track plays. Nothing re-projects the panel (the
        // `Scanned` handler skips a track that is gone), so the panel and the snapshot both keep the
        // real tags and cover...
        state.remove_album(&album);
        assert_eq!(ui.get_now_playing_title(), "Real Title");
        assert_eq!(shown.title, "Real Title");
        assert!(shown.artwork.is_some());
        // ...whereas a fresh projection now would be the file-name fallback with no cover, which is
        // what the widget used to be handed on every publish.
        let fresh = project_now_playing(&state, &key, &info, &fallback);
        assert_eq!((fresh.title.as_str(), fresh.artist.as_str()), ("track.flac", "Unknown Artist"));
        assert!(fresh.art.is_none());
        assert_eq!(state.artwork_revision_of(&key), None);
    }
}

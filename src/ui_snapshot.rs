//! Dev-only offscreen snapshot harness (`§6` Stage 5): renders `MainWindow` with Slint's software
//! renderer to PNG files, so a reviewer can see the UI shell without launching the app, playing
//! audio or touching CoreAudio. This module never constructs `AudioPlayer` and never opens a
//! device. All the sample data below lives only here; `ui/*.slint` never sees it (R3).
//!
//! Run: `LIME_SNAPSHOT_DIR=<dir> cargo test --bin lime-player render_snapshots -- --ignored`.
//! Skips, with a message, when `LIME_SNAPSHOT_DIR` is unset — so a normal `cargo test` run never
//! needs a writable directory or touches the filesystem for this.

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

/// A neutral, obviously-fake `TrackRowData` for the Queue snapshot. Names are placeholders like
/// "Sample Album One", never anything resembling the deleted mock catalog (R3).
fn sample_row(key: &str, number: &str, title: &str, album: &str, badge: &str, duration: &str) -> TrackRowData {
    TrackRowData {
        key: key.into(),
        number: number.into(),
        title: title.into(),
        artist: "Sample Artist".into(),
        album: album.into(),
        year: "2024".into(),
        duration: duration.into(),
        badge: badge.into(),
        hi_res: false,
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

fn sample_album(key: &str, title: &str, artist: &str, subtitle: &str, art: Option<(u32, u32, [u8; 3])>) -> AlbumCardData {
    let (art, has_art) = sample_art(art);
    AlbumCardData { key: key.into(), title: title.into(), artist: artist.into(), subtitle: subtitle.into(), art, has_art }
}

fn sample_album_header(key: &str, title: &str, artist: &str, meta: &str, art: Option<(u32, u32, [u8; 3])>) -> AlbumHeaderData {
    let (art, has_art) = sample_art(art);
    AlbumHeaderData { key: key.into(), title: title.into(), artist: artist.into(), meta: meta.into(), art, has_art }
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

    ui.set_queue_current(sample_row("sample-1", "1", "Sample Track One", "Sample Album One", "FLAC 24/96", "4:05"));
    ui.set_has_queue_current(true);
    ui.set_queue_rows(ModelRc::new(VecModel::from(vec![
        sample_row("sample-2", "2", "Sample Track Two", "Sample Album Two", "FLAC 16/44.1", "3:12"),
        sample_row("sample-3", "3", "Sample Track Three", "Sample Album Two", "MP3", "3:47"),
        sample_row("sample-4", "4", "Sample Track Four", "Sample Album Three", "WavPack 24/96", "5:01"),
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
        .expect("set_platform must succeed: render_snapshots is the only test in this binary that installs one");

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
            sample_album("album-warm-colors", "Warm Colors", "Nina Path", "2021 · 12 tracks", Some((64, 64, [196, 120, 68]))),
            sample_album("album-cold-shapes", "Cold Shapes", "Other Artist", "2019 · 9 tracks", None),
        ])));
        ui.set_recent_albums(ModelRc::new(VecModel::from(vec![
            sample_album("album-night-drive", "Night Drive", "The Collective", "2024 · 8 tracks", Some((64, 64, [80, 140, 190]))),
            sample_album("album-paper-trail", "Paper Trail", "Solo Artist", "2023 · 5 tracks", None),
            sample_album("album-wide-open", "Wide Open", "Nina Path", "2022 · 10 tracks", Some((64, 64, [150, 90, 200]))),
            sample_album("album-low-tide", "Low Tide", "Other Artist", "2021 · 7 tracks", None),
            sample_album("album-fault-lines", "Fault Lines", "The Collective", "2020 · 11 tracks", Some((64, 64, [90, 170, 120]))),
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
            sample_album("album-warm-colors", "Warm Colors", "Nina Path", "2021 · 12 tracks", Some((64, 64, [196, 120, 68]))),
            sample_album("album-cold-shapes", "Cold Shapes", "Other Artist", "2019 · 9 tracks", None),
        ])));
        ui.set_recent_albums(ModelRc::new(VecModel::from(vec![
            sample_album("album-night-drive", "Night Drive", "The Collective", "2024 · 8 tracks", Some((64, 64, [80, 140, 190]))),
            sample_album("album-paper-trail", "Paper Trail", "Solo Artist", "2023 · 5 tracks", None),
            sample_album("album-wide-open", "Wide Open", "Nina Path", "2022 · 10 tracks", Some((64, 64, [150, 90, 200]))),
            sample_album("album-low-tide", "Low Tide", "Other Artist", "2021 · 7 tracks", None),
            sample_album("album-fault-lines", "Fault Lines", "The Collective", "2020 · 11 tracks", Some((64, 64, [90, 170, 120]))),
        ])));
        ui.window().show().unwrap();
        render_scenario(&window, &dir, "home-unified-titlebar");
        ui.window().hide().unwrap();
    }

    // Scenario 1d: the Albums grid (`§6` Stage 6), same art-mix rule as Home's shelves.
    {
        let window = new_window();
        let ui = MainWindow::new().unwrap();
        ui.set_view(View::Albums);
        ui.set_nav_view(View::Albums);
        ui.set_library_empty(false);
        ui.set_albums(ModelRc::new(VecModel::from(vec![
            sample_album("album-warm-colors", "Warm Colors", "Nina Path", "2021 · 12 tracks", Some((64, 64, [196, 120, 68]))),
            sample_album("album-cold-shapes", "Cold Shapes", "Other Artist", "2019 · 9 tracks", None),
            sample_album("album-night-drive", "Night Drive", "The Collective", "2024 · 8 tracks", Some((64, 64, [80, 140, 190]))),
            sample_album("album-paper-trail", "Paper Trail", "Solo Artist", "2023 · 5 tracks", None),
            sample_album("album-wide-open", "Wide Open", "Nina Path", "2022 · 10 tracks", Some((64, 64, [150, 90, 200]))),
            sample_album("album-low-tide", "Low Tide", "Other Artist", "2021 · 7 tracks", None),
        ])));
        ui.window().show().unwrap();
        render_scenario(&window, &dir, "albums-grid");
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

    // Scenario 1f: the Songs table with its summary line (`§6` Stage 6), one Hi-Res row included.
    {
        let window = new_window();
        let ui = MainWindow::new().unwrap();
        ui.set_view(View::Songs);
        ui.set_nav_view(View::Songs);
        ui.set_library_empty(false);
        ui.set_songs_summary("128 songs · 9 h 12 min".into());
        ui.set_songs(ModelRc::new(VecModel::from(vec![
            TrackRowData { hi_res: true, ..sample_row("song-1", "1", "Sunrise", "Warm Colors", "FLAC 24/96", "4:12") },
            sample_row("song-2", "2", "Nightfall", "Cold Shapes", "MP3", "3:47"),
            sample_row("song-3", "3", "Turnaround", "Night Drive", "WavPack 24/96", "5:01"),
            sample_row("song-4", "4", "Outro", "Warm Colors", "FLAC 16/44.1", "2:58"),
        ])));
        ui.window().show().unwrap();
        render_scenario(&window, &dir, "songs-table");
        ui.window().hide().unwrap();
    }

    // Scenario 1g: album detail (`§6` Stage 6): the header band with its four large buttons, a
    // multi-disc album (disc-dash-track numbering), a currently playing row, the back chevron
    // (`can-go-back`), Hi-Res badges (placement right after the title), and a long, classical-style
    // artist credit (eliding instead of overflowing the page, `§6` Stage 6 fix).
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
        ));
        ui.set_now_playing_key("track-3".into());
        ui.set_album_tracks(ModelRc::new(VecModel::from(vec![
            TrackRowData { hi_res: true, ..sample_row("track-1", "1-01", "Intro", "Double Feature", "FLAC 24/96", "3:02") },
            sample_row("track-2", "1-02", "Drifting", "Double Feature", "FLAC 24/96", "5:47"),
            sample_row("track-3", "2-01", "Turnaround", "Double Feature", "FLAC 24/96", "4:18"),
            TrackRowData { hi_res: true, ..sample_row("track-4", "2-02", "Outro", "Double Feature", "FLAC 24/96", "5:25") },
        ])));
        ui.window().show().unwrap();
        render_scenario(&window, &dir, "album-detail");
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
            TrackRowData { hi_res: true, ..sample_row("song-1", "1", "Sunrise", "Warm Colors", "FLAC 24/96", "4:12") },
            sample_row("song-2", "2", "Wide Open", "Wide Open", "FLAC 16/44.1", "3:58"),
            sample_row("song-3", "3", "Nina's Theme", "Night Drive", "WavPack 24/96", "5:01"),
            sample_row("song-4", "4", "Outro", "Warm Colors", "FLAC 16/44.1", "2:58"),
        ])));
        ui.set_search_albums_header("Albums (2)".into());
        ui.set_search_albums(ModelRc::new(VecModel::from(vec![
            sample_album("album-warm-colors", "Warm Colors", "Nina Path", "2021 · 12 tracks", Some((64, 64, [196, 120, 68]))),
            sample_album("album-wide-open", "Wide Open", "Nina Path", "2022 · 10 tracks", Some((64, 64, [150, 90, 200]))),
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

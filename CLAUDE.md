# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Lime Player is a Rust + Slint desktop audio player prototype focused on bit-exact playback of local or already-mounted NAS files (FLAC, WavPack, WAV, MP3) to an explicitly selected output device. `README.md` is the source of truth for playback scope and known limits; read it before changing audio behavior.

## Commands

Requires Rust 1.92+ (Slint 1.18 baseline) and CMake 3.5+ with a C/C++ toolchain (the `wavpack` crate builds bundled libwavpack). `.cargo/config.toml` sets `CMAKE_POLICY_VERSION_MINIMUM=3.5` so that build works under CMake 4.

```sh
cargo run                                   # debug build + launch
cargo test                                  # all tests (inline unit tests only)
cargo test --bin lime-player <test_name>    # single test (binary-only crate, no lib target)
LIME_SNAPSHOT_DIR=<dir> cargo test --bin lime-player render_snapshots -- --ignored
                                             # dev-only offscreen UI snapshots to PNG (src/ui_snapshot.rs); skipped
                                             # with a message when LIME_SNAPSHOT_DIR is unset, so plain `cargo test`
                                             # never needs a writable directory for this
./scripts/package-macos-app.sh [dest.app]   # release build → ad-hoc-signed bundle, default dist/Lime Player.app
```

There is no rustfmt/clippy config; defaults apply. Tests are `#[cfg(test)]` modules inside `src/` files (`tests/` only holds decoder fixtures). No test touches real audio hardware or CoreAudio devices; `view_model::ui_contract` tests do string checks against `ui/*.slint` via `include_str!`, while `main.rs` tests cover the pending-seek/timeline helpers.

The Finder/`open` launch of the packaged app is the manual regression check for the async native file panel (see README).

## Architecture

`ui/app.slint` is compiled by `slint-build` in `build.rs`; it imports design tokens and every icon (`Theme`, `Icons`) from `ui/theme.slint` and shared widgets plus the Slint-side data contract types (`ui/widgets.slint`). `src/main.rs` wires Slint callbacks to `AudioPlayer`, the library scanner, and `AppState`, and polls both `PlaybackEvent`s and `LibraryEvent`s. All audio logic lives in `src/audio/`; the session library lives in `src/library/`.

### Threads and communication

- **UI thread** (`main.rs`): Slint event loop. `on_*` callbacks send `Command`s to the player; a 40 ms repeating `slint::Timer` drains `PlaybackEvent`s with `try_recv()` and updates UI properties. The file picker is RFD's async panel scheduled on Slint's event loop — never a synchronous `runModal()` inside a callback.
- **Controller thread** `lime-audio-controller` (`PlaybackWorker::run` in `player.rs`): owns queue, lookahead, and active stream state; polls `Command`s via crossbeam with a short timeout.
- **Library scanner thread** `lime-library-scanner` (`LibraryScanner`, `src/library/scanner.rs`): probes newly opened paths (`audio::probe_file`) and reads their tags/lyrics/artwork (`audio::metadata`) off both the UI and controller threads. The controller never calls `probe_file` or `read_metadata` itself outside tests (via `fixture_track`) — every `PreparedTrack` that reaches `enqueue`/`play_next`/`ReplaceQueue` was already probed by the scanner. A later `scan()` request's phase 1 (probe) always runs before an earlier request's remaining phase 2 (tags/artwork), so a large first batch over a slow NAS share never blocks a later "Open Files…" from enqueuing and playing promptly; each request still gets its own `BatchDone` once its own phase 2 finishes.
- **Decoder threads** `lime-audio-decoder` (`spawn_track_preparation`): one for the current track plus at most one speculative lookahead, plus up to `MAX_RETIRING_DECODERS` (4) retiring ones — a stop never joins its decoder inline (a NAS read may block), so it moves to `retiring_decoders` for `reap_retiring_decoders` to reclaim. Every immediate-stop path (`seek_to`, `replace_queue`, `previous_track`, `restart_current_from_queue`) refuses once that cap is hit rather than piling up unbounded blocked threads. They push `PcmSample`s into an `rtrb` SPSC ring buffer (`RING_SAMPLES`).
- **CPAL output callback** (`build_stream`): pops from the ring buffer and communicates back only through atomics (buffered count, eof/drained/output_failed flags, consumed-sample counter, diagnostics). Keep it real-time safe: no locks, no allocation, no blocking.

### Session library and view layer

- **`src/library/`** — the session library (`mod.rs`): every opened track, keyed by path, grouped into albums and artists; the decoded values themselves are session-only, rebuilt by re-scanning at each startup. `scanner.rs` is the `lime-library-scanner` thread above, plus `LibraryScanner::scan_folder`, which spawns a dedicated `lime-library-folder-walk` thread per folder scan so a slow NAS walk never blocks other in-flight requests. `walker.rs` is the pure, cancellation-safe recursive directory walk "Open Folder…" and the startup folder rescan use. `store.rs` persists *which* files/folders to re-scan (`library.json`, see "Persistent library" above) — never the scanned content. `format.rs` is pure, I/O-free display-string formatting (track/album subtitles, clock strings, library summaries) shared by `app_state.rs` and `view_model.rs`.
- **`src/app_state.rs`** — UI-thread state: the session `Library` and its decoded-artwork cache, `Navigation` (view stack, back button, artist/album filters, and the live search query — see "Search" below), and pure now-playing/pending-volume helpers.
- **`src/view_model.rs`** — pure projections from `Library`/`AppState`/`QueueTrackSnapshot` values to the generated Slint structs (no Slint window, no I/O), plus the `.slint`-source contract tests (`mod ui_contract`), e.g. no mock catalog strings remain, every icon reference resolves to a file on disk, and every typography token is used consistently.
- **`src/ui_snapshot.rs`** (test-only) — renders `MainWindow` offscreen with Slint's software renderer to PNG files from synthetic data local to the module; never constructs `AudioPlayer` or opens a device. See Commands above for the exact invocation.

### Search

Typing in the top-bar search field shows a dedicated Search view (`SearchResultsView` in `ui/app.slint`) instead of whatever `view` the sidebar last selected, with three sections — Songs (a plain, non-virtualized `TrackTable`, since it shares one `ScrollView` with the other two sections and so cannot own a `ListView`; capped at 200 rows, `SEARCH_SONGS_LIMIT` in `view_model.rs`), Albums (the usual `AlbumGrid`, capped at 48, `SEARCH_ALBUMS_LIMIT`) and Artists (`ArtistListRow`, shared with `ArtistsView`, uncapped) — each hidden when empty and headed by a count or a "Showing first N of M" note (`format_search_section_header`, `library/format.rs`) so a cap never truncates silently. All matching is case- and accent-insensitive (`normalize_for_search`/`track_matches_query` in `library/mod.rs`) across title, artist, album, album artist and genre.

`Navigation` (`app_state.rs`) owns the query, not `main.rs`: `search_active()` (query non-empty after trimming) is what the Slint side actually gates on (`MainWindow.search-active`, pushed by `sync_navigation`/`on_search_edited`), and `current` (the view/nav_view/artist_filter/album_key underneath) is never touched by the query — so clearing it (Esc, the pill's own "x", editing back to empty) always lands back on exactly whatever was already current, with no separate "previous view" to restore. `nav_selected`/`album_opened`/`artist_opened` all clear the query as part of navigating, so opening an album or artist from the Search view's own results actually navigates there instead of leaving the Search view stuck on top of it.

### Playback pipeline and output routes

Library scanner: opened path → `decoder::probe_file` → `PreparedTrack` → `AudioPlayer::enqueue`/`play_next`/`replace_queue`. Controller: `PreparedTrack` → `start_track_prepared` (resolve device, Hog lease, set nominal rate, wait for prebuffer, `build_stream`, re-verify physical format, `play()`).

The route is decided only by `uses_strict_integer_route(&AudioInfo)` (`player.rs`), called from `spawn_track_preparation` and `start_track_prepared`; never inline the predicate again — besides its own definition and tests, `rg -n uses_strict_integer_route src` must show only those two call sites:
- **Strict integer route** (macOS, integer PCM): samples are left-aligned into I32, the CPAL stream is requested as I32 at the exact source rate, and playback is refused unless CoreAudio's physical stream reads back as exact-rate packed signed 32-bit stereo LPCM. Mono is refused, not upmixed.
- **Float fallback** (float WavPack, MP3, non-macOS): F32 stream, labeled "not bit-perfect" in the UI.

### macOS CoreAudio (`src/audio/coreaudio.rs`)

A `cfg(target_os = "macos")` `platform` module does raw `AudioObject*PropertyData` FFI; a non-macOS stub returns errors. It provides `HogLease` (RAII; `Drop` releases only if this process still owns the device), `set_and_verify_sample_rate` (set + readback), and physical ASBD verification/selection. CPAL only reports the virtual callback format and may renegotiate the physical format while building a stream, which is why the physical format is re-checked after `build_stream` and just before `play()`.

### Track transitions

`reconcile_lookahead` pre-decodes the next queued track; `preparation_for_start` reuses it when the path still matches. `advance_if_drained` tears down the stream only after the callback reports `drained` and `OUTPUT_DRAIN_GUARD` (90 ms) has elapsed, then the next track builds a fresh stream. Every track gets its own CPAL stream because each callback owns its track's ring-buffer consumer and atomics — same-rate gapless handoff would require redesigning that ownership. Shutdown cancels lookahead first and uses `join_decoder_until` with a grace period because NAS reads can block in non-interruptible syscalls.

`start_track_prepared` (macOS) also primes the DAC before building the real stream whenever `needs_dac_prime` detects a rate, physical-format, or route change (strict integer ↔ float) versus the last stream this same device actually had active — including a route change the HAL's own before/after `build_stream` snapshot can miss, since the float route never pins an exact physical format. Priming plays a throwaway zero-filling stream on the same device/config briefly, then pauses and drops it, to reproduce the "press play again" workaround that reliably clears a DAC left on the previous track's clock/route. `LIME_DAC_PRIME_SETTLE_MS` controls the settle time on each side of that stream (default 150 ms; `0` disables priming). The real stream that follows a primed start writes a silent pre-roll before the first track sample so the track's audio isn't lost while the DAC un-mutes/relocks; `LIME_DAC_PREROLL_MS` controls its duration (default 1000 ms, clamped 0..=3000, `0` disables it).

### Seek

`AudioPlayer::seek(path, position_ms)` sends `Command::Seek`; the controller's `seek_to` computes the target decoder start frame (`seek_target_frame`, clamped to the track's known duration — a file that never reported a duration refuses any nonzero seek), stops the active stream (`stop_active_now`), and rebuilds it through the same `spawn_track_preparation` / `start_track_prepared` path a fresh track uses, except pinned (`StartOptions::pinned_output`) to the *same* output device, Hog lease, and nominal rate the track was already using — a seek can never trigger a device change or a rate renegotiation; a successful seek reuses the existing Hog lease unchanged, but a failed seek (`fail_seek`) releases it and leaves the track blocked at the queue head, since the restart itself never happened. A stale path (the track already advanced), a skip already pending, a failed decoder/output, or an unsupported duration are all refused via `reject_seek`, which emits `SeekRejected` plus the current `Timeline` so the UI clears any pending seek state immediately instead of showing a scrubber stuck mid-drag.

### Diagnostic vs Status events

`PlaybackEvent` carries both a developer-only `Diagnostic(String)` and a user-facing `Status(String)`. `main.rs` prints every `Diagnostic` to stderr with a `DIAG` prefix and never surfaces it in the UI; `Status` updates the UI's status line. The controller routes plumbing signal-flag changes (decoder/callback nonzero-sample flags) as `Diagnostic`, unconditionally whenever they change (`emit_audio_diagnostics_if_changed`); a buffer underrun is a real, user-visible degradation, so it is instead a rate-limited `Status` (`emit_underrun_status_if_due`) so it stays visible without spamming on every poll.

### Decoder exit guard and prebuffer timeout

`DecoderExitGuard` is held for the whole life of every decoder thread and sets `eof` on any exit — a normal return, an `Err`, or an unwinding panic — so `wait_until_prebuffered` and the drain path can never hang on a decoder that stopped without reaching its usual return path; a panic that is not a deliberate cancel also marks the track failed with an internal-error message. Before a stream is built, the controller waits up to `PREBUFFER_TIMEOUT` (30 s; `wait_until_prebuffered_within`) for the ring buffer to fill enough to start playback; a NAS spin-up slower than that refuses the start with an error instead of blocking indefinitely, and the user can retry.

### Device volume

`AudioPlayer::set_volume` sets the *output device's own* CoreAudio volume (`coreaudio::set_device_volume`), never an internal software gain — there is no per-track or master software volume anywhere in the pipeline. The target device is the active output when a track is playing, falling back to the currently selected-but-not-yet-playing device (`volume_target_uid`); a device with no settable volume control disables the slider with an explanation instead of silently doing nothing. On a device with independent left/right elements, `scale_channel_volumes` scales both channels by the same ratio so the slider preserves the device's existing balance rather than forcing it to equal channels — except when both channels already read zero, where there is nothing to scale a ratio from and the new level is applied to both channels equally (see README "Known limits").

### Settings

`src/settings.rs` persists `Preferences { output_device_id, hog_mode_enabled }` as JSON at `ProjectDirs::from("com", "Lime Player", "Lime Player")` config dir `/settings.json`. Load/save errors are reported, not fatal.

### Persistent library

`src/library/store.rs` persists `LibrarySources { files, folders }` as JSON at the same `ProjectDirs` config dir, `/library.json`, written atomically (temp file + rename). It records only *which paths* to re-scan — individually opened files and added folders — never decoded tags, durations or artwork; the `Library`/`TrackRecord` values built from them stay session-only and are rebuilt every startup by re-running those paths through `LibraryScanner`, tagged `BatchKind::StartupRestore` (`main.rs`) so the restore only fills the library and never enqueues or plays anything. A path that fails to probe (e.g. an unmounted NAS share) is kept in `library.json` regardless — nothing ever prunes an entry — and is surfaced as one aggregate status line ("N saved tracks are unavailable…") instead of a per-file one. A `library.json` load failure is reported the same way and never triggers an overwrite of the file. "Open Folder…" (top bar, next to Open Files) recursively walks a chosen directory off the UI thread (`src/library/walker.rs`, via `LibraryScanner::scan_folder` on its own thread so it never blocks other in-flight scans), skipping hidden/`._` files, directories named `*.wv`, and symlink loops, and accepting the same extensions as Open Files; found tracks are added to the library only, never enqueued, and the folder is persisted for re-walking at the next startup.

### Unified title bar (macOS)

`slint`'s `unstable-winit-030` feature (`Cargo.toml`) is enabled unconditionally, but only used under `cfg(target_os = "macos")` in `main.rs`. Before `MainWindow::new()`, `slint::BackendSelector::new().with_winit_window_attributes_hook(...)` hides the native title bar (`WindowAttributesExtMacOS::with_titlebar_transparent`/`with_title_hidden`/`with_fullsize_content_view`) so the content view extends to the top of the window; `MainWindow.title` stays `"Lime Player"` (macOS still uses it for Mission Control and the Window menu). `window.set_unified_title_bar(true)` then drives `ui/app.slint`'s `MainWindow.unified-title-bar` property, which only exists on macOS — every other platform leaves it `false` and keeps Slint's normal decorated window, reserving no traffic-light space. When `true`, `Sidebar` reserves its top 52 px for the traffic lights and `TopBar` shows the window title on that same row; both carry their own drag `TouchArea` over that empty space (`window-drag-requested`, handled in `main.rs` via `slint::winit_030::WinitWindowAccessor::with_winit_window` and winit's `Window::drag_window()`), and `TopBar`'s doubles as a double-click zoom (`window-zoom-requested`, `Window::set_maximized`), matching a native title bar. All real buttons and the search field sit in the same tree after these `TouchArea`s, so they still receive clicks. The offscreen snapshot harness (`src/ui_snapshot.rs`) renders one Home scenario with `unified-title-bar: true` for visual review; the software renderer never draws native traffic lights, so only the reserved space and alignment can be checked that way.

## Invariants

- Never change the macOS system-wide default output device; always target the selected CPAL device.
- Prefer refusing playback with an error over silently degrading: no hidden sample-rate fallback, no format conversion in the strict route, no playing WavPack hybrid (`.wvc`) streams, no acquiring Hog mode from another owner.
- The 90 ms guard is software-side only. Do not claim verified hardware drain, bit-perfect USB output, or a <200 ms rate transition — those are unverified on device (see README "Known limits").
- The library's *decoded* contents (tags, durations, artwork) are session-only and rebuilt by re-scanning at each startup; only the set of source paths (`library.json`, individually opened files and added folders) is persisted across restarts.

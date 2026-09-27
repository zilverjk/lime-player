# Lime Player

Lime Player is an early Rust + Slint desktop player. The UI keeps the dense, dark three-column library layout of the reference while using Lime branding and neutral artwork. Opening files, or a whole folder via **Open Folder…**, builds a library (Home, Albums, Artists, Songs, Recently Added, album detail, and the queue) from the files' own tags; the set of opened files and added folders is persisted (`library.json`) and re-scanned at the next startup, but the decoded tags/artwork themselves are not — they are rebuilt by that rescan, off the UI thread, without enqueuing or playing anything. Playback routes decoded audio to an explicitly selected output device; a click-drag seek restarts the decoder at the target position on the same device, at the same rate — a successful seek reuses the existing Hog lease, while a failed seek releases it and leaves the track blocked at the queue head; the volume slider sets the selected output device's own CoreAudio volume control (`kAudioDevicePropertyVolumeScalar`) when the device exposes a settable one, with no internal software gain, though whether the device applies it in the analog or digital domain is not verified; and Lime does not read cover art embedded in WAV files (only RIFF INFO tags are read there), while FLAC, MP3, and WavPack tags can carry it and a folder cover image still applies to any format.

## Playback scope

- Decodes FLAC, WavPack (`.wv` / `.wavpack`), WAV, and MP3. WavPack is a required format and uses the `wavpack` crate's bundled libwavpack. WavPack hybrid tracks and correction files (`.wvc`) are unsupported: the player detects hybrid mode or a matching `.wvc` sidecar and refuses playback instead of silently decoding the incomplete lossy `.wv` stream.
- The native file picker accepts local paths and paths on already-mounted network shares (for example `/Volumes/AudioShare`). Lime Player does not mount SMB/NFS shares or manage NAS credentials; mount the share in macOS first.
- On macOS, file selection uses RFD's asynchronous native open panel, scheduled on Slint's UI event loop rather than calling synchronous `NSOpenPanel.runModal()` inside a Slint callback.
- The output selector targets the chosen CPAL device, not macOS's global default output. The app does not set the system-wide default device.
- On macOS, Hog mode defaults on and can be disabled with the Hog mode switch. Lime acquires Hog mode only when the device is free and releases only a lease it acquired itself. If another process owns the DAC or the device rejects a sample-rate request, playback reports an error rather than silently falling back.
- On macOS, integer FLAC/WAV/WavPack PCM uses a strict selected-device route: decoded signed samples are left-aligned into 32-bit integer frames, sent through a raw I32 CPAL callback, and playback is refused unless the selected CoreAudio output stream reads back as exact-rate, packed signed little-endian 32-bit stereo LPCM. This supports exact 16-, 24-, and 32-bit source words without a float conversion; mono-to-stereo mapping is refused in this route.
- Floating-point WavPack, MP3, and integer PCM on non-macOS currently use the float output path; the UI labels it “not bit-perfect”. This is an explicit fallback and is not promoted as bit-perfect playback.
- On macOS, a track is pre-decoded into a bounded buffer, the selected device's nominal rate is set/read back, the output stream is configured for that exact source rate, and the device physical stream rate is checked before the stream starts. Queued files are probed in advance; rate changes happen only after the software PCM ring buffer is depleted and a 90 ms guard elapses. This is **not verified hardware drain**: the app does not yet confirm CoreAudio/DAC render position or that the device has physically rendered the final old-rate frame. Actual render confirmation and the user's <200 ms transition target remain unverified.

## Known limits

- The strict integer route verifies the selected CoreAudio device's physical ASBD and sends raw I32 callback values, but it does not yet verify actual USB payloads or the DAC's final rendered sample with a loopback/hardware measurement; do not treat that as end-to-end bit-perfect proof.
- Actual FiiO K11 Hog-mode acquisition, automatic-rate switching, CoreAudio/DAC render completion, transition timing, and isolation from other applications still require on-device testing. The 90 ms guard is software-side only; <200 ms is not verified. No global output device is changed by the code.
- NAS playback can only be verified after the user mounts the share. Unreadable or disconnected paths should be reported by the file/decoder path.
- The library's decoded contents (tags, durations, artwork) are session-only and rebuilt by re-scanning at each startup; only the list of opened files and added folders persists (`library.json`), and a path that fails to re-scan (e.g. an unmounted NAS share) is kept in that list rather than dropped. The queue can only be replaced wholesale (enqueue, play next, or a full replace) — there is no per-item reorder or removal. Continuous shuffle/repeat modes and Favorites/Liked Songs appear in the UI but are disabled; a one-off Shuffle action (Home, an album) is implemented.
- `ReplaceQueue` (double-clicking a row in Songs, Recently Added, album detail, or Queue, or an album/Home Play or Shuffle action) and `Previous` both stop the currently playing stream immediately, without waiting for the output to drain. Opening files via **Open Files…** appends the selection to the queue instead, and starts it only when nothing is already playing. A sample-rate change on the *same* output device still waits for the existing 90 ms software drain guard before the new rate is set; a different device, or an unchanged rate, does not wait.
- The current decoder slice supports mono/stereo only; it rejects files with more channels.
- On a per-channel (stereo-volume) device, dragging the slider to 0 sets both channels to 0; a later set from `(0, 0)` cannot recover the previous L/R balance and returns to equal channels instead. This matches the spec's own pinned test case and is a known limitation pending a spec decision on preserving balance through zero.
- The right-hand details panel (`NowPlayingPanel`: artwork, format line, lyrics) is hidden below a 1400 px window width, or whenever the user toggles it off. At that width, the one-line playback status (including buffer underrun reports) still shows in the player bar's info column, and the panel-toggle icon in the player bar opens a small popup with the same output-device selector and Hog mode switch — so choosing an output device and seeing errors never require widening the window.

## Build and run

Requirements:

- Rust 1.92 or newer (Slint 1.18 requires this baseline).
- CMake 3.5+ and a C/C++ toolchain for the bundled libwavpack build. On macOS, install/enable Xcode Command Line Tools; Homebrew CMake works as well.

Then run:

```sh
cargo run
```

To build a Finder-launchable local macOS app bundle:

```sh
./scripts/package-macos-app.sh
open "dist/Lime Player.app"
```

The script creates an ad-hoc-signed (not Developer ID signed, not notarized) development bundle for local testing only.

The app icon source is [`assets/icon.svg`](assets/icon.svg); the bundle uses the committed `assets/AppIcon.icns`. After editing the SVG, regenerate the `.icns` with `./scripts/generate-app-icon.sh` (requires `brew install resvg`).

The repository config sets `CMAKE_POLICY_VERSION_MINIMUM=3.5` to allow the older bundled WavPack CMake files to build under CMake 4. The UI is compiled from [`ui/app.slint`](ui/app.slint) by `slint-build` in `build.rs`.

## macOS file-open regression check

Run the packaged app from Finder (or with `open`) so the native panel runs in the real windowed app context. Click **Open Files**, select one mounted FLAC or WavPack file, and confirm the app stays alive, the queue updates, and playback begins when an output is selected. Repeat with multiple files, then cancel the dialog and confirm no track is enqueued. This checks the asynchronous open-panel path; it does not validate DAC output fidelity or Hog/rate transitions.

## License

Lime Player is licensed under the GNU General Public License v3.0 only (`GPL-3.0-only`); see [`LICENSE`](LICENSE). It uses [Slint](https://slint.dev) under Slint's GPLv3 option, so forks and redistributed builds must also remain GPLv3.

Bundled icons in `ui/icons/` include Lucide-derived artwork under the ISC license; see [`ui/icons/LICENSE-lucide.txt`](ui/icons/LICENSE-lucide.txt).

## Contributing

Issues and pull requests are welcome. Before opening a PR, run `cargo build` and `cargo test`; for UI changes, also render the offscreen snapshots (`LIME_SNAPSHOT_DIR=<dir> cargo test --bin lime-player render_snapshots -- --ignored`) and include before/after images. Contributions are accepted under the project's GPL-3.0-only license.

//! `lime-library-scanner`: the background thread that probes opened files and reads their tags
//! and artwork, so neither the UI thread nor the audio controller thread ever blocks on file I/O
//! for that (`§3.3`, `§6` Stage 3 "worker stops probing").

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Cursor;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;

use crossbeam_channel::{Receiver, Sender, unbounded};

use crate::audio::{self, EmbeddedPicture, PreparedTrack};

use super::{ArtworkPixels, ArtworkSource, TrackRecord, album_key, walker};

/// Embedded artwork above this size was already dropped by `audio::metadata::read_metadata`
/// (`§4.4`); folder art is capped again independently below.
const MAX_ART_BYTES: usize = 20 * 1024 * 1024;
/// The longest side of a decoded, cached album picture (`§3.3`).
const ARTWORK_MAX_SIDE: u32 = 400;
/// `image::Limits` applied to every artwork decode, embedded or folder (`§3.3` "Artwork decoding").
const ARTWORK_LIMIT_MAX_DIMENSION: u32 = 8_192;
const ARTWORK_LIMIT_MAX_ALLOC_BYTES: u64 = 128 * 1024 * 1024;

/// Which phase of a scan a `LibraryEvent::Failed` happened in: `main.rs` uses this to decide
/// whether the track was ever added at all (`Probe`) or was already probed, enqueued and added and
/// only its tags/artwork could not be read (`Metadata`) — the two must never be reported, or
/// counted, the same way (`§1.4`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScanFailurePhase {
    /// Phase 1: the file could not be probed, so it never reached the queue or the library.
    Probe,
    /// Phase 2: the file was already probed, enqueued and added; only its tags/artwork failed.
    Metadata,
}

pub enum LibraryEvent {
    /// One event per `scan` request, after phase 1, in request order. `batch` identifies the
    /// request so a later `Failed`/`BatchDone` for the same request can be told apart from one
    /// belonging to a different request whose phase 1 ran in between (`§6`).
    Probed { batch: u64, tracks: Vec<PreparedTrack> },
    // Boxed: `TrackRecord` is much larger than the other variants (`AudioInfo`, tags, artwork
    // pixels), and this event is the highest-volume one during a scan.
    Scanned(Box<TrackRecord>),
    Failed {
        batch: u64,
        path: PathBuf,
        error: String,
        phase: ScanFailurePhase,
    },
    BatchDone {
        batch: u64,
        /// The number of files originally requested in this batch, so a caller can report
        /// "Added {requested - failed} of {requested} files" (`§6`).
        requested: usize,
        // Not consumed yet; a future batch-progress indicator's natural source.
        #[allow(dead_code)]
        count: usize,
    },
    /// Live progress while a folder walk (`LibraryScanner::scan_folder`) is still discovering
    /// files, before any of them enter the ordinary probe/tag pipeline above. `main.rs` uses
    /// `root` to build a "Scanning {root}: {found} tracks found" status line.
    FolderScanProgress { root: PathBuf, found: usize },
}

/// One `scan()`/`scan_folder()` request, tagged with a batch id assigned by the *caller*
/// (`LibraryScanner`'s own `AtomicU64`) rather than by this thread's `run()` loop, so `main.rs` can
/// learn a request's id synchronously — before the request is even sent, and before any file has
/// been walked or probed — and record ahead of time what to do with its `Probed`/`Failed`/
/// `BatchDone` events (enqueue-and-play for "Open Files…", library-only for "Open Folder…" and the
/// startup restore) instead of racing to learn it. `run()` itself is agnostic to why a batch was
/// requested; that bookkeeping lives entirely in `main.rs`.
struct ScanRequest {
    batch: u64,
    paths: Vec<PathBuf>,
}

pub struct LibraryScanner {
    requests: Sender<ScanRequest>,
    events: Receiver<LibraryEvent>,
    /// Kept so `scan_folder`'s walker thread can send its own events (`FolderScanProgress`, and a
    /// missing-root `Failed`) without going through the probe/tag pipeline.
    event_tx: Sender<LibraryEvent>,
    /// Assigns batch ids to both `scan()` and `scan_folder()` requests; shared so both kinds of
    /// batch get distinct ids regardless of which one is called.
    next_batch_id: Arc<AtomicU64>,
    /// Set on `Drop`; polled by any in-flight `scan_folder` walker thread between directories so a
    /// shutdown does not wait for a slow NAS walk to finish (best-effort only — see `walker.rs`).
    cancel_walks: Arc<AtomicBool>,
}

impl LibraryScanner {
    pub fn new() -> Self {
        let (request_tx, request_rx) = unbounded::<ScanRequest>();
        let (event_tx, event_rx) = unbounded::<LibraryEvent>();
        let scanner_event_tx = event_tx.clone();
        thread::Builder::new()
            .name("lime-library-scanner".into())
            .spawn(move || run(request_rx, scanner_event_tx))
            .expect("could not start Lime Player library scanner");
        Self {
            requests: request_tx,
            events: event_rx,
            event_tx,
            next_batch_id: Arc::new(AtomicU64::new(0)),
            cancel_walks: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Assigns a fresh batch id and submits `paths` for probing + tag/artwork reading, returning
    /// the id immediately so the caller can record ahead of time what its events mean.
    pub fn scan(&self, paths: Vec<PathBuf>) -> u64 {
        let batch = self.next_batch_id.fetch_add(1, Ordering::Relaxed);
        let _ = self.requests.send(ScanRequest { batch, paths });
        batch
    }

    /// Recursively walks `root` on a dedicated, cancellation-safe thread (never the scanner's own
    /// probe/tag thread, so a slow NAS folder never delays other in-flight requests), reporting
    /// `FolderScanProgress` as files are discovered, then submits every audio file found through
    /// the same pipeline as `scan`. Returns the batch id immediately, before the walk has even
    /// started, for the same reason `scan` does. A `root` that cannot be read at all (e.g. an
    /// unmounted NAS share) still produces a `Failed`/`BatchDone { requested: 0 }` pair instead of
    /// silently doing nothing.
    pub fn scan_folder(&self, root: PathBuf) -> u64 {
        let batch = self.next_batch_id.fetch_add(1, Ordering::Relaxed);
        let events = self.event_tx.clone();
        let requests = self.requests.clone();
        let cancel = Arc::clone(&self.cancel_walks);
        thread::Builder::new()
            .name("lime-library-folder-walk".into())
            .spawn(move || {
                let mut last_reported = 0usize;
                let files = walker::walk_folder(&root, &cancel, |found| {
                    if found == 1 || found - last_reported >= 20 {
                        last_reported = found;
                        let _ = events.send(LibraryEvent::FolderScanProgress { root: root.clone(), found });
                    }
                });
                if cancel.load(Ordering::Acquire) {
                    return;
                }
                if !std::fs::metadata(&root).is_ok_and(|meta| meta.is_dir()) {
                    // The folder itself could not be read (most commonly a NAS share that is not
                    // currently mounted): report it through the ordinary per-batch failure
                    // bookkeeping (`main.rs`) instead of silently scanning zero files.
                    let _ = events.send(LibraryEvent::Failed {
                        batch,
                        path: root.clone(),
                        error: "not found (is it mounted?)".to_owned(),
                        phase: ScanFailurePhase::Probe,
                    });
                }
                let _ = requests.send(ScanRequest { batch, paths: files });
            })
            .expect("could not start Lime Player folder walk thread");
        batch
    }

    pub fn events(&self) -> Receiver<LibraryEvent> {
        self.events.clone()
    }
}

impl Default for LibraryScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for LibraryScanner {
    fn drop(&mut self) {
        self.cancel_walks.store(true, Ordering::Release);
    }
}

/// One request's phase-2 bookkeeping, tracked while its items sit in `phase2_queue`.
struct BatchState {
    id: u64,
    requested: usize,
    remaining: usize,
    scanned: usize,
}

/// The scanner loop: detached like the audio controller, exits when the request channel
/// disconnects (i.e. when the last `LibraryScanner` is dropped).
///
/// Phase 1 (probe) of a newly arrived request always runs before continuing the phase-2 (tags +
/// artwork) work of an earlier one still in flight, so `Probed` and the queue enqueue for a later
/// "Open Files…" happen promptly instead of waiting for a large first batch's whole phase 2 to
/// finish over a slow NAS share (`§6`). Phase-2 items stay in a single FIFO queue tagged by which
/// batch they belong to, so batches still complete — and `BatchDone` still fires — in arrival order.
fn run(requests: Receiver<ScanRequest>, events: Sender<LibraryEvent>) {
    let mut folder_art_cache: HashMap<PathBuf, Option<PathBuf>> = HashMap::new();
    let mut albums_with_art: HashSet<String> = HashSet::new();
    // Phase-2 work not yet processed, across every request that has run phase 1 so far.
    let mut phase2_queue: VecDeque<PreparedTrack> = VecDeque::new();
    // Batches still in flight, oldest first, matching the prefix grouping of `phase2_queue`.
    let mut batches: VecDeque<BatchState> = VecDeque::new();

    loop {
        if phase2_queue.is_empty() {
            match requests.recv() {
                Ok(request) => enqueue_batch(request, &events, &mut phase2_queue, &mut batches),
                Err(_) => return,
            }
            continue;
        }
        // Never blocks: picks up every request that arrived while phase 2 above was running, so
        // its `Probed` goes out immediately instead of after the older batch's whole phase 2.
        while let Ok(request) = requests.try_recv() {
            enqueue_batch(request, &events, &mut phase2_queue, &mut batches);
        }
        let Some(track) = phase2_queue.pop_front() else { continue };
        let Some(state) = batches.front_mut() else { continue };
        let batch = state.id;
        let scanned = scan_phase2_one(batch, track, &events, &mut folder_art_cache, &mut albums_with_art);
        if scanned {
            state.scanned += 1;
        }
        state.remaining -= 1;
        if state.remaining == 0 {
            let state = batches.pop_front().expect("front just matched above");
            let _ = events.send(LibraryEvent::BatchDone { batch: state.id, requested: state.requested, count: state.scanned });
        }
    }
}

/// Runs phase 1 for one `scan()` request and queues its phase-2 work. A request whose files all
/// failed to probe has nothing left to do, so its `BatchDone { count: 0 }` is sent right away
/// instead of waiting on `phase2_queue`, which would never see any of its items.
fn enqueue_batch(
    request: ScanRequest,
    events: &Sender<LibraryEvent>,
    phase2_queue: &mut VecDeque<PreparedTrack>,
    batches: &mut VecDeque<BatchState>,
) {
    let ScanRequest { batch, paths } = request;
    let requested = paths.len();
    let prepared = probe_phase(batch, &paths, events);
    if prepared.is_empty() {
        let _ = events.send(LibraryEvent::BatchDone { batch, requested, count: 0 });
        return;
    }
    batches.push_back(BatchState { id: batch, requested, remaining: prepared.len(), scanned: 0 });
    phase2_queue.extend(prepared);
}

/// Phase 1: probes every requested path and emits one `Probed` event for the whole batch, so the
/// existing "append the whole selection at once" queue semantics are preserved. Unsupported or
/// unreadable files are skipped (`Failed`) and never reach the queue or the library.
fn probe_phase(batch: u64, paths: &[PathBuf], events: &Sender<LibraryEvent>) -> Vec<PreparedTrack> {
    let prepared = for_each_guarded(batch, paths.to_vec(), events, PathBuf::clone, |path| {
        audio::probe_file(&path).map(|info| PreparedTrack { path, info }).map_err(|error| error.to_string())
    });
    if !prepared.is_empty() {
        let _ = events.send(LibraryEvent::Probed { batch, tracks: prepared.clone() });
    }
    prepared
}

/// Phase 2 for a single track probed in an earlier phase 1, reusing its `AudioInfo` rather than
/// probing again. Sends its own `Scanned` event immediately (so the UI keeps seeing progress
/// across a large batch) and, like `for_each_guarded`, turns a panic into a `Failed` event instead
/// of taking the scanner thread down. Returns whether the scan succeeded, for the caller's
/// per-batch `BatchDone { count }`.
fn scan_phase2_one(
    batch: u64,
    track: PreparedTrack,
    events: &Sender<LibraryEvent>,
    folder_art_cache: &mut HashMap<PathBuf, Option<PathBuf>>,
    albums_with_art: &mut HashSet<String>,
) -> bool {
    let path = track.path.clone();
    match guarded(AssertUnwindSafe(|| {
        let record = scan_details(track, folder_art_cache, albums_with_art);
        let _ = events.send(LibraryEvent::Scanned(Box::new(record)));
        Ok(())
    })) {
        Ok(()) => true,
        Err(error) => {
            let _ = events.send(LibraryEvent::Failed { batch, path, error, phase: ScanFailurePhase::Metadata });
            false
        }
    }
}

/// Runs `f` for every item in `items` through `guarded`, sending `Failed { path, error }` for a
/// panicking or erroring one and continuing with the rest — used by `probe_phase` (`§3.3` "Panic
/// containment"). Returns the successful outputs, in order; `scan_phase2_one` guards a single
/// phase-2 item the same way, since phase 2 runs one file at a time interleaved across batches.
fn for_each_guarded<T, R>(
    batch: u64,
    items: Vec<T>,
    events: &Sender<LibraryEvent>,
    path_of: impl Fn(&T) -> PathBuf,
    mut f: impl FnMut(T) -> Result<R, String>,
) -> Vec<R> {
    let mut results = Vec::with_capacity(items.len());
    for item in items {
        let path = path_of(&item);
        match guarded(AssertUnwindSafe(|| f(item))) {
            Ok(value) => results.push(value),
            Err(error) => {
                let _ = events.send(LibraryEvent::Failed { batch, path, error, phase: ScanFailurePhase::Probe });
            }
        }
    }
    results
}

/// Runs `f`, converting a panic into the same "internal error" message a probe or read failure
/// would produce, so one bad file never takes the scanner thread down (`§3.3` "Panic containment").
fn guarded<T>(f: impl FnOnce() -> Result<T, String> + panic::UnwindSafe) -> Result<T, String> {
    match panic::catch_unwind(f) {
        Ok(result) => result,
        Err(_) => Err("internal error while reading this file".to_owned()),
    }
}

fn scan_details(
    track: PreparedTrack,
    folder_art_cache: &mut HashMap<PathBuf, Option<PathBuf>>,
    albums_with_art: &mut HashSet<String>,
) -> TrackRecord {
    let file_size = std::fs::metadata(&track.path).ok().map(|meta| meta.len());
    let metadata = audio::read_metadata(&track.path).unwrap_or_else(|error| {
        // A write that cannot panic: `eprintln!` unwinds this very `guarded` wrapper (turning an
        // ordinary tag-read error into a lost `Scanned` record) if stderr is a closed pipe —
        // `main.rs`'s `Diagnostic` handler avoids it for the same reason.
        use std::io::Write;
        let _ = writeln!(std::io::stderr().lock(), "DIAG could not read tags for {}: {error}", track.path.display());
        Default::default()
    });

    let mut record = TrackRecord {
        path: track.path,
        info: track.info,
        file_size,
        tags: metadata.tags,
        artwork: None,
        artwork_source: ArtworkSource::None,
        added_seq: 0,
    };

    let key = album_key(&record);
    if !albums_with_art.contains(&key)
        && let Some((pixels, source)) = resolve_artwork(&record.path, metadata.picture.as_ref(), folder_art_cache)
    {
        record.artwork = Some(pixels);
        record.artwork_source = source;
        albums_with_art.insert(key);
    }

    record
}

/// Embedded front cover (or the first embedded picture) first, then folder art
/// (`cover|folder|front|album` + `.jpg/.jpeg/.png`, case-insensitive) — `§1.2` "Artwork".
fn resolve_artwork(
    path: &Path,
    embedded: Option<&EmbeddedPicture>,
    folder_art_cache: &mut HashMap<PathBuf, Option<PathBuf>>,
) -> Option<(ArtworkPixels, ArtworkSource)> {
    if let Some(picture) = embedded
        && let Some(pixels) = decode_artwork(&picture.data)
    {
        return Some((pixels, ArtworkSource::Embedded));
    }
    let folder = path.parent()?.to_path_buf();
    let cover_path = folder_art_cache.entry(folder.clone()).or_insert_with(|| find_folder_art(&folder)).clone()?;
    // Checked before reading: an oversized cover on a NAS share must never be pulled across the
    // network in full just to be thrown away by a length check (`§3.3`, same cap as embedded art).
    if std::fs::metadata(&cover_path).ok()?.len() > MAX_ART_BYTES as u64 {
        return None;
    }
    let bytes = std::fs::read(&cover_path).ok()?;
    let pixels = decode_artwork(&bytes)?;
    Some((pixels, ArtworkSource::Folder))
}

/// The `cover|folder|front|album` stems, in the priority order `§1.2` "Artwork" lists them.
const FOLDER_ART_STEM_PRIORITY: [&str; 4] = ["cover", "folder", "front", "album"];

/// One `read_dir` per folder. Candidates are found by name first, using the cheap file-type bit
/// `read_dir` already returns instead of a separate `stat` per entry; when a folder holds more
/// than one candidate, the `§1.2` stem order wins, then a case-insensitive file name as a
/// deterministic final tie-break.
fn find_folder_art(folder: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(folder).ok()?;
    let mut candidates: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| {
            is_folder_art_name(&entry.path()) && entry.file_type().is_ok_and(|kind| kind.is_file() || kind.is_symlink())
        })
        .map(|entry| entry.path())
        .collect();
    candidates.sort_by_key(|path| folder_art_rank(path));
    candidates.into_iter().next()
}

fn folder_art_rank(path: &Path) -> (usize, String) {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_ascii_lowercase();
    let priority = FOLDER_ART_STEM_PRIORITY.iter().position(|candidate| *candidate == stem).unwrap_or(FOLDER_ART_STEM_PRIORITY.len());
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or_default().to_ascii_lowercase();
    (priority, name)
}

fn is_folder_art_name(path: &Path) -> bool {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { return false };
    let Some(ext) = path.extension().and_then(|s| s.to_str()) else { return false };
    let stem_matches = matches!(stem.to_ascii_lowercase().as_str(), "cover" | "folder" | "front" | "album");
    let ext_matches = matches!(ext.to_ascii_lowercase().as_str(), "jpg" | "jpeg" | "png");
    stem_matches && ext_matches
}

/// Decodes and downscales `bytes` to at most `ARTWORK_MAX_SIDE` px on the longest side, under
/// strict dimension/allocation limits (`§3.3` "Artwork decoding"). Any error, including a limits
/// violation, means no art — never a panic on a hostile or malformed image.
fn decode_artwork(bytes: &[u8]) -> Option<ArtworkPixels> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(ARTWORK_LIMIT_MAX_DIMENSION);
    limits.max_image_height = Some(ARTWORK_LIMIT_MAX_DIMENSION);
    limits.max_alloc = Some(ARTWORK_LIMIT_MAX_ALLOC_BYTES);
    reader.limits(limits);
    let image = reader.decode().ok()?;
    let thumbnail = image.thumbnail(ARTWORK_MAX_SIDE, ARTWORK_MAX_SIDE).to_rgb8();
    let (width, height) = thumbnail.dimensions();
    Some(ArtworkPixels { width, height, rgb: thumbnail.into_raw() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_dir(name: &str) -> PathBuf {
        let suffix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("lime-player-scanner-{name}-{}-{suffix}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fixture_path(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
    }

    fn tiny_png_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([10, 20, 30, 255]));
        let mut bytes = Vec::new();
        image.write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png).unwrap();
        bytes
    }

    /// A real JPEG encode (not a PNG saved under a `.jpg` name): `decode_artwork` sniffs the
    /// format from content, so a mislabeled PNG would never exercise the `image` crate's `jpeg`
    /// feature at all.
    fn tiny_jpeg_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbImage::from_pixel(width, height, image::Rgb([10, 20, 30]));
        let mut bytes = Vec::new();
        image.write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Jpeg).unwrap();
        bytes
    }

    #[test]
    fn artwork_is_downscaled_to_400px_max() {
        let bytes = tiny_png_bytes(1_000, 500);

        let pixels = decode_artwork(&bytes).expect("a well-formed PNG should decode");

        assert!(pixels.width <= ARTWORK_MAX_SIDE);
        assert!(pixels.height <= ARTWORK_MAX_SIDE);
        assert_eq!(pixels.width, ARTWORK_MAX_SIDE);
        assert_eq!(pixels.height, ARTWORK_MAX_SIDE / 2);
    }

    #[test]
    fn artwork_decode_rejects_images_over_dimension_limit() {
        let bytes = tiny_png_bytes(9_000, 1);

        assert!(decode_artwork(&bytes).is_none());
    }

    #[test]
    fn scanner_prefers_embedded_art_then_folder_art() {
        let dir = temp_dir("folder-art");
        let cover_bytes = tiny_jpeg_bytes(10, 10);
        std::fs::write(dir.join("Cover.JPG"), &cover_bytes).unwrap();

        let found = find_folder_art(&dir).expect("a case-insensitively named cover file should be found");
        assert_eq!(found.file_name().unwrap().to_str().unwrap(), "Cover.JPG");

        // An embedded picture, when present, is preferred over folder art.
        let embedded = tiny_png_bytes(20, 20);
        let mut folder_art_cache = HashMap::new();
        let (pixels, source) = resolve_artwork(
            &dir.join("track.flac"),
            Some(&EmbeddedPicture { media_type: None, front_cover: true, data: embedded }),
            &mut folder_art_cache,
        )
        .unwrap();
        assert_eq!(source, ArtworkSource::Embedded);
        // `image::DynamicImage::thumbnail` fits the image to the target box preserving aspect
        // ratio in both directions, so a smaller-than-target square is scaled up to it too.
        assert_eq!(pixels.width, ARTWORK_MAX_SIDE);
        assert_eq!(pixels.height, ARTWORK_MAX_SIDE);

        // With no embedded picture, folder art is used instead — a real JPEG decode, not a PNG
        // wearing a `.jpg` extension.
        let (pixels, source) = resolve_artwork(&dir.join("track.flac"), None, &mut folder_art_cache).unwrap();
        assert_eq!(source, ArtworkSource::Folder);
        assert_eq!(pixels.width, ARTWORK_MAX_SIDE);
        assert_eq!(pixels.height, ARTWORK_MAX_SIDE);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn folder_art_prefers_the_1_2_stem_order_over_byte_sort() {
        let dir = temp_dir("folder-art-priority");
        // Byte/case order would put these first ("F" < "c", and "album" < "cover"); the `§1.2`
        // priority order (cover, folder, front, album) must win instead.
        std::fs::write(dir.join("Folder.jpg"), tiny_jpeg_bytes(4, 4)).unwrap();
        std::fs::write(dir.join("album.jpg"), tiny_jpeg_bytes(4, 4)).unwrap();
        std::fs::write(dir.join("cover.png"), tiny_png_bytes(4, 4)).unwrap();

        let found = find_folder_art(&dir).expect("a cover file should be found");

        assert_eq!(found.file_name().unwrap().to_str().unwrap(), "cover.png");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn scanner_emits_one_probed_event_per_request_in_order_and_failed_for_bad_files() {
        let dir = temp_dir("probe-batch");
        let flac_path = dir.join("a.flac");
        let wav_path = dir.join("b.wav");
        let bad_path = dir.join("c.txt");
        std::fs::copy(fixture_path("decoder-tone.flac"), &flac_path).unwrap();
        std::fs::copy(fixture_path("decoder-tone.wav"), &wav_path).unwrap();
        std::fs::write(&bad_path, b"not audio").unwrap();

        let scanner = LibraryScanner::new();
        let events = scanner.events();
        scanner.scan(vec![flac_path.clone(), wav_path.clone(), bad_path.clone()]);

        // The bad file sorts last, so phase 1 emits its `Failed` inline before the batch's single
        // `Probed`, which only goes out once the whole request has been walked (`§3.3`) — collect
        // both instead of assuming either arrives first.
        let mut probed_tracks: Option<Vec<PreparedTrack>> = None;
        let mut failed_paths = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline && (probed_tracks.is_none() || failed_paths.is_empty()) {
            match events.recv_timeout(Duration::from_millis(200)) {
                Ok(LibraryEvent::Probed { tracks, .. }) => probed_tracks = Some(tracks),
                Ok(LibraryEvent::Failed { path, .. }) => failed_paths.push(path),
                _ => {}
            }
        }

        let tracks = probed_tracks.expect("a Probed event should arrive");
        assert_eq!(tracks.iter().map(|t| t.path.clone()).collect::<Vec<_>>(), vec![flac_path.clone(), wav_path.clone()]);
        assert_eq!(failed_paths, vec![bad_path]);

        // A second `scan()` request must still produce its own `Probed` event, arriving after the
        // first (`§3.3` "one Probed event per scan request, in request order").
        let second_path = dir.join("second.flac");
        std::fs::copy(fixture_path("decoder-tone.flac"), &second_path).unwrap();
        scanner.scan(vec![second_path.clone()]);
        let mut second_probed: Option<Vec<PreparedTrack>> = None;
        let second_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < second_deadline && second_probed.is_none() {
            if let Ok(LibraryEvent::Probed { tracks, .. }) = events.recv_timeout(Duration::from_millis(200)) {
                second_probed = Some(tracks);
            }
        }
        assert_eq!(
            second_probed.expect("a second Probed event should arrive").iter().map(|t| t.path.clone()).collect::<Vec<_>>(),
            vec![second_path]
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Tests `for_each_guarded` directly, per `§3.3`'s Stage 3 test list: a per-file closure that
    /// panics through the same `catch_unwind` wrapper `probe_phase` uses (`scan_phase2_one` guards
    /// a single item the same way) must still emit `Failed` for the panicking item and keep
    /// processing the rest.
    #[test]
    fn for_each_guarded_reports_failed_and_continues_with_the_rest() {
        let (event_tx, event_rx) = unbounded::<LibraryEvent>();
        let panics = PathBuf::from("/nas/album/panics.flac");
        let ok = PathBuf::from("/nas/album/ok.flac");

        let results = for_each_guarded(7, vec![panics.clone(), ok.clone()], &event_tx, PathBuf::clone, |path| {
            if path == panics {
                panic!("simulated failure reading a file");
            }
            Ok(path)
        });

        assert_eq!(results, vec![ok]);
        match event_rx.try_recv() {
            Ok(LibraryEvent::Failed { batch, path, error, phase }) => {
                assert_eq!(batch, 7);
                assert_eq!(path, panics);
                assert_eq!(error, "internal error while reading this file");
                assert_eq!(phase, ScanFailurePhase::Probe, "for_each_guarded backs probe_phase only");
            }
            Ok(_) => panic!("expected a Failed event"),
            Err(_) => panic!("expected a Failed event to have been sent"),
        }
        assert!(event_rx.try_recv().is_err(), "no other event should have been sent");
    }

    /// Drives `run` itself through its own request/event channels (not a hand-rolled
    /// re-implementation of one of its loop iterations) to prove a later request's phase 1 does not
    /// wait for an earlier one's remaining phase-2 work (`§6`): this partly reproduces the R4
    /// symptom ("added tracks show up nowhere") on a slow NAS scan. Both requests are queued before
    /// `run` ever starts pulling from the channel, so the second is always already waiting by the
    /// time `run`'s loop drains `requests.try_recv()` between the first batch's two phase-2 items —
    /// the second batch's `Probed` must arrive before the first batch's `BatchDone`.
    #[test]
    fn a_later_requests_probe_runs_before_an_earlier_batchs_remaining_phase_two() {
        let dir = temp_dir("interleave-live");
        let first_a = dir.join("first-a.flac");
        let first_b = dir.join("first-b.flac");
        let second = dir.join("second.flac");
        std::fs::copy(fixture_path("decoder-tone.flac"), &first_a).unwrap();
        std::fs::copy(fixture_path("decoder-tone.flac"), &first_b).unwrap();
        std::fs::copy(fixture_path("decoder-tone.flac"), &second).unwrap();

        let (request_tx, request_rx) = unbounded::<ScanRequest>();
        let (event_tx, event_rx) = unbounded::<LibraryEvent>();
        request_tx.send(ScanRequest { batch: 0, paths: vec![first_a.clone(), first_b.clone()] }).unwrap();
        request_tx.send(ScanRequest { batch: 1, paths: vec![second.clone()] }).unwrap();
        let worker = thread::spawn(move || run(request_rx, event_tx));

        let mut first_probed = false;
        let mut second_probed = false;
        let mut first_batch_done = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline && !first_batch_done {
            match event_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(LibraryEvent::Probed { batch: 0, tracks }) => {
                    assert!(!first_probed, "the first batch's Probed must only arrive once");
                    first_probed = true;
                    assert_eq!(tracks.iter().map(|t| t.path.clone()).collect::<Vec<_>>(), vec![first_a.clone(), first_b.clone()]);
                }
                Ok(LibraryEvent::Probed { batch: 1, tracks }) => {
                    assert!(first_probed, "the second batch's Probed must arrive after the first batch's own Probed");
                    assert!(!first_batch_done, "the second batch's Probed must arrive before the first batch's BatchDone");
                    second_probed = true;
                    assert_eq!(tracks.iter().map(|t| t.path.clone()).collect::<Vec<_>>(), vec![second.clone()]);
                }
                Ok(LibraryEvent::BatchDone { batch: 0, .. }) => {
                    assert!(second_probed, "the first batch's BatchDone must not arrive before the second batch's Probed");
                    first_batch_done = true;
                }
                Ok(LibraryEvent::Probed { batch, .. }) => panic!("unexpected batch id {batch}"),
                Ok(_) => {}
                Err(_) => {}
            }
        }
        assert!(first_probed, "expected the first batch's Probed");
        assert!(second_probed, "expected the second batch's Probed");
        assert!(first_batch_done, "expected the first batch's BatchDone");

        // Disconnects the request channel so `run` returns once it drains the second batch's own
        // phase-2 work, letting the worker thread join cleanly instead of leaking a live thread.
        drop(request_tx);
        worker.join().expect("the scanner loop must not panic");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `scan_folder` must walk off its own thread, report progress, and end with the same
    /// `Probed`/`BatchDone` shape a plain `scan()` of the same files would (`main.rs` relies on
    /// this to reuse its ordinary batch bookkeeping for folder scans).
    #[test]
    fn scan_folder_reports_progress_then_probes_and_completes_like_a_plain_scan() {
        let dir = temp_dir("scan-folder");
        std::fs::create_dir_all(dir.join("Album")).unwrap();
        std::fs::copy(fixture_path("decoder-tone.flac"), dir.join("Album/a.flac")).unwrap();
        std::fs::copy(fixture_path("decoder-tone.wav"), dir.join("Album/b.wav")).unwrap();
        std::fs::write(dir.join("Album/notes.txt"), b"not audio").unwrap();

        let scanner = LibraryScanner::new();
        let events = scanner.events();
        let batch = scanner.scan_folder(dir.clone());

        let mut probed: Option<Vec<PathBuf>> = None;
        let mut batch_done = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline && !batch_done {
            match events.recv_timeout(Duration::from_millis(200)) {
                Ok(LibraryEvent::Probed { batch: probed_batch, tracks }) => {
                    assert_eq!(probed_batch, batch);
                    probed = Some(tracks.into_iter().map(|t| t.path).collect());
                }
                Ok(LibraryEvent::BatchDone { batch: done_batch, requested, .. }) => {
                    assert_eq!(done_batch, batch);
                    assert_eq!(requested, 2, "notes.txt must be filtered out before it ever reaches the scanner");
                    batch_done = true;
                }
                _ => {}
            }
        }

        let mut probed = probed.expect("a Probed event should arrive for the folder's audio files");
        probed.sort();
        assert_eq!(probed, vec![dir.join("Album/a.flac"), dir.join("Album/b.wav")]);
        assert!(batch_done, "expected BatchDone for the folder scan");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A folder root that does not exist (an unmounted NAS share) must be reported through the
    /// ordinary `Failed`/`BatchDone` pipeline instead of silently doing nothing.
    #[test]
    fn scan_folder_reports_a_missing_root_instead_of_doing_nothing() {
        let dir = temp_dir("scan-folder-missing").join("does-not-exist");

        let scanner = LibraryScanner::new();
        let events = scanner.events();
        let batch = scanner.scan_folder(dir.clone());

        let mut failed = false;
        let mut batch_done = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline && !batch_done {
            match events.recv_timeout(Duration::from_millis(200)) {
                Ok(LibraryEvent::Failed { batch: failed_batch, path, .. }) => {
                    assert_eq!(failed_batch, batch);
                    assert_eq!(path, dir);
                    failed = true;
                }
                Ok(LibraryEvent::BatchDone { batch: done_batch, requested, .. }) => {
                    assert_eq!(done_batch, batch);
                    assert_eq!(requested, 0);
                    batch_done = true;
                }
                _ => {}
            }
        }

        assert!(failed, "expected a Failed event for the missing root");
        assert!(batch_done, "expected BatchDone even for a missing root");
    }
}

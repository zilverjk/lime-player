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

use crate::audio::{self, AudioInfo, EmbeddedPicture, PreparedTrack, TrackTags};

use super::cue::{CueSheet, CueTrack, ResolvedCueFile, cue_time_to_sample_frame, dedupe_cue_sheets, parse_cue, resolve_cue_files};
use super::walker::has_cue_extension;
use super::{ArtworkPixels, ArtworkSource, TrackKey, TrackRecord, album_key, walker};

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
    /// A `.cue` sheet could not be parsed or its `FILE`s could not be resolved to real audio on
    /// disk; treated like `Probe` (the cue-derived tracks never reached the queue or the library),
    /// but reported with its own phase so a caller can tell the two failure kinds apart if needed.
    /// The claimed-would-be files fall through to ordinary whole-file scanning instead.
    Cue,
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
        /// The real resulting track count after cue expansion: every ordinary whole-file track
        /// successfully scanned, plus every cue-derived track successfully scanned (which can be
        /// more or fewer than the number of physical files a cue sheet claimed). `main.rs` uses
        /// this, not `requested`, for "Added {count} tracks" (`CLAUDE.md` "Wrong track count in the
        /// Added N tracks status").
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
    /// Whether `paths` is itself the user's complete, explicit selection (an "Open Files…" pick),
    /// rather than the flattened result of something larger (a folder walk, or the startup restore
    /// of previously-opened items). Controls `expand_cue_sheets_for_paths`'s claim scope
    /// (`CLAUDE.md` "Over-broad CUE claim on Open Files"): only an explicit-selection batch
    /// restricts cue-derived tracks to physical files actually present in `paths`, so opening one
    /// file out of a many-file "per-track" cue never silently pulls in its unselected siblings.
    /// Every other batch kind keeps expanding every cue sheet found in a touched directory in
    /// full, since every file in that directory was already implicitly requested.
    explicit_selection: bool,
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
    ///
    /// `explicit_selection` must be `true` only when `paths` is itself the user's complete,
    /// explicit pick (an "Open Files…" dialog selection) — never for the startup restore of
    /// previously-opened individual files/cue sheets, which behaves like every file in the
    /// relevant directory was already implicitly requested (`ScanRequest::explicit_selection`).
    pub fn scan(&self, paths: Vec<PathBuf>, explicit_selection: bool) -> u64 {
        let batch = self.next_batch_id.fetch_add(1, Ordering::Relaxed);
        let _ = self.requests.send(ScanRequest { batch, paths, explicit_selection });
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
                // A folder walk's own files are never an "explicit selection" (`CLAUDE.md` "Over-
                // broad CUE claim on Open Files"): every file the walk found was already implicitly
                // requested by opening the folder, so cue expansion stays full-directory.
                let _ = requests.send(ScanRequest { batch, paths: files, explicit_selection: false });
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

/// One item of phase-2 work: either an ordinary whole-file track (one `TrackRecord` from one
/// `read_metadata` call), or every track of one cue-claimed physical file, sharing a single
/// `read_metadata` call between them (`CLAUDE.md` scanner two-phase invariant, cue-sheet fix A).
enum Phase2Item {
    WholeFile(PreparedTrack),
    CueFile(CuePhase2File),
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
    let mut phase2_queue: VecDeque<Phase2Item> = VecDeque::new();
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
        let Some(item) = phase2_queue.pop_front() else { continue };
        let Some(state) = batches.front_mut() else { continue };
        let batch = state.id;
        let scanned = match item {
            Phase2Item::WholeFile(track) => {
                usize::from(scan_phase2_one(batch, track, &events, &mut folder_art_cache, &mut albums_with_art))
            }
            Phase2Item::CueFile(file) => scan_cue_phase2_one(batch, file, &events, &mut folder_art_cache),
        };
        state.scanned += scanned;
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
///
/// Before ordinary whole-file probing runs, every directory represented in `paths` is checked for
/// `.cue` sheets (`expand_cue_sheets_for_paths`) — whether the `.cue` itself was directly opened or
/// simply sits next to a requested audio file. Files claimed by a surviving, resolved sheet are
/// excluded from whole-file scanning below; the cue-derived tracks are already fully scanned (tags/
/// artwork read once per physical file during expansion), so they skip phase 2 entirely.
fn enqueue_batch(
    request: ScanRequest,
    events: &Sender<LibraryEvent>,
    phase2_queue: &mut VecDeque<Phase2Item>,
    batches: &mut VecDeque<BatchState>,
) {
    let ScanRequest { batch, paths, explicit_selection } = request;
    let requested = paths.len();

    let (cue_prepared, cue_phase2_files, claimed_paths) = expand_cue_sheets_for_paths(batch, &paths, explicit_selection, events);
    let remaining_paths: Vec<PathBuf> =
        paths.into_iter().filter(|path| !claimed_paths.contains(&canonical_or_self(path)) && !has_cue_extension(path)).collect();

    if !cue_prepared.is_empty() {
        let _ = events.send(LibraryEvent::Probed { batch, tracks: cue_prepared });
    }

    let ordinary_prepared = probe_phase(batch, &remaining_paths, events);

    let remaining_items = cue_phase2_files.len() + ordinary_prepared.len();
    if remaining_items == 0 {
        let _ = events.send(LibraryEvent::BatchDone { batch, requested, count: 0 });
        return;
    }
    batches.push_back(BatchState { id: batch, requested, remaining: remaining_items, scanned: 0 });
    phase2_queue.extend(cue_phase2_files.into_iter().map(Phase2Item::CueFile));
    phase2_queue.extend(ordinary_prepared.into_iter().map(Phase2Item::WholeFile));
}

/// Phase 1: probes every requested path and emits one `Probed` event for the whole batch, so the
/// existing "append the whole selection at once" queue semantics are preserved. Unsupported or
/// unreadable files are skipped (`Failed`) and never reach the queue or the library.
fn probe_phase(batch: u64, paths: &[PathBuf], events: &Sender<LibraryEvent>) -> Vec<PreparedTrack> {
    let prepared = for_each_guarded(batch, paths.to_vec(), events, PathBuf::clone, |path| {
        audio::probe_file(&path).map(|info| PreparedTrack { key: TrackKey::whole_file(path), info }).map_err(|error| error.to_string())
    });
    if !prepared.is_empty() {
        let _ = events.send(LibraryEvent::Probed { batch, tracks: prepared.clone() });
    }
    prepared
}

// ---------------------------------------------------------------------------------------------
// CUE sheet detection and expansion (`CLAUDE.md` CUE-sheet playback support)
// ---------------------------------------------------------------------------------------------

/// One physical file a surviving, resolved cue sheet claims, carried from phase 1 (probed, its
/// track boundaries known) into phase 2 — mirrors how an ordinary whole-file `PreparedTrack`
/// carries state from `probe_phase` into `scan_details`, but batches every track sharing one
/// physical file behind a single `read_metadata` call there (`CLAUDE.md` scanner two-phase
/// invariant, cue-sheet fix A: cue expansion must not do phase-2-grade work in phase 1).
struct CuePhase2File {
    resolved_path: PathBuf,
    /// This file's own tracks, in cue-sheet order: the `TrackKey` already computed from `INDEX 01`/
    /// the next track's own `INDEX 01` (or `None` for the file's last track), the raw `CueTrack`
    /// `merge_cue_tags` needs, and this track's own `AudioInfo` (the whole-file probe, with
    /// `duration_ms` already narrowed to this sub-range).
    tracks: Vec<(TrackKey, CueTrack, AudioInfo)>,
    /// The sheet's own album-level fields (`TITLE`/`PERFORMER`/`REM GENRE`/`REM DATE`), shared by
    /// every track of every `FILE` the sheet resolved to; `Arc`'d so a multi-`FILE` sheet (e.g. one
    /// `.wv` per LP side) clones it once, not once per file.
    sheet: Arc<CueSheet>,
}

/// Whether `a` and `b` name the same file on disk: canonicalized where possible, falling back to a
/// raw comparison when either side can't be canonicalized (e.g. the path doesn't exist). A macOS/
/// APFS volume is typically case- and Unicode-normalization-insensitive, so a cue sheet's `FILE`
/// value in a different case/normalization than the real on-disk name still resolves to the same
/// canonical path (`CLAUDE.md` "Claimed-file path matching is a raw PathBuf equality", fix C).
fn canonical_or_self(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Groups `paths` by parent directory and, for each distinct directory represented, looks for
/// `.cue` sheets in it: a single, non-recursive `read_dir` per directory — never recursive itself,
/// matching however the walker already recurses into subfolders, so each directory the walker or a
/// direct "Open Files" selection touches gets its own independent check. A sheet that fails to
/// parse or resolve is reported as a `LibraryEvent::Failed { phase: ScanFailurePhase::Cue, .. }`
/// and simply ignored — its would-be-claimed files fall through to ordinary whole-file scanning.
/// Sheets that parse and resolve are deduped per directory via `cue::dedupe_cue_sheets`.
///
/// Phase 1 only: every resolved `FILE` is probed (`audio::probe_file`, needed for `sample_rate` to
/// compute `TrackKey`s), but tags/artwork are never read here — that is deferred to phase 2
/// (`build_cue_track_records`, via the returned `CuePhase2File`s), exactly like an ordinary
/// whole-file track's own tag/artwork read only happens once its `PreparedTrack` reaches
/// `scan_details` (`CLAUDE.md` scanner two-phase invariant).
///
/// `explicit_selection` restricts which resolved files are claimed at all (`CLAUDE.md` "Over-broad
/// CUE claim on Open Files", fix B): when `true`, a resolved file is only claimed/expanded if its
/// own path, or the cue sheet's own path, is present in `paths` — so opening one file out of a
/// many-file "per-track" cue via "Open Files…" never silently pulls in its unselected siblings,
/// while opening the one file of a single-`FILE` "image" cue (or the `.cue` sheet itself) still
/// expands normally, since that file — or the sheet naming it — really was requested. When `false`
/// (a folder walk or the startup restore), every resolved file in a touched directory is claimed,
/// as today.
///
/// Returns the phase-1 `PreparedTrack`s for the `Probed` event, the phase-2 carry state
/// (`CuePhase2File`), and the set of claimed physical file paths (canonicalized where possible, see
/// `canonical_or_self`) the caller excludes from ordinary whole-file scanning.
fn expand_cue_sheets_for_paths(
    batch: u64,
    paths: &[PathBuf],
    explicit_selection: bool,
    events: &Sender<LibraryEvent>,
) -> (Vec<PreparedTrack>, Vec<CuePhase2File>, HashSet<PathBuf>) {
    let mut directories: Vec<PathBuf> = Vec::new();
    for path in paths {
        if let Some(dir) = path.parent() {
            let dir = dir.to_path_buf();
            if !directories.contains(&dir) {
                directories.push(dir);
            }
        }
    }

    let requested_canonical: HashSet<PathBuf> =
        if explicit_selection { paths.iter().map(|path| canonical_or_self(path)).collect() } else { HashSet::new() };

    let mut prepared = Vec::new();
    let mut phase2_files = Vec::new();
    let mut claimed = HashSet::new();

    for dir in directories {
        let cue_paths = find_cue_files_in_dir(&dir);
        if cue_paths.is_empty() {
            continue;
        }
        let mut resolved_sheets: Vec<(PathBuf, CueSheet, Vec<ResolvedCueFile>)> = Vec::new();
        for cue_path in cue_paths {
            let bytes = match std::fs::read(&cue_path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    let _ = events.send(LibraryEvent::Failed { batch, path: cue_path, error: error.to_string(), phase: ScanFailurePhase::Cue });
                    continue;
                }
            };
            let sheet = match parse_cue(&bytes) {
                Ok(sheet) => sheet,
                Err(error) => {
                    let _ = events.send(LibraryEvent::Failed { batch, path: cue_path, error: error.to_string(), phase: ScanFailurePhase::Cue });
                    continue;
                }
            };
            let resolved = match resolve_cue_files(&cue_path, &sheet) {
                Ok(resolved) => resolved,
                Err(error) => {
                    let _ = events.send(LibraryEvent::Failed { batch, path: cue_path, error, phase: ScanFailurePhase::Cue });
                    continue;
                }
            };
            resolved_sheets.push((cue_path, sheet, resolved));
        }
        if resolved_sheets.is_empty() {
            continue;
        }
        let dedupe_input: Vec<(PathBuf, Vec<ResolvedCueFile>)> =
            resolved_sheets.iter().map(|(path, _sheet, resolved)| (path.clone(), resolved.clone())).collect();
        let keep_indices = dedupe_cue_sheets(&dedupe_input);
        for index in keep_indices {
            let (cue_path, sheet, resolved) = &resolved_sheets[index];
            expand_one_cue_sheet(
                batch,
                cue_path,
                sheet,
                resolved,
                explicit_selection,
                &requested_canonical,
                events,
                &mut prepared,
                &mut phase2_files,
                &mut claimed,
            );
        }
    }

    (prepared, phase2_files, claimed)
}

/// A plain, single `read_dir` for `.cue` files directly inside `dir` (never recursive). Sorted for
/// a deterministic processing order; a directory that cannot be read yields no cue sheets, exactly
/// like a directory with none.
fn find_cue_files_in_dir(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut found: Vec<PathBuf> =
        entries.flatten().map(|entry| entry.path()).filter(|path| path.is_file() && has_cue_extension(path)).collect();
    found.sort();
    found
}

/// Phase-1 outcome of probing one resolved `FILE` of a cue sheet.
enum CueFileOutcome {
    /// `audio::probe_file` failed: reported via `LibraryEvent::Failed` once the whole sheet is
    /// confirmed well-formed (`CLAUDE.md` "Silent data loss when a claimed file fails to probe",
    /// fix D) — the file is still claimed, so it is never also attempted as a whole-file track.
    ProbeFailed { path: PathBuf, error: String },
    /// Probed successfully, with every track's `TrackKey`/`AudioInfo` already computed and
    /// confirmed strictly increasing (see `expand_one_cue_sheet`'s monotonicity check, fix F).
    Ready { path: PathBuf, tracks: Vec<(TrackKey, CueTrack, AudioInfo)> },
}

/// Expands one surviving, resolved cue sheet, phase 1 only (probe + `TrackKey` math — no tags/
/// artwork; see `expand_cue_sheets_for_paths`'s own doc comment for the phase split and the
/// `explicit_selection` claim-scope restriction).
///
/// Every resolved `FILE`'s tracks must have strictly increasing `start_frame`s — a track's
/// `end_frame` is simply the next track's own `start_frame` in the same file (`None` for the file's
/// last track, decode to EOF) — so a hand-edited or corrupt sheet whose `INDEX 01`s do not advance
/// would otherwise silently clamp to a zero-length "blip" track instead of being rejected
/// (`CLAUDE.md` "INDEX monotonicity not validated", fix F). A violation anywhere in the sheet fails
/// the WHOLE sheet the same way a parse/resolve error does: one `Failed { phase: Cue }` for the
/// sheet, nothing claimed, every file in it left to fall back to ordinary whole-file scanning.
///
/// A per-file probe failure (fix D) does not fail the whole sheet — only that one file is refused;
/// its siblings still expand normally — but IS still deferred until every file has been probed and
/// the sheet as a whole confirmed monotonic, so an unrelated file's malformed `INDEX`s can still
/// discard a probe failure that would otherwise need to be reported twice (once here, once again
/// when the file falls back to whole-file scanning).
#[allow(clippy::too_many_arguments)]
fn expand_one_cue_sheet(
    batch: u64,
    cue_path: &Path,
    sheet: &CueSheet,
    resolved_files: &[ResolvedCueFile],
    explicit_selection: bool,
    requested_canonical: &HashSet<PathBuf>,
    events: &Sender<LibraryEvent>,
    prepared: &mut Vec<PreparedTrack>,
    phase2_files: &mut Vec<CuePhase2File>,
    claimed: &mut HashSet<PathBuf>,
) {
    let mut outcomes = Vec::with_capacity(resolved_files.len());
    let mut malformed: Option<String> = None;

    'files: for file in resolved_files {
        let info = match audio::probe_file(&file.resolved_path) {
            Ok(info) => info,
            Err(error) => {
                outcomes.push(CueFileOutcome::ProbeFailed { path: file.resolved_path.clone(), error: error.to_string() });
                continue;
            }
        };
        let sample_rate = info.sample_rate;
        let whole_file_duration_ms = info.duration_ms;
        let mut tracks = Vec::with_capacity(file.tracks.len());
        let mut previous_start: Option<u64> = None;
        for (index, track) in file.tracks.iter().enumerate() {
            let start_frame = cue_time_to_sample_frame(track.index01, sample_rate);
            if let Some(previous) = previous_start
                && start_frame <= previous
            {
                malformed = Some(format!(
                    "TRACK {:02} in \"{}\" does not start after the previous track's own INDEX 01 (cue sheet is not time-ordered)",
                    track.number, file.file_field
                ));
                break 'files;
            }
            previous_start = Some(start_frame);
            let end_frame = file.tracks.get(index + 1).map(|next| cue_time_to_sample_frame(next.index01, sample_rate));
            let key = TrackKey { path: file.resolved_path.clone(), start_frame, end_frame };
            let mut track_info = info.clone();
            track_info.duration_ms = cue_track_duration_ms(start_frame, end_frame, sample_rate, whole_file_duration_ms);
            tracks.push((key, track.clone(), track_info));
        }
        outcomes.push(CueFileOutcome::Ready { path: file.resolved_path.clone(), tracks });
    }

    if let Some(message) = malformed {
        let _ = events.send(LibraryEvent::Failed { batch, path: cue_path.to_path_buf(), error: message, phase: ScanFailurePhase::Cue });
        return;
    }

    let sheet_arc = Arc::new(sheet.clone());
    for outcome in outcomes {
        match outcome {
            CueFileOutcome::ProbeFailed { path, error } => {
                claimed.insert(canonical_or_self(&path));
                let _ = events.send(LibraryEvent::Failed { batch, path, error, phase: ScanFailurePhase::Probe });
            }
            CueFileOutcome::Ready { path, tracks } => {
                if explicit_selection {
                    let included = requested_canonical.contains(&canonical_or_self(&path))
                        || requested_canonical.contains(&canonical_or_self(cue_path));
                    if !included {
                        continue;
                    }
                }
                claimed.insert(canonical_or_self(&path));
                for (key, _track, info) in &tracks {
                    prepared.push(PreparedTrack { key: key.clone(), info: info.clone() });
                }
                phase2_files.push(CuePhase2File { resolved_path: path, tracks, sheet: Arc::clone(&sheet_arc) });
            }
        }
    }
}

/// Phase 2 for one cue-claimed physical file: reads its tags/artwork exactly once (`§3.3`
/// invariant: one `read_metadata` per physical file regardless of how many cue tracks share it —
/// `CLAUDE.md` scanner two-phase invariant, fix A), then builds one `TrackRecord` per track sharing
/// it. Sends its own `Scanned` events immediately, same as `scan_phase2_one`, and turns a panic into
/// a `Failed` event instead of taking the scanner thread down. Returns how many `Scanned` events
/// were actually sent, for the caller's per-batch `BatchDone { count }` (`CLAUDE.md` "Wrong track
/// count in the Added N tracks status", fix E).
fn scan_cue_phase2_one(
    batch: u64,
    file: CuePhase2File,
    events: &Sender<LibraryEvent>,
    folder_art_cache: &mut HashMap<PathBuf, Option<PathBuf>>,
) -> usize {
    let path = file.resolved_path.clone();
    match guarded(AssertUnwindSafe(|| Ok::<_, String>(build_cue_track_records(file, folder_art_cache)))) {
        Ok(records) => {
            let count = records.len();
            for record in records {
                let _ = events.send(LibraryEvent::Scanned(Box::new(record)));
            }
            count
        }
        Err(error) => {
            let _ = events.send(LibraryEvent::Failed { batch, path, error, phase: ScanFailurePhase::Metadata });
            0
        }
    }
}

/// Reads `file`'s tags/artwork once and builds one `TrackRecord` per track sharing it, tag-merged
/// via `merge_cue_tags`.
///
/// Artwork (`CLAUDE.md` "Artwork duplication per cue track", fix H): `AppState::apply_scanned` only
/// ever consults the album-level artwork cache (keyed by `album_key`, first arrival wins) once a
/// `Scanned` record reaches the UI thread — `TrackRecord.artwork` itself is never read again after
/// that (it is `.take()`n on arrival). Every track derived from one cue sheet shares the same album
/// key regardless of which physical `FILE` it came from (`merge_cue_tags`'s album fields are always
/// sheet-level), so attaching the decoded pixels to only the FIRST track built from this physical
/// file — instead of a deep copy per track — is enough for the whole album's art to resolve, without
/// redundantly cloning a potentially large embedded picture once per track.
fn build_cue_track_records(file: CuePhase2File, folder_art_cache: &mut HashMap<PathBuf, Option<PathBuf>>) -> Vec<TrackRecord> {
    let CuePhase2File { resolved_path, tracks, sheet } = file;
    let file_size = std::fs::metadata(&resolved_path).ok().map(|meta| meta.len());
    let metadata = audio::read_metadata(&resolved_path).unwrap_or_else(|error| {
        // See `scan_details`'s identical comment: a write that cannot panic, since `eprintln!`
        // would otherwise unwind this very `guarded` wrapper if stderr is a closed pipe.
        use std::io::Write;
        let _ = writeln!(std::io::stderr().lock(), "DIAG could not read tags for {}: {error}", resolved_path.display());
        Default::default()
    });
    let artwork = resolve_artwork(&resolved_path, metadata.picture.as_ref(), folder_art_cache);

    let mut records = Vec::with_capacity(tracks.len());
    for (index, (key, track, track_info)) in tracks.into_iter().enumerate() {
        let (record_artwork, artwork_source) = if index == 0 {
            match &artwork {
                Some((pixels, source)) => (Some(pixels.clone()), *source),
                None => (None, ArtworkSource::None),
            }
        } else {
            (None, ArtworkSource::None)
        };
        records.push(TrackRecord {
            key,
            info: track_info,
            file_size,
            tags: merge_cue_tags(&sheet, &track, &metadata.tags),
            artwork: record_artwork,
            artwork_source,
            added_seq: 0,
        });
    }
    records
}

/// The tag-merge policy for a cue-derived track (a design decision — see the change's report for
/// why it was chosen this way, flagged there for confirmation): cue-sheet fields win when present
/// (a cue sheet is taken as authoritative album/track metadata once one exists for a file), falling
/// back to the physical file's own embedded tags for anything the cue sheet leaves blank.
/// `track_number` always comes from the cue `TRACK` number, overriding any embedded tag, since the
/// cue sheet defines the authoritative track order; `track_total`/`disc_number`/`disc_total`/
/// `lyrics` are not modeled by the cue format at all, so they always come straight from the embedded
/// tags.
fn merge_cue_tags(sheet: &CueSheet, track: &CueTrack, embedded: &TrackTags) -> TrackTags {
    TrackTags {
        title: track.title.clone().or_else(|| embedded.title.clone()),
        artist: track.performer.clone().or_else(|| sheet.performer.clone()).or_else(|| embedded.artist.clone()),
        album: sheet.title.clone().or_else(|| embedded.album.clone()),
        album_artist: sheet.performer.clone().or_else(|| embedded.album_artist.clone()),
        genre: sheet.genre.clone().or_else(|| embedded.genre.clone()),
        year: sheet.date.as_deref().and_then(parse_leading_year).or(embedded.year),
        track_number: Some(track.number),
        track_total: embedded.track_total,
        disc_number: embedded.disc_number,
        disc_total: embedded.disc_total,
        lyrics: embedded.lyrics.clone(),
    }
}

/// The year from the leading run of ASCII digits in `date` (a cue `REM DATE` value), taking exactly
/// its first 4 digits — `"2021"` -> `Some(2021)`, `"2021-05-04"` -> `Some(2021)`. `None` when `date`
/// does not start with at least 4 digits.
fn parse_leading_year(date: &str) -> Option<u16> {
    let digits: String = date.chars().take_while(char::is_ascii_digit).collect();
    if digits.len() < 4 { None } else { digits[..4].parse().ok() }
}

/// A cue track's own duration in ms: `(end_frame - start_frame)` when known, or — for the last
/// track of a file (`end_frame: None`) — the physical file's own whole-file duration minus this
/// track's start. `None` when the sample rate is unknown/zero or (for the last-track case) the
/// physical file itself reports no duration.
fn cue_track_duration_ms(start_frame: u64, end_frame: Option<u64>, sample_rate: u32, whole_file_duration_ms: Option<u64>) -> Option<u64> {
    if sample_rate == 0 {
        return None;
    }
    match end_frame {
        Some(end) => Some((u128::from(end.saturating_sub(start_frame)) * 1_000 / u128::from(sample_rate)) as u64),
        None => {
            let whole_ms = whole_file_duration_ms?;
            let start_ms = (u128::from(start_frame) * 1_000 / u128::from(sample_rate)) as u64;
            Some(whole_ms.saturating_sub(start_ms))
        }
    }
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
    let path = track.key.path.clone();
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
    let file_size = std::fs::metadata(&track.key.path).ok().map(|meta| meta.len());
    let metadata = audio::read_metadata(&track.key.path).unwrap_or_else(|error| {
        // A write that cannot panic: `eprintln!` unwinds this very `guarded` wrapper (turning an
        // ordinary tag-read error into a lost `Scanned` record) if stderr is a closed pipe —
        // `main.rs`'s `Diagnostic` handler avoids it for the same reason.
        use std::io::Write;
        let _ = writeln!(std::io::stderr().lock(), "DIAG could not read tags for {}: {error}", track.key.path.display());
        Default::default()
    });

    let mut record = TrackRecord {
        key: track.key,
        info: track.info,
        file_size,
        tags: metadata.tags,
        artwork: None,
        artwork_source: ArtworkSource::None,
        added_seq: 0,
    };

    let key = album_key(&record);
    if !albums_with_art.contains(&key)
        && let Some((pixels, source)) = resolve_artwork(&record.key.path, metadata.picture.as_ref(), folder_art_cache)
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
        scanner.scan(vec![flac_path.clone(), wav_path.clone(), bad_path.clone()], false);

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
        assert_eq!(tracks.iter().map(|t| t.key.path.clone()).collect::<Vec<_>>(), vec![flac_path.clone(), wav_path.clone()]);
        assert_eq!(failed_paths, vec![bad_path]);

        // A second `scan()` request must still produce its own `Probed` event, arriving after the
        // first (`§3.3` "one Probed event per scan request, in request order").
        let second_path = dir.join("second.flac");
        std::fs::copy(fixture_path("decoder-tone.flac"), &second_path).unwrap();
        scanner.scan(vec![second_path.clone()], false);
        let mut second_probed: Option<Vec<PreparedTrack>> = None;
        let second_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < second_deadline && second_probed.is_none() {
            if let Ok(LibraryEvent::Probed { tracks, .. }) = events.recv_timeout(Duration::from_millis(200)) {
                second_probed = Some(tracks);
            }
        }
        assert_eq!(
            second_probed.expect("a second Probed event should arrive").iter().map(|t| t.key.path.clone()).collect::<Vec<_>>(),
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
        request_tx.send(ScanRequest { batch: 0, paths: vec![first_a.clone(), first_b.clone()], explicit_selection: false }).unwrap();
        request_tx.send(ScanRequest { batch: 1, paths: vec![second.clone()], explicit_selection: false }).unwrap();
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
                    assert_eq!(tracks.iter().map(|t| t.key.path.clone()).collect::<Vec<_>>(), vec![first_a.clone(), first_b.clone()]);
                }
                Ok(LibraryEvent::Probed { batch: 1, tracks }) => {
                    assert!(first_probed, "the second batch's Probed must arrive after the first batch's own Probed");
                    assert!(!first_batch_done, "the second batch's Probed must arrive before the first batch's BatchDone");
                    second_probed = true;
                    assert_eq!(tracks.iter().map(|t| t.key.path.clone()).collect::<Vec<_>>(), vec![second.clone()]);
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

    /// Fix A regression (`CLAUDE.md` "Scanner phase-1/phase-2 blocking regression"): a CUE-derived
    /// batch's phase-2 work (its one `read_metadata` call) must not block a later, unrelated batch's
    /// own phase 1, exactly like the plain-file version above — before this fix, cue expansion did
    /// its tag/artwork read synchronously inside `enqueue_batch`, so a cue batch's `Probed`,
    /// `Scanned` AND `BatchDone` all fired before a second, already-queued request was ever even
    /// read off the channel.
    #[test]
    fn a_later_requests_probe_runs_before_an_earlier_cue_batchs_remaining_phase_two() {
        let dir = temp_dir("cue-interleave-live");
        std::fs::copy(fixture_path("decoder-tone.flac"), dir.join("cue-track.flac")).unwrap();
        let cue = "FILE \"cue-track.flac\" WAVE\r\n  TRACK 01 AUDIO\r\n    INDEX 01 00:00:00\r\n";
        let cue_path = dir.join("album.cue");
        std::fs::write(&cue_path, cue).unwrap();
        // A different directory: the second batch must not re-discover/re-expand the first batch's
        // own cue sheet just because it happens to share a parent — each `scan()` request's cue
        // detection is independently directory-scoped, so keeping them apart isolates this test to
        // exactly the phase-1/phase-2 interleaving this fix is about.
        let second_dir = temp_dir("cue-interleave-live-second");
        let second = second_dir.join("second.flac");
        std::fs::copy(fixture_path("decoder-tone.flac"), &second).unwrap();

        let (request_tx, request_rx) = unbounded::<ScanRequest>();
        let (event_tx, event_rx) = unbounded::<LibraryEvent>();
        request_tx.send(ScanRequest { batch: 0, paths: vec![cue_path.clone()], explicit_selection: true }).unwrap();
        request_tx.send(ScanRequest { batch: 1, paths: vec![second.clone()], explicit_selection: false }).unwrap();
        let worker = thread::spawn(move || run(request_rx, event_tx));

        let mut first_probed = false;
        let mut second_probed = false;
        let mut first_batch_done = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline && !first_batch_done {
            match event_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(LibraryEvent::Probed { batch: 0, .. }) => {
                    assert!(!first_probed, "the first (cue) batch's Probed must only arrive once");
                    first_probed = true;
                }
                Ok(LibraryEvent::Probed { batch: 1, tracks }) => {
                    assert!(first_probed, "the second batch's Probed must arrive after the first batch's own Probed");
                    assert!(!first_batch_done, "the second batch's Probed must arrive before the first (cue) batch's BatchDone");
                    second_probed = true;
                    assert_eq!(tracks.iter().map(|t| t.key.path.clone()).collect::<Vec<_>>(), vec![second.clone()]);
                }
                Ok(LibraryEvent::BatchDone { batch: 0, .. }) => {
                    assert!(second_probed, "the first (cue) batch's BatchDone must not arrive before the second batch's Probed");
                    first_batch_done = true;
                }
                Ok(LibraryEvent::Probed { batch, .. }) => panic!("unexpected batch id {batch}"),
                Ok(_) => {}
                Err(_) => {}
            }
        }
        assert!(first_probed, "expected the first (cue) batch's Probed");
        assert!(second_probed, "expected the second batch's Probed");
        assert!(first_batch_done, "expected the first (cue) batch's BatchDone");

        drop(request_tx);
        worker.join().expect("the scanner loop must not panic");
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&second_dir).unwrap();
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
                    probed = Some(tracks.into_iter().map(|t| t.key.path).collect());
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

    // -----------------------------------------------------------------------------------------
    // CUE sheet detection and expansion (`CLAUDE.md` CUE-sheet playback support)
    // -----------------------------------------------------------------------------------------

    const SYNTHETIC_CUE: &str = "TITLE \"Test Album\"\r\n\
PERFORMER \"Album Artist\"\r\n\
REM GENRE \"Ambient\"\r\n\
REM DATE \"2019-05-01\"\r\n\
FILE \"decoder-tone.flac\" WAVE\r\n\
  TRACK 01 AUDIO\r\n\
    TITLE \"Track One\"\r\n\
    PERFORMER \"Track One Artist\"\r\n\
    INDEX 01 00:00:00\r\n\
  TRACK 02 AUDIO\r\n\
    TITLE \"Track Two\"\r\n\
    INDEX 01 00:01:00\r\n";

    /// A synthetic multi-track cue sheet naming the real `decoder-tone.flac` fixture (44.1 kHz),
    /// expanded directly through `expand_cue_sheets_for_paths` (phase 1 only, the same function
    /// `enqueue_batch` calls): asserts the produced `TrackKey`s have the exact start/end frames
    /// `cue::cue_time_to_sample_frame` computes, and that phase 1 never reads tags/artwork — fix A
    /// (`CLAUDE.md` "Scanner phase-1/phase-2 blocking regression"): only `PreparedTrack`s (key +
    /// `AudioInfo`) come back, plus the phase-2 carry state, never a fully-tagged `TrackRecord`.
    #[test]
    fn cue_phase1_expansion_produces_correct_track_keys_without_reading_tags() {
        let dir = temp_dir("cue-expansion");
        std::fs::copy(fixture_path("decoder-tone.flac"), dir.join("decoder-tone.flac")).unwrap();
        let cue_path = dir.join("album.cue");
        std::fs::write(&cue_path, SYNTHETIC_CUE).unwrap();

        let (event_tx, event_rx) = unbounded::<LibraryEvent>();
        let (prepared, phase2_files, claimed) = expand_cue_sheets_for_paths(0, std::slice::from_ref(&cue_path), false, &event_tx);
        drop(event_tx);
        assert!(event_rx.try_recv().is_err(), "a well-formed cue sheet must not emit any Failed event");

        assert_eq!(claimed.len(), 1);
        assert!(claimed.contains(&canonical_or_self(&dir.join("decoder-tone.flac"))));

        assert_eq!(prepared.len(), 2, "the sheet's two TRACKs must expand to two PreparedTracks");
        assert_eq!(phase2_files.len(), 1, "both tracks share one physical file, so exactly one phase-2 item carries them");
        let phase2_file = &phase2_files[0];
        assert_eq!(phase2_file.resolved_path, dir.join("decoder-tone.flac"));
        assert_eq!(phase2_file.tracks.len(), 2);

        let sample_rate = prepared[0].info.sample_rate;
        assert_eq!(sample_rate, 44_100, "sanity: the fixture's real sample rate");

        let track_one = prepared.iter().find(|t| t.key.start_frame == 0).expect("track 1");
        assert_eq!(track_one.key.path, dir.join("decoder-tone.flac"));
        assert_eq!(track_one.key.end_frame, Some(44_100), "track 1 ends exactly at track 2's own INDEX 01 (1 s @ 44.1 kHz)");

        let track_two = prepared.iter().find(|t| t.key.start_frame == 44_100).expect("track 2");
        assert_eq!(track_two.key.end_frame, None, "the last track of a file has no end_frame (decode to EOF)");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The same synthetic cue sheet, this time driven through the real `LibraryScanner::scan`
    /// pipeline end to end: proves the tag-merge policy (`merge_cue_tags`) produces the expected
    /// `TrackTags` once phase 2 (`build_cue_track_records`) actually reads the physical file's
    /// embedded tags — for both a cue track with its own title/performer and one that falls back to
    /// the sheet's own performer.
    #[test]
    fn cue_phase2_scan_produces_correctly_merged_tags() {
        let dir = temp_dir("cue-expansion-tags");
        std::fs::copy(fixture_path("decoder-tone.flac"), dir.join("decoder-tone.flac")).unwrap();
        let cue_path = dir.join("album.cue");
        std::fs::write(&cue_path, SYNTHETIC_CUE).unwrap();

        let scanner = LibraryScanner::new();
        let events = scanner.events();
        scanner.scan(vec![cue_path.clone()], true);

        let mut scanned: Vec<TrackRecord> = Vec::new();
        let mut batch_done = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline && !batch_done {
            match events.recv_timeout(Duration::from_millis(200)) {
                Ok(LibraryEvent::Scanned(record)) => scanned.push(*record),
                Ok(LibraryEvent::BatchDone { .. }) => batch_done = true,
                Ok(LibraryEvent::Failed { error, .. }) => panic!("unexpected scan failure: {error}"),
                _ => {}
            }
        }
        assert!(batch_done, "expected BatchDone");
        assert_eq!(scanned.len(), 2, "the sheet's two TRACKs must expand to two scanned TrackRecords");

        let track_one = scanned.iter().find(|r| r.tags.track_number == Some(1)).expect("track 1");
        assert_eq!(track_one.key.start_frame, 0);
        assert_eq!(track_one.tags.title.as_deref(), Some("Track One"), "the cue TRACK's own title wins");
        assert_eq!(track_one.tags.artist.as_deref(), Some("Track One Artist"), "the cue TRACK's own performer wins");
        assert_eq!(track_one.tags.album.as_deref(), Some("Test Album"));
        assert_eq!(track_one.tags.album_artist.as_deref(), Some("Album Artist"));
        assert_eq!(track_one.tags.genre.as_deref(), Some("Ambient"));
        assert_eq!(track_one.tags.year, Some(2019));
        assert_eq!(track_one.tags.track_number, Some(1));

        let track_two = scanned.iter().find(|r| r.tags.track_number == Some(2)).expect("track 2");
        assert_eq!(track_two.tags.title.as_deref(), Some("Track Two"));
        assert_eq!(
            track_two.tags.artist.as_deref(),
            Some("Album Artist"),
            "no per-track performer: falls back to the sheet's own PERFORMER"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A physical file claimed by a surviving cue sheet must never also be scanned as a whole
    /// file, while an unrelated file in the same directory that no cue claims still loads normally
    /// (`CLAUDE.md` CUE-sheet playback support, §6 step 7).
    #[test]
    fn cue_claimed_file_is_excluded_from_whole_file_scanning_while_unclaimed_file_still_loads() {
        let dir = temp_dir("cue-claims");
        std::fs::copy(fixture_path("decoder-tone.flac"), dir.join("decoder-tone.flac")).unwrap();
        std::fs::copy(fixture_path("decoder-tone.wav"), dir.join("unclaimed.wav")).unwrap();
        std::fs::write(dir.join("album.cue"), SYNTHETIC_CUE).unwrap();

        let scanner = LibraryScanner::new();
        let events = scanner.events();
        let batch = scanner.scan(vec![dir.join("decoder-tone.flac"), dir.join("unclaimed.wav")], true);

        let mut probed_tracks: Vec<PreparedTrack> = Vec::new();
        let mut batch_done = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline && !batch_done {
            match events.recv_timeout(Duration::from_millis(200)) {
                Ok(LibraryEvent::Probed { batch: probed_batch, tracks }) => {
                    assert_eq!(probed_batch, batch);
                    probed_tracks.extend(tracks);
                }
                Ok(LibraryEvent::BatchDone { batch: done_batch, .. }) => {
                    assert_eq!(done_batch, batch);
                    batch_done = true;
                }
                _ => {}
            }
        }
        assert!(batch_done, "expected BatchDone");

        let flac_path = dir.join("decoder-tone.flac");
        assert!(
            !probed_tracks.iter().any(|t| t.key.path == flac_path && t.key.start_frame == 0 && t.key.end_frame.is_none()),
            "the cue-claimed file must never also appear as a whole-file track"
        );
        assert_eq!(
            probed_tracks.iter().filter(|t| t.key.path == flac_path).count(),
            2,
            "the cue-claimed file must expand to exactly its two cue tracks"
        );
        assert!(
            probed_tracks.iter().any(|t| t.key.path == dir.join("unclaimed.wav") && t.key.start_frame == 0 && t.key.end_frame.is_none()),
            "a file no cue sheet claims must still load as a normal whole-file track"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A `.cue` sheet that fails to parse (here: a `TRACK` missing its required `INDEX 01`) must be
    /// reported as a `Failed { phase: ScanFailurePhase::Cue }` event and its would-be-claimed file
    /// must fall through to ordinary whole-file scanning instead of being silently dropped
    /// (`CLAUDE.md` CUE-sheet playback support, §6 step 2).
    #[test]
    fn malformed_cue_falls_back_to_normal_whole_file_scanning() {
        let dir = temp_dir("cue-malformed");
        std::fs::copy(fixture_path("decoder-tone.flac"), dir.join("decoder-tone.flac")).unwrap();
        let malformed_cue = "FILE \"decoder-tone.flac\" WAVE\r\n  TRACK 01 AUDIO\r\n    TITLE \"No Index\"\r\n";
        std::fs::write(dir.join("broken.cue"), malformed_cue).unwrap();

        let scanner = LibraryScanner::new();
        let events = scanner.events();
        let batch = scanner.scan(vec![dir.join("decoder-tone.flac")], true);

        let mut saw_cue_failure = false;
        let mut probed_tracks: Vec<PreparedTrack> = Vec::new();
        let mut batch_done = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline && !batch_done {
            match events.recv_timeout(Duration::from_millis(200)) {
                Ok(LibraryEvent::Failed { batch: failed_batch, phase: ScanFailurePhase::Cue, .. }) => {
                    assert_eq!(failed_batch, batch);
                    saw_cue_failure = true;
                }
                Ok(LibraryEvent::Probed { batch: probed_batch, tracks }) => {
                    assert_eq!(probed_batch, batch);
                    probed_tracks.extend(tracks);
                }
                Ok(LibraryEvent::BatchDone { batch: done_batch, .. }) => {
                    assert_eq!(done_batch, batch);
                    batch_done = true;
                }
                _ => {}
            }
        }

        assert!(saw_cue_failure, "a malformed cue sheet must be reported as a Cue-phase Failed event");
        assert!(batch_done, "expected BatchDone");
        let flac_path = dir.join("decoder-tone.flac");
        assert!(
            probed_tracks.iter().any(|t| t.key.path == flac_path && t.key.start_frame == 0 && t.key.end_frame.is_none()),
            "the file the broken cue would have claimed must fall through to ordinary whole-file scanning"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A cue sheet's `INDEX 01`s must strictly advance from one `TRACK` to the next within a `FILE`
    /// (`CLAUDE.md` "INDEX monotonicity not validated", fix F): a hand-edited/corrupt sheet where a
    /// later track's `INDEX 01` does not come after the previous one's must be rejected outright —
    /// the same fallback path as a parse error — rather than silently producing a zero-or-negative-
    /// length "blip" track.
    #[test]
    fn non_monotonic_cue_index_order_is_reported_as_malformed_and_falls_back_to_whole_file_scanning() {
        let dir = temp_dir("cue-non-monotonic");
        std::fs::copy(fixture_path("decoder-tone.flac"), dir.join("decoder-tone.flac")).unwrap();
        // TRACK 02's own INDEX 01 (30s) does not come after TRACK 01's (60s).
        let non_monotonic_cue = "FILE \"decoder-tone.flac\" WAVE\r\n\
            TRACK 01 AUDIO\r\n    INDEX 01 00:01:00\r\n\
            TRACK 02 AUDIO\r\n    INDEX 01 00:00:30\r\n";
        std::fs::write(dir.join("broken.cue"), non_monotonic_cue).unwrap();

        let scanner = LibraryScanner::new();
        let events = scanner.events();
        let batch = scanner.scan(vec![dir.join("decoder-tone.flac")], true);

        let mut saw_cue_failure = false;
        let mut probed_tracks: Vec<PreparedTrack> = Vec::new();
        let mut batch_done = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline && !batch_done {
            match events.recv_timeout(Duration::from_millis(200)) {
                Ok(LibraryEvent::Failed { batch: failed_batch, phase: ScanFailurePhase::Cue, .. }) => {
                    assert_eq!(failed_batch, batch);
                    saw_cue_failure = true;
                }
                Ok(LibraryEvent::Probed { batch: probed_batch, tracks }) => {
                    assert_eq!(probed_batch, batch);
                    probed_tracks.extend(tracks);
                }
                Ok(LibraryEvent::BatchDone { batch: done_batch, .. }) => {
                    assert_eq!(done_batch, batch);
                    batch_done = true;
                }
                _ => {}
            }
        }

        assert!(saw_cue_failure, "a non-monotonic cue sheet must be reported as a malformed, Cue-phase Failed event");
        assert!(batch_done, "expected BatchDone");
        let flac_path = dir.join("decoder-tone.flac");
        assert_eq!(
            probed_tracks.iter().filter(|t| t.key.path == flac_path).count(),
            1,
            "the file a malformed cue would have claimed must fall through to exactly one ordinary whole-file track, not any cue sub-range"
        );
        assert!(probed_tracks.iter().any(|t| t.key.path == flac_path && t.key.start_frame == 0 && t.key.end_frame.is_none()));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A file already marked "claimed" by an otherwise well-formed cue sheet that then fails to
    /// probe (corrupt/unsupported) must be reported via `LibraryEvent::Failed`, exactly like an
    /// unclaimed file's own probe failure — not silently dropped, which would make the whole album
    /// vanish from the library with no explanation (`CLAUDE.md` "Silent data loss when a claimed
    /// file fails to probe", fix D, "refuse rather than silently degrade").
    #[test]
    fn cue_claimed_file_that_fails_to_probe_is_reported_not_silently_dropped() {
        let dir = temp_dir("cue-probe-failure");
        std::fs::write(dir.join("corrupt.flac"), b"not a real flac file, just garbage bytes").unwrap();
        let cue = "FILE \"corrupt.flac\" WAVE\r\n  TRACK 01 AUDIO\r\n    INDEX 01 00:00:00\r\n";
        let cue_path = dir.join("album.cue");
        std::fs::write(&cue_path, cue).unwrap();

        let scanner = LibraryScanner::new();
        let events = scanner.events();
        let batch = scanner.scan(vec![cue_path.clone()], true);

        let mut saw_probe_failure = false;
        let mut scanned = Vec::new();
        let mut batch_done = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline && !batch_done {
            match events.recv_timeout(Duration::from_millis(200)) {
                Ok(LibraryEvent::Failed { batch: failed_batch, path, phase: ScanFailurePhase::Probe, .. }) => {
                    assert_eq!(failed_batch, batch);
                    assert_eq!(path, dir.join("corrupt.flac"));
                    saw_probe_failure = true;
                }
                Ok(LibraryEvent::Scanned(record)) => scanned.push(*record),
                Ok(LibraryEvent::BatchDone { batch: done_batch, .. }) => {
                    assert_eq!(done_batch, batch);
                    batch_done = true;
                }
                _ => {}
            }
        }

        assert!(saw_probe_failure, "a claimed file that fails to probe must be reported, not silently dropped");
        assert!(scanned.is_empty(), "no track can come from a file that never probed successfully");
        assert!(batch_done, "expected BatchDone");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    const PER_TRACK_CUE: &str = "FILE \"track1.flac\" WAVE\r\n  TRACK 01 AUDIO\r\n    INDEX 01 00:00:00\r\n\
FILE \"track2.flac\" WAVE\r\n  TRACK 01 AUDIO\r\n    INDEX 01 00:00:00\r\n";

    /// "Open Files…" (`explicit_selection: true`) opening one file out of a many-file "per-track"
    /// cue (many distinct physical files, one track each) must never silently also claim/enqueue
    /// its unselected siblings (`CLAUDE.md` "Over-broad CUE claim on Open Files", fix B).
    #[test]
    fn explicit_open_files_selection_restricts_per_track_cue_expansion_to_the_requested_file() {
        let dir = temp_dir("cue-per-track-explicit");
        std::fs::copy(fixture_path("decoder-tone.flac"), dir.join("track1.flac")).unwrap();
        std::fs::copy(fixture_path("decoder-tone.flac"), dir.join("track2.flac")).unwrap();
        std::fs::write(dir.join("album.cue"), PER_TRACK_CUE).unwrap();

        let scanner = LibraryScanner::new();
        let events = scanner.events();
        let batch = scanner.scan(vec![dir.join("track1.flac")], true);

        let mut probed_tracks: Vec<PreparedTrack> = Vec::new();
        let mut batch_done = false;
        let mut count = 0usize;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline && !batch_done {
            match events.recv_timeout(Duration::from_millis(200)) {
                Ok(LibraryEvent::Probed { batch: probed_batch, tracks }) => {
                    assert_eq!(probed_batch, batch);
                    probed_tracks.extend(tracks);
                }
                Ok(LibraryEvent::BatchDone { batch: done_batch, count: done_count, .. }) => {
                    assert_eq!(done_batch, batch);
                    batch_done = true;
                    count = done_count;
                }
                _ => {}
            }
        }

        assert!(batch_done, "expected BatchDone");
        assert_eq!(count, 1, "only the requested file's own track must be scanned, not its unselected sibling");
        assert_eq!(probed_tracks.len(), 1);
        assert_eq!(probed_tracks[0].key.path, dir.join("track1.flac"));
        assert!(
            !probed_tracks.iter().any(|t| t.key.path == dir.join("track2.flac")),
            "an unselected sibling file from the same per-track cue must never be silently pulled in"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The same per-track cue sheet, but scanned with `explicit_selection: false` (a folder walk or
    /// the startup restore): every file the cue sheet claims in the touched directory still expands
    /// in full, exactly as before fix B — only an explicit "Open Files…" selection restricts.
    #[test]
    fn non_explicit_scan_keeps_full_directory_cue_expansion_for_a_per_track_cue() {
        let dir = temp_dir("cue-per-track-full");
        std::fs::copy(fixture_path("decoder-tone.flac"), dir.join("track1.flac")).unwrap();
        std::fs::copy(fixture_path("decoder-tone.flac"), dir.join("track2.flac")).unwrap();
        std::fs::write(dir.join("album.cue"), PER_TRACK_CUE).unwrap();

        let scanner = LibraryScanner::new();
        let events = scanner.events();
        let batch = scanner.scan(vec![dir.join("track1.flac")], false);

        let mut probed_tracks: Vec<PreparedTrack> = Vec::new();
        let mut batch_done = false;
        let mut count = 0usize;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline && !batch_done {
            match events.recv_timeout(Duration::from_millis(200)) {
                Ok(LibraryEvent::Probed { batch: probed_batch, tracks }) => {
                    assert_eq!(probed_batch, batch);
                    probed_tracks.extend(tracks);
                }
                Ok(LibraryEvent::BatchDone { batch: done_batch, count: done_count, .. }) => {
                    assert_eq!(done_batch, batch);
                    batch_done = true;
                    count = done_count;
                }
                _ => {}
            }
        }

        assert!(batch_done, "expected BatchDone");
        assert_eq!(count, 2, "a non-explicit scan must still expand every file the cue sheet claims in this directory");
        assert!(probed_tracks.iter().any(|t| t.key.path == dir.join("track1.flac")));
        assert!(probed_tracks.iter().any(|t| t.key.path == dir.join("track2.flac")));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `canonical_or_self` in isolation (`CLAUDE.md` "Claimed-file path matching is a raw PathBuf
    /// equality", fix C): on this dev machine's filesystem (macOS/APFS, typically case-insensitive),
    /// two differently-cased references to the same real file canonicalize to the identical path —
    /// while raw `PathBuf` equality treats them as different files, which is the exact bug a
    /// case-mismatched cue `FILE` value used to trip over.
    #[test]
    fn canonical_or_self_resolves_case_insensitive_duplicates_on_this_filesystem() {
        let dir = temp_dir("cue-case-insensitive-helper");
        let real_path = dir.join("Track.flac");
        std::fs::copy(fixture_path("decoder-tone.flac"), &real_path).unwrap();
        let differently_cased = dir.join("TRACK.FLAC");

        assert_ne!(real_path, differently_cased, "sanity: raw PathBuf equality must see these as different");
        assert_eq!(
            canonical_or_self(&real_path),
            canonical_or_self(&differently_cased),
            "canonicalization must resolve a case-mismatched reference to the same real file"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// End-to-end version of the same fix, driven through the real scanner: a cue sheet whose
    /// `FILE` value differs only in case from the real on-disk filename (plausible from a different
    /// ripping tool) must still be recognized as the same physical file the user opened, not
    /// double-counted as both a cue track and a duplicate whole-file track.
    #[test]
    fn cue_file_field_with_mismatched_case_does_not_duplicate_as_a_whole_file_track() {
        let dir = temp_dir("cue-case-mismatch-dedup");
        std::fs::copy(fixture_path("decoder-tone.flac"), dir.join("Track.flac")).unwrap();
        let cue = "FILE \"TRACK.FLAC\" WAVE\r\n  TRACK 01 AUDIO\r\n    INDEX 01 00:00:00\r\n";
        std::fs::write(dir.join("album.cue"), cue).unwrap();

        let scanner = LibraryScanner::new();
        let events = scanner.events();
        // `explicit_selection: false` deliberately — this isolates the fix C dedup check
        // (`claimed_paths.contains`) from fix B's own restriction, which would otherwise also
        // (for the wrong reason) exclude the cue track here since the resolved path's case differs
        // from the requested path's.
        let batch = scanner.scan(vec![dir.join("Track.flac")], false);

        let mut probed_tracks: Vec<PreparedTrack> = Vec::new();
        let mut batch_done = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline && !batch_done {
            match events.recv_timeout(Duration::from_millis(200)) {
                Ok(LibraryEvent::Probed { batch: probed_batch, tracks }) => {
                    assert_eq!(probed_batch, batch);
                    probed_tracks.extend(tracks);
                }
                Ok(LibraryEvent::BatchDone { batch: done_batch, .. }) => {
                    assert_eq!(done_batch, batch);
                    batch_done = true;
                }
                _ => {}
            }
        }

        assert!(batch_done, "expected BatchDone");
        assert_eq!(
            probed_tracks.len(),
            1,
            "a case-mismatched cue FILE reference must still be recognized as the same physical file, not duplicated"
        );
        assert_eq!(probed_tracks[0].key.start_frame, 0);
        assert!(probed_tracks[0].key.end_frame.is_none());

        std::fs::remove_dir_all(&dir).unwrap();
    }
}

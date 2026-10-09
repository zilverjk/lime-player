//! `lime-library-scanner`: the background thread that probes opened files and reads their tags
//! and artwork, so neither the UI thread nor the audio controller thread ever blocks on file I/O
//! for that (`§3.3`, `§6` Stage 3 "worker stops probing").

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Cursor, Read};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;

use crossbeam_channel::{Receiver, Sender, unbounded};

use crate::audio::rate_repair::needs_rate_repair;
use crate::audio::{self, AudioInfo, EmbeddedPicture, PreparedTrack, TrackTags};

use super::cue::{CueSheet, CueTrack, ResolvedCueFile, cue_time_to_sample_frame, dedupe_cue_sheets, parse_cue, resolve_cue_files};
use super::repair::{RateRepairConfig, RepairDone, RepairOutcome, RepairWorker};
use super::walker::has_cue_extension;
use super::{ArtworkPixels, ArtworkSource, EffectiveAlbumArtist, TrackKey, TrackRecord, album_key, walker};

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
    /// A folder walk (`scan_folder`) finished and its listing can be trusted as the folder's
    /// complete contents: every directory listed without an I/O error, and `root` itself is a
    /// readable, non-empty directory (an empty mount point left behind by an unmounted share is not
    /// evidence that its files are gone). Sent before the walk's own `Probed`/`BatchDone`, and never
    /// for a walk that was cancelled, incomplete or whose root was unreadable. `files` are the audio
    /// files found (cue sheets are not listed); `empty_dirs` are directories that listed with no
    /// entries at all, whose cached tracks the cache must not judge by that listing. `main.rs` feeds it to the persistent library cache
    /// (`cache::RefreshTracker`), which prunes cached tracks the walk did not find.
    FolderWalked { batch: u64, root: PathBuf, files: Vec<PathBuf>, empty_dirs: Vec<PathBuf> },
    /// A FLAC at a non-standard sample rate was converted in place (`audio::rate_repair`). Its
    /// original is kept at `backup`. The track's own `Probed` follows and carries the new rate.
    RateRepaired { batch: u64, path: PathBuf, from_rate: u32, to_rate: u32, backup: PathBuf },
    /// The repair was attempted but changed nothing (read-only volume, unsupported layout, failed
    /// verification, ...). The track continues through the scan as it is on disk; `main.rs` prints
    /// this as a developer diagnostic and, outside the quiet startup restore, a status line.
    RateRepairFailed { batch: u64, path: PathBuf, error: String },
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
    /// The tracks `main.rs` drops from this request's `Probed`/`Scanned` events because "Remove from
    /// Library" excluded them (`LibrarySources::excluded_tracks`), as they were when the request was
    /// dispatched. Phase 2 still reads their tags, but they never take part in artwork resolution
    /// (`BatchArtwork`): a record that is thrown away must not suppress the picture of an included
    /// sibling of the same album.
    excluded: HashSet<TrackKey>,
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
    /// A scanner that never modifies a file.
    pub fn new() -> Self {
        Self::spawn(None)
    }

    /// A scanner that also converts FLAC files at a non-standard sample rate to one output devices
    /// accept, on its own `lime-rate-repair` thread (`audio::rate_repair`, `CLAUDE.md` "Sample-rate
    /// repair"). Only what `Preferences::repair_nonstandard_sample_rates` allows should reach here.
    pub fn with_rate_repair(config: RateRepairConfig) -> Self {
        Self::spawn(Some(config))
    }

    fn spawn(rate_repair: Option<RateRepairConfig>) -> Self {
        let (request_tx, request_rx) = unbounded::<ScanRequest>();
        let (event_tx, event_rx) = unbounded::<LibraryEvent>();
        let scanner_event_tx = event_tx.clone();
        let repair_worker = rate_repair.map(RepairWorker::spawn);
        thread::Builder::new()
            .name("lime-library-scanner".into())
            .spawn(move || run_with_repair(request_rx, scanner_event_tx, repair_worker))
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
    ///
    /// `excluded` is a snapshot of the tracks the caller will drop from this request's events
    /// (`ScanRequest::excluded`); take it after lifting the exclusions an explicit re-open overrides.
    pub fn scan(&self, paths: Vec<PathBuf>, explicit_selection: bool, excluded: HashSet<TrackKey>) -> u64 {
        let batch = self.next_batch_id.fetch_add(1, Ordering::Relaxed);
        let _ = self.requests.send(ScanRequest { batch, paths, explicit_selection, excluded });
        batch
    }

    /// Recursively walks `root` on a dedicated, cancellation-safe thread (never the scanner's own
    /// probe/tag thread, so a slow NAS folder never delays other in-flight requests), reporting
    /// `FolderScanProgress` as files are discovered, then submits every audio file found through
    /// the same pipeline as `scan`. Returns the batch id immediately, before the walk has even
    /// started, for the same reason `scan` does. A `root` that cannot be read at all (e.g. an
    /// unmounted NAS share) still produces a `Failed`/`BatchDone { requested: 0 }` pair instead of
    /// silently doing nothing. `excluded` is the same snapshot `scan` takes, captured now rather than
    /// when the walk finishes.
    pub fn scan_folder(&self, root: PathBuf, excluded: HashSet<TrackKey>) -> u64 {
        let batch = self.next_batch_id.fetch_add(1, Ordering::Relaxed);
        let events = self.event_tx.clone();
        let requests = self.requests.clone();
        let cancel = Arc::clone(&self.cancel_walks);
        thread::Builder::new()
            .name("lime-library-folder-walk".into())
            .spawn(move || {
                let mut last_reported = 0usize;
                let outcome = walker::walk_folder_checked(&root, &cancel, |found| {
                    if found == 1 || found - last_reported >= 20 {
                        last_reported = found;
                        let _ = events.send(LibraryEvent::FolderScanProgress { root: root.clone(), found });
                    }
                });
                let walk_is_reliable = outcome.complete && std::fs::read_dir(&root).is_ok_and(|mut entries| entries.next().is_some());
                let files = outcome.files;
                let empty_dirs = outcome.empty_dirs;
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
                if walk_is_reliable {
                    let _ = events.send(LibraryEvent::FolderWalked { batch, root: root.clone(), files: files.clone(), empty_dirs });
                }
                // A folder walk's own files are never an "explicit selection" (`CLAUDE.md` "Over-
                // broad CUE claim on Open Files"): every file the walk found was already implicitly
                // requested by opening the folder, so cue expansion stays full-directory.
                let _ = requests.send(ScanRequest { batch, paths: files, explicit_selection: false, excluded });
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
    /// Items not finished yet: queued phase-2 work, plus (`RepairWorker`) tracks still waiting on
    /// a sample-rate repair, which enter `phase2_queue` only once it reports.
    remaining: usize,
    scanned: usize,
    /// Kept here so it lives exactly as long as the batch: it is dropped with the state when the
    /// batch finishes, however that happens (`finish_item`).
    artwork: BatchArtwork,
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
/// batch they belong to, so batches still complete — and `BatchDone` still fires — in arrival
/// order, except that a batch waiting on a sample-rate repair finishes when that repair does.
///
/// This is the loop without the repair worker, which the tests drive directly; the app always
/// starts `run_with_repair`.
#[cfg(test)]
fn run(requests: Receiver<ScanRequest>, events: Sender<LibraryEvent>) {
    run_with_repair(requests, events, None);
}

/// `run`, optionally with the sample-rate repair worker (`LibraryScanner::with_rate_repair`).
fn run_with_repair(requests: Receiver<ScanRequest>, events: Sender<LibraryEvent>, repair: Option<RepairWorker>) {
    // Phase-2 work not yet processed, across every request that has run phase 1 so far.
    let mut phase2_queue: VecDeque<(u64, Phase2Item)> = VecDeque::new();
    // Batches still in flight, oldest first.
    let mut batches: VecDeque<BatchState> = VecDeque::new();
    // Never fires when there is no repair worker (or once it is gone).
    let mut repair_results = repair.as_ref().map_or_else(crossbeam_channel::never, RepairWorker::results);

    loop {
        if phase2_queue.is_empty() {
            crossbeam_channel::select! {
                recv(requests) -> request => match request {
                    Ok(request) => enqueue_batch(request, &events, repair.as_ref(), &mut phase2_queue, &mut batches),
                    Err(_) => return,
                },
                recv(repair_results) -> done => match done {
                    Ok(done) => resume_after_repair(done, &events, &mut phase2_queue, &mut batches),
                    Err(_) => repair_results = crossbeam_channel::never(),
                },
            }
            continue;
        }
        // Never blocks: picks up every request that arrived while phase 2 above was running, so
        // its `Probed` goes out immediately instead of after the older batch's whole phase 2.
        while let Ok(request) = requests.try_recv() {
            enqueue_batch(request, &events, repair.as_ref(), &mut phase2_queue, &mut batches);
        }
        while let Ok(done) = repair_results.try_recv() {
            resume_after_repair(done, &events, &mut phase2_queue, &mut batches);
        }
        let Some((batch, item)) = phase2_queue.pop_front() else { continue };
        // A queued item keeps its own batch open, so the state is always found; the throwaway one
        // only keeps this from having to assume that.
        let mut unattached = BatchArtwork::default();
        let artwork = batches.iter_mut().find(|state| state.id == batch).map_or(&mut unattached, |state| &mut state.artwork);
        let scanned = match item {
            Phase2Item::WholeFile(track) => usize::from(scan_phase2_one(batch, track, &events, artwork)),
            Phase2Item::CueFile(file) => scan_cue_phase2_one(batch, file, &events, artwork),
        };
        finish_item(batch, scanned, &events, &mut batches);
    }
}

/// Marks one item of `batch` finished (`scanned` tracks scanned successfully) and sends
/// `BatchDone` once it has none left.
fn finish_item(batch: u64, scanned: usize, events: &Sender<LibraryEvent>, batches: &mut VecDeque<BatchState>) {
    let Some(index) = batches.iter().position(|state| state.id == batch) else { return };
    let state = &mut batches[index];
    state.scanned += scanned;
    state.remaining -= 1;
    if state.remaining == 0 {
        let state = batches.remove(index).expect("index just found above");
        let _ = events.send(LibraryEvent::BatchDone { batch: state.id, requested: state.requested, count: state.scanned });
    }
}

/// Continues a track that was held back for a sample-rate repair: reports how the repair went, then
/// probes the file again (its rate may have changed) and sends it down the ordinary path — its
/// `Probed`, then phase 2 — exactly like a track that never needed repair. A track whose repair
/// failed or was skipped continues as it is on disk.
fn resume_after_repair(
    done: RepairDone,
    events: &Sender<LibraryEvent>,
    phase2_queue: &mut VecDeque<(u64, Phase2Item)>,
    batches: &mut VecDeque<BatchState>,
) {
    let RepairDone { batch, path, outcome } = done;
    match outcome {
        RepairOutcome::Repaired(report) => {
            let _ = events.send(LibraryEvent::RateRepaired {
                batch,
                path: path.clone(),
                from_rate: report.from_rate,
                to_rate: report.to_rate,
                backup: report.backup,
            });
        }
        RepairOutcome::Failed(error) => {
            let _ = events.send(LibraryEvent::RateRepairFailed { batch, path: path.clone(), error });
        }
        RepairOutcome::Skipped => {}
    }
    match guarded(AssertUnwindSafe(|| audio::probe_file(&path).map_err(|error| error.to_string()))) {
        Ok(info) => {
            let track = PreparedTrack { key: TrackKey::whole_file(path), info };
            let _ = events.send(LibraryEvent::Probed { batch, tracks: vec![track.clone()] });
            phase2_queue.push_back((batch, Phase2Item::WholeFile(track)));
        }
        Err(error) => {
            let _ = events.send(LibraryEvent::Failed { batch, path, error, phase: ScanFailurePhase::Probe });
            finish_item(batch, 0, events, batches);
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
    repair: Option<&RepairWorker>,
    phase2_queue: &mut VecDeque<(u64, Phase2Item)>,
    batches: &mut VecDeque<BatchState>,
) {
    let ScanRequest { batch, paths, explicit_selection, excluded } = request;
    let requested = paths.len();

    let (cue_prepared, cue_phase2_files, claimed_paths) = expand_cue_sheets_for_paths(batch, &paths, explicit_selection, events);
    let remaining_paths: Vec<PathBuf> =
        paths.into_iter().filter(|path| !claimed_paths.contains(&canonical_or_self(path)) && !has_cue_extension(path)).collect();

    if !cue_prepared.is_empty() {
        let _ = events.send(LibraryEvent::Probed { batch, tracks: cue_prepared });
    }

    let (ordinary_prepared, awaiting_repair) = probe_phase(batch, &remaining_paths, events, repair);

    let remaining_items = cue_phase2_files.len() + ordinary_prepared.len() + awaiting_repair;
    if remaining_items == 0 {
        let _ = events.send(LibraryEvent::BatchDone { batch, requested, count: 0 });
        return;
    }
    batches.push_back(BatchState { id: batch, requested, remaining: remaining_items, scanned: 0, artwork: BatchArtwork::new(excluded) });
    phase2_queue.extend(cue_phase2_files.into_iter().map(|file| (batch, Phase2Item::CueFile(file))));
    phase2_queue.extend(ordinary_prepared.into_iter().map(|track| (batch, Phase2Item::WholeFile(track))));
}

/// Phase 1: probes every requested path and emits one `Probed` event for the whole batch, so the
/// existing "append the whole selection at once" queue semantics are preserved. Unsupported or
/// unreadable files are skipped (`Failed`) and never reach the queue or the library.
///
/// With a repair worker, a FLAC at a non-standard sample rate (`needs_rate_repair`) is handed to it
/// instead and left out of both the `Probed` event and the returned tracks: it is counted in the
/// second return value, and `resume_after_repair` sends it on later, once the file on disk is final.
fn probe_phase(
    batch: u64,
    paths: &[PathBuf],
    events: &Sender<LibraryEvent>,
    repair: Option<&RepairWorker>,
) -> (Vec<PreparedTrack>, usize) {
    let probed = for_each_guarded(batch, paths.to_vec(), events, PathBuf::clone, |path| {
        audio::probe_file(&path).map(|info| PreparedTrack { key: TrackKey::whole_file(path), info }).map_err(|error| error.to_string())
    });
    let mut prepared = Vec::with_capacity(probed.len());
    let mut awaiting_repair = 0;
    for track in probed {
        if let Some(worker) = repair
            && needs_rate_repair(&track.info)
            && worker.submit(batch, track.key.path.clone())
        {
            awaiting_repair += 1;
        } else {
            prepared.push(track);
        }
    }
    if !prepared.is_empty() {
        let _ = events.send(LibraryEvent::Probed { batch, tracks: prepared.clone() });
    }
    (prepared, awaiting_repair)
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
    artwork: &mut BatchArtwork,
) -> usize {
    let path = file.resolved_path.clone();
    match guarded(AssertUnwindSafe(|| Ok::<_, String>(build_cue_track_records(file, artwork)))) {
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
/// ever consults the album-level artwork cache (keyed by `album_key`, tier-aware — the highest
/// `ArtworkSource` wins) once a `Scanned` record reaches the UI thread — `TrackRecord.artwork` itself
/// is never read again after that (it is `.take()`n on arrival). Every track derived from one cue
/// sheet shares the same album key regardless of which physical `FILE` it came from
/// (`merge_cue_tags`'s album fields are always sheet-level), so attaching the decoded pixels, and
/// their `ArtworkSource`, to only the FIRST track built from this physical file — instead of a deep
/// copy per track — is enough for the whole album's art to resolve, without redundantly cloning a
/// potentially large embedded picture once per track. Resolution follows the same tiers and the
/// same per-album dedup as any other track (`attach_artwork`), keyed by that shared album key, so a
/// multi-`FILE` sheet (one `.flac` per track, one `.wv` per LP side) reads and decodes its folder
/// cover once, not once per physical file. The picture rides on the first track that is not
/// excluded (`BatchArtwork::is_excluded`), since `main.rs` drops an excluded track's record with
/// whatever it carries.
fn build_cue_track_records(file: CuePhase2File, artwork: &mut BatchArtwork) -> Vec<TrackRecord> {
    let CuePhase2File { resolved_path, tracks, sheet } = file;
    let file_size = std::fs::metadata(&resolved_path).ok().map(|meta| meta.len());
    let metadata = audio::read_metadata(&resolved_path).unwrap_or_else(|error| {
        // See `scan_details`'s identical comment: a write that cannot panic, since `eprintln!`
        // would otherwise unwind this very `guarded` wrapper if stderr is a closed pipe.
        use std::io::Write;
        let _ = writeln!(std::io::stderr().lock(), "DIAG could not read tags for {}: {error}", resolved_path.display());
        Default::default()
    });

    let mut records = Vec::with_capacity(tracks.len());
    for (key, track, track_info) in tracks {
        records.push(TrackRecord {
            key,
            info: track_info,
            file_size,
            tags: merge_cue_tags(&sheet, &track, &metadata.tags),
            artwork: None,
            artwork_source: ArtworkSource::None,
            added_seq: 0,
            // Also transient/pre-`Library` here — see `scan_details`'s identical comment. Every
            // track of one CUE sheet shares the sheet-level album/album-artist tags anyway, so once
            // these reach `Library::upsert`, they resolve to the same group regardless.
            effective_album_artist: EffectiveAlbumArtist::Unresolved,
        });
    }
    if let Some(first) = records.iter_mut().find(|record| !artwork.is_excluded(&record.key)) {
        attach_artwork(first, metadata.picture.as_ref(), artwork);
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
    artwork: &mut BatchArtwork,
) -> bool {
    let path = track.key.path.clone();
    match guarded(AssertUnwindSafe(|| {
        let record = scan_details(track, artwork);
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

fn scan_details(track: PreparedTrack, artwork: &mut BatchArtwork) -> TrackRecord {
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
        // This record is never upserted into a `Library` — it is transient, scanner-thread-only
        // state built purely to decide artwork resolution below — so `effective_album_artist` stays
        // `Unresolved`. `album_key` then falls back to this one record's own tags for the
        // `BatchArtwork::tiers` dedup key (see `attach_artwork` for what a wrong guess costs).
        effective_album_artist: EffectiveAlbumArtist::Unresolved,
    };

    attach_artwork(&mut record, metadata.picture.as_ref(), artwork);
    record
}

/// One scan batch's artwork bookkeeping (`BatchState::artwork`); a request never inherits another's.
///
/// `tiers` is the best artwork tier already sent for each (approximate, see `attach_artwork`) album
/// key of the batch. It is per batch, not scanner-lifetime, because a later request can land tracks
/// on an album key that has no cached picture yet, even though the approximate key looks the same:
/// after "Remove from Library" only part of an album may be re-opened, and its group can then resolve
/// to a different effective artist, i.e. a different final key. A scanner-lifetime map would keep
/// suppressing the picture such a key needs. `AppState` keeps the best tier per album across
/// batches, so resolving once per batch is enough.
///
/// `excluded` is the request's exclusion snapshot (`ScanRequest::excluded`). `main.rs` drops an
/// excluded track's `Scanned` record, so that track must neither record a tier in `tiers` (it would
/// suppress an included sibling of the same album) nor carry a picture. The snapshot is taken when
/// the request is dispatched: a track excluded while its batch already runs still counts as included
/// here, and its dropped record can suppress a sibling's picture for the rest of that batch.
///
/// `folder_reads` is what the batch remembers about folder images (`FolderPictureReads`): which files
/// each folder holds, which of them failed, and the last picture it decoded. It is per batch for the
/// same reason `tiers` is: a later request or an explicit re-open must see the folder as it is by then.
#[derive(Default)]
struct BatchArtwork {
    tiers: HashMap<String, ArtworkSource>,
    excluded: HashSet<TrackKey>,
    folder_reads: FolderPictureReads,
}

impl BatchArtwork {
    fn new(excluded: HashSet<TrackKey>) -> Self {
        Self { excluded, ..Self::default() }
    }

    fn is_excluded(&self, key: &TrackKey) -> bool {
        self.excluded.contains(key)
    }
}

/// Resolves `record`'s artwork and attaches it, but only when it would beat what an earlier track of
/// the same album already produced in this batch (`resolve_artwork`'s `best_so_far`): a picture is
/// sent at most once per tier per album, and a later folder's named cover still overrides an earlier
/// track's embedded picture. `artwork.tiers` is updated with the tier sent. An excluded track is
/// left alone entirely (`BatchArtwork`).
///
/// The album key is only an approximation: `record` is transient (never upserted into a `Library`),
/// so `album_key` falls back to this one record's own tags, exactly as before group-level resolution
/// existed. A wrong guess is usually harmless — the same album resolves one extra time — but two
/// different albums can collide on the unscoped `aa:` key (same album title and `ALBUMARTIST`, e.g.
/// one folder with a mistagged stray track): the first one's picture then suppresses the second's
/// within the batch. `AppState`'s final-key cache never shows a wrong picture for that, but the
/// suppressed album can be left with the placeholder.
fn attach_artwork(record: &mut TrackRecord, picture: Option<&EmbeddedPicture>, artwork: &mut BatchArtwork) {
    if artwork.is_excluded(&record.key) {
        return;
    }
    let key = album_key(record);
    let best_so_far = artwork.tiers.get(&key).copied().unwrap_or(ArtworkSource::None);
    if let Some((pixels, source)) = resolve_artwork(&record.key.path, picture, &mut artwork.folder_reads, best_so_far) {
        record.artwork = Some(pixels);
        record.artwork_source = source;
        artwork.tiers.insert(key, source);
    }
}

/// The stems of a named folder cover (`ArtworkSource::Folder`), in priority order: `cover`, `art`,
/// `album art`, `album`, `front`. Each entry lists the spellings of one stem, matched
/// case-insensitively, so "album art" also accepts `album_art`, `album-art` and `albumart` at the
/// same priority. Only whole stems match: `AlbumArtSmall`, `artwork` or `back` never do.
const NAMED_FOLDER_ART_STEMS: [&[&str]; 5] = [&["cover"], &["art"], &["album art", "album_art", "album-art", "albumart"], &["album"], &["front"]];
/// The legacy folder-art stem (`ArtworkSource::FolderFallback`): accepted before the named list
/// above existed, so it stays a last resort instead of being dropped.
const FALLBACK_FOLDER_ART_STEM: &str = "folder";
/// Image extensions a folder-art file may have, matched case-insensitively (the `image` crate is
/// built with its PNG and JPEG decoders only).
const FOLDER_ART_EXTENSIONS: [&str; 3] = ["jpg", "jpeg", "png"];

/// One folder image that could serve as album art.
struct FolderArtCandidate {
    path: PathBuf,
    /// Set once reading this file produced no picture in this batch (`first_readable_candidate`: not
    /// a regular file, over `MAX_ART_BYTES`, corrupt, a decoder panic, an I/O error), so a bad cover
    /// on a NAS share is not fetched and re-decoded for every following track of the album. It lives
    /// in the batch's listing (`FolderPictureReads::listings`), so the next scan tries the file again.
    unusable: bool,
}

/// What one scan batch remembers about folder images (`BatchArtwork::folder_reads`). All of it is
/// dropped with the batch, so nothing stale survives an explicit re-open or the next scan: a
/// `cover.jpg` added, renamed or repaired since is found, and a failure (a `read_dir` that hit an
/// SMB timeout while a share reconnected, a candidate that could not be read) is never remembered
/// beyond the batch it happened in.
#[derive(Default)]
struct FolderPictureReads {
    /// `find_folder_art` per folder: one `read_dir` per folder however many tracks the batch holds
    /// there. A `read_dir` that failed is kept as an empty listing, so the batch does not hammer an
    /// unresponsive share once per track. Paths and flags only — decoded pixels are never kept here.
    listings: HashMap<PathBuf, FolderArt>,
    /// The last folder picture this batch decoded, and the file it came from. A folder's tracks arrive
    /// one after another, yet each can resolve from scratch under its own approximate album key (say a
    /// different `ALBUMARTIST` per track, or many single-track albums sharing one `cover.jpg`): without
    /// this every such key would read and decode the same cover from the share again. One entry
    /// bounds it to a single thumbnail.
    last_picture: Option<(PathBuf, ArtworkPixels)>,
}

/// What the single `read_dir` of one folder found, best-ranked candidate first. Paths and flags
/// only — decoded pixels are never cached here.
#[derive(Default)]
struct FolderArt {
    /// Named covers (`ArtworkSource::Folder`), in `NAMED_FOLDER_ART_STEMS` order.
    named: Vec<FolderArtCandidate>,
    /// Legacy `folder.*` images (`ArtworkSource::FolderFallback`).
    fallback: Vec<FolderArtCandidate>,
}

/// Resolves the album picture a track can contribute, in precedence order (`ArtworkSource`):
///
/// 1. A named folder cover (`cover`, `art`, `album art`, `album`, `front` + `.jpg/.jpeg/.png`,
///    case-insensitive) in the track's own folder, then — for a track inside a disc subfolder
///    (`CD1`, `Disc 2`, ...) — in the RELEASE folder above it, since a multi-disc rip's cover often
///    sits there once rather than being duplicated into every disc subfolder (`CLAUDE.md` "Album
///    grouping"). A candidate that cannot be read or decoded is skipped for the next one.
/// 2. The embedded front cover (or the first embedded picture).
/// 3. The legacy `folder.jpg/.jpeg/.png`, own folder then release folder — only when the album has
///    no picture at all yet, so an album with neither a named cover nor embedded art still gets one.
///
/// `best_so_far` is the best tier already found for this track's album; only a strictly better
/// picture is looked for and returned (`None` otherwise), so an album whose folder cover was already
/// found never touches the disk again, and one with embedded art never re-decodes it for each track.
/// Pass `ArtworkSource::None` to resolve from scratch. The returned source always ranks above
/// `best_so_far`. `reads` is the batch's folder-image state: each folder is listed once per batch, and
/// a cover already decoded for an earlier track of the folder is reused instead of read again, so an
/// album key the batch has not seen yet still costs no second read of the same `cover.jpg`. Pass a
/// fresh `FolderPictureReads` to resolve as a new batch would.
fn resolve_artwork(
    path: &Path,
    embedded: Option<&EmbeddedPicture>,
    reads: &mut FolderPictureReads,
    best_so_far: ArtworkSource,
) -> Option<(ArtworkPixels, ArtworkSource)> {
    if best_so_far >= ArtworkSource::Folder {
        return None;
    }
    let folders = art_folders(path);
    if let Some(pixels) = folder_art_of_tier(&folders, ArtworkSource::Folder, reads) {
        return Some((pixels, ArtworkSource::Folder));
    }
    if best_so_far < ArtworkSource::Embedded
        && let Some(picture) = embedded
        && let Some(pixels) = decode_artwork(&picture.data)
    {
        return Some((pixels, ArtworkSource::Embedded));
    }
    if best_so_far == ArtworkSource::None
        && let Some(pixels) = folder_art_of_tier(&folders, ArtworkSource::FolderFallback, reads)
    {
        return Some((pixels, ArtworkSource::FolderFallback));
    }
    None
}

/// The folders `resolve_artwork` looks in for a track, most specific first: its own folder, plus the
/// release folder above it when its own name looks like a disc subfolder (`is_disc_subfolder_name`).
fn art_folders(path: &Path) -> Vec<PathBuf> {
    let Some(folder) = path.parent() else { return Vec::new() };
    let mut folders = vec![folder.to_path_buf()];
    let is_disc_subfolder = folder.file_name().and_then(|name| name.to_str()).is_some_and(super::is_disc_subfolder_name);
    if is_disc_subfolder && let Some(release_folder) = folder.parent() {
        folders.push(release_folder.to_path_buf());
    }
    folders
}

/// The first decodable picture of `tier` (`Folder` or `FolderFallback`) across `folders`, in order.
fn folder_art_of_tier(folders: &[PathBuf], tier: ArtworkSource, reads: &mut FolderPictureReads) -> Option<ArtworkPixels> {
    folders.iter().find_map(|folder| folder_art_in(folder, tier, reads))
}

/// One folder's worth of the folder-art lookup: finds (listed once per batch) the folder's candidates
/// of `tier` and decodes the first usable one, enforcing the same size cap either folder level uses.
/// A candidate that fails is flagged, so the next track of the batch skips it instead of reading it
/// again.
fn folder_art_in(folder: &Path, tier: ArtworkSource, reads: &mut FolderPictureReads) -> Option<ArtworkPixels> {
    let FolderPictureReads { listings, last_picture } = reads;
    let art = listings.entry(folder.to_path_buf()).or_insert_with(|| find_folder_art(folder));
    let candidates = match tier {
        ArtworkSource::Folder => &mut art.named,
        ArtworkSource::FolderFallback => &mut art.fallback,
        ArtworkSource::Embedded | ArtworkSource::None => return None,
    };
    first_readable_candidate(candidates, |path| read_folder_art_reusing(path, last_picture))
}

/// The first not-yet-failed candidate `read` turns into a picture. A candidate is flagged unusable
/// BEFORE it is read and cleared only on success, so even a `read` that unwinds leaves the verdict
/// recorded: one bad cover is never re-read by every following track of the album.
fn first_readable_candidate(
    candidates: &mut [FolderArtCandidate],
    mut read: impl FnMut(&Path) -> Option<ArtworkPixels>,
) -> Option<ArtworkPixels> {
    for candidate in candidates {
        if candidate.unusable {
            continue;
        }
        candidate.unusable = true;
        if let Some(pixels) = read(&candidate.path) {
            candidate.unusable = false;
            return Some(pixels);
        }
    }
    None
}

/// `read_folder_art`, answered from `last_picture` when it holds this very file (a picture this
/// batch already decoded for an earlier track of the folder), and otherwise remembered there.
fn read_folder_art_reusing(path: &Path, last_picture: &mut Option<(PathBuf, ArtworkPixels)>) -> Option<ArtworkPixels> {
    if let Some((cached_path, pixels)) = last_picture.as_ref()
        && cached_path == path
    {
        return Some(pixels.clone());
    }
    let pixels = read_folder_art(path)?;
    *last_picture = Some((path.to_path_buf(), pixels.clone()));
    Some(pixels)
}

/// Reads and decodes one folder image. Only a regular file (after following a symlink) within
/// `MAX_ART_BYTES` is read, and the read itself is capped too, so a `cover.jpg` symlinked to a FIFO
/// or a device, or one that grows while it is read, can neither block the scanner thread nor exhaust
/// memory. A decode that panics counts as this candidate failing: it costs the folder picture, not
/// the track's tags (`guarded` would otherwise turn the whole track into a `Failed`).
fn read_folder_art(path: &Path) -> Option<ArtworkPixels> {
    // Checked before reading: an oversized cover on a NAS share must never be pulled across the
    // network in full just to be thrown away by a length check (`§3.3`, same cap as embedded art).
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_ART_BYTES as u64 {
        return None;
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path).and_then(|file| file.take(MAX_ART_BYTES as u64 + 1).read_to_end(&mut bytes)).ok()?;
    if bytes.len() > MAX_ART_BYTES {
        return None;
    }
    contain_panic(|| decode_artwork(&bytes))
}

/// `f`'s result, or `None` when it panics.
fn contain_panic<T>(f: impl FnOnce() -> Option<T>) -> Option<T> {
    panic::catch_unwind(AssertUnwindSafe(f)).ok().flatten()
}

/// One `read_dir` per folder (per batch: `FolderPictureReads::listings`). Candidates are found by name first, using the cheap file-type bit
/// `read_dir` already returns instead of a separate `stat` per entry, and ranked within their tier:
/// the `NAMED_FOLDER_ART_STEMS` order first, then a case-insensitive file name (and finally the exact
/// one) as a deterministic tie-break, so the same folder always ranks the same on any file system.
fn find_folder_art(folder: &Path) -> FolderArt {
    let Ok(entries) = std::fs::read_dir(folder) else { return FolderArt::default() };
    let mut named: Vec<(usize, PathBuf)> = Vec::new();
    let mut fallback: Vec<(usize, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let Some((tier, stem_rank)) = classify_folder_art(Path::new(&entry.file_name())) else { continue };
        if !entry.file_type().is_ok_and(|kind| kind.is_file() || kind.is_symlink()) {
            continue;
        }
        match tier {
            ArtworkSource::Folder => named.push((stem_rank, entry.path())),
            _ => fallback.push((stem_rank, entry.path())),
        }
    }
    let ranked = |mut found: Vec<(usize, PathBuf)>| -> Vec<FolderArtCandidate> {
        found.sort_by_cached_key(|(stem_rank, path)| folder_art_rank(*stem_rank, path));
        found.into_iter().map(|(_, path)| FolderArtCandidate { path, unusable: false }).collect()
    };
    FolderArt { named: ranked(named), fallback: ranked(fallback) }
}

fn folder_art_rank(stem_rank: usize, path: &Path) -> (usize, String, String) {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or_default();
    (stem_rank, name.to_ascii_lowercase(), name.to_owned())
}

/// Whether the file name `path` is folder art, and which tier it would serve with its stem's rank
/// within that tier (`NAMED_FOLDER_ART_STEMS` position; always 0 for the single legacy stem), or
/// `None` when it is not folder art at all. Matches the whole stem and the extension
/// case-insensitively.
fn classify_folder_art(path: &Path) -> Option<(ArtworkSource, usize)> {
    let stem = path.file_stem()?.to_str()?.to_ascii_lowercase();
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if !FOLDER_ART_EXTENSIONS.contains(&ext.as_str()) {
        return None;
    }
    if stem == FALLBACK_FOLDER_ART_STEM {
        return Some((ArtworkSource::FolderFallback, 0));
    }
    let stem_rank = NAMED_FOLDER_ART_STEMS.iter().position(|spellings| spellings.contains(&stem.as_str()))?;
    Some((ArtworkSource::Folder, stem_rank))
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
    use crate::audio::rate_repair::test_support::write_tagged_test_flac;
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

    // -----------------------------------------------------------------------------------------
    // Artwork precedence: named folder cover > embedded picture > legacy folder.jpg
    // -----------------------------------------------------------------------------------------

    // The shapes the test pictures resolve to. `image::DynamicImage::thumbnail` fits an image into
    // the `ARTWORK_MAX_SIDE` box preserving its aspect ratio in both directions, so a small picture
    // is scaled up too — and a different aspect ratio per source tells which file was actually read.
    const SQUARE: (u32, u32) = (ARTWORK_MAX_SIDE, ARTWORK_MAX_SIDE);
    const WIDE: (u32, u32) = (ARTWORK_MAX_SIDE, ARTWORK_MAX_SIDE / 2);
    const TALL: (u32, u32) = (ARTWORK_MAX_SIDE / 2, ARTWORK_MAX_SIDE);
    const PANORAMA: (u32, u32) = (ARTWORK_MAX_SIDE, ARTWORK_MAX_SIDE / 4);

    fn shape(pixels: &ArtworkPixels) -> (u32, u32) {
        (pixels.width, pixels.height)
    }

    fn wide_jpeg() -> Vec<u8> {
        tiny_jpeg_bytes(40, 20)
    }

    fn tall_png() -> Vec<u8> {
        tiny_png_bytes(20, 40)
    }

    fn panorama_jpeg() -> Vec<u8> {
        tiny_jpeg_bytes(80, 20)
    }

    fn embedded_square() -> EmbeddedPicture {
        EmbeddedPicture { media_type: None, front_cover: true, data: tiny_png_bytes(20, 20) }
    }

    /// Resolves from scratch (no earlier track of the album has produced anything yet), in a batch of
    /// its own: nothing is reused from an earlier call, the folder is listed again every time.
    fn resolve_new(path: &Path, embedded: Option<&EmbeddedPicture>) -> Option<(ArtworkPixels, ArtworkSource)> {
        resolve_artwork(path, embedded, &mut FolderPictureReads::default(), ArtworkSource::None)
    }

    fn candidate_names(candidates: &[FolderArtCandidate]) -> Vec<String> {
        candidates.iter().map(|candidate| candidate.path.file_name().unwrap().to_str().unwrap().to_owned()).collect()
    }

    #[test]
    fn a_named_folder_cover_beats_the_embedded_picture() {
        let dir = temp_dir("named-beats-embedded");
        std::fs::write(dir.join("Cover.JPG"), wide_jpeg()).unwrap();

        let (pixels, source) = resolve_new(&dir.join("track.flac"), Some(&embedded_square())).unwrap();

        assert_eq!(source, ArtworkSource::Folder);
        assert_eq!(shape(&pixels), WIDE, "the folder cover's pixels, not the embedded picture's");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_embedded_picture_is_the_fallback_when_the_folder_has_no_named_cover() {
        let dir = temp_dir("embedded-fallback");

        let (pixels, source) = resolve_new(&dir.join("track.flac"), Some(&embedded_square())).unwrap();

        assert_eq!(source, ArtworkSource::Embedded);
        assert_eq!(shape(&pixels), SQUARE);
        assert!(resolve_new(&dir.join("track.flac"), None).is_none(), "nothing at all yields no art");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `folder.jpg` was accepted before the named list existed; it stays as the very last resort so
    /// an album with neither a named cover nor embedded art keeps its thumbnail.
    #[test]
    fn embedded_art_beats_a_legacy_folder_jpg_which_only_serves_when_nothing_else_does() {
        let dir = temp_dir("legacy-folder-jpg");
        std::fs::write(dir.join("Folder.JPG"), panorama_jpeg()).unwrap();

        let (pixels, source) = resolve_new(&dir.join("track.flac"), Some(&embedded_square())).unwrap();
        assert_eq!(source, ArtworkSource::Embedded);
        assert_eq!(shape(&pixels), SQUARE);

        let (pixels, source) = resolve_new(&dir.join("track.flac"), None).unwrap();
        assert_eq!(source, ArtworkSource::FolderFallback);
        assert_eq!(shape(&pixels), PANORAMA);

        // A named cover beats folder.jpg even with no embedded picture around.
        std::fs::write(dir.join("front.png"), tall_png()).unwrap();
        let (pixels, source) = resolve_new(&dir.join("track.flac"), None).unwrap();
        assert_eq!(source, ArtworkSource::Folder);
        assert_eq!(shape(&pixels), TALL);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn every_accepted_cover_name_is_recognized_case_insensitively() {
        // (name, priority of its stem: cover, art, album art and its spellings, album, front)
        let names = [
            ("cover.jpg", 0), ("COVER.JPG", 0), ("Cover.Jpeg", 0), ("art.png", 1), ("ART.PNG", 1), ("album art.jpg", 2), ("Album Art.JPG", 2),
            ("album_art.jpg", 2), ("album-art.jpeg", 2), ("albumart.png", 2), ("ALBUMART.PNG", 2), ("album.jpg", 3), ("Album.PNG", 3),
            ("front.jpg", 4), ("FRONT.JPEG", 4),
        ];
        for (name, stem_rank) in names {
            assert_eq!(classify_folder_art(Path::new(name)), Some((ArtworkSource::Folder, stem_rank)), "{name} is a named cover of that priority");

            // The content is what gets decoded, not the extension: a JPEG under a `.png` name works.
            let dir = temp_dir("accepted-name");
            std::fs::write(dir.join(name), wide_jpeg()).unwrap();
            let (pixels, source) =
                resolve_new(&dir.join("track.flac"), Some(&embedded_square())).unwrap_or_else(|| panic!("{name} should resolve"));
            assert_eq!(source, ArtworkSource::Folder, "{name} must beat the embedded picture");
            assert_eq!(shape(&pixels), WIDE, "{name}");
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn names_that_are_not_an_accepted_cover_are_ignored() {
        let names = [
            "AlbumArtSmall.jpg", "artwork.jpg", "back.jpg", "cover.gif", "cover.jpg.bak", "cover", "covers.jpg", "cover1.jpg", "my cover.jpg",
            "album art small.jpg", "cover.webp", "folder.gif", "Thumbs.db",
        ];
        let dir = temp_dir("ignored-names");
        for name in names {
            assert_eq!(classify_folder_art(Path::new(name)), None, "{name} must not match");
            std::fs::write(dir.join(name), wide_jpeg()).unwrap();
        }

        let (_pixels, source) = resolve_new(&dir.join("track.flac"), Some(&embedded_square())).unwrap();
        assert_eq!(source, ArtworkSource::Embedded, "none of them may stand in for a cover");
        assert!(resolve_new(&dir.join("track.flac"), None).is_none());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn named_covers_rank_cover_art_album_art_album_front_and_folder_stays_a_fallback() {
        let dir = temp_dir("named-priority");
        // Byte/case order would put these differently ("A" < "c" < "f"): the priority list must win.
        for name in ["front.jpg", "album.png", "Album Art.jpg", "art.jpg", "cover.jpeg", "folder.jpg", "Folder.png"] {
            std::fs::write(dir.join(name), tiny_jpeg_bytes(4, 4)).unwrap();
        }

        let art = find_folder_art(&dir);

        assert_eq!(candidate_names(&art.named), ["cover.jpeg", "art.jpg", "Album Art.jpg", "album.png", "front.jpg"]);
        assert_eq!(candidate_names(&art.fallback), ["folder.jpg", "Folder.png"], "legacy folder.* images, by case-insensitive name");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn spellings_of_one_stem_tie_break_by_case_insensitive_file_name() {
        let dir = temp_dir("named-tie-break");
        for name in ["front.png", "album.jpg", "album_art.png", "AlbumArt.jpg", "album-art.jpg", "album art.jpg", "cover.PNG", "Cover.jpg"] {
            std::fs::write(dir.join(name), tiny_jpeg_bytes(4, 4)).unwrap();
        }

        let art = find_folder_art(&dir);

        assert_eq!(
            candidate_names(&art.named),
            ["Cover.jpg", "cover.PNG", "album art.jpg", "album-art.jpg", "album_art.png", "AlbumArt.jpg", "album.jpg", "front.png"],
            "cover before album art, every album art spelling before album and front; within each stem, by lowercased file name"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A multi-disc release's cover sitting only in the RELEASE folder — not duplicated into every
    /// `CD1`/`CD2` subfolder — must still resolve for a track inside a disc subfolder, once its own
    /// folder comes up empty (`CLAUDE.md` "Album grouping", artwork fallback order).
    #[test]
    fn scanner_falls_back_to_release_folder_art_for_a_disc_subfolder_track() {
        let release_dir = temp_dir("release-art");
        let disc_dir = release_dir.join("CD1");
        std::fs::create_dir_all(&disc_dir).unwrap();
        std::fs::write(release_dir.join("cover.jpg"), tiny_jpeg_bytes(10, 10)).unwrap();

        let (pixels, source) = resolve_new(&disc_dir.join("01 - Track.flac"), None)
            .expect("the release folder's cover must be found for a track with none in its own disc folder");
        assert_eq!(source, ArtworkSource::Folder);
        assert_eq!(pixels.width, ARTWORK_MAX_SIDE);

        std::fs::remove_dir_all(&release_dir).unwrap();
    }

    /// Own disc folder's named cover > release folder's named cover > embedded picture > legacy
    /// folder.jpg (of either folder): the release folder is only ever a fallback, never preferred.
    #[test]
    fn disc_subfolder_art_precedence_is_own_named_then_release_named_then_embedded() {
        let release_dir = temp_dir("release-art-priority");
        let disc_dir = release_dir.join("CD2");
        std::fs::create_dir_all(&disc_dir).unwrap();
        let track = disc_dir.join("01 - Track.flac");
        std::fs::write(release_dir.join("cover.jpg"), tall_png()).unwrap();
        std::fs::write(disc_dir.join("cover.jpg"), wide_jpeg()).unwrap();
        std::fs::write(release_dir.join("folder.jpg"), panorama_jpeg()).unwrap();

        let mut reads = FolderPictureReads::default();
        let (pixels, source) = resolve_artwork(&track, Some(&embedded_square()), &mut reads, ArtworkSource::None).unwrap();
        assert_eq!((source, shape(&pixels)), (ArtworkSource::Folder, WIDE), "the disc folder's own cover wins");
        assert!(!reads.listings.contains_key(&release_dir), "the release folder is not even listed when the disc folder has a cover");

        std::fs::remove_file(disc_dir.join("cover.jpg")).unwrap();
        let (pixels, source) = resolve_new(&track, Some(&embedded_square())).unwrap();
        assert_eq!((source, shape(&pixels)), (ArtworkSource::Folder, TALL), "then the release folder's named cover, over the embedded picture");

        std::fs::remove_file(release_dir.join("cover.jpg")).unwrap();
        let (pixels, source) = resolve_new(&track, Some(&embedded_square())).unwrap();
        assert_eq!((source, shape(&pixels)), (ArtworkSource::Embedded, SQUARE), "then the embedded picture, over a legacy folder.jpg");

        let (pixels, source) = resolve_new(&track, None).unwrap();
        assert_eq!((source, shape(&pixels)), (ArtworkSource::FolderFallback, PANORAMA), "the release folder's folder.jpg is the last resort");

        std::fs::remove_dir_all(&release_dir).unwrap();
    }

    /// The own folder is searched before the release folder as a whole: a lower-ranked stem beside
    /// the track still beats a higher-ranked one in the release folder.
    #[test]
    fn a_disc_folders_own_lower_ranked_cover_beats_the_release_folders_higher_ranked_one() {
        let release_dir = temp_dir("own-folder-first");
        let disc_dir = release_dir.join("CD1");
        std::fs::create_dir_all(&disc_dir).unwrap();
        std::fs::write(release_dir.join("cover.jpg"), tall_png()).unwrap();
        std::fs::write(disc_dir.join("front.jpg"), wide_jpeg()).unwrap();

        let (pixels, source) = resolve_new(&disc_dir.join("01.flac"), None).unwrap();

        assert_eq!((source, shape(&pixels)), (ArtworkSource::Folder, WIDE));

        std::fs::remove_dir_all(&release_dir).unwrap();
    }

    /// A release-folder named cover also beats the disc folder's OWN legacy folder.jpg: tiers are
    /// compared before folders.
    #[test]
    fn a_release_folder_named_cover_beats_the_disc_folders_legacy_folder_jpg() {
        let release_dir = temp_dir("release-named-vs-disc-legacy");
        let disc_dir = release_dir.join("Disc 1");
        std::fs::create_dir_all(&disc_dir).unwrap();
        std::fs::write(release_dir.join("front.png"), tall_png()).unwrap();
        std::fs::write(disc_dir.join("folder.jpg"), panorama_jpeg()).unwrap();

        let (pixels, source) = resolve_new(&disc_dir.join("01.flac"), None).unwrap();

        assert_eq!((source, shape(&pixels)), (ArtworkSource::Folder, TALL));

        std::fs::remove_dir_all(&release_dir).unwrap();
    }

    #[test]
    fn a_corrupt_named_cover_falls_back_to_the_next_candidate_then_to_embedded() {
        let dir = temp_dir("corrupt-cover");
        let track = dir.join("track.flac");
        std::fs::write(dir.join("cover.jpg"), b"definitely not an image").unwrap();
        std::fs::write(dir.join("art.png"), tall_png()).unwrap();

        // Next ranked file in the same folder.
        let mut reads = FolderPictureReads::default();
        let (pixels, source) = resolve_artwork(&track, Some(&embedded_square()), &mut reads, ArtworkSource::None).unwrap();
        assert_eq!((source, shape(&pixels)), (ArtworkSource::Folder, TALL), "art.png stands in for the corrupt cover.jpg");
        let named = &reads.listings[&dir].named;
        assert_eq!(candidate_names(named), ["cover.jpg", "art.png"]);
        assert!(named[0].unusable && !named[1].unusable, "only the corrupt candidate is flagged");

        // No other named file: the embedded picture.
        std::fs::remove_file(dir.join("art.png")).unwrap();
        let (pixels, source) = resolve_new(&track, Some(&embedded_square())).unwrap();
        assert_eq!((source, shape(&pixels)), (ArtworkSource::Embedded, SQUARE));

        // ... and with no embedded picture either, the legacy folder.jpg.
        std::fs::write(dir.join("folder.jpg"), panorama_jpeg()).unwrap();
        let (pixels, source) = resolve_new(&track, None).unwrap();
        assert_eq!((source, shape(&pixels)), (ArtworkSource::FolderFallback, PANORAMA));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A corrupt cover in the disc folder falls through to the release folder's named cover before
    /// any embedded picture is considered.
    #[test]
    fn a_corrupt_disc_folder_cover_falls_back_to_the_release_folder_cover() {
        let release_dir = temp_dir("corrupt-disc-cover");
        let disc_dir = release_dir.join("CD1");
        std::fs::create_dir_all(&disc_dir).unwrap();
        std::fs::write(disc_dir.join("cover.jpg"), b"not an image").unwrap();
        std::fs::write(release_dir.join("cover.jpg"), tall_png()).unwrap();

        let (pixels, source) = resolve_new(&disc_dir.join("01.flac"), Some(&embedded_square())).unwrap();

        assert_eq!((source, shape(&pixels)), (ArtworkSource::Folder, TALL));

        std::fs::remove_dir_all(&release_dir).unwrap();
    }

    /// Within one batch a folder is listed once and a candidate that failed is remembered: neither is
    /// looked at again for the following tracks, so one bad cover on a slow share is not re-read per
    /// track. The next batch starts over (`a_cover_added_or_repaired_after_a_batch_is_found_by_the_next_one`).
    #[test]
    fn a_folder_is_listed_once_and_a_failed_candidate_is_not_read_again_within_a_batch() {
        let dir = temp_dir("listed-once");
        let track = dir.join("track.flac");
        let mut reads = FolderPictureReads::default();

        assert!(resolve_artwork(&track, None, &mut reads, ArtworkSource::None).is_none());
        // A valid cover appears after the listing: only a cached listing can still find nothing.
        std::fs::write(dir.join("cover.jpg"), wide_jpeg()).unwrap();
        assert!(resolve_artwork(&track, None, &mut reads, ArtworkSource::None).is_none(), "the folder is not listed a second time");
        assert_eq!(reads.listings.len(), 1);

        // A cover that is corrupt from the start: read once, flagged, and never read again.
        std::fs::write(dir.join("cover.jpg"), b"corrupt").unwrap();
        let mut reads = FolderPictureReads::default();
        assert!(resolve_artwork(&track, Some(&embedded_square()), &mut reads, ArtworkSource::None).is_some_and(|(_, source)| source == ArtworkSource::Embedded));
        // The cover is repaired on disk, but the batch's verdict stands.
        std::fs::write(dir.join("cover.jpg"), wide_jpeg()).unwrap();
        let (_pixels, source) = resolve_artwork(&track, Some(&embedded_square()), &mut reads, ArtworkSource::None).unwrap();
        assert_eq!(source, ArtworkSource::Embedded, "a candidate that failed is not read again");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Nothing about a folder outlives its batch (`FolderPictureReads`): a cover added after the folder
    /// was first listed, one that was corrupt and has been repaired, and a folder whose `read_dir`
    /// failed (an unmounted or reconnecting share) are all picked up by the next batch, which is what
    /// an explicit re-open or a later scan is.
    #[test]
    fn a_cover_added_or_repaired_after_a_batch_is_found_by_the_next_one() {
        let base = temp_dir("next-batch");
        let dir = base.join("album");
        let track = dir.join("track.flac");
        let embedded = embedded_square();

        // `read_dir` fails: the batch lists the folder as empty and keeps that answer, but only for itself.
        let mut reads = FolderPictureReads::default();
        assert!(resolve_artwork(&track, None, &mut reads, ArtworkSource::None).is_none());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cover.jpg"), b"corrupt").unwrap();
        assert!(resolve_artwork(&track, None, &mut reads, ArtworkSource::None).is_none(), "the failed listing is kept for the rest of the batch");
        assert!(resolve_new(&track, None).is_none(), "the next batch lists the folder again and finds only a corrupt cover");

        // The corrupt cover is replaced: a batch that already gave up on it keeps its verdict, the next one reads it.
        let mut reads = FolderPictureReads::default();
        assert!(resolve_artwork(&track, Some(&embedded), &mut reads, ArtworkSource::None).is_some_and(|(_, source)| source == ArtworkSource::Embedded));
        std::fs::write(dir.join("cover.jpg"), wide_jpeg()).unwrap();
        assert!(resolve_artwork(&track, Some(&embedded), &mut reads, ArtworkSource::None).is_some_and(|(_, source)| source == ArtworkSource::Embedded));
        let (pixels, source) = resolve_new(&track, Some(&embedded)).unwrap();
        assert_eq!((source, shape(&pixels)), (ArtworkSource::Folder, WIDE));

        // A cover that is added after the folder was listed without one wins over the embedded picture, too.
        std::fs::remove_file(dir.join("cover.jpg")).unwrap();
        let mut reads = FolderPictureReads::default();
        assert!(resolve_artwork(&track, Some(&embedded), &mut reads, ArtworkSource::None).is_some_and(|(_, source)| source == ArtworkSource::Embedded));
        std::fs::write(dir.join("Front.png"), tall_png()).unwrap();
        assert!(resolve_artwork(&track, Some(&embedded), &mut reads, ArtworkSource::None).is_some_and(|(_, source)| source == ArtworkSource::Embedded));
        let (pixels, source) = resolve_new(&track, Some(&embedded)).unwrap();
        assert_eq!((source, shape(&pixels)), (ArtworkSource::Folder, TALL));

        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A candidate is flagged unusable BEFORE it is read, so even a read that unwinds leaves the
    /// verdict recorded: the next track skips it instead of reading (and unwinding on) it again.
    #[test]
    fn a_candidate_is_flagged_unusable_before_it_is_read() {
        let mut candidates = vec![
            FolderArtCandidate { path: PathBuf::from("cover.jpg"), unusable: false },
            FolderArtCandidate { path: PathBuf::from("art.png"), unusable: false },
        ];

        let unwound = panic::catch_unwind(AssertUnwindSafe(|| first_readable_candidate(&mut candidates, |_| panic!("decoder blew up"))));

        assert!(unwound.is_err());
        assert!(candidates[0].unusable, "the candidate that unwound stays flagged");
        assert!(!candidates[1].unusable);

        let mut reads = Vec::new();
        let pixels = ArtworkPixels { width: 1, height: 1, rgb: vec![1, 2, 3] };
        let found = first_readable_candidate(&mut candidates, |path| {
            reads.push(path.to_path_buf());
            Some(pixels.clone())
        });
        assert_eq!(found, Some(pixels));
        assert_eq!(reads, [PathBuf::from("art.png")], "the next track goes straight to the next candidate");
        assert!(!candidates[1].unusable, "a successful read leaves the candidate usable");
    }

    /// A candidate that fails for any reason (corrupt, unreadable, gone since the listing) is skipped by
    /// the rest of the batch, and the lookup falls through to the next ranked one.
    #[test]
    fn a_failed_candidate_is_skipped_for_the_rest_of_the_batch_and_the_next_one_is_tried() {
        let candidate = |name: &str| FolderArtCandidate { path: PathBuf::from(name), unusable: false };
        let mut candidates = vec![candidate("cover.jpg"), candidate("art.png"), candidate("front.png")];
        let pixels = ArtworkPixels { width: 1, height: 1, rgb: vec![1, 2, 3] };

        let mut attempts = Vec::new();
        let found = first_readable_candidate(&mut candidates, |path| {
            attempts.push(path.to_path_buf());
            (path == Path::new("front.png")).then(|| pixels.clone())
        });

        assert_eq!(found, Some(pixels.clone()));
        assert_eq!(attempts, [PathBuf::from("cover.jpg"), PathBuf::from("art.png"), PathBuf::from("front.png")]);

        attempts.clear();
        let found = first_readable_candidate(&mut candidates, |path| {
            attempts.push(path.to_path_buf());
            Some(pixels.clone())
        });
        assert_eq!(found, Some(pixels));
        assert_eq!(attempts, [PathBuf::from("front.png")], "the two that failed are not tried again");
    }

    /// A cover that could not be read (here: gone since the folder was listed) costs its batch the
    /// picture, and the next batch gets it once the file is readable again.
    #[test]
    fn a_cover_that_cannot_be_read_costs_its_batch_the_picture_and_the_next_batch_retries() {
        let dir = temp_dir("cover-io-error");
        let track = dir.join("track.flac");
        let cover = dir.join("cover.jpg");
        std::fs::write(&cover, wide_jpeg()).unwrap();
        let mut batch = FolderPictureReads::default();
        batch.listings.insert(dir.clone(), find_folder_art(&dir));
        let embedded = embedded_square();
        std::fs::rename(&cover, dir.join("moved-away")).unwrap();

        let (pixels, source) = resolve_artwork(&track, Some(&embedded), &mut batch, ArtworkSource::None).unwrap();
        assert_eq!((source, shape(&pixels)), (ArtworkSource::Embedded, SQUARE), "the batch falls back to the embedded picture");

        // Same batch: the file is back, but the candidate is not hammered again.
        std::fs::rename(dir.join("moved-away"), &cover).unwrap();
        let (_pixels, source) = resolve_artwork(&track, Some(&embedded), &mut batch, ArtworkSource::None).unwrap();
        assert_eq!(source, ArtworkSource::Embedded);

        // The next batch lists the folder again and reads it.
        let (pixels, source) = resolve_new(&track, Some(&embedded)).unwrap();
        assert_eq!((source, shape(&pixels)), (ArtworkSource::Folder, WIDE));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Every new approximate album key of a batch resolves from scratch (`BatchArtwork::tiers`), but
    /// the folder cover is read and decoded once: the second key is served from the batch's last
    /// decoded picture, so it still works after the file is gone, while a batch of its own reads again.
    #[test]
    fn a_folder_cover_is_decoded_once_per_batch_however_many_album_keys_resolve_it() {
        let dir = temp_dir("cover-reused-per-batch");
        let track = dir.join("track.flac");
        let cover = dir.join("cover.jpg");
        std::fs::write(&cover, wide_jpeg()).unwrap();
        let embedded = embedded_square();
        let mut batch = FolderPictureReads::default();

        let (first, source) = resolve_artwork(&track, Some(&embedded), &mut batch, ArtworkSource::None).unwrap();
        assert_eq!((source, shape(&first)), (ArtworkSource::Folder, WIDE));

        // A file that cannot be read any more proves the next resolve did not touch the disk.
        std::fs::remove_file(&cover).unwrap();
        let (second, source) = resolve_artwork(&track, Some(&embedded), &mut batch, ArtworkSource::None).unwrap();
        assert_eq!((source, second), (ArtworkSource::Folder, first));

        let (_pixels, source) =
            resolve_artwork(&track, Some(&embedded), &mut FolderPictureReads::default(), ArtworkSource::None).unwrap();
        assert_eq!(source, ArtworkSource::Embedded, "another batch does not reuse it: it reads the folder again");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A decode that panics is contained per candidate, so it costs the folder picture and not the
    /// track's tags (`guarded` would otherwise fail the whole track).
    #[test]
    fn a_panicking_decode_counts_as_no_picture() {
        assert_eq!(contain_panic(|| -> Option<u8> { panic!("decoder blew up") }), None);
        assert_eq!(contain_panic(|| Some(7)), Some(7));
        assert_eq!(contain_panic(|| None::<u8>), None);
    }

    /// Only a regular file is ever read: a `cover.jpg` that is a symlink to a directory, or to a FIFO
    /// (which reports length 0, passes the size cap and used to block the read forever), is refused.
    #[cfg(unix)]
    #[test]
    fn a_cover_that_is_not_a_regular_file_is_refused_without_blocking() {
        let dir = temp_dir("cover-not-a-file");
        let target_dir = dir.join("a-directory");
        std::fs::create_dir(&target_dir).unwrap();
        std::os::unix::fs::symlink(&target_dir, dir.join("cover.jpg")).unwrap();
        assert!(read_folder_art(&dir.join("cover.jpg")).is_none());

        let fifo = dir.join("front.png");
        if std::process::Command::new("mkfifo").arg(&fifo).status().is_ok_and(|status| status.success()) {
            let (done, result) = std::sync::mpsc::channel();
            thread::spawn(move || {
                let _ = done.send(read_folder_art(&fifo).is_none());
            });
            assert_eq!(result.recv_timeout(Duration::from_secs(5)), Ok(true), "a FIFO is refused instead of blocking the scanner thread");
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `best_so_far` is the album's best tier already sent: only a strictly better picture is looked
    /// for, so an album is not re-resolved (or its embedded picture re-decoded) for every track.
    #[test]
    fn resolve_artwork_only_returns_pictures_that_beat_the_best_so_far() {
        let dir = temp_dir("best-so-far");
        let track = dir.join("track.flac");
        let embedded = embedded_square();
        let mut reads = FolderPictureReads::default();

        // Nothing here: an embedded-only album produces its picture once, then nothing.
        assert!(resolve_artwork(&track, Some(&embedded), &mut reads, ArtworkSource::None).is_some_and(|(_, source)| source == ArtworkSource::Embedded));
        assert!(resolve_artwork(&track, Some(&embedded), &mut reads, ArtworkSource::Embedded).is_none());
        assert!(resolve_artwork(&track, Some(&embedded), &mut reads, ArtworkSource::Folder).is_none());

        // A legacy folder.jpg serves only an album with nothing yet, and an embedded picture upgrades it.
        let legacy_dir = temp_dir("best-so-far-legacy");
        let legacy_track = legacy_dir.join("track.flac");
        std::fs::write(legacy_dir.join("folder.jpg"), panorama_jpeg()).unwrap();
        assert!(resolve_artwork(&legacy_track, None, &mut reads, ArtworkSource::None).is_some_and(|(_, source)| source == ArtworkSource::FolderFallback));
        assert!(resolve_artwork(&legacy_track, None, &mut reads, ArtworkSource::FolderFallback).is_none());
        assert!(
            resolve_artwork(&legacy_track, Some(&embedded), &mut reads, ArtworkSource::FolderFallback)
                .is_some_and(|(_, source)| source == ArtworkSource::Embedded)
        );

        // A named cover found in any later folder upgrades embedded, and once found nothing else runs.
        let named_dir = temp_dir("best-so-far-named");
        let named_track = named_dir.join("track.flac");
        std::fs::write(named_dir.join("cover.jpg"), wide_jpeg()).unwrap();
        let (pixels, source) = resolve_artwork(&named_track, Some(&embedded), &mut reads, ArtworkSource::Embedded).unwrap();
        assert_eq!((source, shape(&pixels)), (ArtworkSource::Folder, WIDE));
        assert!(resolve_artwork(&named_track, Some(&embedded), &mut reads, ArtworkSource::Folder).is_none());

        for dir in [dir, legacy_dir, named_dir] {
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    /// The `Scanned` records of one request, in arrival order.
    fn scan_records(scanner: &LibraryScanner, events: &Receiver<LibraryEvent>, paths: Vec<PathBuf>, explicit_selection: bool) -> Vec<TrackRecord> {
        scan_records_excluding(scanner, events, paths, explicit_selection, HashSet::new())
    }

    /// `scan_records` for a request dispatched with an exclusion snapshot: phase 2 still reports every
    /// track (`main.rs` is what drops the excluded ones).
    fn scan_records_excluding(
        scanner: &LibraryScanner,
        events: &Receiver<LibraryEvent>,
        paths: Vec<PathBuf>,
        explicit_selection: bool,
        excluded: HashSet<TrackKey>,
    ) -> Vec<TrackRecord> {
        let batch = scanner.scan(paths, explicit_selection, excluded);
        collect_until_batch_done(events, batch)
            .into_iter()
            .filter_map(|event| match event {
                LibraryEvent::Scanned(record) => Some(*record),
                LibraryEvent::Failed { path, error, .. } => panic!("scanning {} failed: {error}", path.display()),
                _ => None,
            })
            .collect()
    }

    /// A two-disc release whose tracks share one album key (no ALBUM tag, so `dir:<release folder>`
    /// for both): `CD1/01.flac` carries an embedded picture and has no folder art, `CD2/01.flac` has
    /// no embedded picture but a named `cover.jpg` beside it.
    fn two_disc_release(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let release_dir = temp_dir(name);
        let (cd1, cd2) = (release_dir.join("CD1"), release_dir.join("CD2"));
        std::fs::create_dir_all(&cd1).unwrap();
        std::fs::create_dir_all(&cd2).unwrap();
        write_tagged_test_flac(&cd1.join("01.flac"), 44_100, 4_410);
        std::fs::copy(fixture_path("decoder-tone.flac"), cd2.join("01.flac")).unwrap();
        std::fs::write(cd2.join("cover.jpg"), wide_jpeg()).unwrap();
        (release_dir.clone(), cd1.join("01.flac"), cd2.join("01.flac"))
    }

    /// An album merged from two folders where only the later-scanned one has a named cover: the
    /// scanner must not treat the first track's embedded picture as "album already has art" — the
    /// later track sends its named cover, which then wins in `AppState`.
    #[test]
    fn a_later_track_with_a_named_cover_still_sends_it_after_an_earlier_embedded_only_track() {
        let (release_dir, cd1_track, cd2_track) = two_disc_release("art-embedded-then-named");
        let scanner = LibraryScanner::new();
        let events = scanner.events();

        // One request, so the dedup between the two tracks is what is under test.
        let records = scan_records(&scanner, &events, vec![cd1_track.clone(), cd2_track.clone()], false);

        let (first, second) = (&records[0], &records[1]);
        assert_eq!((&first.key.path, &second.key.path), (&cd1_track, &cd2_track));
        assert_eq!(album_key(first), album_key(second), "the two tracks must land in one album for this test to mean anything");
        assert_eq!(first.artwork_source, ArtworkSource::Embedded);
        assert_eq!(first.artwork.as_ref().map(shape), Some(SQUARE));
        assert_eq!(second.artwork_source, ArtworkSource::Folder, "the later folder's named cover is still emitted");
        assert_eq!(second.artwork.as_ref().map(shape), Some(WIDE));

        std::fs::remove_dir_all(&release_dir).unwrap();
    }

    /// The other scan order: once the album's named cover was sent, a later track (whose own folder
    /// only has an embedded picture) resolves nothing more.
    #[test]
    fn a_later_embedded_only_track_sends_nothing_once_the_albums_named_cover_was_sent() {
        let (release_dir, cd1_track, cd2_track) = two_disc_release("art-named-then-embedded");
        let scanner = LibraryScanner::new();
        let events = scanner.events();

        let records = scan_records(&scanner, &events, vec![cd2_track.clone(), cd1_track.clone()], false);

        let (first, second) = (&records[0], &records[1]);
        assert_eq!((&first.key.path, &second.key.path), (&cd2_track, &cd1_track));
        assert_eq!(album_key(first), album_key(second));
        assert_eq!(first.artwork_source, ArtworkSource::Folder);
        assert!(second.artwork.is_none(), "an embedded picture can never improve on a named cover");
        assert_eq!(second.artwork_source, ArtworkSource::None);

        std::fs::remove_dir_all(&release_dir).unwrap();
    }

    /// An embedded-only album sends its picture once, not once per track.
    #[test]
    fn an_embedded_only_album_sends_its_picture_once() {
        let dir = temp_dir("art-embedded-once");
        write_tagged_test_flac(&dir.join("01.flac"), 44_100, 4_410);
        write_tagged_test_flac(&dir.join("02.flac"), 44_100, 4_410);
        let scanner = LibraryScanner::new();
        let events = scanner.events();

        let records = scan_records(&scanner, &events, vec![dir.join("01.flac"), dir.join("02.flac")], false);

        assert_eq!(records.len(), 2);
        assert_eq!(records.iter().filter(|record| record.artwork.is_some()).count(), 1);
        assert_eq!(records.iter().filter(|record| record.artwork_source == ArtworkSource::Embedded).count(), 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The dedup lives for one request only. A later request can land tracks on an album key that has
    /// no cached picture yet (after "Remove from Library" only part of an album is re-opened and its
    /// group resolves to a different effective artist), so re-opening an album must resolve its
    /// picture again instead of trusting that an earlier request already sent one; `AppState` keeps
    /// the best tier per album across requests anyway.
    #[test]
    fn a_later_request_resolves_an_albums_artwork_again() {
        let dir = temp_dir("art-per-request");
        write_tagged_test_flac(&dir.join("01.flac"), 44_100, 4_410);
        let scanner = LibraryScanner::new();
        let events = scanner.events();

        let first = scan_records(&scanner, &events, vec![dir.join("01.flac")], false);
        let second = scan_records(&scanner, &events, vec![dir.join("01.flac")], false);

        for records in [&first, &second] {
            assert_eq!(records[0].artwork_source, ArtworkSource::Embedded);
            assert_eq!(records[0].artwork.as_ref().map(shape), Some(SQUARE));
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The user's own scenario, end to end: a folder is opened while it has no named cover (its tracks
    /// carry embedded pictures), a `cover.jpg` is dropped into it, and the folder is opened again. The
    /// second request lists the folder afresh (nothing about a folder outlives its batch), so its record
    /// carries the cover at the `Folder` tier, which `AppState::cache_artwork` lets replace the embedded
    /// picture the first request produced.
    #[test]
    fn a_cover_added_between_two_requests_is_found_by_the_second_and_outranks_the_embedded_picture() {
        let dir = temp_dir("art-cover-added-later");
        write_tagged_test_flac(&dir.join("01.flac"), 44_100, 4_410);
        let scanner = LibraryScanner::new();
        let events = scanner.events();

        let first = scan_records(&scanner, &events, vec![dir.join("01.flac")], false);
        assert_eq!(first[0].artwork_source, ArtworkSource::Embedded);
        assert_eq!(first[0].artwork.as_ref().map(shape), Some(SQUARE));

        std::fs::write(dir.join("cover.jpg"), wide_jpeg()).unwrap();
        let second = scan_records(&scanner, &events, vec![dir.join("01.flac")], false);

        assert_eq!(second[0].artwork_source, ArtworkSource::Folder);
        assert!(second[0].artwork_source > first[0].artwork_source);
        assert_eq!(second[0].artwork.as_ref().map(shape), Some(WIDE), "the folder cover's pixels, not the tag picture's");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `main.rs` drops an excluded track's record, so it must not be the one that "already sent" the
    /// album's picture: with the album's first track excluded, an included sibling of the same album
    /// (same folder, no album tag, so one `dir:` key) still carries the named cover, in either order.
    #[test]
    fn an_excluded_track_never_suppresses_an_included_siblings_picture() {
        let dir = temp_dir("art-excluded-sibling");
        let (first, second) = (dir.join("01.flac"), dir.join("11.flac"));
        write_tagged_test_flac(&first, 44_100, 4_410);
        write_tagged_test_flac(&second, 44_100, 4_410);
        std::fs::write(dir.join("cover.jpg"), wide_jpeg()).unwrap();
        let scanner = LibraryScanner::new();
        let events = scanner.events();

        let records = scan_records(&scanner, &events, vec![first.clone(), second.clone()], false);
        assert_eq!(album_key(&records[0]), album_key(&records[1]), "one album for this test to mean anything");
        assert!(records[0].artwork.is_some() && records[1].artwork.is_none(), "without exclusions the first track carries the album's picture");

        for paths in [vec![first.clone(), second.clone()], vec![second.clone(), first.clone()]] {
            let excluded = HashSet::from([TrackKey::whole_file(first.clone())]);
            let records = scan_records_excluding(&scanner, &events, paths, false, excluded);
            assert_eq!(records.len(), 2, "phase 2 itself still reports every track");
            let excluded_record = records.iter().find(|record| record.key.path == first).unwrap();
            let included_record = records.iter().find(|record| record.key.path == second).unwrap();
            assert!(excluded_record.artwork.is_none() && excluded_record.artwork_source == ArtworkSource::None);
            assert_eq!(included_record.artwork_source, ArtworkSource::Folder);
            assert_eq!(included_record.artwork.as_ref().map(shape), Some(WIDE));
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The same for a cue-derived file: its one picture rides on the first track that is not excluded,
    /// and on none when every track is.
    #[test]
    fn a_cue_files_picture_rides_on_its_first_non_excluded_track() {
        let dir = temp_dir("cue-art-excluded");
        write_tagged_test_flac(&dir.join("decoder-tone.flac"), 44_100, 88_200);
        std::fs::write(dir.join("album.cue"), SYNTHETIC_CUE).unwrap();
        std::fs::write(dir.join("cover.jpg"), wide_jpeg()).unwrap();
        let scanner = LibraryScanner::new();
        let events = scanner.events();
        let cue = || vec![dir.join("album.cue")];

        let records = scan_records(&scanner, &events, cue(), true);
        let first_key = records.iter().find(|record| record.tags.track_number == Some(1)).unwrap().key.clone();
        let second_key = records.iter().find(|record| record.tags.track_number == Some(2)).unwrap().key.clone();

        let records = scan_records_excluding(&scanner, &events, cue(), true, HashSet::from([first_key.clone()]));
        let first = records.iter().find(|record| record.key == first_key).unwrap();
        let second = records.iter().find(|record| record.key == second_key).unwrap();
        assert!(first.artwork.is_none() && first.artwork_source == ArtworkSource::None, "the excluded track carries nothing");
        assert_eq!(second.artwork_source, ArtworkSource::Folder);
        assert_eq!(second.artwork.as_ref().map(shape), Some(WIDE));

        let records = scan_records_excluding(&scanner, &events, cue(), true, HashSet::from([first_key, second_key]));
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|record| record.artwork.is_none()), "nothing to carry it when every track is excluded");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The cue path resolves with the same tiers: a named cover beats the physical file's embedded
    /// picture, which beats nothing, and the source travels on the record (attached to the first cue
    /// track only) so `AppState` can compare tiers.
    #[test]
    fn cue_tracks_resolve_artwork_with_the_same_tiers() {
        let with_cover = temp_dir("cue-art-named");
        let embedded_only = temp_dir("cue-art-embedded");
        for dir in [&with_cover, &embedded_only] {
            // Two seconds, so the sheet's second TRACK (at 00:01:00) starts inside the file.
            write_tagged_test_flac(&dir.join("decoder-tone.flac"), 44_100, 88_200);
            std::fs::write(dir.join("album.cue"), SYNTHETIC_CUE).unwrap();
        }
        std::fs::write(with_cover.join("Front.jpg"), wide_jpeg()).unwrap();
        let scanner = LibraryScanner::new();
        let events = scanner.events();

        let records = scan_records(&scanner, &events, vec![with_cover.join("album.cue")], true);
        assert_eq!(records.len(), 2, "the sheet's two TRACKs");
        let first = records.iter().find(|record| record.tags.track_number == Some(1)).unwrap();
        let second = records.iter().find(|record| record.tags.track_number == Some(2)).unwrap();
        assert_eq!(first.artwork_source, ArtworkSource::Folder, "a named cover beats the embedded picture for cue tracks too");
        assert_eq!(first.artwork.as_ref().map(shape), Some(WIDE));
        assert!(second.artwork.is_none() && second.artwork_source == ArtworkSource::None, "the picture rides on one track per physical file");

        let records = scan_records(&scanner, &events, vec![embedded_only.join("album.cue")], true);
        let first = records.iter().find(|record| record.tags.track_number == Some(1)).unwrap();
        assert_eq!(first.artwork_source, ArtworkSource::Embedded);
        assert_eq!(first.artwork.as_ref().map(shape), Some(SQUARE));

        for dir in [with_cover, embedded_only] {
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    /// Every `FILE` of one sheet shares its album key, so a multi-`FILE` sheet (one `.flac` per track)
    /// reads and decodes the folder cover once — not once per physical file, each time over the NAS.
    #[test]
    fn a_multi_file_cue_sheet_resolves_its_artwork_once() {
        let dir = temp_dir("cue-art-two-files");
        write_tagged_test_flac(&dir.join("a.flac"), 44_100, 44_100);
        write_tagged_test_flac(&dir.join("b.flac"), 44_100, 44_100);
        std::fs::write(dir.join("cover.jpg"), wide_jpeg()).unwrap();
        std::fs::write(
            dir.join("album.cue"),
            "TITLE \"Two Files\"\r\nPERFORMER \"Album Artist\"\r\n\
FILE \"a.flac\" WAVE\r\n  TRACK 01 AUDIO\r\n    INDEX 01 00:00:00\r\n\
FILE \"b.flac\" WAVE\r\n  TRACK 02 AUDIO\r\n    INDEX 01 00:00:00\r\n",
        )
        .unwrap();
        let scanner = LibraryScanner::new();
        let events = scanner.events();

        let records = scan_records(&scanner, &events, vec![dir.join("album.cue")], true);

        assert_eq!(records.len(), 2, "one track per FILE");
        assert_eq!(album_key(&records[0]), album_key(&records[1]), "both FILEs land in one album for this test to mean anything");
        let with_art: Vec<&TrackRecord> = records.iter().filter(|record| record.artwork.is_some()).collect();
        assert_eq!(with_art.len(), 1, "the album's picture is sent once");
        assert_eq!(with_art[0].artwork_source, ArtworkSource::Folder);
        assert_eq!(with_art[0].artwork.as_ref().map(shape), Some(WIDE));

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
        scanner.scan(vec![flac_path.clone(), wav_path.clone(), bad_path.clone()], false, HashSet::new());

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
        scanner.scan(vec![second_path.clone()], false, HashSet::new());
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
        request_tx.send(ScanRequest { batch: 0, paths: vec![first_a.clone(), first_b.clone()], explicit_selection: false, excluded: HashSet::new() }).unwrap();
        request_tx.send(ScanRequest { batch: 1, paths: vec![second.clone()], explicit_selection: false, excluded: HashSet::new() }).unwrap();
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
        request_tx.send(ScanRequest { batch: 0, paths: vec![cue_path.clone()], explicit_selection: true, excluded: HashSet::new() }).unwrap();
        request_tx.send(ScanRequest { batch: 1, paths: vec![second.clone()], explicit_selection: false, excluded: HashSet::new() }).unwrap();
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
        let batch = scanner.scan_folder(dir.clone(), HashSet::new());

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

    /// The walk's listing reaches the persistent library cache only when it can be trusted: a
    /// readable folder reports `FolderWalked` (before its `Probed`/`BatchDone`), a missing root and an
    /// empty directory (a stale mount point) do not.
    #[test]
    fn scan_folder_reports_its_listing_only_when_it_is_reliable() {
        let walked = |root: PathBuf| -> Option<Vec<PathBuf>> {
            let scanner = LibraryScanner::new();
            let events = scanner.events();
            let batch = scanner.scan_folder(root, HashSet::new());
            let mut listing = None;
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while std::time::Instant::now() < deadline {
                match events.recv_timeout(Duration::from_millis(200)) {
                    Ok(LibraryEvent::FolderWalked { batch: walked_batch, files, .. }) => {
                        assert_eq!(walked_batch, batch);
                        listing = Some(files);
                    }
                    Ok(LibraryEvent::BatchDone { .. }) => break,
                    _ => {}
                }
            }
            listing
        };

        let dir = temp_dir("scan-folder-walked");
        std::fs::copy(fixture_path("decoder-tone.flac"), dir.join("a.flac")).unwrap();
        assert_eq!(walked(dir.clone()), Some(vec![dir.join("a.flac")]));

        let empty = temp_dir("scan-folder-walked-empty");
        assert_eq!(walked(empty), None, "an empty directory proves nothing about its files");
        assert_eq!(walked(dir.join("does-not-exist")), None);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A folder root that does not exist (an unmounted NAS share) must be reported through the
    /// ordinary `Failed`/`BatchDone` pipeline instead of silently doing nothing.
    #[test]
    fn scan_folder_reports_a_missing_root_instead_of_doing_nothing() {
        let dir = temp_dir("scan-folder-missing").join("does-not-exist");

        let scanner = LibraryScanner::new();
        let events = scanner.events();
        let batch = scanner.scan_folder(dir.clone(), HashSet::new());

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
        scanner.scan(vec![cue_path.clone()], true, HashSet::new());

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
        let batch = scanner.scan(vec![dir.join("decoder-tone.flac"), dir.join("unclaimed.wav")], true, HashSet::new());

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
        let batch = scanner.scan(vec![dir.join("decoder-tone.flac")], true, HashSet::new());

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
        let batch = scanner.scan(vec![dir.join("decoder-tone.flac")], true, HashSet::new());

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
        let batch = scanner.scan(vec![cue_path.clone()], true, HashSet::new());

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
        let batch = scanner.scan(vec![dir.join("track1.flac")], true, HashSet::new());

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
        let batch = scanner.scan(vec![dir.join("track1.flac")], false, HashSet::new());

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
        let batch = scanner.scan(vec![dir.join("Track.flac")], false, HashSet::new());

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

    // -----------------------------------------------------------------------------------------
    // Sample-rate repair (`audio::rate_repair`, `library::repair`)
    // -----------------------------------------------------------------------------------------

    /// Every event of `batch`, in arrival order, up to and including its `BatchDone`.
    fn collect_until_batch_done(events: &Receiver<LibraryEvent>, batch: u64) -> Vec<LibraryEvent> {
        let mut collected = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            let wait = deadline.saturating_duration_since(std::time::Instant::now());
            let event = events.recv_timeout(wait).expect("the batch should finish");
            let done = matches!(&event, LibraryEvent::BatchDone { batch: id, .. } if *id == batch);
            collected.push(event);
            if done {
                return collected;
            }
        }
    }

    fn probed_sample_rates(events: &[LibraryEvent]) -> Vec<(PathBuf, u32)> {
        events
            .iter()
            .filter_map(|event| match event {
                LibraryEvent::Probed { tracks, .. } => Some(tracks.iter().map(|track| (track.key.path.clone(), track.info.sample_rate))),
                _ => None,
            })
            .flatten()
            .collect()
    }

    fn scanner_with_rate_repair(dir: &Path) -> LibraryScanner {
        LibraryScanner::with_rate_repair(RateRepairConfig { backup_dir: dir.join("backups") })
    }

    #[test]
    fn a_37800_hz_flac_is_repaired_before_it_reaches_the_library() {
        let dir = temp_dir("repair-scan");
        let path = dir.join("heat.flac");
        write_tagged_test_flac(&path, 37_800, 30_000);
        let original = std::fs::read(&path).unwrap();

        let scanner = scanner_with_rate_repair(&dir);
        let events = scanner.events();
        let batch = scanner.scan(vec![path.clone()], true, HashSet::new());
        let collected = collect_until_batch_done(&events, batch);

        let repaired = collected.iter().find_map(|event| match event {
            LibraryEvent::RateRepaired { from_rate, to_rate, backup, .. } => Some((*from_rate, *to_rate, backup.clone())),
            _ => None,
        });
        let (from_rate, to_rate, backup) = repaired.expect("a RateRepaired event");
        assert_eq!((from_rate, to_rate), (37_800, 44_100));
        assert_eq!(std::fs::read(backup).unwrap(), original, "the untouched original is kept");

        // The track is announced once, with the rate of the file that is now on disk.
        assert_eq!(probed_sample_rates(&collected), vec![(path.clone(), 44_100)]);
        let scanned: Vec<_> = collected.iter().filter_map(|event| match event {
            LibraryEvent::Scanned(record) => Some((record.key.path.clone(), record.info.sample_rate, record.tags.title.clone())),
            _ => None,
        }).collect();
        assert_eq!(scanned, vec![(path.clone(), 44_100, Some("Heat - The Heat Is On".to_owned()))], "tags survive the rewrite");
        assert!(matches!(collected.last(), Some(LibraryEvent::BatchDone { requested: 1, count: 1, .. })));
        assert!(!collected.iter().any(|event| matches!(event, LibraryEvent::Failed { .. } | LibraryEvent::RateRepairFailed { .. })));

        // Scanning the repaired file again is a plain scan: idempotent, nothing is rewritten.
        let repaired_bytes = std::fs::read(&path).unwrap();
        let batch = scanner.scan(vec![path.clone()], true, HashSet::new());
        let collected = collect_until_batch_done(&events, batch);
        assert!(!collected.iter().any(|event| matches!(event, LibraryEvent::RateRepaired { .. } | LibraryEvent::RateRepairFailed { .. })));
        assert_eq!(std::fs::read(&path).unwrap(), repaired_bytes);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Repairing one file must not hold up the rest of its batch: the standard-rate file is announced
    /// straight from phase 1, and the batch only finishes once the held-back file has been resumed.
    #[test]
    fn a_repair_does_not_delay_the_other_files_of_its_batch() {
        let dir = temp_dir("repair-siblings");
        let odd = dir.join("a-odd.flac");
        let standard = dir.join("b-standard.flac");
        write_tagged_test_flac(&odd, 37_800, 30_000);
        std::fs::copy(fixture_path("decoder-tone.flac"), &standard).unwrap();

        let scanner = scanner_with_rate_repair(&dir);
        let events = scanner.events();
        let batch = scanner.scan(vec![odd.clone(), standard.clone()], false, HashSet::new());
        let collected = collect_until_batch_done(&events, batch);

        match &collected[0] {
            LibraryEvent::Probed { tracks, .. } => {
                assert_eq!(tracks.iter().map(|track| track.key.path.clone()).collect::<Vec<_>>(), vec![standard.clone()]);
            }
            _ => panic!("the first event must be the standard-rate file's Probed"),
        }
        let mut rates = probed_sample_rates(&collected);
        rates.sort();
        assert_eq!(rates, vec![(odd.clone(), 44_100), (standard.clone(), 44_100)]);
        let scanned = collected.iter().filter(|event| matches!(event, LibraryEvent::Scanned(_))).count();
        assert_eq!(scanned, 2);
        assert!(matches!(collected.last(), Some(LibraryEvent::BatchDone { requested: 2, count: 2, .. })));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_scanner_without_rate_repair_never_touches_the_file() {
        let dir = temp_dir("repair-off");
        let path = dir.join("heat.flac");
        write_tagged_test_flac(&path, 37_800, 20_000);
        let original = std::fs::read(&path).unwrap();

        let scanner = LibraryScanner::new();
        let events = scanner.events();
        let batch = scanner.scan(vec![path.clone()], false, HashSet::new());
        let collected = collect_until_batch_done(&events, batch);

        assert_eq!(probed_sample_rates(&collected), vec![(path.clone(), 37_800)]);
        assert!(!collected.iter().any(|event| matches!(event, LibraryEvent::RateRepaired { .. } | LibraryEvent::RateRepairFailed { .. })));
        assert_eq!(std::fs::read(&path).unwrap(), original);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A file that cannot be rewritten (read-only here) is still added exactly as it is on disk, the
    /// failure is reported once, and rescanning it does not try again.
    #[test]
    fn a_failed_repair_reports_once_and_the_track_is_still_added_unchanged() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("repair-fails");
        let path = dir.join("locked.flac");
        write_tagged_test_flac(&path, 37_800, 20_000);
        let original = std::fs::read(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();

        let scanner = scanner_with_rate_repair(&dir);
        let events = scanner.events();
        let batch = scanner.scan(vec![path.clone()], false, HashSet::new());
        let collected = collect_until_batch_done(&events, batch);

        let failures: Vec<&String> = collected
            .iter()
            .filter_map(|event| match event {
                LibraryEvent::RateRepairFailed { error, .. } => Some(error),
                _ => None,
            })
            .collect();
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("read-only"), "{}", failures[0]);
        assert_eq!(probed_sample_rates(&collected), vec![(path.clone(), 37_800)]);
        assert!(matches!(collected.last(), Some(LibraryEvent::BatchDone { requested: 1, count: 1, .. })));
        assert_eq!(std::fs::read(&path).unwrap(), original);

        // No retry loop: the second scan of the same path is a plain scan.
        let batch = scanner.scan(vec![path.clone()], false, HashSet::new());
        let collected = collect_until_batch_done(&events, batch);
        assert!(!collected.iter().any(|event| matches!(event, LibraryEvent::RateRepairFailed { .. })));
        assert_eq!(probed_sample_rates(&collected), vec![(path.clone(), 37_800)]);
        assert!(matches!(collected.last(), Some(LibraryEvent::BatchDone { count: 1, .. })));

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Batch accounting is per batch id, not "whatever is at the front": a batch still waiting on its
    /// repair must not swallow, or be finished by, the items of a batch that arrived after it.
    #[test]
    fn a_batch_waiting_on_a_repair_does_not_misattribute_a_later_batchs_items() {
        let dir = temp_dir("repair-two-batches");
        let odd = dir.join("odd.flac");
        let standard = dir.join("standard.flac");
        write_tagged_test_flac(&odd, 37_800, 30_000);
        std::fs::copy(fixture_path("decoder-tone.flac"), &standard).unwrap();

        let scanner = scanner_with_rate_repair(&dir);
        let events = scanner.events();
        let first = scanner.scan(vec![odd.clone()], false, HashSet::new());
        let second = scanner.scan(vec![standard.clone()], false, HashSet::new());

        let mut done = std::collections::HashMap::new();
        let mut scanned_by_path = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while done.len() < 2 {
            let wait = deadline.saturating_duration_since(std::time::Instant::now());
            match events.recv_timeout(wait).expect("both batches should finish") {
                LibraryEvent::BatchDone { batch, requested, count } => {
                    done.insert(batch, (requested, count));
                }
                LibraryEvent::Scanned(record) => scanned_by_path.push(record.key.path.clone()),
                _ => {}
            }
        }
        assert_eq!(done[&first], (1, 1));
        assert_eq!(done[&second], (1, 1));
        scanned_by_path.sort();
        assert_eq!(scanned_by_path, vec![odd, standard]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

}



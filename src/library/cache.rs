//! The persistent library cache (`CLAUDE.md` "Persistent library"): the scanned library — every
//! `TrackRecord`'s probe result and tags, plus the album pictures the UI shows — saved to disk so
//! the next launch shows its library immediately, even when a NAS share is not mounted yet. The
//! background startup rescan (`BatchKind::StartupRestore`) then refreshes it.
//!
//! Layout, under `<cache dir>/library-cache/`:
//! - `index.json`: `IndexFile`, compact JSON with a `version`. One `CachedTrack` per track in
//!   `added_seq` order, and per album key the pictures (`CachedPicture`) the art cache holds.
//! - `art/<hash>.rgb`: one decoded thumbnail per distinct picture (`ART_MAGIC`, width, height as
//!   little-endian `u32`, then raw RGB8), named by a content hash (`ArtHash`) so identical covers are
//!   stored once however many albums or contributors share them.
//!
//! Loading ([`load`]) reads only these files, never a track path, so it cannot block on an
//! unmounted share. Writing ([`write`]) is atomic (temp file + `rename`, like `store.rs`) and runs on
//! a worker thread; `CacheRuntime` coalesces requests. A missing, corrupt or version-mismatched cache
//! is ignored and rebuilt by the rescan, and nothing here ever touches `library.json`.
//!
//! Pruning (`RefreshTracker`, [`find_missing`]) decides which cached tracks the rescan proves gone.
//! Its rule is "when in doubt, keep": a cached track is dropped only on positive evidence.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use slint::{Rgb8Pixel, SharedPixelBuffer};

use super::walker::is_reachable_by_walk;
use super::{ArtworkSource, EffectiveAlbumArtist, TrackKey, TrackRecord};
use crate::app_state::{AppState, Contributor};
use crate::audio::{AudioInfo, TrackTags};

/// Bump when `IndexFile` (or the art file format) changes incompatibly: an index with another
/// version is ignored and rebuilt.
pub const CACHE_VERSION: u32 = 1;

const INDEX_FILE: &str = "index.json";
const ART_DIR: &str = "art";
const ART_EXTENSION: &str = "rgb";
const ART_MAGIC: &[u8; 4] = b"LRGB";
const ART_HEADER_LEN: usize = 12;
/// A decoded thumbnail is capped at 400 px by the scanner; anything larger in a cache file is not
/// one of ours.
const ART_MAX_SIDE: u32 = 4096;
/// While a scan keeps running, a dirty cache is still written at most this often, so quitting in
/// the middle of a long first scan does not lose all of it.
const WRITE_INTERVAL_DURING_SCAN: Duration = Duration::from_secs(30);
/// After a failed write (read-only or full disk) no new attempt starts for this long, so a cache
/// that cannot be written costs one clone, thread and `DIAG` line per interval, not per timer tick.
const WRITE_RETRY_BACKOFF: Duration = Duration::from_secs(30);
/// Temp files older than this are leftovers of a crashed write and are removed; a younger one may
/// belong to another running instance and is left alone.
const STALE_TEMP_AGE: Duration = Duration::from_secs(600);

/// Content hash of a decoded picture (dimensions and pixels), two independent FNV-1a 64 passes. It
/// names the picture's file, so two pictures with the same pixels share one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ArtHash(u64, u64);

impl ArtHash {
    pub fn of(width: u32, height: u32, rgb: &[u8]) -> Self {
        let dimensions = [width.to_le_bytes(), height.to_le_bytes()].concat();
        Self(fnv1a(0xcbf2_9ce4_8422_2325, &dimensions, rgb), fnv1a(0x9e37_79b9_7f4a_7c15, &dimensions, rgb))
    }

    fn to_hex(self) -> String {
        format!("{:016x}{:016x}", self.0, self.1)
    }

    fn from_hex(text: &str) -> Option<Self> {
        if text.len() != 32 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        Some(Self(u64::from_str_radix(&text[..16], 16).ok()?, u64::from_str_radix(&text[16..], 16).ok()?))
    }
}

fn fnv1a(seed: u64, head: &[u8], body: &[u8]) -> u64 {
    let mut hash = seed;
    for &byte in head.iter().chain(body) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

impl Serialize for ArtHash {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for ArtHash {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::from_hex(&text).ok_or_else(|| serde::de::Error::custom("invalid picture hash"))
    }
}

/// What the library keeps of one track. The album-artist resolution and `added_seq` are not stored:
/// both are recomputed from the records themselves (`Library::load_records`), in saved order.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CachedTrack {
    pub key: TrackKey,
    pub info: AudioInfo,
    pub file_size: Option<u64>,
    pub tags: TrackTags,
    pub artwork_source: ArtworkSource,
}

impl CachedTrack {
    fn from_record(record: &TrackRecord) -> Self {
        Self {
            key: record.key.clone(),
            info: record.info.clone(),
            file_size: record.file_size,
            tags: record.tags.clone(),
            artwork_source: record.artwork_source,
        }
    }

    fn into_record(self) -> TrackRecord {
        TrackRecord {
            key: self.key,
            info: self.info,
            file_size: self.file_size,
            tags: self.tags,
            artwork: None,
            artwork_source: self.artwork_source,
            added_seq: 0,
            effective_album_artist: EffectiveAlbumArtist::Unresolved,
        }
    }
}

/// One picture the art cache holds for an album key: which file, which precedence tier, and the
/// contributor (resolution group) it follows when its tracks move to another key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedPicture {
    pub hash: ArtHash,
    pub source: ArtworkSource,
    pub contributor: Option<Contributor>,
}

/// The pictures of one album key, in the art cache's order (which breaks a tier tie).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedAlbumArt {
    pub album_key: String,
    pub pictures: Vec<CachedPicture>,
}

#[derive(Serialize, Deserialize)]
struct IndexFile {
    version: u32,
    tracks: Vec<CachedTrack>,
    artwork: Vec<CachedAlbumArt>,
}

/// Just the version, parsed first so an index from another version is reported as such instead of as
/// a parse error.
#[derive(Deserialize)]
struct IndexHeader {
    version: u32,
}

/// Everything one write needs, copied out of the UI-thread state so the file work can run on another
/// thread. Pixel buffers are reference-counted: only pictures not already on disk are included.
pub struct CacheSnapshot {
    pub tracks: Vec<CachedTrack>,
    pub artwork: Vec<CachedAlbumArt>,
    pub new_pixels: Vec<(ArtHash, SharedPixelBuffer<Rgb8Pixel>)>,
}

impl CacheSnapshot {
    /// Snapshots `state`. `on_disk` is the set of pictures known to be in the art directory already,
    /// whose pixels are therefore not copied again. A track whose path is not valid UTF-8 is left out
    /// (JSON cannot hold it); the rescan still finds it.
    pub fn of(state: &AppState, on_disk: &HashSet<ArtHash>) -> Self {
        let tracks = state.library.records().iter().filter(|record| record.key.path.to_str().is_some()).map(CachedTrack::from_record).collect();
        let (artwork, new_pixels) = state.artwork_snapshot(on_disk);
        Self { tracks, artwork, new_pixels }
    }
}

/// A picture read back from disk, ready for `AppState::restore_artwork`.
pub struct LoadedPicture {
    pub album_key: String,
    pub hash: ArtHash,
    pub source: ArtworkSource,
    pub contributor: Option<Contributor>,
    pub pixels: SharedPixelBuffer<Rgb8Pixel>,
}

pub struct LoadedCache {
    pub tracks: Vec<TrackRecord>,
    pub artwork: Vec<LoadedPicture>,
    /// Pictures that exist on disk (read successfully), so the first write does not copy them again.
    pub on_disk: HashSet<ArtHash>,
}

pub enum LoadOutcome {
    /// No cache yet (first run).
    Missing,
    /// A cache exists but cannot be used (corrupt, other version); the reason is for a diagnostic.
    Ignored(String),
    Loaded(LoadedCache),
}

/// Where the cache lives: the OS cache directory, since everything in it is rebuilt by a rescan.
pub fn cache_dir() -> Option<PathBuf> {
    ProjectDirs::from("com", "Lime Player", "Lime Player").map(|project| project.cache_dir().join("library-cache"))
}

/// Reads the cache in `dir`, leaving out every track in `excluded` ("Remove from Library" always
/// wins over a cached copy). Touches only files inside `dir`. A picture that is missing or damaged is
/// skipped (its album shows the placeholder until the rescan brings the cover back); it never fails
/// the load.
pub fn load(dir: &Path, excluded: &HashSet<TrackKey>) -> LoadOutcome {
    remove_stale_temps(dir, STALE_TEMP_AGE);
    let bytes = match fs::read(dir.join(INDEX_FILE)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return LoadOutcome::Missing,
        Err(error) => return LoadOutcome::Ignored(format!("could not read {INDEX_FILE}: {error}")),
    };
    match serde_json::from_slice::<IndexHeader>(&bytes) {
        Ok(header) if header.version == CACHE_VERSION => {}
        Ok(header) => return LoadOutcome::Ignored(format!("version {} (this build reads version {CACHE_VERSION})", header.version)),
        Err(error) => return LoadOutcome::Ignored(format!("corrupt {INDEX_FILE}: {error}")),
    }
    let index: IndexFile = match serde_json::from_slice(&bytes) {
        Ok(index) => index,
        Err(error) => return LoadOutcome::Ignored(format!("corrupt {INDEX_FILE}: {error}")),
    };

    let tracks: Vec<TrackRecord> = index.tracks.into_iter().filter(|track| !excluded.contains(&track.key)).map(CachedTrack::into_record).collect();

    let mut buffers: HashMap<ArtHash, Option<SharedPixelBuffer<Rgb8Pixel>>> = HashMap::new();
    let mut artwork = Vec::new();
    for album in index.artwork {
        for picture in album.pictures {
            let buffer = buffers.entry(picture.hash).or_insert_with(|| read_art_file(&art_path(dir, picture.hash)));
            if let Some(pixels) = buffer {
                artwork.push(LoadedPicture {
                    album_key: album.album_key.clone(),
                    hash: picture.hash,
                    source: picture.source,
                    contributor: picture.contributor,
                    pixels: pixels.clone(),
                });
            }
        }
    }
    let on_disk = buffers.into_iter().filter_map(|(hash, buffer)| buffer.map(|_| hash)).collect();
    LoadOutcome::Loaded(LoadedCache { tracks, artwork, on_disk })
}

fn art_path(dir: &Path, hash: ArtHash) -> PathBuf {
    dir.join(ART_DIR).join(format!("{}.{ART_EXTENSION}", hash.to_hex()))
}

fn read_art_file(path: &Path) -> Option<SharedPixelBuffer<Rgb8Pixel>> {
    let bytes = fs::read(path).ok()?;
    if bytes.len() < ART_HEADER_LEN || &bytes[..4] != ART_MAGIC {
        return None;
    }
    let width = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
    let height = u32::from_le_bytes(bytes[8..12].try_into().ok()?);
    if width == 0 || height == 0 || width > ART_MAX_SIDE || height > ART_MAX_SIDE {
        return None;
    }
    let rgb = &bytes[ART_HEADER_LEN..];
    if rgb.len() != width as usize * height as usize * 3 {
        return None;
    }
    Some(SharedPixelBuffer::<Rgb8Pixel>::clone_from_slice(rgb, width, height))
}

fn encode_art(buffer: &SharedPixelBuffer<Rgb8Pixel>) -> Vec<u8> {
    let rgb = buffer.as_bytes();
    let mut bytes = Vec::with_capacity(ART_HEADER_LEN + rgb.len());
    bytes.extend_from_slice(ART_MAGIC);
    bytes.extend_from_slice(&buffer.width().to_le_bytes());
    bytes.extend_from_slice(&buffer.height().to_le_bytes());
    bytes.extend_from_slice(rgb);
    bytes
}

/// Writes `snapshot` into `dir`: new pictures first, then the index atomically, then the removal of
/// picture files the new index no longer references (and leftover temp files). Returns the pictures
/// on disk afterwards. A crash at any point leaves either the old index or the new one, and every
/// picture the surviving index names.
pub fn write(dir: &Path, snapshot: &CacheSnapshot) -> Result<HashSet<ArtHash>, String> {
    let art_dir = dir.join(ART_DIR);
    fs::create_dir_all(&art_dir).map_err(|error| format!("{}: {error}", art_dir.display()))?;

    // `new_pixels` holds only pictures not verified on disk (see `CacheSnapshot::of`), so a file that
    // is there anyway is a damaged leftover: it is rewritten, which heals it.
    for (hash, buffer) in &snapshot.new_pixels {
        let path = art_path(dir, *hash);
        let tmp = art_dir.join(temp_name(&hash.to_hex()));
        write_durably(&tmp, &encode_art(buffer))?;
        fs::rename(&tmp, &path).map_err(|error| format!("{}: {error}", path.display()))?;
    }

    let index = serde_json::to_vec(&IndexFileRef { version: CACHE_VERSION, tracks: &snapshot.tracks, artwork: &snapshot.artwork })
        .map_err(|error| error.to_string())?;
    let tmp = dir.join(temp_name(INDEX_FILE));
    write_durably(&tmp, &index)?;
    fs::rename(&tmp, dir.join(INDEX_FILE)).map_err(|error| error.to_string())?;

    let referenced: HashSet<ArtHash> = snapshot.artwork.iter().flat_map(|album| album.pictures.iter().map(|picture| picture.hash)).collect();
    let present = remove_orphans(&art_dir, &referenced);
    remove_stale_temps(dir, STALE_TEMP_AGE);
    Ok(present)
}

/// `.<stem>.<pid>.<nanos>.tmp`: process id plus a timestamp, so neither two instances nor two writes
/// of one instance share a temp file.
fn temp_name(stem: &str) -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_nanos());
    format!(".{stem}.{}.{nanos}.tmp", std::process::id())
}

/// Writes `bytes` to a new file at `path` and flushes it to stable storage before returning, so the
/// `rename` that follows never publishes contents that are still only in the page cache. Uses the
/// repair's `flush_durably` (`F_FULLFSYNC`, falling back to `fsync`); a filesystem that supports
/// neither (smbfs answers ENOTSUP) is tolerated, since the cache is disposable and the rescan
/// rebuilds it. Any other flush error fails the write.
fn write_durably(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let fail = |error: io::Error| format!("{}: {error}", path.display());
    let mut file = fs::File::create(path).map_err(fail)?;
    file.write_all(bytes).map_err(fail)?;
    match crate::audio::rate_repair::flush_durably(&file) {
        Err(error) if !crate::audio::rate_repair::is_flush_unsupported(&error) => Err(fail(error)),
        _ => Ok(()),
    }
}

/// Removes leftover `.…tmp` files (a crashed write) from the cache directory and its art directory
/// that are older than `max_age`. A younger one may be another running instance's write in flight.
fn remove_stale_temps(dir: &Path, max_age: Duration) {
    for directory in [dir.to_path_buf(), dir.join(ART_DIR)] {
        let Ok(entries) = fs::read_dir(&directory) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !(name.starts_with('.') && name.ends_with(".tmp")) {
                continue;
            }
            let old_enough = entry.metadata().and_then(|meta| meta.modified()).ok().and_then(|modified| modified.elapsed().ok()).is_some_and(|age| age >= max_age);
            if old_enough {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

/// Borrowing twin of `IndexFile` so a write does not clone the snapshot to serialize it.
#[derive(Serialize)]
struct IndexFileRef<'a> {
    version: u32,
    tracks: &'a [CachedTrack],
    artwork: &'a [CachedAlbumArt],
}

/// Deletes picture files nothing references any more and stray temp files; returns the referenced
/// pictures that are actually present.
fn remove_orphans(art_dir: &Path, referenced: &HashSet<ArtHash>) -> HashSet<ArtHash> {
    let mut present = HashSet::new();
    let Ok(entries) = fs::read_dir(art_dir) else { return present };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            // A temp file: `remove_stale_temps` decides, it may be another instance's live write.
            continue;
        }
        let hash = name.strip_suffix(&format!(".{ART_EXTENSION}")).and_then(ArtHash::from_hex);
        match hash {
            Some(hash) if referenced.contains(&hash) => {
                present.insert(hash);
            }
            // An unreferenced picture, a stray temp file from a crashed write, or anything else
            // that is not ours to keep in the art directory.
            _ => {
                let _ = fs::remove_file(&path);
            }
        }
    }
    present
}

/// A `DIAG` line on stderr, written the way `main.rs`'s `write_diagnostic` does (no panic on a
/// closed pipe).
pub fn diag(line: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr().lock(), "DIAG library cache: {line}");
}

// ---------------------------------------------------------------------------------------------
// Pruning
// ---------------------------------------------------------------------------------------------

/// What one folder walk (`LibraryEvent::FolderWalked`) established, kept until its batch finishes.
struct WalkRecord {
    root: PathBuf,
    files: HashSet<PathBuf>,
    /// Physical files this batch produced at least one `TrackKey` for (a CUE-split file produces
    /// several).
    probed_paths: HashSet<PathBuf>,
    /// A cue sheet of this batch failed to parse or resolve, so its files fell back to whole-file
    /// tracks: that says nothing reliable about which CUE tracks still exist.
    cue_failed: bool,
    /// Directories that listed empty without an error: not evidence about the files below them.
    empty_dirs: HashSet<PathBuf>,
}

/// A prune that would drop more than half of a set of at least this many cached tracks is refused
/// and the tracks are kept: the listing that "proved" it is far more likely to be a flaky share than
/// a real mass deletion (the user removes those through "Remove from Library"). Below this size a
/// majority is ordinary (one deleted file of one opened file).
const MASS_DELETION_MIN_TRACKS: usize = 10;

fn mass_deletion_refused(gone: usize, total: usize) -> bool {
    total >= MASS_DELETION_MIN_TRACKS && gone * 2 > total
}

/// Follows the startup rescan against the cached tracks and decides which of them are provably gone.
/// A cached track is "unconfirmed" until the rescan produces its exact `TrackKey` (`Probed` or
/// `Scanned`); only unconfirmed tracks can ever be pruned. Two sources of proof:
/// - [`RefreshTracker::finish_batch`]: a folder walk that was complete and reliable did not list the
///   file, or listed it and produced other `TrackKey`s for it (a CUE sheet re-split it);
/// - [`find_missing`] over [`RefreshTracker::sweep_candidates`]: the file's directory lists fine and
///   the file is not in it (individually opened files and cue sheets, or a folder whose walk failed).
///
/// A source that failed (share not mounted, directory unreadable) yields neither, so its cached
/// tracks stay.
#[derive(Default)]
pub struct RefreshTracker {
    /// Every track the cache loaded (confirmed or not): the denominator of the mass-deletion guard.
    cached: HashSet<TrackKey>,
    unconfirmed: HashSet<TrackKey>,
    walks: HashMap<u64, WalkRecord>,
    /// Roots of walks that finished reliably. Their unconfirmed leftovers are files that exist but
    /// did not scan this time (a probe failure), which is not proof of anything either.
    walked_roots: Vec<PathBuf>,
}

impl RefreshTracker {
    pub fn new(cached: impl IntoIterator<Item = TrackKey>) -> Self {
        let cached: HashSet<TrackKey> = cached.into_iter().collect();
        Self { unconfirmed: cached.clone(), cached, ..Self::default() }
    }

    #[cfg(test)]
    pub fn is_idle(&self) -> bool {
        self.unconfirmed.is_empty()
    }

    pub fn note_walk(&mut self, batch: u64, root: PathBuf, files: Vec<PathBuf>, empty_dirs: Vec<PathBuf>) {
        self.walks.insert(
            batch,
            WalkRecord {
                root,
                files: files.into_iter().collect(),
                probed_paths: HashSet::new(),
                cue_failed: false,
                empty_dirs: empty_dirs.into_iter().collect(),
            },
        );
    }

    /// Keeps, of `keys` (a verdict computed on another thread), only the tracks still unconfirmed
    /// now, and stops tracking them: anything the rescan, an explicit re-open or "Remove from
    /// Library" touched while the verdict was being computed is no longer cache-only and must not be
    /// dropped on a stale answer.
    pub fn take_still_unconfirmed(&mut self, keys: Vec<TrackKey>) -> Vec<TrackKey> {
        keys.into_iter().filter(|key| self.unconfirmed.remove(key)).collect()
    }

    /// The rescan found these tracks (phase 1): they exist, so they are no longer cache-only.
    pub fn note_probed<'a>(&mut self, batch: u64, keys: impl IntoIterator<Item = &'a TrackKey>) {
        for key in keys {
            self.unconfirmed.remove(key);
            if let Some(walk) = self.walks.get_mut(&batch) {
                walk.probed_paths.insert(key.path.clone());
            }
        }
    }

    pub fn note_scanned(&mut self, key: &TrackKey) {
        self.unconfirmed.remove(key);
    }

    pub fn note_cue_failure(&mut self, batch: u64) {
        if let Some(walk) = self.walks.get_mut(&batch) {
            walk.cue_failed = true;
        }
    }

    /// Tracks the user removed from the library; nothing left to prune.
    pub fn forget<'a>(&mut self, keys: impl IntoIterator<Item = &'a TrackKey>) {
        for key in keys {
            self.unconfirmed.remove(key);
        }
    }

    /// The batch is done: returns the cached tracks its folder walk proves gone (and stops tracking
    /// them). Empty for a batch with no reliable walk.
    pub fn finish_batch(&mut self, batch: u64) -> Vec<TrackKey> {
        let Some(walk) = self.walks.remove(&batch) else { return Vec::new() };
        let gone: Vec<TrackKey> = self
            .unconfirmed
            .iter()
            .filter(|key| key.path.starts_with(&walk.root) && is_reachable_by_walk(&walk.root, &key.path))
            .filter(|key| !key.path.ancestors().skip(1).any(|ancestor| walk.empty_dirs.contains(ancestor)))
            .filter(|key| {
                if !walk.files.contains(&key.path) {
                    return true;
                }
                !walk.cue_failed && walk.probed_paths.contains(&key.path)
            })
            .cloned()
            .collect();
        self.walked_roots.push(walk.root.clone());
        let total = self.cached.iter().filter(|key| key.path.starts_with(&walk.root)).count();
        if mass_deletion_refused(gone.len(), total) {
            diag(&format!(
                "keeping {} cached track(s) under {}: the scan claims {} of {total} are gone, which looks like an unreliable listing",
                gone.len(),
                walk.root.display(),
                gone.len()
            ));
            return Vec::new();
        }
        for key in &gone {
            self.unconfirmed.remove(key);
        }
        gone
    }

    /// Cached tracks still unconfirmed that no reliable walk covered: candidates for the file-system
    /// check in [`find_missing`].
    pub fn sweep_candidates(&self) -> Vec<TrackKey> {
        self.unconfirmed.iter().filter(|key| !self.walked_roots.iter().any(|root| key.path.starts_with(root))).cloned().collect()
    }
}

/// Which of `candidates` are positively gone: the file's own directory lists successfully and is not
/// empty, the file is not in the listing, and a direct look at the path says "not found". Anything
/// else keeps the track: a directory that is missing or unreadable (an unmounted share, a stale or
/// empty mount point), any error other than not-found, a listing that disagrees with the direct look
/// (a Unicode-normalization difference in the name). A whole deleted directory is therefore kept
/// here; a folder source's walk covers that case.
///
/// Blocks on the file system (a NAS may be slow or hang): run it on a worker thread, never the UI
/// thread.
pub fn find_missing(candidates: Vec<TrackKey>) -> Vec<TrackKey> {
    let total = candidates.len();
    let mut by_directory: HashMap<PathBuf, Vec<TrackKey>> = HashMap::new();
    for key in candidates {
        if let Some(parent) = key.path.parent() {
            by_directory.entry(parent.to_path_buf()).or_default().push(key);
        }
    }
    let mut missing = Vec::new();
    for (directory, keys) in by_directory {
        let Ok(entries) = fs::read_dir(&directory) else { continue };
        let names: HashSet<std::ffi::OsString> = entries.flatten().map(|entry| entry.file_name()).collect();
        if names.is_empty() {
            continue;
        }
        for key in keys {
            let Some(name) = key.path.file_name() else { continue };
            if names.contains(name) {
                continue;
            }
            if matches!(fs::metadata(&key.path), Err(error) if error.kind() == io::ErrorKind::NotFound) {
                missing.push(key);
            }
        }
    }
    if mass_deletion_refused(missing.len(), total) {
        diag(&format!("keeping {} cached track(s): the check claims {} of {total} are gone, which looks like an unreliable listing", missing.len(), missing.len()));
        return Vec::new();
    }
    missing
}

// ---------------------------------------------------------------------------------------------
// Runtime: debounced background writes
// ---------------------------------------------------------------------------------------------

type WriteResult = Result<HashSet<ArtHash>, String>;

/// The UI-thread side of the cache: remembers that the library changed, writes it on a worker
/// thread when that is due (coalescing the many batches of a startup rescan into few writes), and
/// owns the pruning bookkeeping. Not `Send`; lives with the rest of the UI-thread state.
pub struct CacheRuntime {
    dir: Option<PathBuf>,
    on_disk: HashSet<ArtHash>,
    dirty: bool,
    last_write: Instant,
    /// Set by a failed write; `flush_if_due` starts nothing before it.
    retry_at: Option<Instant>,
    retry_backoff: Duration,
    writer: Option<JoinHandle<WriteResult>>,
    pub tracker: RefreshTracker,
    sweep_pending: bool,
    sweep: Option<mpsc::Receiver<Vec<TrackKey>>>,
}

impl CacheRuntime {
    pub fn new(dir: Option<PathBuf>) -> Self {
        Self {
            dir,
            on_disk: HashSet::new(),
            dirty: false,
            last_write: Instant::now(),
            retry_at: None,
            retry_backoff: WRITE_RETRY_BACKOFF,
            writer: None,
            tracker: RefreshTracker::default(),
            sweep_pending: false,
            sweep: None,
        }
    }

    /// Reads the cache into `state` and starts tracking its tracks for pruning. Reports problems as
    /// `DIAG` lines; never fails.
    pub fn load_into(&mut self, state: &mut AppState, excluded: &HashSet<TrackKey>) {
        let Some(dir) = self.dir.clone() else { return };
        match load(&dir, excluded) {
            LoadOutcome::Missing => {}
            LoadOutcome::Ignored(reason) => diag(&format!("ignoring {}: {reason}", dir.display())),
            LoadOutcome::Loaded(loaded) => {
                self.on_disk = loaded.on_disk.clone();
                self.tracker = RefreshTracker::new(loaded.tracks.iter().map(|record| record.key.clone()));
                self.sweep_pending = !loaded.tracks.is_empty();
                state.restore_cache(loaded);
            }
        }
    }

    /// The library changed in a way the cache should reflect.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Reaps a finished writer; a failure re-arms `dirty` so the next flush retries.
    fn reap_writer(&mut self) {
        if self.writer.as_ref().is_some_and(JoinHandle::is_finished) {
            self.finish_writer();
        }
    }

    fn finish_writer(&mut self) {
        let Some(handle) = self.writer.take() else { return };
        match handle.join() {
            Ok(Ok(on_disk)) => {
                self.on_disk = on_disk;
                self.retry_at = None;
            }
            Ok(Err(error)) => {
                diag(&format!("could not write the cache: {error}"));
                self.write_failed();
            }
            Err(_) => {
                diag("the cache writer panicked");
                self.write_failed();
            }
        }
    }

    fn write_failed(&mut self) {
        self.dirty = true;
        self.retry_at = Some(Instant::now() + self.retry_backoff);
    }

    /// Starts a background write when the library changed and either no scan is running or the last
    /// write is old enough. At most one write is in flight. Called from the UI timer.
    pub fn flush_if_due(&mut self, state: &AppState, scanning: bool) {
        self.reap_writer();
        if !self.dirty || self.writer.is_some() || self.dir.is_none() || self.retry_at.is_some_and(|at| Instant::now() < at) {
            return;
        }
        if scanning && self.last_write.elapsed() < WRITE_INTERVAL_DURING_SCAN {
            return;
        }
        let Some(dir) = self.dir.clone() else { return };
        let snapshot = CacheSnapshot::of(state, &self.on_disk);
        self.dirty = false;
        self.last_write = Instant::now();
        match thread::Builder::new().name("lime-library-cache".into()).spawn(move || write(&dir, &snapshot)) {
            Ok(handle) => self.writer = Some(handle),
            Err(error) => {
                diag(&format!("could not start the cache writer: {error}"));
                self.write_failed();
            }
        }
    }

    /// Writes any pending change and waits for it: the clean-exit flush.
    pub fn flush_blocking(&mut self, state: &AppState) {
        self.finish_writer();
        if !self.dirty {
            return;
        }
        let Some(dir) = self.dir.clone() else { return };
        let snapshot = CacheSnapshot::of(state, &self.on_disk);
        match write(&dir, &snapshot) {
            Ok(on_disk) => {
                self.on_disk = on_disk;
                self.dirty = false;
            }
            Err(error) => diag(&format!("could not write the cache: {error}")),
        }
    }

    /// Once the startup rescan has finished, starts the file-system check of the cached tracks no
    /// reliable folder walk covered. Runs at most once.
    pub fn start_sweep_if_ready(&mut self, scanning: bool) {
        if scanning || !self.sweep_pending {
            return;
        }
        self.sweep_pending = false;
        let candidates = self.tracker.sweep_candidates();
        if candidates.is_empty() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        if thread::Builder::new()
            .name("lime-library-cache-sweep".into())
            .spawn(move || {
                let _ = tx.send(find_missing(candidates));
            })
            .is_ok()
        {
            self.sweep = Some(rx);
        }
    }

    /// The sweep's verdict, once it has one.
    pub fn take_sweep_result(&mut self) -> Option<Vec<TrackKey>> {
        let result = match self.sweep.as_ref()?.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => Vec::new(),
        };
        self.sweep = None;
        Some(self.tracker.take_still_unconfirmed(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::ArtworkPixels;
    use std::time::SystemTime;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let suffix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let dir = std::env::temp_dir().join(format!("lime-player-library-cache-{name}-{}-{suffix}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn info() -> AudioInfo {
        AudioInfo { sample_rate: 44_100, duration_ms: Some(180_000), source_channels: 2, bits_per_sample: 16, is_float: false, integer_pcm: true, format: "FLAC".into() }
    }

    fn key(path: &str) -> TrackKey {
        TrackKey::whole_file(PathBuf::from(path))
    }

    fn record(path: &str, album: &str, artist: &str) -> TrackRecord {
        let mut record = TrackRecord::minimal(key(path), info());
        record.tags.title = Some(format!("Song {path}"));
        record.tags.album = Some(album.into());
        record.tags.artist = Some(artist.into());
        record.tags.album_artist = Some(artist.into());
        record.tags.lyrics = Some("la la".into());
        record.file_size = Some(1234);
        record
    }

    fn pixels(side: u32, shade: u8) -> ArtworkPixels {
        ArtworkPixels { width: side, height: side, rgb: vec![shade; (side * side * 3) as usize] }
    }

    fn picture_count(state: &AppState) -> usize {
        state.artwork_snapshot(&HashSet::new()).0.iter().map(|album| album.pictures.len()).sum()
    }

    fn write_state(dir: &Path, state: &AppState) -> HashSet<ArtHash> {
        write(dir, &CacheSnapshot::of(state, &HashSet::new())).unwrap()
    }

    fn loaded(outcome: LoadOutcome) -> LoadedCache {
        match outcome {
            LoadOutcome::Loaded(cache) => cache,
            LoadOutcome::Missing => panic!("expected a cache, found none"),
            LoadOutcome::Ignored(reason) => panic!("cache unexpectedly ignored: {reason}"),
        }
    }

    #[test]
    fn records_and_artwork_tiers_round_trip() {
        let dir = TempDir::new("round-trip");
        let mut state = AppState::new();
        let mut first = record("/m/Album/01.flac", "Album", "Artist");
        first.artwork = Some(pixels(4, 10));
        first.artwork_source = ArtworkSource::Folder;
        state.apply_scanned(first);
        let mut second = record("/m/Album/02.flac", "Album", "Artist");
        second.artwork_source = ArtworkSource::None;
        state.apply_scanned(second);
        let mut lone = record("/m/Other/01.flac", "Other", "Someone");
        lone.artwork = Some(pixels(6, 200));
        lone.artwork_source = ArtworkSource::Embedded;
        state.apply_scanned(lone);

        write_state(dir.path(), &state);
        let cache = loaded(load(dir.path(), &HashSet::new()));

        let mut restored = AppState::new();
        restored.restore_cache(cache);
        let originals = state.library.records();
        let copies = restored.library.records();
        assert_eq!(originals.len(), copies.len());
        for (original, copy) in originals.iter().zip(copies) {
            assert_eq!(copy.key, original.key);
            assert_eq!(copy.info, original.info);
            assert_eq!(copy.tags, original.tags);
            assert_eq!(copy.file_size, original.file_size);
            assert_eq!(copy.artwork_source, original.artwork_source);
            assert_eq!(copy.added_seq, original.added_seq);
            assert_eq!(copy.effective_album_artist, original.effective_album_artist);
        }
        assert_eq!(restored.artwork_snapshot(&HashSet::new()).0, state.artwork_snapshot(&HashSet::new()).0);
        let album = crate::library::album_key(&restored.library.records()[0]);
        assert!(restored.artwork_for_key(&album).is_some());
        let other = crate::library::album_key(&restored.library.records()[2]);
        assert_eq!(restored.artwork_for_key(&other).unwrap().to_rgb8().unwrap().width(), 6);
    }

    #[test]
    fn restored_artwork_keeps_the_precedence_rules_working() {
        let dir = TempDir::new("tiers");
        let mut state = AppState::new();
        let mut embedded = record("/m/Album/01.flac", "Album", "Artist");
        embedded.artwork = Some(pixels(4, 10));
        embedded.artwork_source = ArtworkSource::Embedded;
        state.apply_scanned(embedded);
        write_state(dir.path(), &state);

        let mut restored = AppState::new();
        restored.restore_cache(loaded(load(dir.path(), &HashSet::new())));
        let album = crate::library::album_key(&restored.library.records()[0]);

        // A rescan's higher tier replaces the cached picture; a lower one never does.
        let mut better = record("/m/Album/01.flac", "Album", "Artist");
        better.artwork = Some(pixels(8, 99));
        better.artwork_source = ArtworkSource::Folder;
        restored.apply_scanned(better);
        assert_eq!(restored.artwork_for_key(&album).unwrap().to_rgb8().unwrap().width(), 8);
        let mut worse = record("/m/Album/01.flac", "Album", "Artist");
        worse.artwork = Some(pixels(2, 1));
        worse.artwork_source = ArtworkSource::FolderFallback;
        restored.apply_scanned(worse);
        assert_eq!(restored.artwork_for_key(&album).unwrap().to_rgb8().unwrap().width(), 8);
    }

    #[test]
    fn a_missing_cache_is_not_an_error() {
        let dir = TempDir::new("missing");
        assert!(matches!(load(&dir.path().join("nothing-here"), &HashSet::new()), LoadOutcome::Missing));
    }

    #[test]
    fn a_corrupt_index_is_ignored() {
        let dir = TempDir::new("corrupt");
        fs::write(dir.path().join(INDEX_FILE), b"{ not json").unwrap();
        assert!(matches!(load(dir.path(), &HashSet::new()), LoadOutcome::Ignored(_)));
        fs::write(dir.path().join(INDEX_FILE), br#"{"version":1,"tracks":"wrong"}"#).unwrap();
        assert!(matches!(load(dir.path(), &HashSet::new()), LoadOutcome::Ignored(_)));
    }

    #[test]
    fn another_version_is_ignored_even_when_its_shape_is_unknown() {
        let dir = TempDir::new("version");
        fs::write(dir.path().join(INDEX_FILE), br#"{"version":999,"something":"new"}"#).unwrap();
        match load(dir.path(), &HashSet::new()) {
            LoadOutcome::Ignored(reason) => assert!(reason.contains("999"), "{reason}"),
            _ => panic!("a future version must be ignored"),
        }
    }

    #[test]
    fn an_ignored_cache_is_replaced_by_the_next_write() {
        let dir = TempDir::new("replace");
        fs::write(dir.path().join(INDEX_FILE), b"garbage").unwrap();
        let mut state = AppState::new();
        state.apply_scanned(record("/m/a.flac", "A", "X"));
        write_state(dir.path(), &state);
        assert_eq!(loaded(load(dir.path(), &HashSet::new())).tracks.len(), 1);
    }

    #[test]
    fn excluded_tracks_never_come_back_from_the_cache() {
        let dir = TempDir::new("excluded");
        let mut state = AppState::new();
        state.apply_scanned(record("/m/Album/01.flac", "Album", "Artist"));
        state.apply_scanned(record("/m/Album/02.flac", "Album", "Artist"));
        write_state(dir.path(), &state);

        let excluded: HashSet<TrackKey> = [key("/m/Album/01.flac")].into_iter().collect();
        let cache = loaded(load(dir.path(), &excluded));
        let keys: Vec<_> = cache.tracks.iter().map(|record| record.key.clone()).collect();
        assert_eq!(keys, vec![key("/m/Album/02.flac")]);
    }

    #[test]
    fn a_damaged_picture_file_costs_only_that_picture() {
        let dir = TempDir::new("damaged-art");
        let mut state = AppState::new();
        let mut one = record("/m/A/01.flac", "A", "X");
        one.artwork = Some(pixels(4, 1));
        one.artwork_source = ArtworkSource::Embedded;
        state.apply_scanned(one);
        let mut two = record("/m/B/01.flac", "B", "Y");
        two.artwork = Some(pixels(4, 2));
        two.artwork_source = ArtworkSource::Embedded;
        state.apply_scanned(two);
        write_state(dir.path(), &state);

        let victim = ArtHash::of(4, 4, &pixels(4, 1).rgb);
        fs::write(art_path(dir.path(), victim), b"junk").unwrap();
        let cache = loaded(load(dir.path(), &HashSet::new()));
        assert_eq!(cache.tracks.len(), 2);
        assert_eq!(cache.artwork.len(), 1);
        assert!(!cache.on_disk.contains(&victim));
    }

    #[test]
    fn identical_covers_are_stored_once_and_orphans_are_removed() {
        let dir = TempDir::new("dedup");
        let mut state = AppState::new();
        for (path, album) in [("/m/A/01.flac", "A"), ("/m/B/01.flac", "B")] {
            let mut track = record(path, album, "Same Cover");
            track.artwork = Some(pixels(4, 7));
            track.artwork_source = ArtworkSource::Embedded;
            state.apply_scanned(track);
        }
        let mut distinct = record("/m/C/01.flac", "C", "Other");
        distinct.artwork = Some(pixels(4, 9));
        distinct.artwork_source = ArtworkSource::Embedded;
        state.apply_scanned(distinct);

        let on_disk = write_state(dir.path(), &state);
        assert_eq!(picture_count(&state), 3);
        assert_eq!(on_disk.len(), 2, "two albums share one cover file");
        let files = |dir: &Path| fs::read_dir(dir.join(ART_DIR)).unwrap().count();
        assert_eq!(files(dir.path()), 2);

        // Remove the album with the distinct cover: its file is orphaned and cleaned up.
        let album_c = crate::library::album_key(state.library.get(&key("/m/C/01.flac")).unwrap());
        state.remove_album(&album_c);
        // A stray temp file from a crashed write goes too.
        let stray = dir.path().join(ART_DIR).join(".deadbeef.1.1.tmp");
        fs::write(&stray, b"x").unwrap();
        fs::File::options().write(true).open(&stray).unwrap().set_modified(std::time::SystemTime::now() - Duration::from_secs(3600)).unwrap();
        let on_disk = write_state(dir.path(), &state);
        assert_eq!(on_disk.len(), 1);
        assert_eq!(files(dir.path()), 1);
    }

    #[test]
    fn known_pictures_are_not_copied_again() {
        let mut state = AppState::new();
        let mut track = record("/m/A/01.flac", "A", "X");
        track.artwork = Some(pixels(4, 1));
        track.artwork_source = ArtworkSource::Embedded;
        state.apply_scanned(track);
        let hash = ArtHash::of(4, 4, &pixels(4, 1).rgb);
        assert_eq!(CacheSnapshot::of(&state, &HashSet::new()).new_pixels.len(), 1);
        let known: HashSet<ArtHash> = [hash].into_iter().collect();
        let snapshot = CacheSnapshot::of(&state, &known);
        assert!(snapshot.new_pixels.is_empty());
        assert_eq!(snapshot.artwork[0].pictures[0].hash, hash);
    }

    #[test]
    fn a_failed_write_leaves_the_previous_cache_readable() {
        let dir = TempDir::new("atomic");
        let mut state = AppState::new();
        state.apply_scanned(record("/m/a.flac", "A", "X"));
        write_state(dir.path(), &state);
        // The art directory path is occupied by a file, so the next write cannot even start.
        fs::remove_dir_all(dir.path().join(ART_DIR)).unwrap();
        fs::write(dir.path().join(ART_DIR), b"in the way").unwrap();
        state.apply_scanned(record("/m/b.flac", "B", "Y"));
        assert!(write(dir.path(), &CacheSnapshot::of(&state, &HashSet::new())).is_err());
        assert_eq!(loaded(load(dir.path(), &HashSet::new())).tracks.len(), 1);
    }

    #[test]
    fn loading_resolves_album_groups_like_scanning_does() {
        let dir = TempDir::new("groups");
        let mut state = AppState::new();
        // Two tracks of one folder, no ALBUMARTIST, a featured guest on one: the group resolves to
        // the primary artist however the tracks arrive.
        for (file, artist) in [("01.flac", "Dua Lipa"), ("02.flac", "Dua Lipa feat. Miguel"), ("03.flac", "Dua Lipa")] {
            let mut track = record(&format!("/m/Album/{file}"), "Album", artist);
            track.tags.album_artist = None;
            state.apply_scanned(track);
        }
        write_state(dir.path(), &state);
        let mut restored = AppState::new();
        restored.restore_cache(loaded(load(dir.path(), &HashSet::new())));
        let keys = |state: &AppState| state.library.records().iter().map(crate::library::album_key).collect::<Vec<_>>();
        assert_eq!(keys(&restored), keys(&state));
        assert_eq!(restored.library.albums("", None).len(), 1);
    }

    // ----- pruning ---------------------------------------------------------------------------

    fn tracker(paths: &[&str]) -> RefreshTracker {
        RefreshTracker::new(paths.iter().map(|path| key(path)))
    }

    #[test]
    fn a_reliable_walk_prunes_the_cached_tracks_it_did_not_find() {
        let mut tracker = tracker(&["/m/A/01.flac", "/m/A/02.flac", "/m/A/gone.flac", "/other/x.flac"]);
        tracker.note_walk(1, PathBuf::from("/m"), vec![PathBuf::from("/m/A/01.flac"), PathBuf::from("/m/A/02.flac")], Vec::new());
        tracker.note_probed(1, [&key("/m/A/01.flac")]);
        tracker.note_scanned(&key("/m/A/01.flac"));
        assert_eq!(tracker.finish_batch(1), vec![key("/m/A/gone.flac")]);
    }

    #[test]
    fn a_source_with_no_reliable_walk_keeps_its_cached_tracks() {
        let mut tracker = tracker(&["/m/A/01.flac"]);
        // No FolderWalked event (unmounted, incomplete or empty listing): finishing prunes nothing.
        assert!(tracker.finish_batch(1).is_empty());
        assert_eq!(tracker.sweep_candidates(), vec![key("/m/A/01.flac")]);
    }

    #[test]
    fn a_present_file_that_failed_to_probe_is_kept() {
        let mut tracker = tracker(&["/m/A/01.flac"]);
        tracker.note_walk(1, PathBuf::from("/m"), vec![PathBuf::from("/m/A/01.flac")], Vec::new());
        assert!(tracker.finish_batch(1).is_empty());
        assert!(tracker.sweep_candidates().is_empty(), "a walked file needs no separate file-system check");
    }

    fn cue_key(path: &str, start: u64, end: Option<u64>) -> TrackKey {
        TrackKey { path: PathBuf::from(path), start_frame: start, end_frame: end }
    }

    #[test]
    fn a_resplit_cue_file_drops_its_old_tracks_only_when_the_new_split_is_known() {
        let cached = || [cue_key("/m/D/disc.flac", 0, Some(100)), cue_key("/m/D/disc.flac", 100, None)];
        let walk = |tracker: &mut RefreshTracker| tracker.note_walk(1, PathBuf::from("/m"), vec![PathBuf::from("/m/D/disc.flac")], Vec::new());

        // The sheet now splits the file differently: the new keys confirm themselves, the old ones go.
        let mut resplit = RefreshTracker::new(cached());
        walk(&mut resplit);
        resplit.note_probed(1, [&cue_key("/m/D/disc.flac", 0, Some(50)), &cue_key("/m/D/disc.flac", 50, None)]);
        let mut gone = resplit.finish_batch(1);
        gone.sort();
        assert_eq!(gone, cached().to_vec());

        // The same cue tracks come back: nothing to prune.
        let mut same = RefreshTracker::new(cached());
        walk(&mut same);
        let keys = cached();
        same.note_probed(1, keys.iter());
        assert!(same.finish_batch(1).is_empty());

        // The sheet failed to parse this time (files fell back to whole-file tracks): keep the cached
        // CUE tracks rather than trust a transient failure.
        let mut failed = RefreshTracker::new(cached());
        walk(&mut failed);
        failed.note_cue_failure(1);
        failed.note_probed(1, [&key("/m/D/disc.flac")]);
        assert!(failed.finish_batch(1).is_empty());
    }

    #[test]
    fn a_cue_file_deleted_from_a_walked_folder_loses_all_of_its_tracks() {
        let mut tracker = RefreshTracker::new([cue_key("/m/D/disc.flac", 0, Some(100)), cue_key("/m/D/disc.flac", 100, None)]);
        tracker.note_walk(1, PathBuf::from("/m"), vec![PathBuf::from("/m/E/other.flac")], Vec::new());
        assert_eq!(tracker.finish_batch(1).len(), 2);
    }

    #[test]
    fn files_a_walk_never_lists_are_not_judged_by_it() {
        let mut tracker = tracker(&["/m/.hidden/a.flac", "/m/Disc.wv/a.flac", "/m/notes/a.cue"]);
        tracker.note_walk(1, PathBuf::from("/m"), Vec::new(), Vec::new());
        assert!(tracker.finish_batch(1).is_empty());
    }

    #[test]
    fn a_track_the_user_removed_is_forgotten() {
        let mut tracker = tracker(&["/m/A/01.flac"]);
        tracker.forget([&key("/m/A/01.flac")]);
        assert!(tracker.is_idle());
    }

    #[test]
    fn find_missing_needs_a_readable_directory_that_lacks_the_file() {
        let dir = TempDir::new("sweep");
        fs::create_dir_all(dir.path().join("Album")).unwrap();
        fs::write(dir.path().join("Album/present.flac"), b"x").unwrap();
        fs::write(dir.path().join("Album/sibling.flac"), b"x").unwrap();
        fs::create_dir_all(dir.path().join("Empty")).unwrap();

        let present = TrackKey::whole_file(dir.path().join("Album/present.flac"));
        let gone = TrackKey::whole_file(dir.path().join("Album/gone.flac"));
        let gone_cue = cue_key(dir.path().join("Album/gone.flac").to_str().unwrap(), 10, None);
        // The directory itself is missing (an unmounted share): keep.
        let unmounted = TrackKey::whole_file(dir.path().join("NotMounted/a.flac"));
        // The directory exists but is empty (a stale mount point): keep.
        let in_empty = TrackKey::whole_file(dir.path().join("Empty/a.flac"));

        let mut missing = find_missing(vec![present, gone.clone(), gone_cue.clone(), unmounted, in_empty]);
        missing.sort();
        let mut expected = vec![gone, gone_cue];
        expected.sort();
        assert_eq!(missing, expected);
    }

    #[test]
    fn pruning_drops_tracks_and_the_pictures_of_albums_that_vanish() {
        let mut state = AppState::new();
        for (path, album) in [("/m/A/01.flac", "A"), ("/m/A/02.flac", "A"), ("/m/B/01.flac", "B")] {
            let mut track = record(path, album, "X");
            track.artwork = Some(pixels(4, path.len() as u8));
            track.artwork_source = ArtworkSource::Embedded;
            state.apply_scanned(track);
        }
        let album_b = crate::library::album_key(state.library.get(&key("/m/B/01.flac")).unwrap());
        let album_a = crate::library::album_key(state.library.get(&key("/m/A/01.flac")).unwrap());

        let gone: HashSet<TrackKey> = [key("/m/A/02.flac"), key("/m/B/01.flac")].into_iter().collect();
        let (_, vanished) = state.prune_tracks(&gone);

        assert_eq!(vanished, vec![album_b.clone()]);
        assert_eq!(state.library.records().len(), 1);
        assert!(state.artwork_for_key(&album_b).is_none());
        assert!(state.artwork_for_key(&album_a).is_some(), "the album that kept a track keeps its picture");
    }

    #[test]
    fn a_failing_write_backs_off_instead_of_retrying_every_tick() {
        let dir = TempDir::new("backoff");
        // The cache directory path is occupied by a file, so every write fails.
        let blocked = dir.path().join("blocked");
        fs::write(&blocked, b"in the way").unwrap();
        let mut runtime = CacheRuntime::new(Some(blocked.clone()));
        let mut state = AppState::new();
        state.apply_scanned(record("/m/a.flac", "A", "X"));

        runtime.mark_dirty();
        runtime.flush_if_due(&state, false);
        runtime.finish_writer();
        assert!(runtime.dirty, "a failed write stays pending");
        assert!(runtime.retry_at.is_some());

        runtime.flush_if_due(&state, false);
        assert!(runtime.writer.is_none(), "no new attempt inside the backoff");

        // Once the backoff has passed and the disk works again, the pending change is written.
        fs::remove_file(&blocked).unwrap();
        runtime.retry_at = Some(Instant::now() - Duration::from_secs(1));
        runtime.flush_if_due(&state, false);
        runtime.finish_writer();
        assert!(blocked.join(INDEX_FILE).exists());
        assert!(runtime.retry_at.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_picture_from_a_non_utf8_path_does_not_break_the_write() {
        use std::os::unix::ffi::OsStrExt;
        let dir = TempDir::new("non-utf8");
        let mut state = AppState::new();
        state.apply_scanned(record("/m/ok/01.flac", "Ok", "X"));
        let odd = PathBuf::from(std::ffi::OsStr::from_bytes(b"/m/caf\xe9"));
        let album = "aa:odd|odd";
        state.cache_artwork(album, &pixels(4, 3), ArtworkSource::Embedded, Some(&("odd".to_owned(), odd)));
        state.cache_artwork("aa:x|ok", &pixels(4, 5), ArtworkSource::Embedded, Some(&("ok".to_owned(), PathBuf::from("/m/ok"))));

        let snapshot = CacheSnapshot::of(&state, &HashSet::new());
        assert_eq!(snapshot.artwork.len(), 1, "only the UTF-8 contributor's picture is cached");
        assert_eq!(snapshot.new_pixels.len(), 1);
        write(dir.path(), &snapshot).unwrap();
        assert_eq!(loaded(load(dir.path(), &HashSet::new())).tracks.len(), 1);
    }

    #[test]
    fn a_damaged_picture_file_is_rewritten_when_the_rescan_brings_the_cover_back() {
        let dir = TempDir::new("heal");
        let mut state = AppState::new();
        let mut track = record("/m/A/01.flac", "A", "X");
        track.artwork = Some(pixels(4, 1));
        track.artwork_source = ArtworkSource::Embedded;
        state.apply_scanned(track.clone());
        write_state(dir.path(), &state);
        let hash = ArtHash::of(4, 4, &pixels(4, 1).rgb);
        fs::write(art_path(dir.path(), hash), b"junk").unwrap();

        // Next launch: the damaged picture is skipped, so it is not "on disk"; the rescan re-caches it.
        let mut restored = AppState::new();
        let cache = loaded(load(dir.path(), &HashSet::new()));
        let on_disk = cache.on_disk.clone();
        restored.restore_cache(cache);
        restored.apply_scanned(track);
        write(dir.path(), &CacheSnapshot::of(&restored, &on_disk)).unwrap();

        assert_eq!(loaded(load(dir.path(), &HashSet::new())).artwork.len(), 1);
    }

    #[test]
    fn written_files_are_published_complete_and_leftover_temps_are_cleaned() {
        let dir = TempDir::new("temps");
        let mut state = AppState::new();
        state.apply_scanned(record("/m/a.flac", "A", "X"));
        write_state(dir.path(), &state);
        let leftovers = |dir: &Path| {
            fs::read_dir(dir).unwrap().flatten().filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp")).count()
        };
        assert_eq!(leftovers(dir.path()), 0, "a finished write leaves no temp file");

        let old = SystemTime::now() - Duration::from_secs(3600);
        let stale = dir.path().join(".index.json.4242.1.tmp");
        let live = dir.path().join(".index.json.4343.2.tmp");
        fs::write(&stale, b"x").unwrap();
        fs::write(&live, b"x").unwrap();
        fs::File::options().write(true).open(&stale).unwrap().set_modified(old).unwrap();

        loaded(load(dir.path(), &HashSet::new()));
        assert!(!stale.exists(), "an old temp file is a crashed write's leftover");
        assert!(live.exists(), "a young one may be another instance's write in flight");
    }

    #[test]
    fn temp_names_differ_between_writes() {
        assert_ne!(temp_name("index.json"), temp_name("index.json"));
        assert!(temp_name("index.json").contains(&std::process::id().to_string()));
    }

    #[test]
    fn a_directory_that_listed_empty_proves_nothing_about_its_cached_tracks() {
        let mut tracker = tracker(&["/m/Flaky/01.flac", "/m/Flaky/Sub/02.flac", "/m/Real/gone.flac"]);
        tracker.note_walk(1, PathBuf::from("/m"), Vec::new(), vec![PathBuf::from("/m/Flaky")]);
        // Only the file whose directories all listed non-empty counts as gone.
        assert_eq!(tracker.finish_batch(1), vec![key("/m/Real/gone.flac")]);
    }

    #[test]
    fn a_walk_that_would_drop_most_of_a_root_is_refused() {
        let paths: Vec<String> = (0..12).map(|index| format!("/m/A/{index:02}.flac")).collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        // The walk lists only 5 of 12: 7 "gone" is more than half.
        let mut refused = tracker(&refs);
        refused.note_walk(1, PathBuf::from("/m"), paths[..5].iter().map(PathBuf::from).collect(), Vec::new());
        assert!(refused.finish_batch(1).is_empty());
        assert_eq!(refused.unconfirmed.len(), 12, "nothing was dropped or forgotten");

        // 5 of 12 gone is under half: pruned.
        let mut allowed = tracker(&refs);
        allowed.note_walk(1, PathBuf::from("/m"), paths[..7].iter().map(PathBuf::from).collect(), Vec::new());
        assert_eq!(allowed.finish_batch(1).len(), 5);

        assert!(!mass_deletion_refused(1, 1), "a single deleted file is an ordinary deletion");
        assert!(!mass_deletion_refused(5, 10));
        assert!(mass_deletion_refused(6, 10));
    }

    #[test]
    fn the_sweep_refuses_a_mass_deletion_too() {
        let dir = TempDir::new("sweep-guard");
        fs::create_dir_all(dir.path().join("Album")).unwrap();
        fs::write(dir.path().join("Album/other.flac"), b"x").unwrap();
        let candidates: Vec<TrackKey> = (0..10).map(|index| TrackKey::whole_file(dir.path().join(format!("Album/{index}.flac")))).collect();
        assert!(find_missing(candidates).is_empty(), "every candidate missing from a readable directory looks like a flaky listing");
    }

    #[test]
    fn a_sweep_verdict_is_dropped_for_tracks_the_rescan_touched_meanwhile() {
        let mut tracker = tracker(&["/m/a.flac", "/m/b.flac", "/m/c.flac"]);
        // The sweep started with all three as candidates and judged them gone; meanwhile a re-open
        // probed a, a scan reached b, and the user removed c's album.
        tracker.note_probed(7, [&key("/m/a.flac")]);
        tracker.note_scanned(&key("/m/b.flac"));
        tracker.forget([&key("/m/c.flac")]);
        let verdict = vec![key("/m/a.flac"), key("/m/b.flac"), key("/m/c.flac")];
        assert!(tracker.take_still_unconfirmed(verdict).is_empty());

        let mut untouched = RefreshTracker::new([key("/m/d.flac")]);
        assert_eq!(untouched.take_still_unconfirmed(vec![key("/m/d.flac")]), vec![key("/m/d.flac")]);
    }

    #[test]
    fn the_runtime_writes_when_due_and_flushes_on_exit() {
        let dir = TempDir::new("runtime");
        let mut runtime = CacheRuntime::new(Some(dir.path().to_path_buf()));
        let mut state = AppState::new();
        state.apply_scanned(record("/m/a.flac", "A", "X"));

        // Clean: nothing to write.
        runtime.flush_if_due(&state, false);
        assert!(!dir.path().join(INDEX_FILE).exists());

        // Dirty during a fresh scan: held back (coalesced) until the scan ends or the interval passes.
        runtime.mark_dirty();
        runtime.flush_if_due(&state, true);
        assert!(!dir.path().join(INDEX_FILE).exists());

        // Dirty and idle: written on a worker thread.
        runtime.flush_if_due(&state, false);
        runtime.finish_writer();
        assert!(dir.path().join(INDEX_FILE).exists());

        // A later change is picked up by the exit flush, which waits for it.
        state.apply_scanned(record("/m/b.flac", "B", "Y"));
        runtime.mark_dirty();
        runtime.flush_blocking(&state);
        assert_eq!(loaded(load(dir.path(), &HashSet::new())).tracks.len(), 2);
    }

    #[test]
    fn the_runtime_loads_its_cache_and_tracks_it_for_pruning() {
        let dir = TempDir::new("runtime-load");
        let mut state = AppState::new();
        state.apply_scanned(record("/m/a.flac", "A", "X"));
        state.apply_scanned(record("/m/b.flac", "B", "Y"));
        write_state(dir.path(), &state);

        let mut runtime = CacheRuntime::new(Some(dir.path().to_path_buf()));
        let mut restored = AppState::new();
        let excluded: HashSet<TrackKey> = [key("/m/b.flac")].into_iter().collect();
        runtime.load_into(&mut restored, &excluded);
        assert_eq!(restored.library.records().len(), 1);
        assert_eq!(runtime.tracker.sweep_candidates(), vec![key("/m/a.flac")]);
    }
}

//! The session library (`§3.3`): everything opened this run, keyed by path, grouped into albums
//! and artists. `store::LibrarySources` persists *which paths* to re-scan (individually opened
//! files and added folders) so `main.rs` can rebuild this library at the next startup by re-running
//! them through `scanner::LibraryScanner`; `cache` additionally saves the decoded records and album
//! pictures so the library shows at once at launch, before (and even without) that rescan.

pub mod cache;
pub mod cue;
pub mod format;
mod grouping;
#[cfg(test)]
mod grouping_tests;
pub mod repair;
pub mod scanner;
pub mod store;
pub mod walker;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::audio::{AudioInfo, PreparedTrack, TrackTags};
use format::aggregate_album_pill;
pub use grouping::effective_disc_number;
use grouping::{
    CopyTrack, Derived, SourceKey, clean_album_title, derive, normalize_album_title, normalize_artist_key, parse_disc_designator, plan_copies,
    resolve_scope_albums, source_key,
};

/// Decoded, downscaled album art (RGB8, longest side capped at 400 px by `scanner::decode_artwork`).
/// Converted to a `slint::Image` on the UI thread only (`§3.3` "Artwork cache").
#[derive(Clone, Debug, PartialEq)]
pub struct ArtworkPixels {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

/// Where a resolved album picture came from — its precedence tier (`scanner::resolve_artwork`,
/// `CLAUDE.md` "Album grouping" -> "Artwork").
///
/// The derived `Ord` IS the precedence: variants are declared from lowest to highest, so a picture
/// replaces another for the same album only when its source compares strictly greater. The scanner's
/// per-album dedup and `AppState`'s artwork cache both rely on this one ordering; do not reorder the
/// variants (`artwork_source_orders_by_precedence` pins it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ArtworkSource {
    /// No picture was found.
    None,
    /// The legacy `folder.jpg/.jpeg/.png`: only ever used when no named folder cover and no embedded
    /// picture exists, so an album with neither still gets a thumbnail.
    FolderFallback,
    /// The picture embedded in the track's tags.
    Embedded,
    /// An image file next to the audio (or in the release folder above a disc subfolder) named
    /// `cover`, `art`, `album art`, `album` or `front`: beats the embedded picture.
    Folder,
}

/// Identity of a logical track: a physical file path plus the `[start_frame, end_frame)`
/// sample-frame sub-range it plays within that file. A normal whole-file track is
/// `TrackKey { path, start_frame: 0, end_frame: None }`. A CUE sub-range track's
/// `start_frame`/`end_frame` are in sample frames of the underlying physical file, computed via
/// `cue::cue_time_to_sample_frame` once the file's real sample rate is known; `end_frame` is
/// exclusive (`[start_frame, end_frame)`), and the last track of a physical file has `end_frame:
/// None`, meaning "to EOF". `Ord`/`Hash` let this serve as a `HashMap`/`Library` key and give a
/// deterministic sort/dedup order (path, then start_frame, then end_frame).
// `Serialize`/`Deserialize` so a `TrackKey` can be persisted verbatim in `library.json`'s
// `excluded_tracks` (`store::LibrarySources`, "Library exclusions" below): a "Remove from Library"
// exclusion is keyed on the exact `TrackKey` (path + sample-frame sub-range) rather than the bare
// path, so removing one CUE sub-range track never excludes its sibling tracks that share the same
// physical file (`CLAUDE.md` "Library exclusions").
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TrackKey {
    pub path: PathBuf,
    pub start_frame: u64,
    pub end_frame: Option<u64>,
}

impl TrackKey {
    /// The key for an ordinary, non-CUE whole-file track.
    pub fn whole_file(path: PathBuf) -> Self {
        Self { path, start_frame: 0, end_frame: None }
    }
}

/// The stable string form of a `TrackKey`, used for `TrackRowData.key`/`now-playing-key` on the
/// Slint side, where two CUE tracks derived from the same physical file must render distinct
/// strings (`key.path` alone would collide). A whole-file track's key keeps its original plain
/// `path.display()` format unchanged, so nothing downstream that already parses/compares it as a
/// bare path breaks.
pub fn track_key_string(key: &TrackKey) -> String {
    if key.start_frame == 0 && key.end_frame.is_none() {
        key.path.display().to_string()
    } else {
        format!("{}#{}", key.path.display(), key.start_frame)
    }
}

/// The Library-level "effective album artist" for a `TrackRecord` (`§3.3` "Album grouping key"),
/// resolved across every track that shares its (normalized album title, parent directory) group —
/// not from this one record's own tags in isolation. `Library::upsert`/`remove_album` keep this in
/// sync on every mutation (`Library::regroup_scope`); a record built outside a `Library` (a
/// test, or the scanner thread's transient pre-upsert records) stays `Unresolved`, and `album_key`
/// falls back to this one record's own tags for it, matching the pre-group-resolution behavior.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum EffectiveAlbumArtist {
    #[default]
    Unresolved,
    /// A single artist for the whole group: either the majority explicit `ALBUMARTIST` among the
    /// group's tracks, or — when none of them carry one — the one `ARTIST` value every track in
    /// the group shares.
    Known(String),
    /// No consistent artist could be resolved for the group (a compilation/soundtrack folder with
    /// different track artists and no `ALBUMARTIST` tag at all).
    VariousArtists,
}

/// The album a `Library`-resolved track belongs to (`CLAUDE.md` "Album grouping" — folder-majority
/// title): not necessarily the track's own ALBUM tag, since a minority title folds into its folder's
/// dominant one and an untagged track joins it. `norm` is the grouping-key form
/// (`grouping::normalize_album_title`), `title` the title shown to the user (the group's most common
/// cleaned raw title). `None` on a record means "not resolved by a `Library`, or no album at all".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectiveAlbum {
    pub norm: String,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TrackRecord {
    pub key: TrackKey,
    // Read by `Library::prepared` (queue actions from library rows) and `summarize_album`
    // (album-detail headers, `§6` Stage 6).
    pub info: AudioInfo,
    pub file_size: Option<u64>,
    pub tags: TrackTags,
    /// Taken by the UI thread on arrival (`AppState::apply_scanned`), then `None` in the library
    /// from that point on — the decoded pixels live only in the artwork cache (`§3.3`).
    pub artwork: Option<ArtworkPixels>,
    /// The precedence tier the scanned `artwork` came from (`ArtworkSource::None` when the scanner
    /// produced no picture); it stays set after `AppState::apply_scanned` takes the pixels, which
    /// passes it to `cache_artwork` so a better picture can replace a worse one already cached for
    /// the album.
    pub artwork_source: ArtworkSource,
    pub added_seq: u64,
    /// See `EffectiveAlbumArtist`. Only `Library` ever writes a resolved value; every other
    /// constructor leaves it `Unresolved`.
    pub effective_album_artist: EffectiveAlbumArtist,
    /// See `EffectiveAlbum`. Only `Library` ever writes it.
    pub effective_album: Option<EffectiveAlbum>,
    /// `Some(winner)` when this track is a lower-quality duplicate copy of the song `winner` inside the
    /// same album (`CLAUDE.md` "Album grouping" — duplicate copies): kept in the `Library` (so
    /// "Remove from Library" still covers it, `get` still finds it for playback) but hidden from every
    /// view. Only `Library` ever writes it.
    pub hidden_copy_of: Option<TrackKey>,
}

impl TrackRecord {
    /// A record with no tags, no art and no known size, built from a scanner `Probed` event so
    /// the file shows up under its fallback name immediately (`§3.3` "UI handling of scanner
    /// events").
    pub fn minimal(key: TrackKey, info: AudioInfo) -> Self {
        Self {
            key,
            info,
            file_size: None,
            tags: TrackTags::default(),
            artwork: None,
            artwork_source: ArtworkSource::None,
            added_seq: 0,
            effective_album_artist: EffectiveAlbumArtist::Unresolved,
            effective_album: None,
            hidden_copy_of: None,
        }
    }

    /// Whether views show this track (it is not a hidden duplicate copy).
    pub fn is_visible(&self) -> bool {
        self.hidden_copy_of.is_none()
    }
}

// `AlbumSummary`/`ArtistSummary`, the `Library` view/query methods below `upsert`/`get`, and the
// pure `shuffled` helper are the full `§3.3` library API; their UI callers (Home/Albums/Artists/
// Songs/album-detail views and queue actions, `§6` Stage 6) live in `view_model.rs`.
#[derive(Clone, Debug, PartialEq)]
pub struct AlbumSummary {
    pub key: String,
    pub title: String,
    pub artist: String,
    pub year: Option<u16>,
    pub track_count: usize,
    pub total_duration_ms: u64,
    pub first_added_seq: u64,
    /// The album format pill's label/variant (`§5.5`/`§5.7`), precomputed once here — in the same
    /// pass over this album's tracks that builds the rest of the summary — rather than recomputed
    /// from scratch (`Library::album_tracks` + `aggregate_album_format`) on every album-card/header
    /// render (`view_model::album_format_pill`, removed): an O(tracks) rescan per card made
    /// rendering a grid of many albums O(albums × tracks) on the UI thread on every reprojection
    /// (search keystrokes, nav, throttled rescans). Empty strings for an album with no tracks (only
    /// possible via `AlbumHeaderData::default()`'s own empty header, never through `summarize_album`
    /// itself, which is never called with an empty slice).
    pub format_label: String,
    pub format_variant: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtistSummary {
    pub name: String,
    pub album_count: usize,
    pub track_count: usize,
}

/// One album-key change a `Library` mutation caused (`Library::upsert_detailed`): the tracks that
/// moved from `old_key` to `new_key`, and the resolution groups (`resolution_group_key`) they belong
/// to — which pictures cached under `old_key` follow them (`AppState::move_artwork`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyTransition {
    pub old_key: String,
    pub new_key: String,
    pub groups: Vec<(String, PathBuf)>,
}

#[derive(Default)]
pub struct Library {
    tracks: Vec<TrackRecord>,
    by_key: HashMap<TrackKey, usize>,
    /// Parallel to `tracks`: each track's `grouping_scope_dir` and `album_key`, cached because the
    /// regrouping on every insert and every query would otherwise recompute them for the whole
    /// library. Kept in sync whenever a record's tags or resolution change.
    scopes: Vec<PathBuf>,
    keys: Vec<String>,
    /// Parallel to `tracks`: the normalized values the grouping rules read (`grouping::Derived`),
    /// computed once when a record is stored instead of on every regroup.
    derived: Vec<Derived>,
    /// Indices of the tracks of each grouping scope / each album key, maintained on insert, key change
    /// and removal, so a regroup or a copy plan touches one folder or one album and never scans the
    /// whole library.
    scope_members: HashMap<PathBuf, Vec<usize>>,
    album_members: HashMap<String, Vec<usize>>,
    /// Interned copy-detection sources (`grouping::SourceKey`); ids are only ever compared for equality.
    sources: HashMap<SourceKey, u32>,
    next_seq: u64,
}

impl Library {
    /// Inserts `record`, or replaces the record at the same `TrackKey` in place while keeping its
    /// original `added_seq` (`§3.3`). Two records that share a path but differ in `start_frame`/
    /// `end_frame` (distinct CUE sub-ranges of the same physical file) coexist as separate entries.
    ///
    /// Then re-resolves everything the new record can influence (`regroup_scope`): the album each
    /// track of its folder belongs to (folder-majority title) and the effective album artist of each
    /// album whose membership changed, and afterwards which tracks of the affected albums are hidden
    /// duplicate copies (`recompute_copies`). The result depends only on the current set of records,
    /// never on the order they arrived in.
    ///
    /// Returns every distinct `(old_key, new_key)` album-key transition this call caused — not only
    /// `record`'s own (though that is included whenever it actually changed): re-resolving the folder
    /// can also change an already-stored SIBLING track's own `album_key` purely as a side effect (a
    /// minority title folding into the dominant one, the group's artist majority flipping, an untagged
    /// track joining). `AppState::apply_scanned` (`CLAUDE.md` "Album grouping") uses this to migrate its
    /// artwork cache and recently-played list off every key this call abandoned — missing a sibling's
    /// silent transition was the real cause of the "CD1 shows no artwork" bug.
    pub fn upsert(&mut self, record: TrackRecord) -> Vec<(String, String)> {
        self.upsert_detailed(record).into_iter().map(|transition| (transition.old_key, transition.new_key)).collect()
    }

    /// `upsert`, with the resolution groups that moved in each transition (`KeyTransition`).
    pub fn upsert_detailed(&mut self, mut record: TrackRecord) -> Vec<KeyTransition> {
        let incoming_key = record.key.clone();
        let incoming_group = resolution_group_key(&record);
        let scope = grouping_scope_dir(&record);
        let previous = self.by_key.get(&incoming_key).map(|&index| (self.keys[index].clone(), resolution_group_key(&self.tracks[index])));
        let mut derived = derive(&record);
        derived.source = self.source_id(&record);

        let index = match self.by_key.get(&incoming_key) {
            Some(&index) => {
                record.added_seq = self.tracks[index].added_seq;
                // Keep the previous resolution until `regroup_scope` replaces it, so the album the
                // record leaves (if any) is known to lose a member.
                record.effective_album = self.tracks[index].effective_album.clone();
                record.effective_album_artist = self.tracks[index].effective_album_artist.clone();
                self.tracks[index] = record;
                self.derived[index] = derived;
                index
            }
            None => {
                record.added_seq = self.next_seq;
                self.next_seq += 1;
                let index = self.tracks.len();
                self.by_key.insert(incoming_key.clone(), index);
                self.tracks.push(record);
                self.derived.push(derived);
                self.scopes.push(scope.clone());
                self.keys.push(String::new());
                self.scope_members.entry(scope.clone()).or_default().push(index);
                index
            }
        };
        // The transient, unresolved key; `regroup_scope` replaces it with the resolved one.
        let transient = album_key(&self.tracks[index]);
        self.set_key(index, transient);

        let moved = self.regroup_scope(&scope, Some(index), false);
        let mut transitions: Vec<KeyTransition> = Vec::new();
        let mut affected: HashSet<String> = HashSet::new();
        for (track_index, before_key) in moved {
            let after_key = self.keys[track_index].clone();
            affected.insert(before_key.clone());
            affected.insert(after_key.clone());
            // The incoming record's own before/after is reported below instead, using the key it
            // truly held *before this whole call*: `regroup_scope`'s "before" for it is only the
            // transient, unresolved key set a few lines up, which nothing outside this function ever
            // observed or cached anything under.
            if track_index == index {
                continue;
            }
            note_transition(&mut transitions, before_key, after_key, resolution_group_key(&self.tracks[track_index]).into_iter().collect());
        }
        if let Some((previous_key, previous_group)) = previous {
            let groups = incoming_group.into_iter().chain(previous_group).collect();
            affected.insert(previous_key.clone());
            note_transition(&mut transitions, previous_key, self.keys[index].clone(), groups);
        }
        self.recompute_copies(&affected);
        transitions
    }

    /// The interned id of `record`'s copy-detection source.
    fn source_id(&mut self, record: &TrackRecord) -> u32 {
        let next = self.sources.len() as u32;
        *self.sources.entry(source_key(record)).or_insert(next)
    }

    /// Moves track `index` onto album key `key` in the cached keys and the album index.
    fn set_key(&mut self, index: usize, key: String) {
        let old = std::mem::replace(&mut self.keys[index], key);
        if old == self.keys[index] {
            return;
        }
        if !old.is_empty()
            && let Some(members) = self.album_members.get_mut(&old)
        {
            if let Some(position) = members.iter().position(|&member| member == index) {
                members.swap_remove(position);
            }
            if members.is_empty() {
                self.album_members.remove(&old);
            }
        }
        self.album_members.entry(self.keys[index].clone()).or_default().push(index);
    }

    /// Recomputes, from the folder's current membership alone, which album every track whose
    /// `grouping_scope_dir` is `scope` belongs to and each affected album's effective album artist
    /// (`CLAUDE.md` "Album grouping"):
    ///
    /// 1. `resolve_scope_albums`: a track's album is its own ALBUM title unless the folder-majority
    ///    rule folds it into the folder's dominant title (or, untagged, joins it).
    /// 2. Per album whose membership changed (or all of them with `all_albums`),
    ///    `resolve_effective_album_artist` picks one artist for all its tracks. An album whose members
    ///    did not change keeps its artist: it is a pure function of those members' tags.
    ///
    /// `forced` is the record just inserted or replaced: its album is always re-resolved and it is
    /// always reported. Returns `(index, key before)` for every track whose album key changed, plus
    /// `forced` — the "before" is the cached key from the last time the track was resolved, so for the
    /// forced record it is the transient unresolved key (see `upsert_detailed`). Empty for a scope with
    /// no tracks.
    fn regroup_scope(&mut self, scope: &Path, forced: Option<usize>, all_albums: bool) -> Vec<(usize, String)> {
        let Some(indices) = self.scope_members.get(scope).cloned() else { return Vec::new() };
        if indices.is_empty() {
            return Vec::new();
        }
        let resolved = {
            let tracks: Vec<&Derived> = indices.iter().map(|&index| &self.derived[index]).collect();
            resolve_scope_albums(&tracks)
        };

        let mut dirty: HashSet<String> = HashSet::new();
        let mut changed: Vec<usize> = Vec::new();
        for (position, &index) in indices.iter().enumerate() {
            let new_album = resolved.assignment[position].map(|group| &resolved.groups[group]);
            let record = &mut self.tracks[index];
            let mut touched = forced == Some(index);
            if record.effective_album.as_ref() != new_album {
                // The album the track left loses a member, the one it joins gains one.
                if let Some(old) = &record.effective_album {
                    dirty.insert(old.norm.clone());
                }
                record.effective_album = new_album.cloned();
                if new_album.is_none() {
                    record.effective_album_artist = EffectiveAlbumArtist::Unresolved;
                }
                touched = true;
            }
            if touched || all_albums {
                if let Some(album) = new_album {
                    dirty.insert(album.norm.clone());
                }
            }
            if touched {
                changed.push(position);
            }
        }

        if !dirty.is_empty() {
            let mut members: HashMap<usize, Vec<usize>> = HashMap::new();
            for (position, group) in resolved.assignment.iter().enumerate() {
                if let Some(group) = group
                    && dirty.contains(&resolved.groups[*group].norm)
                {
                    members.entry(*group).or_default().push(position);
                }
            }
            for positions in members.values() {
                let artist = resolve_effective_album_artist(positions.iter().map(|&position| &self.derived[indices[position]]));
                for &position in positions {
                    let record = &mut self.tracks[indices[position]];
                    if record.effective_album_artist != artist {
                        record.effective_album_artist = artist.clone();
                        changed.push(position);
                    }
                }
            }
        }

        changed.sort_unstable();
        changed.dedup();
        let mut moved = Vec::new();
        for position in changed {
            let index = indices[position];
            let key = album_key(&self.tracks[index]);
            if key != self.keys[index] || forced == Some(index) {
                let before = self.keys[index].clone();
                self.set_key(index, key);
                moved.push((index, before));
            }
        }
        moved
    }

    /// Recomputes `hidden_copy_of` for every track whose album key is in `keys` (`plan_copies` per
    /// album, from the album's current members alone).
    fn recompute_copies(&mut self, keys: &HashSet<String>) {
        let mut plans: Vec<(usize, Option<TrackKey>)> = Vec::new();
        for key in keys {
            let Some(members) = self.album_members.get(key) else { continue };
            let source = self.derived[members[0]].source;
            let hidden = if members.iter().all(|&index| self.derived[index].source == source) {
                // One source: nothing can be a copy.
                vec![None; members.len()]
            } else {
                let tracks: Vec<CopyTrack> = members.iter().map(|&index| CopyTrack { record: &self.tracks[index], derived: &self.derived[index] }).collect();
                plan_copies(&tracks)
            };
            for (&index, hidden) in members.iter().zip(hidden) {
                if self.tracks[index].hidden_copy_of != hidden {
                    plans.push((index, hidden));
                }
            }
        }
        for (index, hidden) in plans {
            self.tracks[index].hidden_copy_of = hidden;
        }
    }

    /// Every hidden duplicate copy with the album key it belongs to (dev-only library dump).
    #[cfg(test)]
    pub fn hidden_copies(&self) -> Vec<(&str, &TrackRecord)> {
        self.tracks.iter().zip(&self.keys).filter(|(track, _)| !track.is_visible()).map(|(track, key)| (key.as_str(), track)).collect()
    }

    pub fn get(&self, key: &TrackKey) -> Option<&TrackRecord> {
        self.by_key.get(key).map(|&index| &self.tracks[index])
    }

    /// Every record in insertion (`added_seq`) order; the persistent library cache saves this
    /// (`cache::CacheSnapshot`). Only raw records are saved: the grouping state derived from them
    /// (`effective_album`, `effective_album_artist`, `hidden_copy_of`) is recomputed on load.
    pub fn records(&self) -> &[TrackRecord] {
        &self.tracks
    }

    /// Inserts a whole batch of records (the persistent cache load, `cache::load`) in the given
    /// order, which becomes their `added_seq` order. Equivalent to calling `upsert` for each record (a
    /// repeated `TrackKey` replaces the earlier one in place) — same albums, effective album artists,
    /// disc numbers, hidden copies and indices, whatever the order — except every touched folder is
    /// regrouped ONCE and the duplicate copies of every touched album are planned ONCE, after all the
    /// records are stored: `upsert` regroups its folder per call, which is quadratic for a big folder
    /// and far too slow on the startup path. Any grouping state the incoming records carry is
    /// discarded and recomputed. Reports no key transitions, since nothing downstream has seen the
    /// intermediate keys: only call it before any artwork, recent album or `Navigation` entry exists
    /// (its one app caller, `AppState::restore_cache`, runs at startup before the first scan). A
    /// later reload path would have to report transitions like `upsert_detailed` does.
    pub fn load_records(&mut self, records: impl IntoIterator<Item = TrackRecord>) {
        let mut touched_scopes: HashSet<PathBuf> = HashSet::new();
        let mut affected: HashSet<String> = HashSet::new();
        for mut record in records {
            record.effective_album = None;
            record.effective_album_artist = EffectiveAlbumArtist::Unresolved;
            record.hidden_copy_of = None;
            let scope = grouping_scope_dir(&record);
            let mut derived = derive(&record);
            derived.source = self.source_id(&record);
            let index = match self.by_key.get(&record.key) {
                Some(&index) => {
                    record.added_seq = self.tracks[index].added_seq;
                    affected.insert(self.keys[index].clone());
                    if self.scopes[index] != scope {
                        // The record moved folders: its old folder loses a member.
                        let old = std::mem::replace(&mut self.scopes[index], scope.clone());
                        if let Some(members) = self.scope_members.get_mut(&old) {
                            members.retain(|&member| member != index);
                        }
                        touched_scopes.insert(old);
                        self.scope_members.entry(scope.clone()).or_default().push(index);
                    }
                    self.tracks[index] = record;
                    self.derived[index] = derived;
                    index
                }
                None => {
                    record.added_seq = self.next_seq;
                    self.next_seq += 1;
                    let index = self.tracks.len();
                    self.by_key.insert(record.key.clone(), index);
                    self.tracks.push(record);
                    self.derived.push(derived);
                    self.scopes.push(scope.clone());
                    self.keys.push(String::new());
                    self.scope_members.entry(scope.clone()).or_default().push(index);
                    index
                }
            };
            // The transient, unresolved key; `regroup_scope` replaces it with the resolved one.
            let transient = album_key(&self.tracks[index]);
            self.set_key(index, transient);
            touched_scopes.insert(scope);
        }
        for scope in &touched_scopes {
            for (index, before_key) in self.regroup_scope(scope, None, true) {
                affected.insert(before_key);
                affected.insert(self.keys[index].clone());
            }
            // Albums that did not move still need their copies planned (every record is new here).
            if let Some(members) = self.scope_members.get(scope) {
                affected.extend(members.iter().map(|&index| self.keys[index].clone()));
            }
        }
        self.recompute_copies(&affected);
    }

    /// Removes exactly the tracks in `keys` (a pruned cache entry whose file is gone, `cache`) and
    /// re-resolves everything they touched, like `remove_album_detailed` does for a whole album: the
    /// remaining tracks of the folders involved may land on other album keys, and a removed visible
    /// copy promotes the next one. Returns every album-key transition that caused, for the caller to
    /// migrate artwork and navigation (`AppState::prune_tracks`).
    pub fn remove_tracks(&mut self, keys: &HashSet<TrackKey>) -> Vec<KeyTransition> {
        let (removed, touched_scopes, removed_albums) = self.remove_indices(|library, index| keys.contains(&library.tracks[index].key));
        if removed.is_empty() {
            return Vec::new();
        }
        self.reresolve_after_removal(touched_scopes, removed_albums)
    }

    /// Removes every track belonging to album `key` from the session library — its visible tracks AND
    /// its hidden duplicate copies — and returns their `TrackKey`s ("Remove from Library", `CLAUDE.md`
    /// "Library exclusions"). Non-destructive: this only drops the in-memory `TrackRecord`s here — it
    /// never touches a file on disk and never reaches into `AudioPlayer`'s queue or active stream (the
    /// library and playback are separate, session-only state; see `main.rs`'s
    /// `on_remove_album_requested` for where the two are kept decoupled). The caller is expected to
    /// persist the returned keys as an exclusion (`store::LibrarySources::exclude_tracks`) so a later
    /// rescan (startup restore) does not bring any copy back. Returns an empty `Vec` if `key` matches
    /// no album. Test-only shorthand: the app goes through `remove_album_detailed` (via
    /// `AppState::remove_album`) so survivors' key transitions are migrated.
    #[cfg(test)]
    pub fn remove_album(&mut self, key: &str) -> Vec<TrackKey> {
        self.remove_album_detailed(key).0
    }

    /// `remove_album`, plus every album-key transition the removal caused for the SURVIVORS: the
    /// removed tracks' folders are re-resolved, and a removal can shift what is left (the dominant
    /// title's share grows, the fold cap shrinks, an album's members change), moving an
    /// already-stored track onto another key exactly like `upsert_detailed` can. The caller migrates
    /// its artwork cache and navigation off the abandoned keys the same way it does for a scan.
    pub fn remove_album_detailed(&mut self, key: &str) -> (Vec<TrackKey>, Vec<KeyTransition>) {
        let (removed, touched_scopes, removed_albums) = self.remove_indices(|library, index| library.keys[index] == key);
        let transitions = self.reresolve_after_removal(touched_scopes, removed_albums);
        (removed, transitions)
    }

    /// Drops every track `doomed` selects and rebuilds the position-dependent indices. Returns the
    /// removed `TrackKey`s, every folder a removed track sat in and every album key it belonged to.
    fn remove_indices(&mut self, doomed: impl Fn(&Library, usize) -> bool) -> (Vec<TrackKey>, HashSet<PathBuf>, HashSet<String>) {
        let mut removed = Vec::new();
        // Every folder a removed track sat in: the remaining tracks there are re-resolved afterwards,
        // which keeps their grouping correct even if a removal ever drops less than a whole folder
        // group, and leaves no stale resolution behind for a track a caller re-adds later.
        let mut touched_scopes: HashSet<PathBuf> = HashSet::new();
        let mut removed_albums: HashSet<String> = HashSet::new();
        let mut kept = 0usize;
        for index in 0..self.tracks.len() {
            if doomed(self, index) {
                removed.push(self.tracks[index].key.clone());
                touched_scopes.insert(self.scopes[index].clone());
                removed_albums.insert(self.keys[index].clone());
            } else {
                self.tracks.swap(kept, index);
                self.scopes.swap(kept, index);
                self.keys.swap(kept, index);
                self.derived.swap(kept, index);
                kept += 1;
            }
        }
        if removed.is_empty() {
            return (removed, touched_scopes, removed_albums);
        }
        // Survivors are compacted in their original relative order, so `added_seq` ordering holds.
        self.tracks.truncate(kept);
        self.scopes.truncate(kept);
        self.keys.truncate(kept);
        self.derived.truncate(kept);
        // The index maps hold indices into `tracks`, which just shifted; cheap to rebuild outright
        // rather than patch in place — a removal is a rare, user-initiated action, not a hot path.
        self.by_key.clear();
        self.scope_members.clear();
        self.album_members.clear();
        for (index, record) in self.tracks.iter().enumerate() {
            self.by_key.insert(record.key.clone(), index);
            self.scope_members.entry(self.scopes[index].clone()).or_default().push(index);
            self.album_members.entry(self.keys[index].clone()).or_default().push(index);
        }
        (removed, touched_scopes, removed_albums)
    }

    /// Regroups the folders a removal touched and re-plans the duplicate copies of every album it
    /// touched (the albums the removed tracks left, and the ones survivors moved between). Returns
    /// the key transitions of the survivors.
    fn reresolve_after_removal(&mut self, touched_scopes: HashSet<PathBuf>, removed_albums: HashSet<String>) -> Vec<KeyTransition> {
        let mut affected = removed_albums;
        let mut transitions: Vec<KeyTransition> = Vec::new();
        for scope in touched_scopes {
            for (index, before_key) in self.regroup_scope(&scope, None, true) {
                let after_key = self.keys[index].clone();
                affected.insert(before_key.clone());
                affected.insert(after_key.clone());
                note_transition(&mut transitions, before_key, after_key, resolution_group_key(&self.tracks[index]).into_iter().collect());
            }
        }
        self.recompute_copies(&affected);
        transitions
    }
}

/// Records the `old_key` -> `new_key` move of `groups` in `transitions` (merging with an existing
/// entry for the same pair); a no-op move is ignored.
fn note_transition(transitions: &mut Vec<KeyTransition>, old_key: String, new_key: String, groups: Vec<(String, PathBuf)>) {
    if old_key == new_key {
        return;
    }
    match transitions.iter_mut().find(|t| t.old_key == old_key && t.new_key == new_key) {
        Some(existing) => {
            for group in groups {
                if !existing.groups.contains(&group) {
                    existing.groups.push(group);
                }
            }
        }
        None => transitions.push(KeyTransition { old_key, new_key, groups }),
    }
}

/// The rest of the `§3.3` query API, called from `view_model.rs`'s projections (the library-empty
/// check, Home/Albums/Artists/Songs/album-detail views, and queue actions built from library rows).
impl Library {
    // Only test code builds a `Library` directly instead of through `AppState::default()`.
    #[cfg(test)]
    pub fn new() -> Self {
        Self::default()
    }

    /// `key` + `info`, ready for a queue command (`enqueue`/`play_next`/`replace_queue`); `None`
    /// for a key the library has no record of.
    pub fn prepared(&self, key: &TrackKey) -> Option<PreparedTrack> {
        self.get(key).map(|record| PreparedTrack { key: record.key.clone(), info: record.info.clone() })
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    /// Every album, sorted by artist then title, optionally restricted to `artist_filter` and/or
    /// matching `query` in title or artist. `artist_filter` matches a track's `display_artist`, the
    /// same way `artists()` counts albums per artist (`§3.6` `artist-opened` from `ArtistsView`) —
    /// so an artist shown with `album_count >= 1` never opens onto an empty Albums page — or the
    /// album's own summary artist (`album_artist`, or the synthesized "Various Artists"), which is
    /// what the album-detail artist link sends (`§5.7`); without the second check, a compilation or
    /// an album whose `album_artist` differs from its track artists would open an empty page.
    pub fn albums(&self, query: &str, artist_filter: Option<&str>) -> Vec<AlbumSummary> {
        let filter_key = artist_filter.map(str::trim).filter(|s| !s.is_empty()).map(str::to_lowercase);
        let needle = normalize_for_search(query.trim());
        let mut summaries: Vec<AlbumSummary> = self
            .album_groups()
            .into_values()
            .filter_map(|tracks| {
                let summary = summarize_album(&tracks);
                let matches_artist = match &filter_key {
                    Some(key) => {
                        tracks.iter().any(|track| display_artist(track).to_lowercase() == *key)
                            || summary.artist.to_lowercase() == *key
                    }
                    None => true,
                };
                // An album matches the search on any of its tracks' title/artist/album/album
                // artist/genre (`track_matches_query`, `§...` Search), not just its own title or
                // artist: a query that only hits a track's genre (e.g. "jazz") must still surface
                // the album it belongs to, the same way `filtered_tracks` already works.
                let matches_query = needle.is_empty() || tracks.iter().any(|track| track_matches_query(track, &needle));
                (matches_artist && matches_query).then_some(summary)
            })
            .collect();
        summaries.sort_by_cached_key(|album| (album.artist.to_lowercase(), album.title.to_lowercase()));
        summaries
    }

    /// Every album matching `query`, newest-first by the album's first added track.
    pub fn albums_recent(&self, query: &str) -> Vec<AlbumSummary> {
        let mut summaries = self.albums(query, None);
        summaries.sort_by_key(|album| std::cmp::Reverse(album.first_added_seq));
        summaries
    }

    pub fn album(&self, key: &str) -> Option<AlbumSummary> {
        let tracks = self.visible_album_tracks(key);
        if tracks.is_empty() { None } else { Some(summarize_album(&tracks)) }
    }

    /// The visible tracks of album `key`, in library order (hidden duplicate copies excluded).
    fn visible_album_tracks(&self, key: &str) -> Vec<&TrackRecord> {
        self.tracks.iter().zip(&self.keys).filter(|(track, track_key)| track_key.as_str() == key && track.is_visible()).map(|(track, _)| track).collect()
    }

    /// The tracks of album `key`, sorted by disc, track number, then title, then path (a stable
    /// tie-break for tracks that carry no numbering at all).
    pub fn album_tracks(&self, key: &str) -> Vec<&TrackRecord> {
        let mut tracks = self.visible_album_tracks(key);
        // `sort_by_cached_key`, not `sort_by`: an O(n log n) comparator would otherwise lowercase
        // `display_title` again on every comparison instead of once per track (`§6`, same fix as
        // `songs` below).
        tracks.sort_by_cached_key(|track| {
            (effective_disc_number(track).unwrap_or(0), track.tags.track_number.unwrap_or(0), display_title(track).to_lowercase(), track.key.clone())
        });
        tracks
    }

    /// Every track matching `query`, sorted by artist, album, disc, track, then title.
    pub fn songs(&self, query: &str) -> Vec<&TrackRecord> {
        let mut tracks = self.filtered_tracks(query);
        // `sort_by_cached_key` computes each track's key once, up front, instead of `sort_by`
        // re-lowercasing `display_artist`/`display_album`/`display_title` on every comparison —
        // roughly 1M allocations at 10k tracks with a plain comparator (`§6`, R1: a long scan's
        // library reprojection sharing the UI-thread tick with the progress bar/elapsed time).
        tracks.sort_by_cached_key(|track| {
            (
                display_artist(track).to_lowercase(),
                display_album(track).to_lowercase(),
                effective_disc_number(track).unwrap_or(0),
                track.tags.track_number.unwrap_or(0),
                display_title(track).to_lowercase(),
            )
        });
        tracks
    }

    /// Every track matching `query`, newest-added first.
    pub fn songs_recent(&self, query: &str) -> Vec<&TrackRecord> {
        let mut tracks = self.filtered_tracks(query);
        tracks.sort_by_key(|track| std::cmp::Reverse(track.added_seq));
        tracks
    }

    /// Every artist matching `query` by name, sorted by name, with album/track counts. Grouped
    /// case-insensitively by `display_artist` (so "Nina" and "nina" are one artist, not two),
    /// keeping the first-seen casing as the display name; `albums(_, Some(name))` filters by the
    /// exact same lowercased key, so `album_count` always matches what that call returns.
    pub fn artists(&self, query: &str) -> Vec<ArtistSummary> {
        let needle = normalize_for_search(query.trim());
        let mut display_names: HashMap<String, String> = HashMap::new();
        let mut track_counts: HashMap<String, usize> = HashMap::new();
        let mut albums_by_artist: HashMap<String, HashSet<String>> = HashMap::new();
        // An artist matches once any of their tracks matches the search (`track_matches_query`),
        // not just their own name: a genre- or album-only query (e.g. "jazz") must still surface
        // the artists behind those tracks, the same broadening `albums` gets above.
        let mut matched: HashSet<String> = HashSet::new();
        for (track, track_album_key) in self.tracks.iter().zip(&self.keys).filter(|(track, _)| track.is_visible()) {
            let artist = display_artist(track);
            let key = artist.to_lowercase();
            display_names.entry(key.clone()).or_insert(artist);
            *track_counts.entry(key.clone()).or_insert(0) += 1;
            albums_by_artist.entry(key.clone()).or_default().insert(track_album_key.clone());
            if needle.is_empty() || track_matches_query(track, &needle) {
                matched.insert(key);
            }
        }
        let mut summaries: Vec<ArtistSummary> = track_counts
            .into_iter()
            .filter(|(key, _)| matched.contains(key))
            .map(|(key, track_count)| {
                let album_count = albums_by_artist.get(&key).map(HashSet::len).unwrap_or(0);
                let name = display_names.remove(&key).unwrap_or(key);
                ArtistSummary { name, album_count, track_count }
            })
            .collect();
        summaries.sort_by_key(|artist| artist.name.to_lowercase());
        summaries
    }

    fn album_groups(&self) -> HashMap<String, Vec<&TrackRecord>> {
        let mut groups: HashMap<String, Vec<&TrackRecord>> = HashMap::new();
        for (track, key) in self.tracks.iter().zip(&self.keys).filter(|(track, _)| track.is_visible()) {
            groups.entry(key.clone()).or_default().push(track);
        }
        groups
    }

    /// A case- and accent-insensitive substring match on title, artist, album, album artist and
    /// genre; an empty query matches every track (`§3.3` "Search").
    fn filtered_tracks(&self, query: &str) -> Vec<&TrackRecord> {
        let needle = normalize_for_search(query.trim());
        if needle.is_empty() {
            return self.tracks.iter().filter(|track| track.is_visible()).collect();
        }
        self.tracks.iter().filter(|track| track.is_visible() && track_matches_query(track, &needle)).collect()
    }
}

/// Whether `track` matches an already-normalized `needle` (see `normalize_for_search`) on title,
/// artist, album, album artist or genre (`§3.3` "Search"). Shared by `filtered_tracks`, `albums`
/// and `artists` so every section of the dedicated Search view (Songs/Albums/Artists) agrees on
/// what counts as a match. An empty `needle` matches everything.
fn track_matches_query(track: &TrackRecord, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    normalize_for_search(&display_title(track)).contains(needle)
        || normalize_for_search(&display_artist(track)).contains(needle)
        || normalize_for_search(&display_album(track)).contains(needle)
        || track.tags.album_artist.as_deref().is_some_and(|s| normalize_for_search(s).contains(needle))
        || track.tags.genre.as_deref().is_some_and(|s| normalize_for_search(s).contains(needle))
}

/// Lowercases and strips common Latin diacritics for case- and accent-insensitive search (`§3.3`
/// "Search"): `"Música"` and `"musica"` normalize to the same string. Covers the accented Latin
/// letters a real-world music library realistically carries (Spanish, Portuguese, French, German,
/// Nordic); a character outside that table — including a whole different script (Cyrillic, CJK) —
/// passes through unchanged, so search on those still works, just not accent-folded.
pub fn normalize_for_search(input: &str) -> String {
    input.to_lowercase().chars().map(strip_latin_diacritic).collect()
}

pub(super) fn strip_latin_diacritic(c: char) -> char {
    match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => 'a',
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => 'e',
        'ì' | 'í' | 'î' | 'ï' | 'ī' | 'ĭ' | 'į' => 'i',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => 'o',
        'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => 'u',
        'ý' | 'ÿ' => 'y',
        'ñ' | 'ń' => 'n',
        'ç' | 'ć' | 'č' => 'c',
        other => other,
    }
}

fn summarize_album(tracks: &[&TrackRecord]) -> AlbumSummary {
    let key = album_key(tracks[0]);
    // The tracks of one album key can come from several folders (a FLAC folder and a WavPack
    // sibling) whose resolved title/artist spellings differ; show the most common one, ties broken
    // by the smallest string, so the header never depends on library order.
    let title = most_common(tracks.iter().map(|track| display_album(track))).unwrap_or_default();
    // Every track in `tracks` shares one album key. The `Unresolved` arm is only a defensive
    // fallback: `summarize_album`'s one caller (`album_groups`, over `self.tracks`) only ever sees
    // Library-resolved records.
    let artist = most_common(tracks.iter().map(|track| match &track.effective_album_artist {
        EffectiveAlbumArtist::Known(artist) => artist.clone(),
        EffectiveAlbumArtist::VariousArtists => "Various Artists".to_owned(),
        EffectiveAlbumArtist::Unresolved => match resolve_effective_album_artist(std::iter::once(&derive(track))) {
            EffectiveAlbumArtist::Known(artist) => artist,
            _ => "Various Artists".to_owned(),
        },
    }))
    .unwrap_or_default();
    let year = tracks.iter().filter_map(|track| track.tags.year).min();
    let total_duration_ms = tracks.iter().filter_map(|track| track.info.duration_ms).sum();
    let first_added_seq = tracks.iter().map(|track| track.added_seq).min().unwrap_or(0);
    let (format_label, format_variant) = match aggregate_album_pill(tracks.iter().map(|track| (track.info.format.as_str(), track.info.bits_per_sample, track.info.sample_rate))) {
        Some((format, variant)) => (format.label().to_owned(), variant.to_owned()),
        None => (String::new(), String::new()),
    };
    AlbumSummary { key, title, artist, year, track_count: tracks.len(), total_duration_ms, first_added_seq, format_label, format_variant }
}

/// Deterministic album grouping key (`§3.3` "Album grouping key"). Used both by `Library`'s own
/// grouping and, standalone, by callers that already hold a `&TrackRecord` fetched from a `Library`
/// (`AppState::note_played_key`, `view_model::project_album_header` via `Library::album`, the
/// remove-from-library code).
///
/// - No album tag at all: grouped by grouping-scope directory alone (`dir:`).
/// - A resolved `Known` effective album artist (an explicit majority `ALBUMARTIST`, a CUE sheet's
///   own `PERFORMER` once merged into a cue-derived track's `ALBUMARTIST` tag, or a dominant primary
///   artist fallback — see `EffectiveAlbumArtist`): `aa:{artist}\u{1f}{album}`, deliberately *not*
///   folder-scoped, so the same artist+title merges across sibling folders — a multi-disc release
///   split across `CD1`/`CD2` subfolders, or the same album ripped once to FLAC and again to
///   WavPack in a different root folder.
/// - A resolved `VariousArtists` (a compilation/soundtrack folder with disagreeing track artists
///   and no `ALBUMARTIST`): `va:{album}\u{1f}{scope}`, scoped to the grouping-scope directory
///   (`grouping_scope_dir`) so two unrelated compilations that happen to share a title never merge,
///   while a multi-disc "Various Artists" box set split across `CD1`/`CD2` still merges into one.
/// - `Unresolved` (a record built outside `Library::upsert`, e.g. directly in a test or the scanner
///   thread's transient pre-upsert record): falls back to this one record's own tags, exactly the
///   way `album_key` behaved before group-level resolution existed (`aa:`/`af:`/`dir:`).
pub fn album_key(record: &TrackRecord) -> String {
    let album = match &record.effective_album {
        Some(album) => album.norm.clone(),
        None => match normalized_album(record) {
            Some(album) => album,
            None => return format!("dir:{}", grouping_scope_dir(record).display()),
        },
    };
    match &record.effective_album_artist {
        EffectiveAlbumArtist::Known(artist) => format!("aa:{}\u{1f}{}", normalize_artist_key(artist), album),
        EffectiveAlbumArtist::VariousArtists => format!("va:{}\u{1f}{}", album, grouping_scope_dir(record).display()),
        EffectiveAlbumArtist::Unresolved => {
            match record.tags.album_artist.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                Some(album_artist) => format!("aa:{}\u{1f}{}", normalize_artist_key(album_artist), album),
                None => format!("af:{}\u{1f}{}", album, grouping_scope_dir(record).display()),
            }
        }
    }
}

/// The grouping-key form of an album title (dev-only library dump).
#[cfg(test)]
pub fn album_title_key(title: &str) -> String {
    normalize_album_title(title)
}

/// The most frequent string of `items`, ties broken by the smallest one.
fn most_common(items: impl Iterator<Item = String>) -> Option<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for item in items {
        *counts.entry(item).or_default() += 1;
    }
    counts.into_iter().max_by(|(a, count_a), (b, count_b)| count_a.cmp(count_b).then_with(|| b.cmp(a))).map(|(item, _)| item)
}

fn parent_dir(record: &TrackRecord) -> PathBuf {
    record.key.path.parent().map(Path::to_path_buf).unwrap_or_default()
}

/// The directory that scopes a track's album grouping/resolution (`§3.3` "Album grouping key",
/// `CLAUDE.md` "Album grouping" — multi-disc releases): the track's own parent directory, unless
/// that directory's own name looks like a disc/CD subfolder of a larger release
/// (`is_disc_subfolder_name`, e.g. `CD1`, `Disc 2`), in which case the scope is the folder ABOVE
/// it — the release folder both discs share. This is what lets `CD1`/`CD2` (or `Disc 1`/`Disc 2`)
/// resolve as one group and merge into one album/one "Various Artists" bucket instead of two,
/// without changing anything for the far more common case of an album that already sits directly in
/// its own folder.
///
/// A narrow, accepted edge case: a whole (single-disc) album whose OWN folder happens to be named
/// like a disc subfolder (e.g. a user-created folder literally called `CD1` holding one album
/// directly, not nested under a release folder) scopes one level higher than its own folder — the
/// same folder-scoped-bucket collision risk `af:`/`va:`/`dir:` already carry for two differently
/// unrelated albums that happen to share a title in sibling folders, just one level up.
fn grouping_scope_dir(record: &TrackRecord) -> PathBuf {
    let parent = parent_dir(record);
    match parent.file_name().and_then(|name| name.to_str()) {
        Some(name) if is_disc_subfolder_name(name) => parent.parent().map(Path::to_path_buf).unwrap_or(parent),
        _ => parent,
    }
}

/// Whether `name` (a directory's own file name, not a full path) looks like a disc/CD subfolder of
/// a larger multi-disc release (`CLAUDE.md` "Album grouping"): `CD1`, `CD 1`, `CD01`, `Disc 1`,
/// `Disk 1`, `Disc One`, `"Disc 1 - Bonus Tracks"`, `"CD1 (Remastered)"`, and similar —
/// case-insensitively, with optional punctuation/whitespace between the word and the number.
/// Only the prefix is checked: whatever follows the disc number (a dash, a subtitle, a
/// parenthetical) never disqualifies it, since a real-world rip often adds one. Deliberately does
/// NOT match a bare `"CD"`/`"Disc"` with no number (ambiguous — could be a genre or label folder)
/// or a word that merely starts the same way (`"Discography"`, `"Disco"`).
pub(crate) fn is_disc_subfolder_name(name: &str) -> bool {
    parse_disc_designator(&name.to_ascii_lowercase()).is_some()
}

fn normalized_album(record: &TrackRecord) -> Option<String> {
    record.tags.album.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(normalize_album_title)
}

/// The artwork contributor of a record (`AppState::cache_artwork`): its own (normalized album title,
/// grouping-scope directory) pair — `grouping_scope_dir` makes the disc subfolders of one release
/// share a scope — computed from the record's OWN tags, so it is stable however the folder-majority
/// rule later regroups the track. The empty title stands for "no album tag" (the `dir:` fallback).
///
/// `AppState`'s artwork cache files each picture under its contributor's group: the group is the
/// unit that moves to another album key together when its resolution changes
/// (`KeyTransition::groups`).
pub(crate) fn resolution_group_key(record: &TrackRecord) -> Option<(String, PathBuf)> {
    Some((normalized_album(record).unwrap_or_default(), grouping_scope_dir(record)))
}

/// A dominant primary artist must cover at least this share of the group's tracks to win (`§3.3`
/// "Album grouping key" fallback (b)) — e.g. 15 of 17 "Dua Lipa"-primary tracks on a disc that also
/// has a couple of "Dua Lipa feat. Miguel" credits. Below this share, the group is too genuinely
/// mixed to call it anything but `VariousArtists`.
const PRIMARY_ARTIST_MAJORITY_THRESHOLD: f64 = 0.6;

/// The Library-level "effective album artist" for a group of tracks that already share one
/// (normalized album title, grouping-scope directory) (`§3.3` "Album grouping key"). Real-world
/// tagging is often inconsistent within a single physical release — a stray file with no
/// `ALBUMARTIST` at all, a duet/bonus track tagged with a combined artist that differs from the rest
/// of the album, or a disc's tracks individually crediting featured guests with no `ALBUMARTIST`
/// anywhere — so this picks ONE artist for the whole group rather than trusting each track's own tag
/// in isolation, the same way foobar2000/MusicBee/Plex resolve an album's artist from its folder:
///
/// 1. If any track carries a non-empty `ALBUMARTIST`, the most common such value (by exact,
///    case-insensitive, whitespace/Unicode-normalized text) wins and is used for every track in the
///    group, including ones that lack an `ALBUMARTIST` tag entirely or disagree with it. Ties are
///    broken by the lexicographically smallest surviving original-cased value, so the result depends
///    only on the current multiset of tag values, never on arrival order. A CUE-derived track's own
///    `ALBUMARTIST` is already the cue sheet's sheet-level `PERFORMER` when the sheet has one
///    (`scanner::merge_cue_tags`), so a cue sheet's `PERFORMER` reaches this step automatically
///    without needing its own separate code path here.
/// 2. Otherwise, the dominant PRIMARY artist: each track's own `ARTIST` (`display_artist`) with a
///    trailing featured-guest credit stripped (`strip_feat_suffix` — `"Dua Lipa feat. Miguel"` ->
///    `"Dua Lipa"`; never a blind `&`/`x`/`with` split, which would break a real band name like
///    `"Earth, Wind & Fire"`), majority-voted; if one primary artist — after also folding in any
///    track whose OWN primary looks like `"<that artist> & Someone"`/`"<that artist> x Someone"`/
///    `"<that artist> with Someone"` (`dominant_primary_artist`) — covers at least
///    `PRIMARY_ARTIST_MAJORITY_THRESHOLD` of the group's tracks, that artist is used.
/// 3. Otherwise, `VariousArtists`.
fn resolve_effective_album_artist<'a>(tracks: impl Iterator<Item = &'a Derived>) -> EffectiveAlbumArtist {
    let tracks: Vec<&Derived> = tracks.collect();

    let mut album_artist_variants: HashMap<&str, Vec<&str>> = HashMap::new();
    for track in &tracks {
        if let Some((norm, raw)) = &track.album_artist {
            album_artist_variants.entry(norm.as_str()).or_default().push(raw.as_str());
        }
    }
    if !album_artist_variants.is_empty() {
        return EffectiveAlbumArtist::Known(pick_majority_variant(album_artist_variants));
    }

    // A track with no artist tag at all (an untagged file that joined the album through the
    // folder-majority rule) has no vote: its "Unknown Artist" placeholder must not dilute the real
    // artist's share. When no track has an artist the placeholder is unanimous, as it always was.
    let mut voters: Vec<&Derived> = tracks.iter().copied().filter(|track| track.has_artist).collect();
    if voters.is_empty() {
        voters = tracks.clone();
    }
    let mut primary_variants: HashMap<&str, Vec<&str>> = HashMap::new();
    for track in &voters {
        primary_variants.entry(track.primary.0.as_str()).or_default().push(track.primary.1.as_str());
    }
    match dominant_primary_artist(&primary_variants, voters.len()) {
        Some(artist) => EffectiveAlbumArtist::Known(artist),
        None => EffectiveAlbumArtist::VariousArtists,
    }
}

/// Strips a trailing featured-guest credit from an `ARTIST` tag for primary-artist extraction
/// (`resolve_effective_album_artist` fallback (b)): `"Dua Lipa feat. Miguel"`, `"Dua Lipa (feat.
/// Miguel)"`, `"Dua Lipa ft. Miguel"` and `"Dua Lipa featuring Miguel"` all become `"Dua Lipa"`.
/// Only ever strips a `feat`/`ft`/`featuring` marker (bracketed or inline) — never `&`/`x`/`with`,
/// which a plain artist/band name can legitimately contain (`"Earth, Wind & Fire"`, `"Simon &
/// Garfunkel"`, `"Florence + The Machine"`); those are only ever treated as a featuring separator
/// contextually, by `dominant_primary_artist`'s own majority-aware second pass, never unconditionally
/// here. Returns `artist` trimmed, unchanged, when no marker is found.
pub(super) fn strip_feat_suffix(artist: &str) -> &str {
    let lower = artist.to_ascii_lowercase();
    const INLINE_MARKERS: [&str; 5] = [" feat. ", " feat ", " ft. ", " ft ", " featuring "];
    const BRACKET_MARKERS: [&str; 4] = ["(feat", "(ft.", "(ft ", "[feat"];
    let cut = INLINE_MARKERS
        .into_iter()
        .chain(BRACKET_MARKERS)
        .filter_map(|marker| lower.find(marker))
        .min();
    match cut {
        // `to_ascii_lowercase` never changes UTF-8 byte length/boundaries, so a byte offset found in
        // `lower` slices `artist` itself safely, even with multi-byte characters elsewhere in it.
        Some(pos) => artist[..pos].trim().trim_end_matches(','),
        None => artist.trim(),
    }
}

/// The winning primary artist for `resolve_effective_album_artist` fallback (b), or `None` when no
/// candidate reaches `PRIMARY_ARTIST_MAJORITY_THRESHOLD` of `total` tracks. `variants` maps each
/// primary artist's normalized text to every original-cased occurrence seen (already feat-stripped
/// by the caller).
///
/// First finds the plain majority (most occurrences, ties broken like `pick_majority_variant`), then
/// folds in any OTHER variant that reads as `"<winner> & Guest"`/`"<winner> x Guest"`/`"<winner> with
/// Guest"` (`collaborator_prefix_matches`) — a featuring credit spelled with a separator instead of
/// `feat.`/`ft.`/`featuring`. This is intentionally only ever checked relative to an already-
/// established winner, never as a blind split of every `&`: a uniform album by "Earth, Wind & Fire"
/// already reaches 100% agreement on the full name in the first pass and never even reaches this
/// fold-in step, so a real band name is never split apart.
fn dominant_primary_artist(variants: &HashMap<&str, Vec<&str>>, total: usize) -> Option<String> {
    if total == 0 {
        return None;
    }
    let (winner_norm, _) = variants
        .iter()
        .map(|(norm, occurrences)| (*norm, occurrences.len()))
        .max_by(|(norm_a, count_a), (norm_b, count_b)| count_a.cmp(count_b).then_with(|| norm_b.cmp(norm_a)))?;

    let covered: usize = variants
        .iter()
        .filter(|(norm, occurrences)| **norm == winner_norm || occurrences.iter().any(|raw| collaborator_prefix_matches(raw, winner_norm)))
        .map(|(_, occurrences)| occurrences.len())
        .sum();
    if (covered as f64) / (total as f64) < PRIMARY_ARTIST_MAJORITY_THRESHOLD {
        return None;
    }

    // Winning display string: the winner's own most common original spelling (a collaborator
    // variant's spelling never contributes), same tie-break as `pick_majority_variant`.
    variants.get(winner_norm).and_then(|occurrences| most_common(occurrences.iter().map(|raw| (*raw).to_owned())))
}

/// Whether primary-artist text `candidate` (as tagged) reads as `"{winner} & ..."`/`"{winner} x
/// ..."`/`"{winner} with ..."` — a featuring credit spelled with a separator rather than
/// `feat.`/`ft.`/`featuring` (`dominant_primary_artist`). `winner` is an already-normalized artist
/// key (`normalize_artist_key`), so the comparison ignores case, accents and punctuation.
fn collaborator_prefix_matches(candidate: &str, winner: &str) -> bool {
    let lower = candidate.to_ascii_lowercase();
    // `to_ascii_lowercase` keeps byte offsets, so an offset found in `lower` slices `candidate` safely.
    [" & ", " x ", " with "]
        .into_iter()
        .filter_map(|separator| lower.find(separator))
        .min()
        .is_some_and(|position| normalize_artist_key(&candidate[..position]) == winner)
}

/// Picks the winning display string among `variants` (normalized text -> every original-cased
/// occurrence seen): the normalized key with the most occurrences, ties broken by the
/// lexicographically smallest normalized key, then that key's most common original spelling, ties
/// broken by the smallest string — a pure function of the current value multiset, so the result
/// never depends on iteration or arrival order.
fn pick_majority_variant(variants: HashMap<&str, Vec<&str>>) -> String {
    let (_, winners) = variants
        .into_iter()
        .max_by(|(norm_a, occurrences_a), (norm_b, occurrences_b)| {
            occurrences_a.len().cmp(&occurrences_b.len()).then_with(|| norm_b.cmp(norm_a))
        })
        .expect("variants is non-empty");
    most_common(winners.into_iter().map(str::to_owned)).expect("each variant has at least one occurrence")
}

pub fn display_title(record: &TrackRecord) -> String {
    record.tags.title.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).unwrap_or_else(|| file_stem(&record.key.path))
}

pub fn display_artist(record: &TrackRecord) -> String {
    record
        .tags
        .artist
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| record.tags.album_artist.as_deref().map(str::trim).filter(|s| !s.is_empty()))
        .map(str::to_owned)
        .unwrap_or_else(|| "Unknown Artist".to_owned())
}

pub fn display_album(record: &TrackRecord) -> String {
    if let Some(album) = &record.effective_album {
        return album.title.clone();
    }
    record
        .tags
        .album
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(clean_album_title)
        .or_else(|| record.key.path.parent().and_then(Path::file_name).and_then(|name| name.to_str()).map(str::to_owned))
        .unwrap_or_else(|| "Unknown Album".to_owned())
}

fn file_stem(path: &Path) -> String {
    path.file_stem().and_then(|stem| stem.to_str()).map(str::to_owned).unwrap_or_else(|| "Unknown".to_owned())
}

/// A Fisher-Yates shuffle over a deterministic xorshift64* stream, so replaying the same `seed`
/// (Album Shuffle / Home "Shuffle all", `§3.7`) always yields the same order without pulling in a
/// new crate.
pub fn shuffled<T: Clone>(items: &[T], seed: u64) -> Vec<T> {
    let mut result = items.to_vec();
    let mut state = if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed };
    for i in (1..result.len()).rev() {
        state = xorshift64star(state);
        let j = (state % (i as u64 + 1)) as usize;
        result.swap(i, j);
    }
    result
}

fn xorshift64star(mut x: u64) -> u64 {
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(path: &str) -> TrackKey {
        TrackKey::whole_file(PathBuf::from(path))
    }

    fn track(path: &str, tags: TrackTags, duration_ms: Option<u64>, added_seq: u64) -> TrackRecord {
        TrackRecord {
            key: key(path),
            info: AudioInfo {
                sample_rate: 44_100,
                duration_ms,
                source_channels: 2,
                bits_per_sample: 16,
                is_float: false,
                integer_pcm: true,
                format: "FLAC".into(),
            },
            file_size: None,
            tags,
            artwork: None,
            artwork_source: ArtworkSource::None,
            added_seq,
            effective_album_artist: EffectiveAlbumArtist::Unresolved,
            effective_album: None,
            hidden_copy_of: None,
        }
    }

    fn tags(title: &str, artist: &str, album: &str, album_artist: Option<&str>) -> TrackTags {
        TrackTags {
            title: Some(title.into()),
            artist: Some(artist.into()),
            album: Some(album.into()),
            album_artist: album_artist.map(str::to_owned),
            ..TrackTags::default()
        }
    }

    fn library_with(mut records: Vec<TrackRecord>) -> Library {
        let mut library = Library::new();
        for record in records.drain(..) {
            library.upsert(record);
        }
        library
    }

    #[test]
    fn album_key_prefers_album_artist_then_folder() {
        let with_album_artist = track("/music/Various/CD1/song.flac", tags("Song", "Artist A", "Comp", Some("VA")), None, 0);
        assert_eq!(album_key(&with_album_artist), "aa:va\u{1f}comp");

        let without_album_artist = track("/music/Some Album/song.flac", tags("Song", "Artist B", "Some Album", None), None, 0);
        assert_eq!(album_key(&without_album_artist), format!("af:some album\u{1f}{}", Path::new("/music/Some Album").display()));

        let mut no_album_tags = tags("Song", "Artist C", "", None);
        no_album_tags.album = None;
        let untagged = track("/music/Untagged Folder/song.flac", no_album_tags, None, 0);
        assert_eq!(album_key(&untagged), format!("dir:{}", Path::new("/music/Untagged Folder").display()));
    }

    #[test]
    fn albums_group_multi_disc_folders_by_album_artist() {
        let disc1 = track("/music/Album/CD1/01.flac", tags("A1", "Artist", "Double Album", Some("Band")), Some(200_000), 0);
        let disc2 = track("/music/Album/CD2/01.flac", tags("B1", "Artist", "Double Album", Some("Band")), Some(180_000), 1);
        let library = library_with(vec![disc1, disc2]);

        let albums = library.albums("", None);

        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].track_count, 2);
        assert_eq!(albums[0].artist, "Band");
        assert_eq!(albums[0].total_duration_ms, 380_000);
    }

    /// `AlbumSummary.format_label`/`format_variant` (§I "album_format_pill recomputes format
    /// aggregation from scratch on every album-card render"): computed once in `summarize_album`
    /// from the album's own tracks, matching what `aggregate_album_format` would compute directly —
    /// a single shared format pins the label/variant, and disagreeing formats fall back to "Mix
    /// Formats"/"mix", exactly like the removed per-render `view_model::album_format_pill` did.
    #[test]
    fn album_summary_precomputes_format_label_and_variant() {
        fn track_with_format(path: &str, format: &str, added_seq: u64) -> TrackRecord {
            // A distinct title per file: two same-titled tracks of different formats would be hidden
            // duplicate copies of one song.
            let mut record = track(path, tags(&format!("Song {path}"), "Artist", "Uniform", None), Some(60_000), added_seq);
            record.info.format = format.to_owned();
            record
        }

        let uniform = library_with(vec![
            track_with_format("/music/Uniform/a.flac", "FLAC", 0),
            track_with_format("/music/Uniform/b.flac", "FLAC", 1),
        ]);
        let uniform_album = uniform.albums("", None).into_iter().next().expect("one album");
        assert_eq!(uniform_album.format_label, "FLAC");
        assert_eq!(uniform_album.format_variant, "flac");

        let mut mixed_a = track_with_format("/music/Mixed/a.flac", "FLAC", 0);
        mixed_a.tags.album = Some("Mixed".into());
        let mut mixed_b = track_with_format("/music/Mixed/b.mp3", "MP3", 1);
        mixed_b.tags.album = Some("Mixed".into());
        let mixed = library_with(vec![mixed_a, mixed_b]);
        let mixed_album = mixed.albums("", None).into_iter().find(|album| album.title == "Mixed").expect("mixed album");
        assert_eq!(mixed_album.format_label, "Mix Formats");
        assert_eq!(mixed_album.format_variant, "mix");
    }

    /// The album pill turns gold only when every track is hi-res FLAC/WavPack (`aggregate_album_pill`).
    #[test]
    fn album_summary_variant_is_hires_only_when_every_track_is_hires() {
        fn hires_track(path: &str, bits: u32, rate: u32, added_seq: u64) -> TrackRecord {
            let mut record = track(path, tags("Song", "Artist", "Gold", None), Some(60_000), added_seq);
            record.info.format = "FLAC".to_owned();
            record.info.bits_per_sample = bits;
            record.info.sample_rate = rate;
            record
        }

        let gold = library_with(vec![hires_track("/music/Gold/a.flac", 24, 96_000, 0), hires_track("/music/Gold/b.flac", 24, 192_000, 1)]);
        let gold_album = gold.albums("", None).into_iter().next().expect("one album");
        assert_eq!((gold_album.format_label.as_str(), gold_album.format_variant.as_str()), ("FLAC", "hires"));

        let partial = library_with(vec![hires_track("/music/Gold/a.flac", 24, 96_000, 0), hires_track("/music/Gold/b.flac", 16, 44_100, 1)]);
        let partial_album = partial.albums("", None).into_iter().next().expect("one album");
        assert_eq!(partial_album.format_variant, "flac");
    }

    #[test]
    fn album_tracks_sort_by_disc_track_title() {
        let mut second = tags("Second", "Artist", "Album", Some("Band"));
        second.disc_number = Some(1);
        second.track_number = Some(2);
        let mut first = tags("First", "Artist", "Album", Some("Band"));
        first.disc_number = Some(1);
        first.track_number = Some(1);
        let mut disc2 = tags("Disc Two", "Artist", "Album", Some("Band"));
        disc2.disc_number = Some(2);
        disc2.track_number = Some(1);

        let library = library_with(vec![
            track("/music/Album/second.flac", second, None, 0),
            track("/music/Album/disc2.flac", disc2, None, 1),
            track("/music/Album/first.flac", first, None, 2),
        ]);
        let key = album_key(library.get(&key("/music/Album/first.flac")).unwrap());

        let ordered = library.album_tracks(&key);

        assert_eq!(
            ordered.iter().map(|record| display_title(record)).collect::<Vec<_>>(),
            vec!["First".to_string(), "Second".to_string(), "Disc Two".to_string()]
        );
    }

    #[test]
    fn songs_search_is_case_insensitive_across_fields() {
        let mut collective = tags("Echoes", "Someone", "Echoes Album", Some("The Collective"));
        collective.genre = Some("Ambient Jazz".into());
        let library = library_with(vec![
            track("/music/a.flac", tags("Sunrise", "Nina Path", "Warm Colors", None), None, 0),
            track("/music/b.flac", tags("Nightfall", "Other", "Cold Shapes", None), None, 1),
            track("/music/c.flac", collective, None, 2),
        ]);

        assert_eq!(library.songs("SUNRISE").len(), 1);
        assert_eq!(library.songs("nina").len(), 1);
        assert_eq!(library.songs("warm").len(), 1);
        assert_eq!(library.songs("collective").len(), 1, "album artist must be searched");
        assert_eq!(library.songs("AMBIENT").len(), 1, "genre must be searched");
        assert!(library.songs("nonexistent").is_empty());
        assert_eq!(library.songs("").len(), 3);
    }

    #[test]
    fn search_is_accent_insensitive_across_fields() {
        let mut record = tags("Música Buena", "José", "Cumbia Fácil", Some("Álbum Artista"));
        record.genre = Some("Salsa Romántica".into());
        let library = library_with(vec![track("/music/a.flac", record, None, 0)]);

        // Typing the plain-ASCII form must still find the accented tag, on every searched field.
        assert_eq!(library.songs("musica").len(), 1, "title must match without the accent");
        assert_eq!(library.songs("jose").len(), 1, "artist must match without the accent");
        assert_eq!(library.songs("facil").len(), 1, "album must match without the accent");
        assert_eq!(library.songs("album artista").len(), 1, "album artist must match without the accent");
        assert_eq!(library.songs("romantica").len(), 1, "genre must match without the accent");
        // And the reverse: typing an accent the tag doesn't need still matches, since both sides
        // are folded the same way.
        assert_eq!(library.songs("MÚSICA").len(), 1, "matching must also be case-insensitive on the accented form");
    }

    #[test]
    fn albums_and_artists_search_broadens_to_any_track_field() {
        // Neither the album title nor the artist name contains "jazz" — only the genre does — so
        // `albums`/`artists` must fall back to a per-track match (`track_matches_query`), the same
        // way `songs`/`filtered_tracks` already do, or a genre-only query would find no album/
        // artist at all even though the matching tracks are right there in `songs`.
        let mut jazzy = tags("Blue Skies", "Nina Session", "Warm Sessions", None);
        jazzy.genre = Some("Jazz".into());
        let other = tags("Other Song", "Someone Else", "Other Album", None);
        let library = library_with(vec![
            track("/music/a.flac", jazzy, None, 0),
            track("/music/b.flac", other, None, 1),
        ]);

        let albums = library.albums("jazz", None);
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].title, "Warm Sessions");

        let artists = library.artists("jazz");
        assert_eq!(artists.len(), 1);
        assert_eq!(artists[0].name, "Nina Session");
    }

    #[test]
    fn upsert_same_path_keeps_added_seq() {
        let mut library = Library::new();
        // Upserted first so the target path below gets a non-zero seq: with both records at seq
        // 0, a regression that kept the *incoming* record's seq (always 0 from the scanner) would
        // still pass this test by accident.
        library.upsert(track("/music/zzz.flac", tags("Other", "Artist", "Album", None), None, 0));
        library.upsert(track("/music/a.flac", tags("Old Title", "Artist", "Album", None), None, 0));
        let first_seq = library.get(&key("/music/a.flac")).unwrap().added_seq;
        assert_ne!(first_seq, 0);

        library.upsert(track("/music/a.flac", tags("New Title", "Artist", "Album", None), None, 99));

        let record = library.get(&key("/music/a.flac")).unwrap();
        assert_eq!(record.added_seq, first_seq);
        assert_ne!(record.added_seq, 99);
        assert_eq!(display_title(record), "New Title");
    }

    fn track_with_key(key: TrackKey, tags: TrackTags, duration_ms: Option<u64>, added_seq: u64) -> TrackRecord {
        TrackRecord {
            key,
            info: AudioInfo {
                sample_rate: 44_100,
                duration_ms,
                source_channels: 2,
                bits_per_sample: 16,
                is_float: false,
                integer_pcm: true,
                format: "FLAC".into(),
            },
            file_size: None,
            tags,
            artwork: None,
            artwork_source: ArtworkSource::None,
            added_seq,
            effective_album_artist: EffectiveAlbumArtist::Unresolved,
            effective_album: None,
            hidden_copy_of: None,
        }
    }

    #[test]
    fn track_key_equality_hash_and_ordering() {
        let whole = TrackKey::whole_file(PathBuf::from("/music/a.flac"));
        let same_whole = TrackKey { path: PathBuf::from("/music/a.flac"), start_frame: 0, end_frame: None };
        let sub_range_a = TrackKey { path: PathBuf::from("/music/a.flac"), start_frame: 100, end_frame: Some(200) };
        let sub_range_b = TrackKey { path: PathBuf::from("/music/a.flac"), start_frame: 200, end_frame: None };

        assert_eq!(whole, same_whole, "two keys with identical fields must be equal");
        assert_ne!(whole, sub_range_a, "a whole-file key must differ from a sub-range key on the same path");
        assert_ne!(sub_range_a, sub_range_b, "two sub-ranges of the same file with different bounds must differ");

        // Usable as a HashMap key: distinct keys map to distinct slots, equal keys collide.
        let mut map = HashMap::new();
        map.insert(whole.clone(), "whole");
        map.insert(sub_range_a.clone(), "a");
        map.insert(sub_range_b.clone(), "b");
        assert_eq!(map.len(), 3);
        assert_eq!(map.get(&same_whole), Some(&"whole"));

        // Deterministic ordering: sorted primarily by path, then start_frame, then end_frame —
        // `derive(Ord)` on the struct's field order already gives this, asserted directly so a
        // future field reorder cannot silently change the sort order relied on for dedup.
        let mut keys = vec![sub_range_b.clone(), whole.clone(), sub_range_a.clone()];
        keys.sort();
        assert_eq!(keys, vec![whole, sub_range_a, sub_range_b]);
    }

    #[test]
    fn library_keyed_by_track_key_lets_sub_ranges_of_one_file_coexist() {
        let mut library = Library::new();
        let path = PathBuf::from("/music/album.flac");
        let track_one = TrackKey { path: path.clone(), start_frame: 0, end_frame: Some(1_000) };
        let track_two = TrackKey { path: path.clone(), start_frame: 1_000, end_frame: None };

        library.upsert(track_with_key(track_one.clone(), tags("Track One", "Artist", "Album", None), Some(20_000), 0));
        library.upsert(track_with_key(track_two.clone(), tags("Track Two", "Artist", "Album", None), Some(30_000), 0));

        assert_eq!(library.get(&track_one).map(display_title), Some("Track One".to_owned()));
        assert_eq!(library.get(&track_two).map(display_title), Some("Track Two".to_owned()));
        assert_eq!(library.songs("").len(), 2, "two distinct sub-ranges of one physical file are two separate tracks");

        // The exact same key (same path AND same sub-range) replaces in place, exactly like a
        // whole-file upsert already does.
        library.upsert(track_with_key(track_one.clone(), tags("Track One Retagged", "Artist", "Album", None), Some(20_000), 0));
        assert_eq!(library.songs("").len(), 2, "re-upserting the same sub-range key must replace, not add a third track");
        assert_eq!(library.get(&track_one).map(display_title), Some("Track One Retagged".to_owned()));
    }

    /// "Remove from Library" (`CLAUDE.md` "Library exclusions"): removing an album drops every one
    /// of its tracks from every projection (`albums`/`album_tracks`/`songs`/`artists`) and returns
    /// their `TrackKey`s so the caller can persist an exclusion; a sibling album untouched by the
    /// removal must survive intact.
    #[test]
    fn remove_album_drops_its_tracks_from_every_projection_and_returns_their_keys() {
        let removed_a = track("/music/Removed/a.flac", tags("A", "Artist", "Removed Album", None), Some(100_000), 0);
        let removed_b = track("/music/Removed/b.flac", tags("B", "Artist", "Removed Album", None), Some(100_000), 1);
        let kept = track("/music/Kept/a.flac", tags("C", "Other Artist", "Kept Album", None), Some(100_000), 2);
        let removed_keys = [removed_a.key.clone(), removed_b.key.clone()];
        let kept_key = kept.key.clone();
        let mut library = library_with(vec![removed_a, removed_b, kept]);
        // Read the library-resolved keys back, not ones computed from the records before they were
        // upserted: neither track carries an `ALBUMARTIST`, so `Library` resolves each one's
        // `effective_album_artist` via the shared-`ARTIST` fallback (`EffectiveAlbumArtist`), which
        // only the stored record reflects.
        let removed_album_key = album_key(library.get(&removed_keys[0]).unwrap());
        let kept_album_key = album_key(library.get(&kept_key).unwrap());

        let returned = library.remove_album(&removed_album_key);

        let mut expected = removed_keys.to_vec();
        expected.sort();
        let mut returned_sorted = returned.clone();
        returned_sorted.sort();
        assert_eq!(returned_sorted, expected, "must return exactly the removed album's TrackKeys");

        assert!(library.album(&removed_album_key).is_none(), "the removed album must no longer resolve");
        assert!(library.albums("", None).iter().all(|album| album.key != removed_album_key));
        assert!(library.songs("").iter().all(|track| !removed_keys.contains(&track.key)));
        assert!(library.artists("").iter().all(|artist| artist.name != "Artist"), "the removed album's only artist must disappear too");

        // A sibling album is untouched.
        assert!(library.album(&kept_album_key).is_some());
        assert_eq!(library.songs("").len(), 1);

        for key in &removed_keys {
            assert!(library.get(key).is_none());
        }
    }

    #[test]
    fn remove_album_with_an_unknown_key_returns_empty_and_changes_nothing() {
        let only = track("/music/Only/a.flac", tags("A", "Artist", "Only Album", None), Some(100_000), 0);
        let mut library = library_with(vec![only]);

        let returned = library.remove_album("does-not-exist");

        assert!(returned.is_empty());
        assert_eq!(library.songs("").len(), 1);
    }

    #[test]
    fn artist_filter_matches_artist_album_count() {
        let solo = track("/music/Solo/song.flac", tags("Solo Song", "Nina", "Solo Album", None), None, 0);
        let disc1 = track("/music/Album/CD1/01.flac", tags("A1", "Artist", "Double Album", Some("Band")), Some(200_000), 1);
        let disc2 = track("/music/Album/CD2/01.flac", tags("B1", "Artist", "Double Album", Some("Band")), Some(180_000), 2);
        let comp_a = track("/music/Comp/a.flac", tags("Comp A", "A", "Comp", Some("Various Artists")), None, 3);
        let comp_b = track("/music/Comp/b.flac", tags("Comp B", "B", "Comp", Some("Various Artists")), None, 4);
        let case_variant = track("/music/Other/song.flac", tags("Other Song", "nina", "Other Album", None), None, 5);

        let library = library_with(vec![solo, disc1, disc2, comp_a, comp_b, case_variant]);

        let artists = library.artists("");
        let nina = artists.iter().find(|artist| artist.name == "Nina").expect("Nina and nina must merge into one artist");
        assert_eq!(nina.album_count, 2, "case-insensitive grouping should count both of Nina's albums");

        for artist in &artists {
            assert_eq!(
                library.albums("", Some(&artist.name)).len(),
                artist.album_count,
                "opening artist {:?} from the Artists list must show exactly its album_count albums",
                artist.name
            );
        }
    }

    #[test]
    fn wav_without_info_chunk_falls_back_to_filename() {
        // `decoder-tone.wav` carries a LIST/INFO chunk with only ISFT (no INAM), so this exercises
        // the real RIFF INFO path through `read_metadata`, not a hand-built record.
        let dir = temp_test_dir("wav-fallback");
        let path = dir.join("My Song.wav");
        std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/decoder-tone.wav"), &path).unwrap();

        let info = crate::audio::probe_file(&path).unwrap();
        let metadata = crate::audio::read_metadata(&path).unwrap();
        let record = TrackRecord {
            key: TrackKey::whole_file(path.clone()),
            info,
            file_size: None,
            tags: metadata.tags,
            artwork: None,
            artwork_source: ArtworkSource::None,
            added_seq: 0,
            effective_album_artist: EffectiveAlbumArtist::Unresolved,
            effective_album: None,
            hidden_copy_of: None,
        };

        assert!(record.tags.title.is_none(), "the fixture's INFO chunk has no INAM tag");
        assert_eq!(display_title(&record), "My Song");
        assert_eq!(display_album(&record), dir.file_name().unwrap().to_str().unwrap());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn temp_test_dir(name: &str) -> PathBuf {
        let suffix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("lime-player-library-{name}-{}-{suffix}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn shuffled_is_a_permutation_and_seed_deterministic() {
        let items: Vec<u32> = (0..20).collect();

        let first = shuffled(&items, 42);
        let second = shuffled(&items, 42);
        let different_seed = shuffled(&items, 7);

        assert_eq!(first, second, "the same seed must produce the same order");
        assert_ne!(first, different_seed, "a different seed should (almost certainly) differ");
        let mut sorted_first = first.clone();
        sorted_first.sort();
        assert_eq!(sorted_first, items, "shuffling must be a permutation of the input");
    }

    /// The real-world "one disc shows as two albums" bug (`CLAUDE.md` "Album grouping"): a subset
    /// of a release's tracks carries an explicit `ALBUMARTIST` and the rest don't (e.g. Yellowcard's
    /// "Ocean Avenue" — 4 tracks tagged `ALBUMARTIST=Yellowcard`, 9 with only `ARTIST=Yellowcard`).
    /// Before group-level resolution, the 9 untagged tracks fell back to a folder-only key while the
    /// 4 tagged ones keyed on `ALBUMARTIST` alone, splitting one 13-track disc into two albums. The
    /// fix must merge all 13 into one album, and the result must not depend on the order the 13
    /// tracks were scanned/upserted in.
    #[test]
    fn ocean_avenue_pattern_merges_mixed_album_artist_presence_regardless_of_insertion_order() {
        fn build_group() -> Vec<TrackRecord> {
            let mut tracks = Vec::new();
            for i in 1..=4 {
                let t = tags(&format!("Tagged Track {i}"), "Yellowcard", "Ocean Avenue", Some("Yellowcard"));
                tracks.push(track(&format!("/music/Ocean Avenue/tagged{i}.flac"), t, Some(200_000), 0));
            }
            for i in 1..=9 {
                let t = tags(&format!("Untagged Track {i}"), "Yellowcard", "Ocean Avenue", None);
                tracks.push(track(&format!("/music/Ocean Avenue/untagged{i}.flac"), t, Some(200_000), 0));
            }
            tracks
        }

        let forward = library_with(build_group());
        let forward_albums = forward.albums("", None);
        assert_eq!(forward_albums.len(), 1, "all 13 tracks must merge into a single album");
        assert_eq!(forward_albums[0].track_count, 13);
        assert_eq!(forward_albums[0].artist, "Yellowcard");

        let mut reversed_tracks = build_group();
        reversed_tracks.reverse();
        let reversed = library_with(reversed_tracks);
        let reversed_albums = reversed.albums("", None);
        assert_eq!(reversed_albums, forward_albums, "the result must not depend on insertion order");
    }

    /// A folder with no `ALBUMARTIST` anywhere and a different `ARTIST` per track (a compilation or
    /// soundtrack) resolves to the synthesized "Various Artists", scoped to that folder so it never
    /// merges with an unrelated compilation elsewhere that happens to share a title.
    #[test]
    fn compilation_without_album_artist_resolves_to_various_artists_scoped_to_its_folder() {
        let library = library_with(vec![
            track("/music/Mixtape/a.flac", tags("Song A", "Artist One", "Mixtape", None), Some(100_000), 0),
            track("/music/Mixtape/b.flac", tags("Song B", "Artist Two", "Mixtape", None), Some(100_000), 1),
            track("/music/Mixtape/c.flac", tags("Song C", "Artist Three", "Mixtape", None), Some(100_000), 2),
        ]);

        let albums = library.albums("", None);
        assert_eq!(albums.len(), 1, "one folder, one Various Artists album");
        assert_eq!(albums[0].artist, "Various Artists");
        assert_eq!(albums[0].track_count, 3);
        assert_eq!(albums[0].key, format!("va:mixtape\u{1f}{}", Path::new("/music/Mixtape").display()));

        // A second, unrelated compilation in a different folder that happens to share the exact
        // same title must not merge with the first, purely because both resolve to "Various
        // Artists" — the folder scoping in the `va:` key is what keeps them apart.
        let other_library = library_with(vec![
            track("/music/Other Mixtape Folder/x.flac", tags("Song X", "Someone", "Mixtape", None), Some(100_000), 0),
            track("/music/Other Mixtape Folder/y.flac", tags("Song Y", "Someone Else", "Mixtape", None), Some(100_000), 1),
        ]);
        let other_albums = other_library.albums("", None);
        assert_eq!(other_albums.len(), 1);
        assert_ne!(other_albums[0].key, albums[0].key, "two different folders' Various Artists albums must not collide");
    }

    /// Two unrelated releases that happen to share a title (e.g. two different "Greatest Hits") by
    /// different artists in different folders must stay two separate albums — the fix must not
    /// start merging albums purely on title once folder-scoping is dropped for a resolved artist.
    #[test]
    fn same_titled_releases_by_different_artists_in_different_folders_stay_separate() {
        let library = library_with(vec![
            track("/music/Artist A/Greatest Hits/a.flac", tags("Song", "Artist A", "Greatest Hits", None), Some(100_000), 0),
            track("/music/Artist B/Greatest Hits/b.flac", tags("Song", "Artist B", "Greatest Hits", None), Some(100_000), 1),
        ]);

        let albums = library.albums("", None);
        assert_eq!(albums.len(), 2, "same title, different artist and folder, must not merge");
        let artists: HashSet<String> = albums.iter().map(|album| album.artist.clone()).collect();
        assert_eq!(artists, HashSet::from(["Artist A".to_owned(), "Artist B".to_owned()]));
    }

    /// CUE tracks of the same sheet already share both the sheet-level album tag and their physical
    /// file's parent directory, so they resolve as one group by construction — and a sibling
    /// whole-file track in the same folder that never got an `ALBUMARTIST` tag (the Jamiroquai
    /// "Dynamite" pattern: a stray extra file among otherwise consistently-tagged CUE/album tracks)
    /// must still join that same group via the majority vote.
    #[test]
    fn cue_tracks_merge_with_a_sibling_track_missing_album_artist_in_the_same_folder() {
        let audio_path = PathBuf::from("/music/Live Set/live.flac");
        let cue_a = TrackKey { path: audio_path.clone(), start_frame: 0, end_frame: Some(1_000) };
        let cue_b = TrackKey { path: audio_path.clone(), start_frame: 1_000, end_frame: None };
        let library = library_with(vec![
            track_with_key(cue_a, tags("Track One", "Band", "Live Set", Some("Band")), Some(100_000), 0),
            track_with_key(cue_b, tags("Track Two", "Band", "Live Set", Some("Band")), Some(120_000), 1),
            track("/music/Live Set/bonus.flac", tags("Bonus", "Band", "Live Set", None), Some(90_000), 2),
        ]);

        let albums = library.albums("", None);
        assert_eq!(albums.len(), 1, "the untagged bonus track must join the CUE sheet's album via the majority ALBUMARTIST");
        assert_eq!(albums[0].track_count, 3);
        assert_eq!(albums[0].artist, "Band");
    }

    /// Group resolution is always recomputed from the library's *current* membership, never cached
    /// across a removal: after the whole group is removed, re-adding just the track that used to
    /// lack an `ALBUMARTIST` (and so relied on the group's majority) must resolve it fresh — via its
    /// own shared `ARTIST` now that it is alone — rather than linger with the removed group's
    /// resolved artist.
    #[test]
    fn removing_a_track_forces_the_remaining_group_to_re_resolve() {
        let tagged = track("/music/Album/a.flac", tags("A", "Artist", "Album", Some("Band")), Some(100_000), 0);
        let untagged = track("/music/Album/b.flac", tags("B", "Artist", "Album", None), Some(100_000), 1);
        let mut library = library_with(vec![tagged.clone(), untagged.clone()]);
        let merged_key = album_key(library.get(&tagged.key).unwrap());
        assert_eq!(library.album_tracks(&merged_key).len(), 2, "both must merge under the majority ALBUMARTIST first");

        library.remove_album(&merged_key);
        library.upsert(untagged.clone());

        let solo_key = album_key(library.get(&untagged.key).unwrap());
        assert_eq!(
            solo_key, "aa:artist\u{1f}album",
            "re-added alone, it must resolve via its own shared ARTIST, not linger with the removed group's effective artist"
        );
        assert_eq!(library.album_tracks(&solo_key).len(), 1);
    }

    // --- Requirement 1: fallback order for the effective album artist (`CLAUDE.md` "Album
    // grouping") — explicit `ALBUMARTIST` (including a CUE sheet's `PERFORMER` once merged into it),
    // then the dominant primary artist (feat-stripped, majority-voted), then `VariousArtists`. ---

    #[test]
    fn is_disc_subfolder_name_recognizes_common_disc_folder_patterns() {
        for name in [
            "CD1", "cd1", "CD 1", "CD01", "Disc 1", "DISC 2", "Disk 1", "Disc One", "Disc Ten",
            "Disc 1 - Bonus Tracks", "CD1 (Remastered)", "cd_1", "cd-1",
        ] {
            assert!(is_disc_subfolder_name(name), "{name:?} should be recognized as a disc subfolder");
        }
    }

    #[test]
    fn is_disc_subfolder_name_rejects_lookalikes() {
        for name in ["Disco", "Discography", "CD", "Disc", "Comedy", "Cardigans", "2 Unlimited"] {
            assert!(!is_disc_subfolder_name(name), "{name:?} must not be treated as a disc subfolder");
        }
    }

    #[test]
    fn strip_feat_suffix_removes_only_feat_markers() {
        assert_eq!(strip_feat_suffix("Dua Lipa feat. Miguel"), "Dua Lipa");
        assert_eq!(strip_feat_suffix("Dua Lipa feat Miguel"), "Dua Lipa");
        assert_eq!(strip_feat_suffix("Dua Lipa ft. Miguel"), "Dua Lipa");
        assert_eq!(strip_feat_suffix("Dua Lipa ft Miguel"), "Dua Lipa");
        assert_eq!(strip_feat_suffix("Dua Lipa featuring Miguel"), "Dua Lipa");
        assert_eq!(strip_feat_suffix("Dua Lipa (feat. Miguel)"), "Dua Lipa");
        assert_eq!(strip_feat_suffix("Dua Lipa [feat. Miguel]"), "Dua Lipa");
        assert_eq!(strip_feat_suffix("Dua Lipa (ft. Miguel)"), "Dua Lipa");

        // Never touches `&`/`x`/`with` — real band names must survive unchanged.
        assert_eq!(strip_feat_suffix("Earth, Wind & Fire"), "Earth, Wind & Fire");
        assert_eq!(strip_feat_suffix("Simon & Garfunkel"), "Simon & Garfunkel");
        assert_eq!(strip_feat_suffix("Florence + The Machine"), "Florence + The Machine");
    }

    #[test]
    fn dua_lipa_feat_variants_resolve_to_the_dominant_primary_artist() {
        // 15 plain "Dua Lipa" tracks plus 2 "Dua Lipa feat. X" tracks, no ALBUMARTIST anywhere: the
        // dominant primary artist (100% once feat-stripped) must win, not fall to VariousArtists the
        // way exact-string ARTIST agreement alone (the old behavior) would.
        let mut tracks = Vec::new();
        for i in 1..=15 {
            tracks.push(track(&format!("/music/Dua Lipa/{i:02}.flac"), tags(&format!("Track {i}"), "Dua Lipa", "Dua Lipa", None), Some(200_000), 0));
        }
        tracks.push(track("/music/Dua Lipa/16.flac", tags("Lost in Your Light", "Dua Lipa feat. Miguel", "Dua Lipa", None), Some(200_000), 0));
        tracks.push(track("/music/Dua Lipa/17.flac", tags("Room for 2", "Dua Lipa feat. Jax Jones", "Dua Lipa", None), Some(200_000), 0));

        let library = library_with(tracks);
        let albums = library.albums("", None);

        assert_eq!(albums.len(), 1, "the feat.-variant tracks must join the majority album, not split it");
        assert_eq!(albums[0].artist, "Dua Lipa");
        assert_eq!(albums[0].track_count, 17);
    }

    #[test]
    fn below_threshold_primary_artist_agreement_stays_various_artists() {
        // A genuine 50/50 split, no ALBUMARTIST: neither artist reaches the 60% majority, so this
        // must stay a compilation, not arbitrarily pick a "winner".
        let library = library_with(vec![
            track("/music/Split/a1.flac", tags("A1", "Artist One", "Split", None), Some(100_000), 0),
            track("/music/Split/a2.flac", tags("A2", "Artist One", "Split", None), Some(100_000), 1),
            track("/music/Split/b1.flac", tags("B1", "Artist Two", "Split", None), Some(100_000), 2),
            track("/music/Split/b2.flac", tags("B2", "Artist Two", "Split", None), Some(100_000), 3),
        ]);

        let albums = library.albums("", None);
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].artist, "Various Artists");
    }

    #[test]
    fn band_names_with_ampersand_are_never_split_by_primary_artist_extraction() {
        for band in ["Earth, Wind & Fire", "Simon & Garfunkel", "Florence + The Machine"] {
            let library = library_with(vec![
                track("/music/Band/a.flac", tags("Song A", band, "Uniform Album", None), Some(100_000), 0),
                track("/music/Band/b.flac", tags("Song B", band, "Uniform Album", None), Some(100_000), 1),
            ]);
            let albums = library.albums("", None);
            assert_eq!(albums.len(), 1, "{band} must resolve as one album");
            assert_eq!(albums[0].artist, band, "{band} must never be split apart by feat/collaborator normalization");
        }
    }

    #[test]
    fn collaborator_style_featuring_credit_folds_into_the_dominant_primary_artist() {
        // A stylistic featuring credit spelled with `&`/`x`/`with` instead of `feat.` on a MINORITY
        // of tracks must still fold into the majority artist, exactly like a `feat.` credit would —
        // but only relative to an already-established majority (`dominant_primary_artist`), never as
        // a blind split (see the band-name test above).
        let library = library_with(vec![
            track("/music/Solo/a.flac", tags("A", "Solo Artist", "Solo Album", None), Some(100_000), 0),
            track("/music/Solo/b.flac", tags("B", "Solo Artist", "Solo Album", None), Some(100_000), 1),
            track("/music/Solo/c.flac", tags("C", "Solo Artist", "Solo Album", None), Some(100_000), 2),
            track("/music/Solo/d.flac", tags("D", "Solo Artist & Guest", "Solo Album", None), Some(100_000), 3),
        ]);

        let albums = library.albums("", None);
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].artist, "Solo Artist");
        assert_eq!(albums[0].track_count, 4);
    }

    #[test]
    fn collaborator_fold_in_does_not_depend_on_which_spelling_comes_first() {
        // "Band & Friends" and "Band + Friends" share one normalized key, so they are one variant
        // bucket; the fold-in must hold whichever spelling happens to be listed first.
        for (first, second) in [("Band & Friends", "Band + Friends"), ("Band + Friends", "Band & Friends")] {
            let library = library_with(vec![
                track("/music/Order/a.flac", tags("A", "Band", "Order Album", None), Some(100_000), 0),
                track("/music/Order/b.flac", tags("B", "Band", "Order Album", None), Some(100_000), 1),
                track("/music/Order/c.flac", tags("C", first, "Order Album", None), Some(100_000), 2),
                track("/music/Order/d.flac", tags("D", second, "Order Album", None), Some(100_000), 3),
            ]);
            let albums = library.albums("", None);
            assert_eq!(albums.len(), 1, "{first} / {second}");
            assert_eq!(albums[0].artist, "Band", "{first} / {second}");
        }
    }

    #[test]
    fn cue_sheet_performer_becomes_the_effective_album_artist_via_the_ordinary_albumartist_majority() {
        // A cue-derived track's own `ALBUMARTIST` tag is already the sheet's sheet-level `PERFORMER`
        // once one exists (`scanner::merge_cue_tags`), so it reaches step 1 (explicit `ALBUMARTIST`
        // majority) automatically — this documents and locks in that path, independent of each
        // track's own (possibly feat.-tagged) per-track `ARTIST`/`PERFORMER`.
        let library = library_with(vec![
            track("/music/Cue Album/01.flac", tags("Genesis", "Dua Lipa", "Dua Lipa", Some("Dua Lipa")), Some(200_000), 0),
            track("/music/Cue Album/02.flac", tags("Lost in Your Light", "Dua Lipa feat. Miguel", "Dua Lipa", Some("Dua Lipa")), Some(200_000), 1),
        ]);

        let albums = library.albums("", None);
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].artist, "Dua Lipa");
    }

    // --- Requirement 2: multi-disc releases in `CD1`/`CD2`-style subfolders group as ONE album
    // (`CLAUDE.md` "Album grouping"). ---

    #[test]
    fn various_artists_box_set_across_disc_subfolders_merges_into_one_album() {
        let library = library_with(vec![
            track("/music/Box Set/CD1/a.flac", tags("Song A", "Artist One", "Compilation", None), Some(100_000), 0),
            track("/music/Box Set/CD1/b.flac", tags("Song B", "Artist Two", "Compilation", None), Some(100_000), 1),
            track("/music/Box Set/CD2/c.flac", tags("Song C", "Artist Three", "Compilation", None), Some(100_000), 2),
            track("/music/Box Set/CD2/d.flac", tags("Song D", "Artist Four", "Compilation", None), Some(100_000), 3),
        ]);

        let albums = library.albums("", None);

        assert_eq!(albums.len(), 1, "a Various-Artists box set split across CD1/CD2 must merge into one album");
        assert_eq!(albums[0].artist, "Various Artists");
        assert_eq!(albums[0].track_count, 4);
    }

    #[test]
    fn resolution_group_spans_disc_subfolders_so_a_weak_disc_still_reaches_the_combined_majority() {
        // CD1 alone is a 50/50 split (would stay Various Artists in isolation); CD2 alone is
        // uniformly "Band". Resolving CD1+CD2 as ONE group (`grouping_scope_dir`) lets CD1's tracks
        // share in CD2's much larger, uniform majority and resolve to "Band" too, merging into a
        // single album instead of a 4-track "Various Artists" album plus a separate 5-track "Band"
        // album.
        let mut tracks = Vec::new();
        for i in 1..=2 {
            tracks.push(track(&format!("/music/Release/CD1/band{i}.flac"), tags(&format!("Band Track {i}"), "Band", "Release", None), Some(100_000), 0));
        }
        for i in 1..=2 {
            tracks.push(track(&format!("/music/Release/CD1/guest{i}.flac"), tags(&format!("Guest Track {i}"), "Guest", "Release", None), Some(100_000), 0));
        }
        for i in 1..=5 {
            tracks.push(track(&format!("/music/Release/CD2/band{i}.flac"), tags(&format!("Disc 2 Track {i}"), "Band", "Release", None), Some(100_000), 0));
        }

        let library = library_with(tracks);
        let albums = library.albums("", None);

        assert_eq!(albums.len(), 1, "CD1 and CD2 must resolve as one group and merge into one album");
        assert_eq!(albums[0].artist, "Band");
        assert_eq!(albums[0].track_count, 9);
    }

    /// The real-world "Dua Lipa [Complete Edition]" bug (`CLAUDE.md` "Album grouping"): a two-disc
    /// release in `CD1`/`CD2` subfolders, no `ALBUMARTIST` anywhere, `ARTIST` mostly "Dua Lipa" with
    /// a "Dua Lipa feat. X" credit on CD1. Must resolve as ONE 25-track album, artist "Dua Lipa",
    /// tracks ordered by (disc, track), with at least one track's artwork resolvable.
    #[test]
    fn dua_lipa_complete_edition_layout_merges_into_one_album() {
        let root = "/music/Dua Lipa - Dua Lipa [Complete Edition]";
        let cd1_titles = [
            "Genesis", "Lost in Your Light", "Hotter Than Hell", "Be the One", "IDGAF", "Blow Your Mind (Mwah)", "Garden",
            "No Goodbyes", "Thinking \u{2019}bout You", "New Rules", "Begging", "Homesick", "Dreams", "Room for 2", "New Love",
            "Bad Together", "Last Dance",
        ];
        let mut tracks = Vec::new();
        for (i, title) in cd1_titles.iter().enumerate() {
            let track_number = (i + 1) as u32;
            let artist = if track_number == 2 { "Dua Lipa feat. Miguel" } else { "Dua Lipa" };
            let mut tag = tags(title, artist, "Dua Lipa [Complete Edition]", None);
            tag.disc_number = Some(1);
            tag.track_number = Some(track_number);
            let path = format!("{root}/CD1/{track_number:02} - {title}.flac");
            let mut record = track(&path, tag, Some(200_000), 0);
            if track_number == 1 {
                record.artwork_source = ArtworkSource::Embedded;
            }
            tracks.push(record);
        }
        let cd2_titles =
            ["Want To", "Running", "Kiss and Make Up", "One Kiss", "Electricity", "Scared to Be Lonely", "No Lie", "New Rules (live)"];
        for (i, title) in cd2_titles.iter().enumerate() {
            let track_number = (i + 1) as u32;
            let mut tag = tags(title, "Dua Lipa", "Dua Lipa [Complete Edition]", None);
            tag.disc_number = Some(2);
            tag.track_number = Some(track_number);
            let path = format!("{root}/CD2/{track_number:02} - {title}.flac");
            tracks.push(track(&path, tag, Some(200_000), 0));
        }

        let library = library_with(tracks);
        let albums = library.albums("", None);

        assert_eq!(albums.len(), 1, "CD1 and CD2 must merge into a single album");
        let album = &albums[0];
        assert_eq!(album.artist, "Dua Lipa");
        assert_eq!(album.track_count, 25, "17 CD1 tracks + 8 CD2 tracks");

        let ordered = library.album_tracks(&album.key);
        let discs_and_tracks: Vec<(Option<u32>, Option<u32>)> = ordered.iter().map(|t| (t.tags.disc_number, t.tags.track_number)).collect();
        let mut expected: Vec<(Option<u32>, Option<u32>)> = (1..=17).map(|n| (Some(1), Some(n))).collect();
        expected.extend((1..=8).map(|n| (Some(2), Some(n))));
        assert_eq!(discs_and_tracks, expected, "tracks must be ordered by (disc, track)");

        assert!(
            ordered.iter().any(|t| t.artwork_source == ArtworkSource::Embedded),
            "at least one track's embedded artwork must be resolvable for the merged album"
        );
    }

    /// The grouping a library ended up with, independent of insertion order: per record its folder,
    /// album key, hidden-copy winner and effective disc number; the visible albums and songs; and the
    /// incremental indices (`scope_members`, `album_members`) translated to track keys.
    fn grouping_fingerprint(library: &Library) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        for (index, record) in library.tracks.iter().enumerate() {
            assert_eq!(library.by_key[&record.key], index, "by_key points at the record's position");
            assert_eq!(library.keys[index], album_key(record), "the cached key is the record's own key");
            assert_eq!(library.scopes[index], grouping_scope_dir(record), "the cached scope is the record's own scope");
            lines.push(format!(
                "record {:?} scope={:?} key={} hidden_behind={:?} disc={:?}",
                record.key,
                library.scopes[index],
                library.keys[index],
                record.hidden_copy_of,
                effective_disc_number(record)
            ));
        }
        let names = |members: &Vec<usize>| {
            let mut keys: Vec<String> = members.iter().map(|&index| format!("{:?}", library.tracks[index].key)).collect();
            keys.sort();
            keys
        };
        for (scope, members) in &library.scope_members {
            lines.push(format!("scope {scope:?} -> {:?}", names(members)));
        }
        for (key, members) in &library.album_members {
            lines.push(format!("album_members {key} -> {:?}", names(members)));
        }
        for album in library.albums("", None) {
            let songs: Vec<String> = library.album_tracks(&album.key).iter().map(|record| format!("{:?}", record.key)).collect();
            lines.push(format!(
                "album {} {:?} by {:?} x{} {}/{} {:?}",
                album.key, album.title, album.artist, album.track_count, album.format_label, album.format_variant, songs
            ));
        }
        let mut songs: Vec<String> = library.songs("").iter().map(|record| format!("{:?}", record.key)).collect();
        songs.sort();
        lines.push(format!("songs {songs:?}"));
        lines.sort();
        lines
    }

    /// A library with every grouping rule in play: a folder-majority fold (a minority title and an
    /// untagged track), a FLAC album duplicated as a higher-resolution WavPack sibling folder, a
    /// two-disc box in `CD1`/`CD2` subfolders without `DISCNUMBER`, and an unrelated single.
    fn dedupe_fixture() -> Vec<TrackRecord> {
        let mut records = Vec::new();
        let titles = ["One", "Two", "Three", "Four", "Five", "Six"];
        for (number, title) in titles.iter().enumerate() {
            let mut flac = track(&format!("/m/Alpha/{number:02} {title}.flac"), tags(title, "Alpha", "Alpha Album", None), Some(200_000), 0);
            flac.tags.track_number = Some(number as u32 + 1);
            records.push(flac);
            let mut wavpack = track(&format!("/m/Alpha WV/{number:02} {title}.wv"), tags(title, "Alpha", "Alpha Album", Some("Alpha")), Some(200_500), 0);
            wavpack.info.format = "WavPack".into();
            wavpack.info.bits_per_sample = 24;
            wavpack.info.sample_rate = 96_000;
            records.push(wavpack);
        }
        records.push(track("/m/Alpha/06 Bonus.flac", tags("Bonus", "Alpha", "Alpha Album Bonus", None), Some(180_000), 0));
        let mut untagged = track("/m/Alpha/07 Hidden.flac", TrackTags::default(), Some(120_000), 0);
        untagged.tags.title = Some("Hidden".into());
        records.push(untagged);
        for disc in 1..=2 {
            for number in 1..=3 {
                records.push(track(
                    &format!("/m/Box/CD{disc}/{number:02} Song {disc}-{number}.flac"),
                    tags(&format!("Song {disc}-{number}"), "Boxer", &format!("The Box (CD{disc})"), Some("Boxer")),
                    Some(210_000),
                    0,
                ));
            }
        }
        records.push(track("/m/Singles/Lonely.flac", tags("Lonely", "Solo", "Lonely", None), Some(190_000), 0));
        records
    }

    #[test]
    fn load_records_builds_the_same_library_as_upserting_in_any_order() {
        let records = dedupe_fixture();

        let mut upserted = Library::new();
        for record in records.iter().cloned() {
            upserted.upsert(record);
        }
        let expected = grouping_fingerprint(&upserted);
        assert!(upserted.records().iter().any(|record| !record.is_visible()), "the fixture must contain hidden copies");
        assert!(upserted.records().iter().any(|record| record.effective_album.is_some()), "the fixture must exercise the folder fold");
        assert!(
            upserted.records().iter().any(|record| effective_disc_number(record) == Some(2)),
            "the fixture must exercise disc numbers from subfolders"
        );

        let mut reversed = records.clone();
        reversed.reverse();
        let mut shuffled = records.clone();
        shuffled.sort_by_key(|record| record.key.path.to_string_lossy().bytes().map(usize::from).sum::<usize>() % 13);

        for (name, order) in [("saved order", records.clone()), ("reversed", reversed.clone()), ("shuffled", shuffled)] {
            let mut loaded = Library::new();
            loaded.load_records(order);
            assert_eq!(grouping_fingerprint(&loaded), expected, "load_records ({name}) must equal upsert");
        }

        // Upserting in a different order than loading gives the same grouping too (order independence).
        let mut upserted_reversed = Library::new();
        for record in reversed {
            upserted_reversed.upsert(record);
        }
        assert_eq!(grouping_fingerprint(&upserted_reversed), expected);

        // Grouping state carried by incoming records is discarded and recomputed, and a repeated key
        // replaces the earlier record in place.
        let mut stale = records.clone();
        for record in &mut stale {
            record.hidden_copy_of = Some(TrackKey::whole_file(PathBuf::from("/nowhere.flac")));
        }
        let mut loaded = Library::new();
        loaded.load_records(stale);
        loaded.load_records(records.clone());
        assert_eq!(grouping_fingerprint(&loaded), expected, "reloading the same records recomputes everything");
        assert_eq!(loaded.records().len(), records.len());
    }

    #[test]
    fn remove_tracks_regroups_like_a_library_built_without_them() {
        let records = dedupe_fixture();
        let doomed: HashSet<TrackKey> = records.iter().filter(|record| record.info.format == "WavPack").map(|record| record.key.clone()).collect();

        let mut loaded = Library::new();
        loaded.load_records(records.clone());
        assert!(loaded.records().iter().any(|record| !record.is_visible()), "the FLAC copies start hidden");
        loaded.remove_tracks(&doomed);

        let mut expected = Library::new();
        for record in records.into_iter().filter(|record| !doomed.contains(&record.key)) {
            expected.upsert(record);
        }
        assert_eq!(grouping_fingerprint(&loaded), grouping_fingerprint(&expected));
        assert!(loaded.records().iter().all(TrackRecord::is_visible), "with the winners gone every FLAC copy is visible again");
    }

    /// The derived `Ord` is the artwork precedence the scanner and `AppState` compare with: a named
    /// folder cover beats the embedded picture, which beats the legacy `folder.jpg`.
    #[test]
    fn artwork_source_orders_by_precedence() {
        assert!(ArtworkSource::Folder > ArtworkSource::Embedded);
        assert!(ArtworkSource::Embedded > ArtworkSource::FolderFallback);
        assert!(ArtworkSource::FolderFallback > ArtworkSource::None);
    }
}

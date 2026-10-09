//! The session library (`§3.3`): everything opened this run, keyed by path, grouped into albums
//! and artists. `store::LibrarySources` persists *which paths* to re-scan (individually opened
//! files and added folders) so `main.rs` can rebuild this library at the next startup by re-running
//! them through `scanner::LibraryScanner`; `cache` additionally saves the decoded records and album
//! pictures so the library shows at once at launch, before (and even without) that rescan.

pub mod cache;
pub mod cue;
pub mod format;
pub mod repair;
pub mod scanner;
pub mod store;
pub mod walker;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::audio::{AudioInfo, PreparedTrack, TrackTags};
use format::aggregate_album_format;

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
/// sync on every mutation (`Library::resolve_group_for`); a record built outside a `Library` (a
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
        }
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

#[derive(Default)]
pub struct Library {
    tracks: Vec<TrackRecord>,
    by_key: HashMap<TrackKey, usize>,
    next_seq: u64,
}

impl Library {
    /// Inserts `record`, or replaces the record at the same `TrackKey` in place while keeping its
    /// original `added_seq` (`§3.3`). Two records that share a path but differ in `start_frame`/
    /// `end_frame` (distinct CUE sub-ranges of the same physical file) coexist as separate entries.
    ///
    /// Also re-resolves `effective_album_artist` (`§3.3` "Album grouping key") for every track that
    /// shares `record`'s (normalized album title, grouping-scope directory) group, including
    /// `record` itself — not just for `record` alone — since one more/different `ALBUMARTIST` tag
    /// arriving can shift the group's majority for tracks already stored here.
    ///
    /// Returns every distinct `(old_key, new_key)` album-key transition this call caused — not only
    /// `record`'s own (though that is included whenever it actually changed): re-resolving the group
    /// can also change an already-stored SIBLING track's own `effective_album_artist`, and so its
    /// `album_key`, purely as a side effect (e.g. the group's majority flipping from a lone track's
    /// own `ARTIST` to `VariousArtists` once a differently-tagged track joins, or back once enough
    /// tracks establish a dominant primary artist). `AppState::apply_scanned` (`CLAUDE.md` "Album
    /// grouping") uses this to migrate its artwork cache and recently-played list off every key this
    /// call abandoned, not only the one for the track it just scanned — missing a sibling's silent
    /// transition was the real cause of the "CD1 shows no artwork" bug: the first track of a group to
    /// get its embedded/folder art decoded caches it under whatever key was current at that moment,
    /// and a later track joining the group could move the whole group onto a different key without
    /// that art ever being recomputed or re-cached under it.
    pub fn upsert(&mut self, mut record: TrackRecord) -> Vec<(String, String)> {
        let group = resolution_group_key(&record);
        let incoming_key = record.key.clone();
        let previous_key = self.by_key.get(&incoming_key).map(|&index| album_key(&self.tracks[index]));
        if let Some(&index) = self.by_key.get(&incoming_key) {
            record.added_seq = self.tracks[index].added_seq;
            self.tracks[index] = record;
        } else {
            record.added_seq = self.next_seq;
            self.next_seq += 1;
            self.by_key.insert(incoming_key.clone(), self.tracks.len());
            self.tracks.push(record);
        }

        let Some((album_norm, parent)) = group else { return Vec::new() };
        let per_track = self.resolve_group_for(&album_norm, &parent);

        let mut transitions: Vec<(String, String)> = Vec::new();
        for (track_key, before_key, after_key) in per_track {
            // The incoming record's own before/after is reported below instead, using the key it
            // truly held *before this whole call* (`previous_key`) — `resolve_group_for`'s own
            // "before" snapshot for it is only the transient, pre-group-resolution fallback key
            // computed a few lines up (e.g. `af:`), which nothing outside this function ever
            // observed or cached anything under.
            if track_key == incoming_key {
                continue;
            }
            let pair = (before_key, after_key);
            if pair.0 != pair.1 && !transitions.contains(&pair) {
                transitions.push(pair);
            }
        }
        if let Some(previous_key) = previous_key {
            let final_key = self.by_key.get(&incoming_key).map(|&index| album_key(&self.tracks[index])).unwrap_or_default();
            if previous_key != final_key && !transitions.contains(&(previous_key.clone(), final_key.clone())) {
                transitions.push((previous_key, final_key));
            }
        }
        transitions
    }

    /// Recomputes `effective_album_artist` for every currently-stored track whose own (normalized
    /// album title, grouping-scope directory) equals `(album_norm, parent)`, from scratch, using only
    /// the group's *current* membership — never anything cached from before. This is what keeps the
    /// result deterministic regardless of the order tracks arrived in: two libraries built from the
    /// same final set of tracks, in any insertion order, end up with the same resolution for every
    /// group. A no-op (empty result) if the group is currently empty (e.g. its last track was just
    /// removed).
    ///
    /// Returns, for every member whose `album_key()` actually changed, `(TrackKey, old_key,
    /// new_key)` — the "before" is this same function's own snapshot taken just before resolution,
    /// so for a track being inserted/replaced in the same `upsert` call that triggered this, it
    /// reflects the transient pre-resolution fallback key, not whatever it held before that call
    /// (see `upsert`'s own doc comment for how the two are combined).
    fn resolve_group_for(&mut self, album_norm: &str, parent: &Path) -> Vec<(TrackKey, String, String)> {
        let indices: Vec<usize> = self
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, track)| resolution_group_key(track).as_ref().is_some_and(|(a, p)| a == album_norm && p == parent))
            .map(|(index, _)| index)
            .collect();
        self.resolve_group_indices(&indices)
    }

    /// `resolve_group_for` for a group whose member indices the caller already knows (bulk loading
    /// resolves every group once, from one pass over the tracks, instead of one scan per group).
    fn resolve_group_indices(&mut self, indices: &[usize]) -> Vec<(TrackKey, String, String)> {
        if indices.is_empty() {
            return Vec::new();
        }
        let before: Vec<(TrackKey, String)> = indices.iter().map(|&index| (self.tracks[index].key.clone(), album_key(&self.tracks[index]))).collect();
        let effective = resolve_effective_album_artist(indices.iter().map(|&index| &self.tracks[index]));
        for &index in indices {
            self.tracks[index].effective_album_artist = effective.clone();
        }
        indices
            .iter()
            .zip(before)
            .filter_map(|(&index, (track_key, before_key))| {
                let after_key = album_key(&self.tracks[index]);
                (before_key != after_key).then_some((track_key, before_key, after_key))
            })
            .collect()
    }

    pub fn get(&self, key: &TrackKey) -> Option<&TrackRecord> {
        self.by_key.get(key).map(|&index| &self.tracks[index])
    }

    /// Every record in insertion (`added_seq`) order; the persistent library cache saves this
    /// (`cache::CacheSnapshot`).
    pub fn records(&self) -> &[TrackRecord] {
        &self.tracks
    }

    /// Inserts a whole batch of records (the persistent cache load, `cache::load`) in the given
    /// order, which becomes their `added_seq` order. Equivalent to calling `upsert` for each record
    /// (a repeated `TrackKey` replaces the earlier one in place), except every album-artist group is
    /// resolved ONCE at the end, from a single pass over the tracks: `upsert` rescans all tracks per
    /// call, which is quadratic for a library of tens of thousands of tracks and far too slow on the
    /// startup path. Reports no key transitions, since nothing downstream has seen the intermediate
    /// keys.
    pub fn load_records(&mut self, records: impl IntoIterator<Item = TrackRecord>) {
        for mut record in records {
            if let Some(&index) = self.by_key.get(&record.key) {
                record.added_seq = self.tracks[index].added_seq;
                self.tracks[index] = record;
            } else {
                record.added_seq = self.next_seq;
                self.next_seq += 1;
                self.by_key.insert(record.key.clone(), self.tracks.len());
                self.tracks.push(record);
            }
        }
        let mut groups: HashMap<(String, PathBuf), Vec<usize>> = HashMap::new();
        for (index, track) in self.tracks.iter().enumerate() {
            if let Some(group) = resolution_group_key(track) {
                groups.entry(group).or_default().push(index);
            }
        }
        for indices in groups.values() {
            let _ = self.resolve_group_indices(indices);
        }
    }

    /// Removes exactly the tracks in `keys` (a pruned cache entry whose file is gone, `cache`), and
    /// re-resolves the album-artist group of every track it touched. Unlike `remove_album` it can
    /// drop part of an album, so the remaining tracks of a group may land on another album key; every
    /// distinct `(old_key, new_key)` such move is returned for the caller to migrate artwork and
    /// navigation (`AppState::prune_tracks`).
    pub fn remove_tracks(&mut self, keys: &HashSet<TrackKey>) -> Vec<(String, String)> {
        let mut touched_groups: HashSet<(String, PathBuf)> = HashSet::new();
        let before = self.tracks.len();
        self.tracks.retain(|record| {
            if keys.contains(&record.key) {
                if let Some(group) = resolution_group_key(record) {
                    touched_groups.insert(group);
                }
                false
            } else {
                true
            }
        });
        if self.tracks.len() == before {
            return Vec::new();
        }
        self.by_key.clear();
        for (index, record) in self.tracks.iter().enumerate() {
            self.by_key.insert(record.key.clone(), index);
        }
        let mut transitions: Vec<(String, String)> = Vec::new();
        for (album_norm, parent) in touched_groups {
            for (_, old_key, new_key) in self.resolve_group_for(&album_norm, &parent) {
                let pair = (old_key, new_key);
                if !transitions.contains(&pair) {
                    transitions.push(pair);
                }
            }
        }
        transitions
    }

    /// Removes every track belonging to album `key` from the session library and returns their
    /// `TrackKey`s ("Remove from Library", `CLAUDE.md` "Library exclusions"). Non-destructive: this
    /// only drops the in-memory `TrackRecord`s here — it never touches a file on disk and never
    /// reaches into `AudioPlayer`'s queue or active stream (the library and playback are separate,
    /// session-only state; see `main.rs`'s `on_remove_album_requested` for where the two are kept
    /// decoupled). The caller is expected to persist the returned keys as an exclusion
    /// (`store::LibrarySources::exclude_tracks`) so a later rescan (startup restore) does not bring
    /// them back. Returns an empty `Vec` if `key` matches no album.
    pub fn remove_album(&mut self, key: &str) -> Vec<TrackKey> {
        let mut removed = Vec::new();
        // Every resolution group (`resolution_group_key`) touched by a removed track: usually
        // emptied entirely by this removal (an album key's tracks are always whole groups), but
        // re-resolving them afterward — rather than assuming that — keeps this correct even if a
        // future removal path ever drops less than a whole group, and leaves no stale
        // `effective_album_artist` behind for a track a caller re-adds under the same group later.
        let mut touched_groups: HashSet<(String, PathBuf)> = HashSet::new();
        self.tracks.retain(|record| {
            if album_key(record) == key {
                removed.push(record.key.clone());
                if let Some(group) = resolution_group_key(record) {
                    touched_groups.insert(group);
                }
                false
            } else {
                true
            }
        });
        // `by_key` holds indices into `tracks`, which `retain` just shifted; cheap to rebuild
        // outright rather than patch in place — a removal is a rare, user-initiated action, not a
        // hot path, and this keeps the index provably correct.
        self.by_key.clear();
        for (index, record) in self.tracks.iter().enumerate() {
            self.by_key.insert(record.key.clone(), index);
        }
        for (album_norm, parent) in touched_groups {
            // A removal can shift the remaining group's resolution too (e.g. dropping the tracks
            // that made up a majority) — not surfaced to the caller today, same as before this
            // function started returning per-key transitions from `resolve_group_for`. Removal is a
            // rare, user-initiated action that immediately renders "Remove from Library" and never
            // shows the just-removed album's own artwork/back-stack entry again, so a since-changed
            // sibling album's art/recent-albums entry lingering under an abandoned key one poll cycle
            // longer than `upsert`'s own transitions do is an accepted, narrower gap — unlike the
            // scan-time case, there is no equivalent test coverage requiring it here.
            let _ = self.resolve_group_for(&album_norm, &parent);
        }
        removed
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
        let tracks: Vec<&TrackRecord> = self.tracks.iter().filter(|track| album_key(track) == key).collect();
        if tracks.is_empty() { None } else { Some(summarize_album(&tracks)) }
    }

    /// The tracks of album `key`, sorted by disc, track number, then title, then path (a stable
    /// tie-break for tracks that carry no numbering at all).
    pub fn album_tracks(&self, key: &str) -> Vec<&TrackRecord> {
        let mut tracks: Vec<&TrackRecord> = self.tracks.iter().filter(|track| album_key(track) == key).collect();
        // `sort_by_cached_key`, not `sort_by`: an O(n log n) comparator would otherwise lowercase
        // `display_title` again on every comparison instead of once per track (`§6`, same fix as
        // `songs` below).
        tracks.sort_by_cached_key(|track| {
            (track.tags.disc_number.unwrap_or(0), track.tags.track_number.unwrap_or(0), display_title(track).to_lowercase(), track.key.clone())
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
                track.tags.disc_number.unwrap_or(0),
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
        for track in &self.tracks {
            let artist = display_artist(track);
            let key = artist.to_lowercase();
            display_names.entry(key.clone()).or_insert(artist);
            *track_counts.entry(key.clone()).or_insert(0) += 1;
            albums_by_artist.entry(key.clone()).or_default().insert(album_key(track));
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
        for track in &self.tracks {
            groups.entry(album_key(track)).or_default().push(track);
        }
        groups
    }

    /// A case- and accent-insensitive substring match on title, artist, album, album artist and
    /// genre; an empty query matches every track (`§3.3` "Search").
    fn filtered_tracks(&self, query: &str) -> Vec<&TrackRecord> {
        let needle = normalize_for_search(query.trim());
        if needle.is_empty() {
            return self.tracks.iter().collect();
        }
        self.tracks.iter().filter(|track| track_matches_query(track, &needle)).collect()
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

fn strip_latin_diacritic(c: char) -> char {
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
    let title = display_album(tracks[0]);
    // Every track in `tracks` shares one album key, which (once resolved by `Library`) means they
    // all share one `effective_album_artist` too — reading it off `tracks[0]` alone is safe. The
    // `Unresolved` arm is only a defensive fallback: `summarize_album`'s one caller (`album_groups`,
    // over `self.tracks`) only ever sees Library-resolved records.
    let artist = match &tracks[0].effective_album_artist {
        EffectiveAlbumArtist::Known(artist) => artist.clone(),
        EffectiveAlbumArtist::VariousArtists => "Various Artists".to_owned(),
        EffectiveAlbumArtist::Unresolved => match resolve_effective_album_artist(tracks.iter().copied()) {
            EffectiveAlbumArtist::Known(artist) => artist,
            _ => "Various Artists".to_owned(),
        },
    };
    let year = tracks.iter().filter_map(|track| track.tags.year).min();
    let total_duration_ms = tracks.iter().filter_map(|track| track.info.duration_ms).sum();
    let first_added_seq = tracks.iter().map(|track| track.added_seq).min().unwrap_or(0);
    let (format_label, format_variant) = match aggregate_album_format(tracks.iter().map(|track| track.info.format.as_str())) {
        Some(format) => (format.label().to_owned(), format.variant().to_owned()),
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
    let scope = grouping_scope_dir(record);
    let Some(album) = normalized_album(record) else {
        return format!("dir:{}", scope.display());
    };
    match &record.effective_album_artist {
        EffectiveAlbumArtist::Known(artist) => format!("aa:{}\u{1f}{}", normalize_key_text(artist), album),
        EffectiveAlbumArtist::VariousArtists => format!("va:{}\u{1f}{}", album, scope.display()),
        EffectiveAlbumArtist::Unresolved => {
            match record.tags.album_artist.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                Some(album_artist) => format!("aa:{}\u{1f}{}", normalize_key_text(album_artist), album),
                None => format!("af:{}\u{1f}{}", album, scope.display()),
            }
        }
    }
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
    let lower = name.trim().to_ascii_lowercase();
    const WORDS: [&str; 3] = ["disc", "disk", "cd"];
    WORDS.into_iter().any(|word| {
        lower.strip_prefix(word).is_some_and(|rest| disc_number_prefix_len(rest.trim_start_matches([' ', '.', '-', '_'])) > 0)
    })
}

/// `> 0` when `rest` (already lowercased) starts with a disc-number token: one or more ASCII
/// digits, or one of the spelled-out numbers `"one"`..`"ten"` a disc rip's folder name sometimes
/// uses (`"Disc One"`).
fn disc_number_prefix_len(rest: &str) -> usize {
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 {
        return digits;
    }
    const SPELLED: [&str; 10] = ["one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten"];
    SPELLED.into_iter().find(|word| rest.starts_with(word)).map_or(0, str::len)
}

fn normalized_album(record: &TrackRecord) -> Option<String> {
    record.tags.album.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(normalize_key_text)
}

/// The group a record's `effective_album_artist` is resolved within: every track sharing this same
/// (normalized album title, grouping-scope directory) pair (`grouping_scope_dir`), including CUE
/// tracks of the same sheet (they share both the sheet-level album tag and their physical file's
/// directory) and, for a multi-disc release, every disc subfolder's tracks together. `None` for a
/// record with no album tag at all — it never takes part in album-artist resolution, since its
/// `album_key` is the directory-only `dir:` fallback regardless.
///
/// `AppState`'s artwork cache also files each picture under its contributor's group: the group is the
/// unit that moves to another album key together when its resolution changes.
pub(crate) fn resolution_group_key(record: &TrackRecord) -> Option<(String, PathBuf)> {
    normalized_album(record).map(|album| (album, grouping_scope_dir(record)))
}

/// Case-insensitive, trimmed, whitespace-collapsed, Unicode-NFC-normalized text for an album-key
/// component (`§3.3` "Album grouping key"): NFC first, so a title/artist typed or tagged with
/// precomposed accents (`"café"`) and the same text in combining-mark form (`"cafe\u{301}"`) — two
/// different taggers can each produce either for the same characters — group identically.
fn normalize_key_text(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ").nfc().collect::<String>().to_lowercase()
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
fn resolve_effective_album_artist<'a>(tracks: impl Iterator<Item = &'a TrackRecord>) -> EffectiveAlbumArtist {
    let tracks: Vec<&TrackRecord> = tracks.collect();

    let mut album_artist_variants: HashMap<String, Vec<String>> = HashMap::new();
    for track in &tracks {
        if let Some(album_artist) = track.tags.album_artist.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            album_artist_variants.entry(normalize_key_text(album_artist)).or_default().push(album_artist.to_owned());
        }
    }
    if !album_artist_variants.is_empty() {
        return EffectiveAlbumArtist::Known(pick_majority_variant(album_artist_variants));
    }

    let mut primary_variants: HashMap<String, Vec<String>> = HashMap::new();
    for track in &tracks {
        let artist = display_artist(track);
        let primary = strip_feat_suffix(&artist).to_owned();
        primary_variants.entry(normalize_key_text(&primary)).or_default().push(primary);
    }
    match dominant_primary_artist(&primary_variants, tracks.len()) {
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
fn strip_feat_suffix(artist: &str) -> &str {
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
fn dominant_primary_artist(variants: &HashMap<String, Vec<String>>, total: usize) -> Option<String> {
    if total == 0 {
        return None;
    }
    let (winner_norm, _) = variants
        .iter()
        .map(|(norm, occurrences)| (norm.clone(), occurrences.len()))
        .max_by(|(norm_a, count_a), (norm_b, count_b)| count_a.cmp(count_b).then_with(|| norm_b.cmp(norm_a)))?;

    let covered: usize = variants
        .iter()
        .filter(|(norm, _)| **norm == winner_norm || collaborator_prefix_matches(norm, &winner_norm))
        .map(|(_, occurrences)| occurrences.len())
        .sum();
    if (covered as f64) / (total as f64) < PRIMARY_ARTIST_MAJORITY_THRESHOLD {
        return None;
    }

    // Winning display string: majority-vote among the winner's own original-cased occurrences only
    // (a collaborator variant's casing never contributes), same tie-break as `pick_majority_variant`.
    let mut winners = variants.get(&winner_norm).cloned().unwrap_or_default();
    winners.sort();
    winners.into_iter().next()
}

/// Whether normalized primary-artist text `candidate` reads as `"{winner} & ..."`/`"{winner} x
/// ..."`/`"{winner} with ..."` — a featuring credit spelled with a separator rather than
/// `feat.`/`ft.`/`featuring` (`dominant_primary_artist`). Both arguments are already
/// `normalize_key_text`-normalized, so the comparison is case/whitespace/Unicode-form-insensitive.
fn collaborator_prefix_matches(candidate: &str, winner: &str) -> bool {
    [" & ", " x ", " with "].into_iter().any(|separator| candidate.split_once(separator).is_some_and(|(left, _)| left == winner))
}

/// Picks the winning display string among `variants` (normalized text -> every original-cased
/// occurrence seen): the normalized key with the most occurrences, ties broken by the
/// lexicographically smallest normalized key, then the lexicographically smallest original-cased
/// occurrence within it — a pure function of the current value multiset, so the result never depends
/// on iteration or arrival order.
fn pick_majority_variant(variants: HashMap<String, Vec<String>>) -> String {
    let (_, mut winners) = variants
        .into_iter()
        .max_by(|(norm_a, occurrences_a), (norm_b, occurrences_b)| {
            occurrences_a.len().cmp(&occurrences_b.len()).then_with(|| norm_b.cmp(norm_a))
        })
        .expect("variants is non-empty");
    winners.sort();
    winners.into_iter().next().expect("each variant has at least one occurrence")
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
    record
        .tags
        .album
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
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
            let mut record = track(path, tags("Song", "Artist", "Uniform", None), Some(60_000), added_seq);
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

    /// The derived `Ord` is the artwork precedence the scanner and `AppState` compare with: a named
    /// folder cover beats the embedded picture, which beats the legacy `folder.jpg`.
    #[test]
    fn artwork_source_orders_by_precedence() {
        assert!(ArtworkSource::Folder > ArtworkSource::Embedded);
        assert!(ArtworkSource::Embedded > ArtworkSource::FolderFallback);
        assert!(ArtworkSource::FolderFallback > ArtworkSource::None);
    }
}

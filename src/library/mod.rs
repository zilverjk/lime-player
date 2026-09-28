//! The session library (`§3.3`): everything opened this run, keyed by path, grouped into albums
//! and artists. The decoded `Library`/`TrackRecord` values here are still session-only and are
//! never persisted; `store::LibrarySources` separately persists only *which paths* to re-scan
//! (individually opened files and added folders) so `main.rs` can rebuild this same session
//! library at the next startup by re-running them through `scanner::LibraryScanner`.

pub mod cue;
pub mod format;
pub mod scanner;
pub mod store;
pub mod walker;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtworkSource {
    Embedded,
    Folder,
    None,
}

/// Identity of a logical track: a physical file path plus the `[start_frame, end_frame)`
/// sample-frame sub-range it plays within that file. A normal whole-file track is
/// `TrackKey { path, start_frame: 0, end_frame: None }`. A CUE sub-range track's
/// `start_frame`/`end_frame` are in sample frames of the underlying physical file, computed via
/// `cue::cue_time_to_sample_frame` once the file's real sample rate is known; `end_frame` is
/// exclusive (`[start_frame, end_frame)`), and the last track of a physical file has `end_frame:
/// None`, meaning "to EOF". `Ord`/`Hash` let this serve as a `HashMap`/`Library` key and give a
/// deterministic sort/dedup order (path, then start_frame, then end_frame).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
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
    pub artwork_source: ArtworkSource,
    pub added_seq: u64,
}

impl TrackRecord {
    /// A record with no tags, no art and no known size, built from a scanner `Probed` event so
    /// the file shows up under its fallback name immediately (`§3.3` "UI handling of scanner
    /// events").
    pub fn minimal(key: TrackKey, info: AudioInfo) -> Self {
        Self { key, info, file_size: None, tags: TrackTags::default(), artwork: None, artwork_source: ArtworkSource::None, added_seq: 0 }
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
    pub fn upsert(&mut self, mut record: TrackRecord) {
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

    pub fn get(&self, key: &TrackKey) -> Option<&TrackRecord> {
        self.by_key.get(key).map(|&index| &self.tracks[index])
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
    let album_artist = tracks[0].tags.album_artist.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let artist = match album_artist {
        Some(album_artist) => album_artist.to_owned(),
        None => {
            let mut distinct: Vec<String> = Vec::new();
            for track in tracks {
                let candidate = display_artist(track);
                if !distinct.iter().any(|existing| existing.eq_ignore_ascii_case(&candidate)) {
                    distinct.push(candidate);
                }
            }
            match distinct.as_slice() {
                [only] => only.clone(),
                _ => "Various Artists".to_owned(),
            }
        }
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

/// Deterministic, case-insensitive, trimmed album grouping key (`§3.3` "Album grouping key"). Used
/// both by `Library`'s own grouping and, standalone, by callers that already hold a `&TrackRecord`
/// (`AppState::note_played_path`, `view_model::project_album_header`).
pub fn album_key(record: &TrackRecord) -> String {
    let album = record.tags.album.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let album_artist = record.tags.album_artist.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let parent = record.key.path.parent().map(Path::to_path_buf).unwrap_or_default();
    match (album_artist, album) {
        (Some(album_artist), Some(album)) => format!("aa:{}\u{1f}{}", album_artist.to_lowercase(), album.to_lowercase()),
        (None, Some(album)) => format!("af:{}\u{1f}{}", album.to_lowercase(), parent.display()),
        _ => format!("dir:{}", parent.display()),
    }
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
}

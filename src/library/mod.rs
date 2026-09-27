//! The session library (`§3.3`): everything opened this run, keyed by path, grouped into albums
//! and artists. The decoded `Library`/`TrackRecord` values here are still session-only and are
//! never persisted; `store::LibrarySources` separately persists only *which paths* to re-scan
//! (individually opened files and added folders) so `main.rs` can rebuild this same session
//! library at the next startup by re-running them through `scanner::LibraryScanner`.

pub mod format;
pub mod scanner;
pub mod store;
pub mod walker;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::audio::{AudioInfo, PreparedTrack, TrackTags};

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

#[derive(Clone, Debug)]
pub struct TrackRecord {
    pub path: PathBuf,
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
    pub fn minimal(path: PathBuf, info: AudioInfo) -> Self {
        Self { path, info, file_size: None, tags: TrackTags::default(), artwork: None, artwork_source: ArtworkSource::None, added_seq: 0 }
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
    by_path: HashMap<PathBuf, usize>,
    next_seq: u64,
}

impl Library {
    /// Inserts `record`, or replaces the record at the same path in place while keeping its
    /// original `added_seq` (`§3.3`).
    pub fn upsert(&mut self, mut record: TrackRecord) {
        if let Some(&index) = self.by_path.get(&record.path) {
            record.added_seq = self.tracks[index].added_seq;
            self.tracks[index] = record;
        } else {
            record.added_seq = self.next_seq;
            self.next_seq += 1;
            self.by_path.insert(record.path.clone(), self.tracks.len());
            self.tracks.push(record);
        }
    }

    pub fn get(&self, path: &Path) -> Option<&TrackRecord> {
        self.by_path.get(path).map(|&index| &self.tracks[index])
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

    /// `path` + `info`, ready for a queue command (`enqueue`/`play_next`/`replace_queue`); `None`
    /// for a path the library has no record of.
    pub fn prepared(&self, path: &Path) -> Option<PreparedTrack> {
        self.get(path).map(|record| PreparedTrack { path: record.path.clone(), info: record.info.clone() })
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
        let mut summaries: Vec<AlbumSummary> = self
            .album_groups()
            .into_values()
            .filter_map(|tracks| {
                let summary = summarize_album(&tracks);
                let matches = match &filter_key {
                    Some(key) => {
                        tracks.iter().any(|track| display_artist(track).to_lowercase() == *key)
                            || summary.artist.to_lowercase() == *key
                    }
                    None => true,
                };
                matches.then_some(summary)
            })
            .collect();
        let query = query.trim();
        if !query.is_empty() {
            let needle = query.to_lowercase();
            summaries.retain(|album| album.title.to_lowercase().contains(&needle) || album.artist.to_lowercase().contains(&needle));
        }
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
            (track.tags.disc_number.unwrap_or(0), track.tags.track_number.unwrap_or(0), display_title(track).to_lowercase(), track.path.clone())
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
        let mut display_names: HashMap<String, String> = HashMap::new();
        let mut track_counts: HashMap<String, usize> = HashMap::new();
        let mut albums_by_artist: HashMap<String, HashSet<String>> = HashMap::new();
        for track in &self.tracks {
            let artist = display_artist(track);
            let key = artist.to_lowercase();
            display_names.entry(key.clone()).or_insert(artist);
            *track_counts.entry(key.clone()).or_insert(0) += 1;
            albums_by_artist.entry(key).or_default().insert(album_key(track));
        }
        let mut summaries: Vec<ArtistSummary> = track_counts
            .into_iter()
            .map(|(key, track_count)| {
                let album_count = albums_by_artist.get(&key).map(HashSet::len).unwrap_or(0);
                let name = display_names.remove(&key).unwrap_or(key);
                ArtistSummary { name, album_count, track_count }
            })
            .collect();
        let query = query.trim();
        if !query.is_empty() {
            let needle = query.to_lowercase();
            summaries.retain(|artist| artist.name.to_lowercase().contains(&needle));
        }
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

    /// A case-insensitive substring match on title, artist, album, album artist and genre; an
    /// empty query matches every track (`§3.3` "Search").
    fn filtered_tracks(&self, query: &str) -> Vec<&TrackRecord> {
        let query = query.trim();
        if query.is_empty() {
            return self.tracks.iter().collect();
        }
        let needle = query.to_lowercase();
        self.tracks
            .iter()
            .filter(|track| {
                display_title(track).to_lowercase().contains(&needle)
                    || display_artist(track).to_lowercase().contains(&needle)
                    || display_album(track).to_lowercase().contains(&needle)
                    || track.tags.album_artist.as_deref().unwrap_or_default().to_lowercase().contains(&needle)
                    || track.tags.genre.as_deref().unwrap_or_default().to_lowercase().contains(&needle)
            })
            .collect()
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
    AlbumSummary { key, title, artist, year, track_count: tracks.len(), total_duration_ms, first_added_seq }
}

/// Deterministic, case-insensitive, trimmed album grouping key (`§3.3` "Album grouping key"). Used
/// both by `Library`'s own grouping and, standalone, by callers that already hold a `&TrackRecord`
/// (`AppState::note_played_path`, `view_model::project_album_header`).
pub fn album_key(record: &TrackRecord) -> String {
    let album = record.tags.album.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let album_artist = record.tags.album_artist.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let parent = record.path.parent().map(Path::to_path_buf).unwrap_or_default();
    match (album_artist, album) {
        (Some(album_artist), Some(album)) => format!("aa:{}\u{1f}{}", album_artist.to_lowercase(), album.to_lowercase()),
        (None, Some(album)) => format!("af:{}\u{1f}{}", album.to_lowercase(), parent.display()),
        _ => format!("dir:{}", parent.display()),
    }
}

pub fn display_title(record: &TrackRecord) -> String {
    record.tags.title.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).unwrap_or_else(|| file_stem(&record.path))
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
        .or_else(|| record.path.parent().and_then(Path::file_name).and_then(|name| name.to_str()).map(str::to_owned))
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

    fn track(path: &str, tags: TrackTags, duration_ms: Option<u64>, added_seq: u64) -> TrackRecord {
        TrackRecord {
            path: PathBuf::from(path),
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
        let key = album_key(library.get(Path::new("/music/Album/first.flac")).unwrap());

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
    fn upsert_same_path_keeps_added_seq() {
        let mut library = Library::new();
        // Upserted first so the target path below gets a non-zero seq: with both records at seq
        // 0, a regression that kept the *incoming* record's seq (always 0 from the scanner) would
        // still pass this test by accident.
        library.upsert(track("/music/zzz.flac", tags("Other", "Artist", "Album", None), None, 0));
        library.upsert(track("/music/a.flac", tags("Old Title", "Artist", "Album", None), None, 0));
        let first_seq = library.get(Path::new("/music/a.flac")).unwrap().added_seq;
        assert_ne!(first_seq, 0);

        library.upsert(track("/music/a.flac", tags("New Title", "Artist", "Album", None), None, 99));

        let record = library.get(Path::new("/music/a.flac")).unwrap();
        assert_eq!(record.added_seq, first_seq);
        assert_ne!(record.added_seq, 99);
        assert_eq!(display_title(record), "New Title");
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
            path: path.clone(),
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

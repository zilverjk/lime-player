//! Pure projections from Rust state to generated Slint structs (`§3.1`, `§6` Stage 5/6), plus the
//! `.slint`-source contract tests the stage's spec calls `mod ui_contract`. No Slint window, no
//! I/O: everything here is unit-tested directly against `Library`/`AppState`/`QueueTrackSnapshot`
//! values.

use slint::Image;

use crate::app_state::AppState;
use crate::audio::{AudioInfo, AudioPlayer, PreparedTrack, QueueTrackSnapshot};
use crate::library::format::{
    format_album_card_subtitle, format_album_meta, format_badge, format_clock, format_library_summary, format_search_section_header, track_format, track_format_variant,
};
use crate::library::{
    AlbumSummary, ArtistSummary, Library, TrackKey, TrackRecord, display_album, display_artist, display_title, effective_disc_number, shuffled,
    track_key_string,
};
use crate::{AlbumAction, AlbumCardData, AlbumHeaderData, ArtistRowData, TrackRowData};

/// A row's fields when `path` has no library record yet: a track queued before the scanner's
/// second pass reaches it (`§3.3`). `build_track_row` prefers the library record when one exists.
struct RowFallback<'a> {
    title: &'a str,
    artist: &'a str,
    album: &'a str,
    year: &'a str,
    format: &'a str,
    bits_per_sample: u32,
    sample_rate: u32,
    is_float: bool,
    duration_ms: Option<u64>,
}

/// A `TrackRowData` built directly from a library record, with `number` supplied by the caller
/// (a 1-based row index for Songs/Recently Added, or a disc-track label for album detail —
/// `track_number_label`). Shared by `build_track_row`'s library-hit arm and every Stage 6
/// projection that already holds a `&TrackRecord` (`project_song_rows`, `project_album_tracks`).
fn track_row_from_record(record: &TrackRecord, number: String) -> TrackRowData {
    let info = &record.info;
    let format = track_format(&info.format);
    let variant = track_format_variant(&info.format, info.bits_per_sample, info.sample_rate);
    TrackRowData {
        key: track_key_string(&record.key).into(),
        number: number.into(),
        title: display_title(record).into(),
        artist: display_artist(record).into(),
        album: display_album(record).into(),
        year: record.tags.year.map(|year| year.to_string()).unwrap_or_default().into(),
        duration: info.duration_ms.map(format_clock).unwrap_or_else(|| "\u{2014}:\u{2014}".to_owned()).into(),
        badge: format_badge(&info.format, info.bits_per_sample, info.sample_rate, info.is_float).into(),
        format_label: format.label().into(),
        format_variant: variant.into(),
    }
}

fn build_track_row(key: &TrackKey, library: &Library, number: String, fallback: RowFallback<'_>) -> TrackRowData {
    match library.get(key) {
        Some(record) => track_row_from_record(record, number),
        None => {
            let key = track_key_string(key);
            let duration = fallback.duration_ms.map(format_clock).unwrap_or_else(|| "\u{2014}:\u{2014}".to_owned());
            let format = track_format(fallback.format);
            let variant = track_format_variant(fallback.format, fallback.bits_per_sample, fallback.sample_rate);
            TrackRowData {
                key: key.into(),
                number: number.into(),
                title: fallback.title.into(),
                artist: fallback.artist.into(),
                album: fallback.album.into(),
                year: fallback.year.into(),
                duration: duration.into(),
                badge: format_badge(fallback.format, fallback.bits_per_sample, fallback.sample_rate, fallback.is_float).into(),
                format_label: format.label().into(),
                format_variant: variant.into(),
            }
        }
    }
}

/// The library's own placeholder image/flag pair for a card or header whose album key has no
/// cached artwork yet (`§3.3` "Artwork cache"): `Image::default()` plus `has-art: false`, which
/// `ArtworkView` in `widgets.slint` renders as the neutral disc placeholder.
fn art_or_placeholder(art: Option<Image>) -> (Image, bool) {
    match art {
        Some(image) => (image, true),
        None => (Image::default(), false),
    }
}

fn build_album_card(album: &AlbumSummary, state: &AppState) -> AlbumCardData {
    let (art, has_art) = art_or_placeholder(state.artwork_for_key(&album.key));
    AlbumCardData {
        key: album.key.clone().into(),
        title: album.title.clone().into(),
        artist: album.artist.clone().into(),
        subtitle: format_album_card_subtitle(album.year, album.track_count).into(),
        art,
        has_art,
        format_label: album.format_label.clone().into(),
        format_variant: album.format_variant.clone().into(),
    }
}

fn build_artist_row(artist: &ArtistSummary) -> ArtistRowData {
    ArtistRowData { name: artist.name.clone().into(), subtitle: artist_subtitle(artist.album_count, artist.track_count).into() }
}

/// `(3, 41) -> "3 albums · 41 tracks"`, singular for a count of exactly one (`§5.7` Artists row
/// subtitle; `ArtistRowData.subtitle`).
fn artist_subtitle(album_count: usize, track_count: usize) -> String {
    let albums = if album_count == 1 { "1 album".to_owned() } else { format!("{album_count} albums") };
    let tracks = if track_count == 1 { "1 track".to_owned() } else { format!("{track_count} tracks") };
    format!("{albums} · {tracks}")
}

/// The "Jump back in" shelf (`§5.7`, `§5.10` "Recently played"): `state`'s deduped, capped play
/// history, each key resolved to its current `AlbumSummary` and projected. A key the library no
/// longer has an album for is skipped rather than inserted as a blank card — this can briefly
/// happen mid-scan, between the scanner's phase-2 tags moving a played album off its phase-1
/// `dir:` key and `AppState::apply_scanned`'s rekey of `recent_album_keys` catching up. Filtered by
/// `query` with the same title-or-artist substring predicate as `Library::albums` (`§6`), so a
/// search with no matches does not leave this shelf showing unfiltered results and Home's "No
/// matches" `EmptyState` can actually appear.
fn project_jump_back_albums(state: &AppState, query: &str) -> Vec<AlbumCardData> {
    let needle = query.trim().to_lowercase();
    state
        .recent_album_keys()
        .iter()
        .filter_map(|key| state.library.album(key))
        .filter(|album| needle.is_empty() || album.title.to_lowercase().contains(&needle) || album.artist.to_lowercase().contains(&needle))
        .map(|album| build_album_card(&album, state))
        .collect()
}

/// `TrackRowData.number` for an album-detail row (`§5.5`): the bare tag track number normally, or
/// `"{disc}-{track:02}"` once the album spans more than one disc. `row` is the track's 1-based
/// position in `Library::album_tracks`'s order, used only when `track` is untagged — otherwise an
/// untagged track (a folder-grouped WAV album, most often) showed a blank `#` cell on a single-disc
/// album, or a meaningless `"2-00"` on a multi-disc one (`§6` Stage 6 fix).
fn track_number_label(disc: Option<u32>, track: Option<u32>, multi_disc: bool, row: usize) -> String {
    if multi_disc {
        format!("{}-{:02}", disc.unwrap_or(1), track.unwrap_or(row as u32))
    } else {
        track.map_or_else(|| row.to_string(), |number| number.to_string())
    }
}

/// An album is multi-disc once any of its tracks sits on a disc past the first (`§5.5`; a `DISCNUMBER`
/// tag, else the number of its `CD2`-style subfolder): untagged
/// tracks default to disc 1, so a lone `disc_number` of 2 or more is what flips this, not merely
/// having more than one distinct value.
fn album_has_multiple_discs(tracks: &[&TrackRecord]) -> bool {
    tracks.iter().any(|track| effective_disc_number(track).unwrap_or(1) > 1)
}

/// The current album's tracks (`§3.3` `Library::album_tracks` order: disc, track, title, path),
/// projected to rows plus the paths behind them in the same order, for `track-activated`
/// (`TrackListKind.album`) to map a clicked row back to `library.prepared(path)` (`§3.4`).
fn project_album_tracks(library: &Library, key: &str) -> (Vec<TrackRowData>, Vec<TrackKey>) {
    let tracks = library.album_tracks(key);
    let multi_disc = album_has_multiple_discs(&tracks);
    let mut rows = Vec::with_capacity(tracks.len());
    let mut paths = Vec::with_capacity(tracks.len());
    for (index, record) in tracks.iter().enumerate() {
        let number = track_number_label(effective_disc_number(record), record.tags.track_number, multi_disc, index + 1);
        rows.push(track_row_from_record(record, number));
        paths.push(record.key.clone());
    }
    (rows, paths)
}

/// The album-detail header band's data (`§5.7`): title/artist/meta from `Library::album`, art from
/// the cache. `key` naming no known album (a stale/missing selection) projects a default, empty
/// header rather than panicking — the view simply has nothing to show until a real key arrives.
fn project_album_header(library: &Library, state: &AppState, key: &str) -> AlbumHeaderData {
    match library.album(key) {
        Some(album) => {
            let (art, has_art) = art_or_placeholder(state.artwork_for_key(&album.key));
            AlbumHeaderData {
                key: album.key.clone().into(),
                title: album.title.clone().into(),
                artist: album.artist.clone().into(),
                meta: format_album_meta(album.year, album.track_count, album.total_duration_ms).into(),
                art,
                has_art,
                format_label: album.format_label.clone().into(),
                format_variant: album.format_variant.clone().into(),
            }
        }
        None => AlbumHeaderData::default(),
    }
}

/// Songs/Recently Added rows (`§5.7`): `number` is the 1-based row index, not a disc-track label
/// (only album detail uses that), and each row's path is recorded in the same order for
/// `track-activated` (`§3.4`).
fn project_song_rows(records: &[&TrackRecord]) -> (Vec<TrackRowData>, Vec<TrackKey>) {
    let mut rows = Vec::with_capacity(records.len());
    let mut paths = Vec::with_capacity(records.len());
    for (index, record) in records.iter().enumerate() {
        rows.push(track_row_from_record(record, (index + 1).to_string()));
        paths.push(record.key.clone());
    }
    (rows, paths)
}

fn total_known_duration_ms(records: &[&TrackRecord]) -> u64 {
    records.iter().filter_map(|record| record.info.duration_ms).sum()
}

/// At most this many albums for Home's "Recently Added" shelf (`§6` Stage 6 step 2): the shelf's
/// own `AlbumGrid` only ever shows 2 rows, but this caps how many `AlbumCardData` Rust builds (and
/// how many art-cache lookups it does) regardless of library size.
const RECENT_ALBUMS_LIMIT: usize = 24;

/// The dedicated Search view's Songs section renders through the plain, non-virtualized
/// `TrackTable` (it shares one `ScrollView` with the Albums/Artists sections below it, so it
/// cannot own a `ListView` the way `VirtualizedTrackTable` does), so its row count must stay
/// bounded regardless of library size (`§3.3` "Search").
const SEARCH_SONGS_LIMIT: usize = 200;
/// The Search view's Albums section uses the same uncapped `AlbumGrid` as the Albums page, so this
/// bounds how many `AlbumCardData`/art-cache lookups a broad query builds (`§3.3` "Search").
const SEARCH_ALBUMS_LIMIT: usize = 48;

/// Every library-driven view's data in one pass (`§6` Stage 6 step 2, "batch projection"): albums,
/// artists and songs filtered by `query` (an empty query matches everything, `§3.3` "Search"),
/// their newest-first "Recently Added" variants, the "Jump back in" shelf, and the current
/// album-detail header/tracks when `album_key` names one. Each row list carries its own visible-
/// path list in the same order (`songs_paths`/`recent_paths`/`album_paths`), so `track-activated`
/// can map a clicked row back to `library.prepared(path)` without the view re-deriving it.
pub struct LibraryProjection {
    pub library_empty: bool,
    pub jump_back_albums: Vec<AlbumCardData>,
    pub recent_albums: Vec<AlbumCardData>,
    pub albums: Vec<AlbumCardData>,
    pub artists: Vec<ArtistRowData>,
    pub songs: Vec<TrackRowData>,
    pub songs_paths: Vec<TrackKey>,
    pub recent_songs: Vec<TrackRowData>,
    pub recent_paths: Vec<TrackKey>,
    pub songs_summary: String,
    pub recent_summary: String,
    pub album_header: AlbumHeaderData,
    pub album_tracks: Vec<TrackRowData>,
    pub album_paths: Vec<TrackKey>,
    pub search: SearchProjection,
}

/// The dedicated Search view's data (`§3.3` "Search"): Songs/Albums capped and headed with a
/// count (or a "Showing first N of M" note once the cap truncates them, never a silent drop —
/// `format_search_section_header`); Artists reuses the same uncapped, query-matched rows the
/// standalone Artists page shows (`LibraryProjection::artists`), so no separate field is needed
/// here beyond its own header. `has_results` is false only when all three sections are empty,
/// driving the view's "No results for ..." empty state.
pub struct SearchProjection {
    pub songs: Vec<TrackRowData>,
    pub songs_paths: Vec<TrackKey>,
    pub songs_header: String,
    pub albums: Vec<AlbumCardData>,
    pub albums_header: String,
    pub artists_header: String,
    pub has_results: bool,
}

pub fn project_library(state: &AppState, query: &str, artist_filter: Option<&str>, album_key: Option<&str>) -> LibraryProjection {
    let library = &state.library;

    let albums = library.albums(query, artist_filter).iter().map(|album| build_album_card(album, state)).collect();
    let recent_albums =
        library.albums_recent(query).iter().take(RECENT_ALBUMS_LIMIT).map(|album| build_album_card(album, state)).collect();
    let jump_back_albums = project_jump_back_albums(state, query);
    let artists: Vec<ArtistRowData> = library.artists(query).iter().map(build_artist_row).collect();

    let song_records = library.songs(query);
    let (songs, songs_paths) = project_song_rows(&song_records);
    let songs_summary = format_library_summary(song_records.len(), total_known_duration_ms(&song_records));

    let recent_records = library.songs_recent(query);
    let (recent_songs, recent_paths) = project_song_rows(&recent_records);
    let recent_summary = format_library_summary(recent_records.len(), total_known_duration_ms(&recent_records));

    let (album_header, album_tracks, album_paths) = match album_key {
        Some(key) => {
            let header = project_album_header(library, state, key);
            let (tracks, paths) = project_album_tracks(library, key);
            (header, tracks, paths)
        }
        None => (AlbumHeaderData::default(), Vec::new(), Vec::new()),
    };

    // The Search view ignores `artist_filter` deliberately: it always searches the whole library,
    // never scoped to whatever artist the Albums page happened to be filtered by when the user
    // started typing (`§3.3` "Search").
    let search_song_total = song_records.len();
    let (search_songs, search_songs_paths) = project_song_rows(&song_records[..song_records.len().min(SEARCH_SONGS_LIMIT)]);
    let search_songs_header = format_search_section_header("Songs", search_songs.len(), search_song_total);

    let search_album_summaries = library.albums(query, None);
    let search_album_total = search_album_summaries.len();
    let search_albums: Vec<AlbumCardData> =
        search_album_summaries.iter().take(SEARCH_ALBUMS_LIMIT).map(|album| build_album_card(album, state)).collect();
    let search_albums_header = format_search_section_header("Albums", search_albums.len(), search_album_total);

    let search_artists_header = format_search_section_header("Artists", artists.len(), artists.len());
    let search_has_results = search_song_total > 0 || search_album_total > 0 || !artists.is_empty();

    let search = SearchProjection {
        songs: search_songs,
        songs_paths: search_songs_paths,
        songs_header: search_songs_header,
        albums: search_albums,
        albums_header: search_albums_header,
        artists_header: search_artists_header,
        has_results: search_has_results,
    };

    LibraryProjection {
        library_empty: library.is_empty(),
        jump_back_albums,
        recent_albums,
        albums,
        artists,
        songs,
        songs_paths,
        recent_songs,
        recent_paths,
        songs_summary,
        recent_summary,
        album_header,
        album_tracks,
        album_paths,
        search,
    }
}

/// The suffix of `paths` starting at `row`, resolved into `PreparedTrack`s through `library`
/// (`§3.7`): a path the library has no record for anymore is skipped, never turned into a gap.
/// Shared by every `track-activated` row — Songs, Recently Added, album detail and Queue alike.
pub fn prepared_suffix(library: &Library, keys: &[TrackKey], row: usize) -> Vec<PreparedTrack> {
    keys.get(row..).unwrap_or(&[]).iter().filter_map(|key| library.prepared(key)).collect()
}

/// A minimal, test-doubleable view of the player calls an album/queue action needs
/// (`album_actions_map_to_player_calls`, `§6` Stage 6): implemented for `AudioPlayer` and, in
/// tests, by a call-recording double, so `perform_album_action` needs no real `AudioPlayer` (no
/// CoreAudio, no threads) to unit-test.
pub trait PlayerPort {
    fn enqueue(&self, tracks: Vec<PreparedTrack>);
    fn play_next(&self, tracks: Vec<PreparedTrack>);
    fn replace_queue(&self, tracks: Vec<PreparedTrack>);
}

impl PlayerPort for AudioPlayer {
    // Rust always prefers an inherent method over a trait method of the same name for a given
    // receiver, so each `self.<name>(..)` below resolves to `AudioPlayer`'s own inherent method,
    // not back into this trait — these cannot recurse.
    fn enqueue(&self, tracks: Vec<PreparedTrack>) {
        self.enqueue(tracks);
    }
    fn play_next(&self, tracks: Vec<PreparedTrack>) {
        self.play_next(tracks);
    }
    fn replace_queue(&self, tracks: Vec<PreparedTrack>) {
        self.replace_queue(tracks);
    }
}

/// Maps an `AlbumAction` (or Home's "Shuffle all", which reuses `AlbumAction::Shuffle`'s branch
/// through the same call) to the right `PlayerPort` call (`§3.7`). The UI never sends an empty
/// `ReplaceQueue`/`PlayNext`/enqueue, so an empty `tracks` list is a no-op for every action, not
/// just the ones the worker itself already ignores empty lists for.
pub fn perform_album_action(action: AlbumAction, tracks: Vec<PreparedTrack>, shuffle_seed: u64, player: &dyn PlayerPort) {
    if tracks.is_empty() {
        return;
    }
    match action {
        AlbumAction::Play => player.replace_queue(tracks),
        AlbumAction::Shuffle => player.replace_queue(shuffled(&tracks, shuffle_seed)),
        AlbumAction::AddToQueue => player.enqueue(tracks),
        AlbumAction::PlayNext => player.play_next(tracks),
    }
}

/// Queue's "Up Next" table renders every row directly, not virtualized (`§5.5`: "Album detail and
/// Queue use a plain `for`", since the table lives inside Queue's own page-level `ScrollView` and
/// is sized to its exact content — see `widgets.slint`'s `TrackTable` comment for why that rules
/// out `ListView`-based virtualization there). Left uncapped, "Shuffle all" or a double-click near
/// the top of Songs on a large library would instantiate one `TrackRow` per track and rebuild them
/// all on every `QueueSnapshot`. Capped here instead; the UI shows an "...and N more" row for the
/// remainder using the worker's own total (`queue-count`, unaffected by this cap).
const QUEUE_UP_NEXT_LIMIT: usize = 300;

/// Projects the Queue view's rows (`§5.7`, `§3.7`): the currently playing track's row (a default,
/// empty `TrackRowData` when nothing is playing — the caller's own `has-queue-current` tracks
/// visibility), the pending "Up Next" rows in queue order (capped to `QUEUE_UP_NEXT_LIMIT` — only
/// the rendered `TrackRowData`s, so a huge queue does not instantiate one row per track), and the
/// *full, uncapped* path list behind every pending track in the same order, for
/// `track-activated(TrackListKind.queue, i)` to map back to `replace_queue`. The path list must stay
/// uncapped even though the rows are capped: the cap only trims a prefix, so row `i` still maps to
/// `pending[i]` for every rendered row, but double-clicking a queue row is only ever reachable for
/// `i < pending.len()` regardless of the render cap, and activating it must resolve the entire
/// remaining suffix of the real queue, not just the rendered slice. `now_playing` is `(path, info,
/// fallback_title, fallback_album)`, the same fallback shape `now_playing_identity` uses. Replaces
/// `project_queue_snapshot`/`QueueViewRows`/`set_queue_models` (`§6` Stage 5 step 5).
pub fn project_queue_rows(
    now_playing: Option<(&TrackKey, &AudioInfo, &str, &str)>,
    pending: &[QueueTrackSnapshot],
    library: &Library,
) -> (TrackRowData, Vec<TrackRowData>, Vec<TrackKey>) {
    let rendered = &pending[..pending.len().min(QUEUE_UP_NEXT_LIMIT)];
    let current = match now_playing {
        Some((key, info, fallback_title, fallback_album)) => build_track_row(
            key,
            library,
            "1".to_owned(),
            RowFallback {
                title: fallback_title,
                artist: "Unknown Artist",
                album: fallback_album,
                year: "",
                format: &info.format,
                bits_per_sample: info.bits_per_sample,
                sample_rate: info.sample_rate,
                is_float: info.is_float,
                duration_ms: info.duration_ms,
            },
        ),
        None => TrackRowData::default(),
    };

    let mut rows = Vec::with_capacity(rendered.len());
    for (index, track) in rendered.iter().enumerate() {
        rows.push(build_track_row(
            &track.key,
            library,
            (index + 1).to_string(),
            RowFallback {
                title: &track.title,
                artist: "Unknown Artist",
                album: &track.parent_folder,
                year: "",
                format: &track.format,
                bits_per_sample: track.bits_per_sample,
                sample_rate: track.sample_rate,
                is_float: track.is_float,
                duration_ms: track.duration_ms,
            },
        ));
    }
    // Uncapped: `on_track_activated(Queue)` maps a clicked row back to a `replace_queue` call
    // through this exact list, so trimming it here would silently drop every pending track past
    // `QUEUE_UP_NEXT_LIMIT` from the resulting queue whenever a row inside the render cap is
    // activated.
    let paths: Vec<TrackKey> = pending.iter().map(|track| track.key.clone()).collect();
    (current, rows, paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::library::TrackRecord;

    fn key(path: &str) -> TrackKey {
        TrackKey::whole_file(PathBuf::from(path))
    }

    fn info(format: &str, bits: u32, rate: u32, is_float: bool, duration_ms: Option<u64>) -> AudioInfo {
        AudioInfo {
            sample_rate: rate,
            duration_ms,
            source_channels: 2,
            bits_per_sample: bits,
            is_float,
            integer_pcm: !is_float,
            format: format.to_owned(),
        }
    }

    #[test]
    fn queue_rows_projection_preserves_order_and_uses_library_or_snapshot_fallback() {
        let mut library = Library::new();
        let known_key = key("/nas/album-a/first-light.flac");
        let mut known = TrackRecord::minimal(known_key.clone(), info("FLAC", 24, 96_000, false, Some(95_000)));
        known.tags.title = Some("First Light".into());
        known.tags.artist = Some("Real Artist".into());
        known.tags.album = Some("Real Album".into());
        library.upsert(known);

        let pending = vec![
            QueueTrackSnapshot {
                key: known_key.clone(),
                title: "ignored".into(),
                parent_folder: "ignored".into(),
                format: "ignored".into(),
                sample_rate: 0,
                bits_per_sample: 0,
                is_float: false,
                duration_ms: None,
            },
            QueueTrackSnapshot {
                key: key("/nas/album-b/second-track.mp3"),
                title: "Second Track".into(),
                parent_folder: "Album B".into(),
                format: "MP3".into(),
                sample_rate: 44_100,
                bits_per_sample: 16,
                is_float: false,
                duration_ms: None,
            },
            QueueTrackSnapshot {
                key: key("/nas/album-c/third-track.wv"),
                title: "Third Track".into(),
                parent_folder: "Album C".into(),
                format: "WavPack".into(),
                sample_rate: 48_000,
                bits_per_sample: 32,
                is_float: true,
                duration_ms: None,
            },
        ];

        let (current, rows, paths) = project_queue_rows(None, &pending, &library);

        assert_eq!(current, TrackRowData::default(), "nothing playing means an empty current row");
        assert_eq!(
            paths,
            vec![known_key.clone(), key("/nas/album-b/second-track.mp3"), key("/nas/album-c/third-track.wv")],
            "order must be preserved"
        );
        assert_eq!(rows[0].key, track_key_string(&known_key), "key is always the absolute path");
        assert_eq!(rows[0].number, "1");
        assert_eq!(rows[0].title, "First Light", "a library record wins over the snapshot fields");
        assert_eq!(rows[0].artist, "Real Artist");
        assert_eq!(rows[0].badge, "FLAC 24/96");
        assert_eq!(rows[0].duration, "1:35");
        assert_eq!(rows[0].format_label, "FLAC", "a library-backed row's format pill comes from its own AudioInfo.format");
        assert_eq!(rows[0].format_variant, "hires", "FLAC 24/96 is hi-res, so its pill is gold");
        assert_eq!(rows[1].number, "2");
        assert_eq!(rows[1].title, "Second Track", "no library record: falls back to the snapshot title");
        assert_eq!(rows[1].album, "Album B", "no library record: falls back to the snapshot parent folder");
        assert_eq!(rows[1].artist, "Unknown Artist");
        assert_eq!(rows[1].duration, "\u{2014}:\u{2014}");
        assert_eq!(rows[1].format_label, "MP3", "a fallback row (no library record yet) still gets a format pill from the snapshot");
        assert_eq!(rows[1].format_variant, "mp3");
        assert_eq!(
            rows[2].badge, "WavPack 32f/48",
            "a pending row with no library record must still badge as float from the snapshot, not silently as integer"
        );
        assert_eq!(rows[2].format_label, "WavPack");
        assert_eq!(rows[2].format_variant, "wavpack");
    }

    #[test]
    fn queue_rows_projection_builds_the_current_row_from_now_playing_state() {
        let library = Library::new();
        let now_playing_key = key("/nas/album/track.flac");
        let now_playing_info = info("WavPack", 32, 48_000, true, Some(200_000));

        let (current, _, _) =
            project_queue_rows(Some((&now_playing_key, &now_playing_info, "Fallback Title", "Fallback Album")), &[], &library);

        assert_eq!(current.key, track_key_string(&now_playing_key));
        assert_eq!(current.title, "Fallback Title");
        assert_eq!(current.album, "Fallback Album");
        assert_eq!(current.badge, "WavPack 32f/48");
        assert_eq!(current.duration, "3:20");
        assert_eq!(current.format_label, "WavPack");
        assert_eq!(current.format_variant, "wavpack");
    }

    #[test]
    fn queue_rows_projection_caps_up_next_at_the_limit() {
        let library = Library::new();
        let pending: Vec<QueueTrackSnapshot> = (0..QUEUE_UP_NEXT_LIMIT + 50)
            .map(|i| QueueTrackSnapshot {
                key: key(&format!("/nas/queue/track-{i}.flac")),
                title: format!("Track {i}"),
                parent_folder: "Queue".into(),
                format: "FLAC".into(),
                sample_rate: 44_100,
                bits_per_sample: 16,
                is_float: false,
                duration_ms: Some(10_000),
            })
            .collect();

        let (_, rows, paths) = project_queue_rows(None, &pending, &library);

        assert_eq!(rows.len(), QUEUE_UP_NEXT_LIMIT, "Up Next must not instantiate a row per track on a huge queue");
        assert_eq!(
            paths.len(),
            pending.len(),
            "the activation path list must stay uncapped so activating a queue row never drops \
             pending tracks past the render cap"
        );
        assert_eq!(paths[0], pending[0].key, "the cap must keep the queue's own front-to-back order");
        for (index, track) in pending.iter().enumerate() {
            assert_eq!(paths[index], track.key, "path {index} must still map to the same pending track beyond the render cap");
        }
    }

    /// A minimal multi-track record built the same way `library::tests` does, so these Stage 6
    /// tests can set only the tags each one cares about.
    fn tagged_track(path: &str, format: &str, bits: u32, rate: u32, is_float: bool, duration_ms: Option<u64>) -> TrackRecord {
        TrackRecord::minimal(key(path), info(format, bits, rate, is_float, duration_ms))
    }

    #[test]
    fn track_rows_number_multi_disc_as_disc_dash_track() {
        let mut library = Library::new();
        let mut disc1_track1 = tagged_track("/music/Album/CD1/01.flac", "FLAC", 24, 44_100, false, Some(200_000));
        disc1_track1.tags.album = Some("Double Album".into());
        disc1_track1.tags.album_artist = Some("Band".into());
        disc1_track1.tags.disc_number = Some(1);
        disc1_track1.tags.track_number = Some(1);
        disc1_track1.tags.title = Some("Intro".into());
        let mut disc2_track3 = tagged_track("/music/Album/CD2/03.flac", "FLAC", 24, 44_100, false, Some(180_000));
        disc2_track3.tags.album = Some("Double Album".into());
        disc2_track3.tags.album_artist = Some("Band".into());
        disc2_track3.tags.disc_number = Some(2);
        disc2_track3.tags.track_number = Some(3);
        disc2_track3.tags.title = Some("Outro".into());
        let album_key = crate::library::album_key(&disc1_track1);
        library.upsert(disc1_track1);
        library.upsert(disc2_track3);

        let (rows, paths) = project_album_tracks(&library, &album_key);

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].number, "1-01", "disc 1 must still show the disc prefix once the album has more than one disc");
        assert_eq!(rows[1].number, "2-03");
        assert_eq!(paths, vec![key("/music/Album/CD1/01.flac"), key("/music/Album/CD2/03.flac")]);
    }

    #[test]
    fn track_rows_number_plain_track_for_a_single_disc_album() {
        let mut library = Library::new();
        let mut only = tagged_track("/music/Solo/01.flac", "FLAC", 16, 44_100, false, Some(200_000));
        only.tags.album = Some("Solo Album".into());
        only.tags.track_number = Some(1);
        let path = only.key.clone();
        library.upsert(only);
        // The library-resolved key: `only` carries no `ARTIST`/`ALBUMARTIST` at all, so `Library`
        // resolves its `effective_album_artist` from `display_artist`'s "Unknown Artist" fallback
        // (`EffectiveAlbumArtist`), which only the stored record reflects.
        let key = crate::library::album_key(library.get(&path).unwrap());

        let (rows, _) = project_album_tracks(&library, &key);

        assert_eq!(rows[0].number, "1", "a single-disc album must not show a disc prefix");
    }

    #[test]
    fn track_rows_number_falls_back_to_row_index_when_untagged() {
        // Folder-grouped WAV albums usually carry no `track_number` tag at all (`§6` Stage 6 fix).
        let mut library = Library::new();
        let mut first = tagged_track("/music/Folder/a.wav", "WAV", 16, 44_100, false, Some(200_000));
        first.tags.album = Some("Folder Album".into());
        let mut second = tagged_track("/music/Folder/b.wav", "WAV", 16, 44_100, false, Some(180_000));
        second.tags.album = Some("Folder Album".into());
        let path = first.key.clone();
        library.upsert(first);
        library.upsert(second);
        // Library-resolved key — see the identical comment on the single-disc test above.
        let key = crate::library::album_key(library.get(&path).unwrap());

        let (rows, _) = project_album_tracks(&library, &key);

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].number, "1", "an untagged single-disc row must show its 1-based position, not a blank cell");
        assert_eq!(rows[1].number, "2");
    }

    #[test]
    fn track_rows_number_falls_back_to_row_index_on_multi_disc_when_untagged() {
        let mut library = Library::new();
        let mut disc1 = tagged_track("/music/Album/CD1/a.flac", "FLAC", 24, 44_100, false, Some(200_000));
        disc1.tags.album = Some("Double Album".into());
        disc1.tags.album_artist = Some("Band".into());
        disc1.tags.disc_number = Some(1);
        // `disc2` has no `track_number` tag, so the old code rendered "2-00" for it.
        let mut disc2 = tagged_track("/music/Album/CD2/a.flac", "FLAC", 24, 44_100, false, Some(180_000));
        disc2.tags.album = Some("Double Album".into());
        disc2.tags.album_artist = Some("Band".into());
        disc2.tags.disc_number = Some(2);
        let key = crate::library::album_key(&disc1);
        library.upsert(disc1);
        library.upsert(disc2);

        let (rows, _) = project_album_tracks(&library, &key);

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].number, "2-02", "an untagged multi-disc track must fall back to its row index, not \"-00\"");
    }

    #[test]
    fn track_rows_project_a_per_track_format_pill_for_every_format_family() {
        let mut library = Library::new();
        let mut mp3_track = tagged_track("/music/Mixed/01.mp3", "MP3", 16, 44_100, false, Some(60_000));
        mp3_track.tags.album = Some("Mixed Bag".into());
        mp3_track.tags.track_number = Some(1);
        let mut flac_track = tagged_track("/music/Mixed/02.flac", "FLAC", 24, 96_000, false, Some(60_000));
        flac_track.tags.album = Some("Mixed Bag".into());
        flac_track.tags.track_number = Some(2);
        let mut wavpack_track = tagged_track("/music/Mixed/03.wv", "WavPack", 24, 96_000, false, Some(60_000));
        wavpack_track.tags.album = Some("Mixed Bag".into());
        wavpack_track.tags.track_number = Some(3);
        let mut wav_track = tagged_track("/music/Mixed/04.wav", "WAV", 16, 44_100, false, Some(60_000));
        wav_track.tags.album = Some("Mixed Bag".into());
        wav_track.tags.track_number = Some(4);
        let path = mp3_track.key.clone();
        library.upsert(mp3_track);
        library.upsert(flac_track);
        library.upsert(wavpack_track);
        library.upsert(wav_track);
        // Library-resolved key — see the identical comment on the single-disc test above.
        let album_key = crate::library::album_key(library.get(&path).unwrap());

        let (rows, _) = project_album_tracks(&library, &album_key);

        assert_eq!(rows.len(), 4);
        assert_eq!((rows[0].format_label.as_str(), rows[0].format_variant.as_str()), ("MP3", "mp3"));
        assert_eq!((rows[1].format_label.as_str(), rows[1].format_variant.as_str()), ("FLAC", "hires"));
        assert_eq!((rows[2].format_label.as_str(), rows[2].format_variant.as_str()), ("WavPack", "hires"));
        assert_eq!(
            (rows[3].format_label.as_str(), rows[3].format_variant.as_str()),
            ("WAV", "wav"),
            "a per-track pill never aggregates to Mix — it always reflects that one track's own format"
        );
    }

    #[test]
    fn album_cards_use_card_subtitle_and_placeholder_flag() {
        let mut state = AppState::new();
        // 12 tracks (`§6` Stage 6 spec: "2021 · 12 tracks"), not 1 — a singular/plural bug in
        // `format_album_card_subtitle` would not show up with a single track.
        for i in 0..12 {
            let mut record = tagged_track(&format!("/music/Album/{i:02}.flac"), "FLAC", 16, 44_100, false, Some(60_000));
            record.tags.album = Some("Warm Colors".into());
            record.tags.artist = Some("Nina".into());
            record.tags.year = Some(2021);
            state.library.upsert(record);
        }
        let albums = state.library.albums("", None);
        assert_eq!(albums.len(), 1);

        let card = build_album_card(&albums[0], &state);

        assert_eq!(card.subtitle, "2021 · 12 tracks");
        assert!(!card.has_art, "no cached artwork for this album key means the placeholder flag");
        assert_eq!(card.format_label, "FLAC", "every track is FLAC, so the pill must show a single format, not Mix Formats");
        assert_eq!(card.format_variant, "flac");
    }

    #[test]
    fn album_card_and_header_show_mix_formats_once_tracks_disagree() {
        let mut state = AppState::new();
        let mut flac_track = tagged_track("/music/Mixed/01.flac", "FLAC", 16, 44_100, false, Some(60_000));
        flac_track.tags.album = Some("Odds And Ends".into());
        flac_track.tags.artist = Some("Various".into());
        let mut mp3_track = tagged_track("/music/Mixed/02.mp3", "MP3", 16, 44_100, false, Some(60_000));
        mp3_track.tags.album = Some("Odds And Ends".into());
        mp3_track.tags.artist = Some("Various".into());
        state.library.upsert(flac_track);
        state.library.upsert(mp3_track);

        let albums = state.library.albums("", None);
        assert_eq!(albums.len(), 1);
        let card = build_album_card(&albums[0], &state);
        assert_eq!(card.format_label, "Mix Formats");
        assert_eq!(card.format_variant, "mix");

        let header = project_album_header(&state.library, &state, &albums[0].key);
        assert_eq!(header.format_label, "Mix Formats");
        assert_eq!(header.format_variant, "mix");
    }

    #[test]
    fn album_header_pill_is_empty_for_a_missing_album() {
        let state = AppState::new();
        let header = project_album_header(&state.library, &state, "no-such-key");
        assert_eq!(header.format_label, "", "a missing album key must project the default, empty header");
        assert_eq!(header.format_variant, "");
    }

    #[test]
    fn track_activation_maps_visible_row_to_suffix_of_prepared_tracks() {
        let mut state = AppState::new();
        let mut a = tagged_track("/music/a.flac", "FLAC", 16, 44_100, false, Some(10_000));
        a.tags.title = Some("Warm A".into());
        let mut b = tagged_track("/music/b.flac", "FLAC", 16, 44_100, false, Some(10_000));
        b.tags.title = Some("Warm B".into());
        let mut c = tagged_track("/music/c.flac", "FLAC", 16, 44_100, false, Some(10_000));
        c.tags.title = Some("Warm C".into());
        let mut excluded = tagged_track("/music/excluded.flac", "FLAC", 16, 44_100, false, Some(10_000));
        excluded.tags.title = Some("Cold".into());
        state.library.upsert(a);
        state.library.upsert(b);
        state.library.upsert(c);
        state.library.upsert(excluded);

        // A filtered list, per the spec (`§6` Stage 6 "including a filtered list"): the projection
        // must keep `songs`/`songs_paths` index-aligned under a search query, since `track-activated`
        // maps a clicked row straight to `songs_paths[row]` with no other lookup.
        let projection = project_library(&state, "warm", None, None);
        assert_eq!(projection.songs.len(), 3, "the query must exclude the non-matching track");
        for (i, key) in projection.songs_paths.iter().enumerate() {
            assert_eq!(
                projection.songs[i].key,
                track_key_string(key),
                "songs and songs_paths must stay index-aligned under a search filter"
            );
        }

        // A path without a record: simulate the row at index 0 leaving the library between render
        // and click (`§3.7`) with a fresh library that has every visible path except that one.
        let mut library_after_departure = Library::new();
        for key in &projection.songs_paths[1..] {
            library_after_departure.upsert(state.library.get(key).unwrap().clone());
        }

        let tracks = prepared_suffix(&library_after_departure, &projection.songs_paths, 0);

        assert_eq!(tracks.len(), 2, "the path with no library record anymore is skipped, not turned into a gap");
        assert_eq!(tracks[0].key, projection.songs_paths[1]);
        assert_eq!(tracks[1].key, projection.songs_paths[2]);
    }

    // Records the full track list, not just its length (`§6` Stage 6 fix): a call that keeps the
    // right count but reorders tracks, or a Shuffle that forgets to shuffle, used to still pass this
    // test with only 2 tracks (any permutation of 2 elements is either the identity or a full
    // reversal, both plausible "correct-looking" outputs by accident).
    #[derive(Default)]
    struct RecordingPlayer {
        calls: std::cell::RefCell<Vec<(&'static str, Vec<PreparedTrack>)>>,
    }
    impl PlayerPort for RecordingPlayer {
        fn enqueue(&self, tracks: Vec<PreparedTrack>) {
            self.calls.borrow_mut().push(("enqueue", tracks));
        }
        fn play_next(&self, tracks: Vec<PreparedTrack>) {
            self.calls.borrow_mut().push(("play_next", tracks));
        }
        fn replace_queue(&self, tracks: Vec<PreparedTrack>) {
            self.calls.borrow_mut().push(("replace_queue", tracks));
        }
    }

    #[test]
    fn album_actions_map_to_player_calls() {
        let track = |name: &str| PreparedTrack { key: key(name), info: info("FLAC", 16, 44_100, false, Some(10_000)) };
        // 5 tracks: enough that an accidentally-unshuffled or reordered list is not plausibly
        // mistaken for a correct shuffle/album order by chance.
        let tracks: Vec<PreparedTrack> =
            ["a", "b", "c", "d", "e"].iter().map(|name| track(&format!("/music/{name}.flac"))).collect();
        let seed = 1;
        let player = RecordingPlayer::default();

        perform_album_action(AlbumAction::Play, tracks.clone(), seed, &player);
        perform_album_action(AlbumAction::AddToQueue, tracks.clone(), seed, &player);
        perform_album_action(AlbumAction::PlayNext, tracks.clone(), seed, &player);
        perform_album_action(AlbumAction::Shuffle, tracks.clone(), seed, &player);
        perform_album_action(AlbumAction::Play, Vec::new(), seed, &player);

        let calls = player.calls.borrow();
        assert_eq!(calls.len(), 4, "an empty track list (the last call above) must be a no-op for every action");
        assert_eq!(calls[0], ("replace_queue", tracks.clone()), "Play must keep album order");
        assert_eq!(calls[1], ("enqueue", tracks.clone()), "Add to Queue must keep album order");
        assert_eq!(calls[2], ("play_next", tracks.clone()), "Play Next must keep album order");
        assert_eq!(calls[3], ("replace_queue", shuffled(&tracks, seed)), "Shuffle must send the seeded shuffle, not album order");
        assert_ne!(calls[3].1, tracks, "the seeded shuffle of 5 tracks must not land back on album order");
    }

    #[test]
    fn jump_back_in_dedupes_and_limits() {
        let mut state = AppState::new();
        for i in 0..14 {
            let path = format!("/music/album-{i}/track.flac");
            let mut record = tagged_track(&path, "FLAC", 16, 44_100, false, Some(10_000));
            record.tags.album = Some(format!("Album {i}"));
            record.tags.artist = Some("Artist".into());
            state.library.upsert(record);
            state.note_played_key(&key(&path));
        }
        state.note_played_key(&key("/music/album-5/track.flac"));

        let jump_back = project_jump_back_albums(&state, "");

        assert_eq!(jump_back.len(), 12, "capped at 12, same as the underlying play history");
        assert_eq!(jump_back[0].title, "Album 5", "a re-played album moves back to the front");
        assert!(jump_back[1..].iter().all(|card| card.title != "Album 5"), "no duplicate entry is kept");
    }

    #[test]
    fn jump_back_in_is_filtered_by_the_search_query() {
        let mut state = AppState::new();
        let mut jazz = tagged_track("/music/jazz/track.flac", "FLAC", 16, 44_100, false, Some(10_000));
        jazz.tags.album = Some("Kind of Blue".into());
        jazz.tags.artist = Some("Miles Davis".into());
        state.library.upsert(jazz);
        state.note_played_key(&key("/music/jazz/track.flac"));

        let mut rock = tagged_track("/music/rock/track.flac", "FLAC", 16, 44_100, false, Some(10_000));
        rock.tags.album = Some("Nevermind".into());
        rock.tags.artist = Some("Nirvana".into());
        state.library.upsert(rock);
        state.note_played_key(&key("/music/rock/track.flac"));

        assert_eq!(project_jump_back_albums(&state, "").len(), 2, "no query keeps every played album");

        let filtered = project_jump_back_albums(&state, "blue");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].title, "Kind of Blue");

        assert!(
            project_jump_back_albums(&state, "no such album").is_empty(),
            "a query matching nothing must empty the shelf instead of showing every played album"
        );
    }

    #[test]
    fn search_filters_albums_songs_artists() {
        let mut state = AppState::new();
        let mut matching = tagged_track("/music/a.flac", "FLAC", 16, 44_100, false, Some(10_000));
        matching.tags.title = Some("Sunrise".into());
        matching.tags.artist = Some("Nina Path".into());
        matching.tags.album = Some("Warm Colors".into());
        let mut other = tagged_track("/music/b.flac", "FLAC", 16, 44_100, false, Some(10_000));
        other.tags.title = Some("Nightfall".into());
        other.tags.artist = Some("Other".into());
        other.tags.album = Some("Cold Shapes".into());
        state.library.upsert(matching);
        state.library.upsert(other);

        let projection = project_library(&state, "nina", None, None);

        assert_eq!(projection.albums.len(), 1);
        assert_eq!(projection.albums[0].title, "Warm Colors");
        assert_eq!(projection.songs.len(), 1);
        assert_eq!(projection.songs[0].title, "Sunrise");
        assert_eq!(projection.artists.len(), 1);
        assert_eq!(projection.artists[0].name, "Nina Path");
    }

    #[test]
    fn search_projection_sections_report_counts_and_ignore_the_artist_filter() {
        let mut state = AppState::new();
        let mut matching = tagged_track("/music/a.flac", "FLAC", 16, 44_100, false, Some(10_000));
        matching.tags.title = Some("Sunrise".into());
        matching.tags.artist = Some("Nina Path".into());
        matching.tags.album = Some("Warm Colors".into());
        let mut other = tagged_track("/music/b.flac", "FLAC", 16, 44_100, false, Some(10_000));
        other.tags.title = Some("Nightfall".into());
        other.tags.artist = Some("Other".into());
        other.tags.album = Some("Cold Shapes".into());
        state.library.upsert(matching);
        state.library.upsert(other);

        // An `artist_filter` (as if the Albums page were still filtered to some other artist when
        // typing started) must not narrow the Search view: it always searches the whole library.
        let projection = project_library(&state, "nina", Some("Other"), None);

        assert_eq!(projection.search.songs.len(), 1);
        assert_eq!(projection.search.songs[0].title, "Sunrise");
        assert_eq!(projection.search.songs_header, "Songs (1)");
        assert_eq!(projection.search.albums.len(), 1);
        assert_eq!(projection.search.albums[0].title, "Warm Colors");
        assert_eq!(projection.search.albums_header, "Albums (1)");
        assert_eq!(projection.search.artists_header, "Artists (1)");
        assert!(projection.search.has_results);

        let empty = project_library(&state, "no such query", None, None);
        assert!(!empty.search.has_results, "no matches in any section must report no results");
        assert_eq!(empty.search.songs_header, "Songs (0)");
    }

    #[test]
    fn search_projection_songs_are_capped_with_a_showing_first_header_and_index_aligned_paths() {
        let mut state = AppState::new();
        for i in 0..(SEARCH_SONGS_LIMIT + 5) {
            let mut record = tagged_track(&format!("/music/track-{i:03}.flac"), "FLAC", 16, 44_100, false, Some(10_000));
            record.tags.title = Some(format!("Track {i:03}"));
            record.tags.artist = Some("Matching Artist".into());
            state.library.upsert(record);
        }

        let projection = project_library(&state, "matching", None, None);

        assert_eq!(projection.search.songs.len(), SEARCH_SONGS_LIMIT, "the rendered rows must stay capped");
        assert_eq!(
            projection.search.songs_header,
            format!("Songs \u{2014} Showing first {SEARCH_SONGS_LIMIT} of {}", SEARCH_SONGS_LIMIT + 5),
            "a capped section must say so, never truncate silently"
        );
        assert_eq!(projection.search.songs.len(), projection.search.songs_paths.len(), "rows and paths must stay index-aligned");
        for (row, key) in projection.search.songs.iter().zip(&projection.search.songs_paths) {
            assert_eq!(row.key, track_key_string(key), "row `key` and its path entry must refer to the same track");
        }
    }

    #[test]
    fn songs_summary_matches_visible_rows() {
        let mut state = AppState::new();
        let mut a = tagged_track("/music/a.flac", "FLAC", 16, 44_100, false, Some(60_000));
        a.tags.title = Some("Sunrise".into());
        a.tags.album = Some("Warm Colors".into());
        let mut b = tagged_track("/music/b.flac", "FLAC", 16, 44_100, false, Some(120_000));
        b.tags.title = Some("Sunset".into());
        b.tags.album = Some("Warm Colors".into());
        // A much longer, non-matching track: if the summary ever summed the whole library instead
        // of the filtered rows it displays, this would change the expected total below.
        let mut excluded = tagged_track("/music/cold/c.flac", "FLAC", 16, 44_100, false, Some(10_000_000));
        excluded.tags.title = Some("Unrelated".into());
        excluded.tags.album = Some("Cold Shapes".into());
        state.library.upsert(a);
        state.library.upsert(b);
        state.library.upsert(excluded);

        let projection = project_library(&state, "warm", None, None);

        assert_eq!(projection.songs_summary, "2 songs · 3 min", "the summary must match the query-filtered rows, not the whole library");
        assert_eq!(
            projection.recent_summary, "2 songs · 3 min",
            "Recently Added's summary must also match its own query-filtered rows"
        );
    }
}

#[cfg(test)]
mod ui_contract {
    const APP_SLINT: &str = include_str!("../ui/app.slint");
    const THEME_SLINT: &str = include_str!("../ui/theme.slint");
    const WIDGETS_SLINT: &str = include_str!("../ui/widgets.slint");
    const ALL_SOURCES: [&str; 3] = [APP_SLINT, THEME_SLINT, WIDGETS_SLINT];

    /// The text between `component {name}` and the next `component ` keyword in `source`, so a
    /// test can assert on one component's own body without matching text from a sibling.
    fn extract_component<'a>(source: &'a str, name: &str) -> &'a str {
        let marker = format!("component {name}");
        let start = source.find(&marker).unwrap_or_else(|| panic!("`component {name}` not found"));
        let after = &source[start..];
        let body = &after[marker.len()..];
        let end = body.find("component ").map(|pos| marker.len() + pos).unwrap_or(after.len());
        &after[..end]
    }

    /// `source` without its `//` comments (no `.slint` file holds a `//` inside a string), so a test
    /// can assert something is absent from the code even when a comment explains why it is.
    fn without_comments(source: &str) -> String {
        source.lines().map(|line| line.split("//").next().unwrap_or("")).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn slint_sources_contain_no_mock_catalog_data() {
        let banned = [
            "Afterglow",
            "The Still Hours",
            "Prototype UI",
            "\"Sample\"",
            "prototype",
            "Mira North",
            "Paper Skies",
            "First Light",
            "Lyrics preview",
            "Local or network audio",
        ];
        for source in ALL_SOURCES {
            for needle in banned {
                assert!(!source.contains(needle), "found banned mock text {needle:?}");
            }
        }
    }

    #[test]
    fn slint_sources_use_svg_icons_not_symbol_glyphs() {
        let allowed = ['\u{00B7}', '\u{2026}', '\u{2014}', '\u{201C}', '\u{201D}'];
        for source in ALL_SOURCES {
            for ch in source.chars() {
                let code = ch as u32;
                let in_banned_range =
                    (0x2100..=0x2BFF).contains(&code) || (0xFF00..=0xFFEF).contains(&code) || (0x1F000..=0x1FAFF).contains(&code);
                assert!(!in_banned_range || allowed.contains(&ch), "found a symbol glyph {ch:?} (U+{code:04X}) instead of an SVG icon");
            }
        }
    }

    #[test]
    fn every_icon_reference_exists_on_disk() {
        let ui_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui");
        let marker = "@image-url(\"";
        let mut found_any = false;
        for source in ALL_SOURCES {
            let mut rest = source;
            while let Some(start) = rest.find(marker) {
                let after = &rest[start + marker.len()..];
                let end = after.find('"').expect("unterminated @image-url(\"...\") literal");
                let path = &after[..end];
                let resolved = ui_dir.join(path);
                assert!(resolved.exists(), "icon referenced but missing on disk: {path}");
                found_any = true;
                rest = &after[end + 1..];
            }
        }
        assert!(found_any, "expected at least one @image-url(...) reference");
        assert!(ui_dir.join("icons/LICENSE-lucide.txt").exists(), "the Lucide license must ship with the stroke icons");
    }

    #[test]
    fn typography_tokens_are_exact_and_used_everywhere() {
        for (token, value) in [
            ("fs-caption", "10px"),
            ("fs-label", "11px"),
            ("fs-meta", "12px"),
            ("fs-body", "13px"),
            ("fs-title", "15px"),
            ("fs-subtitle", "16px"),
            ("fs-panel-title", "17px"),
            ("fs-section", "20px"),
            ("fs-hero", "30px"),
        ] {
            let needle = format!("{token}: {value}");
            assert!(THEME_SLINT.contains(&needle), "theme.slint must declare `{needle}`");
        }
        for source in [APP_SLINT, WIDGETS_SLINT] {
            let mut rest = source;
            while let Some(pos) = rest.find("font-size:") {
                let after = rest[pos + "font-size:".len()..].trim_start();
                let starts_with_digit = after.chars().next().is_some_and(|c| c.is_ascii_digit());
                assert!(!starts_with_digit, "a literal font-size was found outside theme.slint (must use a Theme.fs-* token)");
                rest = &rest[pos + "font-size:".len()..];
            }
        }
    }

    #[test]
    fn album_format_pill_tokens_are_declared_and_referenced() {
        // `Theme` tokens the format pill added (`§5.5`/`§5.7`): each must be declared once in
        // `theme.slint` and actually consumed somewhere in `widgets.slint`/`app.slint` — no dead
        // token, and (implicitly, by never needing a hex literal here) no hex color introduced
        // outside `theme.slint` for the pill itself.
        for token in [
            "flac-soft", "flac-strong", "hires-soft", "hires-strong", "mix-soft", "mix-strong", "wav-soft", "wav-strong",
        ] {
            let declaration_needle = format!("out property <color> {token}:");
            assert!(THEME_SLINT.contains(&declaration_needle), "theme.slint must declare `{token}`");
            let usage_needle = format!("Theme.{token}");
            let used_somewhere = [WIDGETS_SLINT, APP_SLINT].iter().any(|source| source.contains(&usage_needle));
            assert!(used_somewhere, "`{token}` is declared but never referenced in widgets.slint/app.slint");
        }
    }

    #[test]
    fn player_bar_is_72px_centered_and_uses_left_anchored_scrubbers() {
        assert!(THEME_SLINT.contains("playerbar-h: 72px"));
        let player_bar = extract_component(APP_SLINT, "PlayerBar");
        assert!(player_bar.contains("cross-axis-alignment: center;"));
        assert!(APP_SLINT.contains("height: Theme.playerbar-h;"));
        let scrubber = extract_component(WIDGETS_SLINT, "Scrubber");
        assert!(scrubber.contains("x: 0; y: (root.height - 4px) / 2; width: root.width * root.shown;"));
        assert!(scrubber.contains("width: 18px; height: 12px; border-radius: 6px;"));
        assert!(APP_SLINT.contains("released(f) => { root.seek-requested(f); }"));
        assert!(APP_SLINT.contains("value: root.playback-progress;"));
    }

    /// `NowPlayingPanel` is invisible below `Theme.panel-threshold` or whenever the user hides it
    /// (`§6`), which would otherwise hide decoder errors, output-format mismatches, "Seek
    /// failed…", and — the hard `§1.4`/`§4.6.4` invariant — buffer underrun reports. `PlayerBar`
    /// must carry its own copy so the status line stays reachable at every allowed window width.
    #[test]
    fn playback_status_is_bound_outside_now_playing_panel() {
        let player_bar = extract_component(APP_SLINT, "PlayerBar");
        assert!(
            player_bar.contains("text: root.playback-status;"),
            "PlayerBar must bind playback-status too, not only NowPlayingPanel"
        );
    }

    #[test]
    fn icon_text_rows_are_vertically_centered() {
        for name in ["NavItem", "TrackRow", "ActionButton"] {
            let block = extract_component(WIDGETS_SLINT, name);
            assert!(block.contains("cross-axis-alignment: center"), "{name} must center its icon+text row");
        }
        let top_bar = extract_component(APP_SLINT, "TopBar");
        assert!(top_bar.contains("cross-axis-alignment: center"));
    }

    #[test]
    fn queue_view_contract() {
        assert!(APP_SLINT.contains("track-activated(TrackListKind.queue"));
        assert!(APP_SLINT.contains("No tracks are waiting in the queue."));
    }

    #[test]
    fn volume_scrubber_keeps_a_disabled_state_and_a_visible_explanation() {
        let player_bar = extract_component(APP_SLINT, "PlayerBar");
        assert!(player_bar.contains("enabled: root.volume-available;"), "the volume Scrubber must disable itself, not just fade");
        assert!(APP_SLINT.contains("Text { text: root.volume-detail;"), "the right panel must render the explanation text");
        // The right panel's own copy of this text is hidden below `Theme.panel-threshold` and
        // whenever the panel is toggled off, so the player bar itself must carry a visible
        // explanation too (`§5.6` item 14): a search over the whole file would also match the
        // right-panel `Text` above, so this must look inside `PlayerBar`'s own block specifically.
        assert!(
            player_bar.contains("Tooltip { Text { text: root.volume-detail;"),
            "PlayerBar must render a Tooltip bound to volume-detail on the volume icon itself"
        );
    }

    /// `output-popup` (`PlayerBar`, narrow-window Output/Hog mode) must not use the default
    /// `close-on-click` policy: a `ComboBox` inside it opens its own dropdown on the same
    /// click/release that would otherwise close this popup first, dropping the popup's component
    /// and orphaning the dropdown before a selection can ever reach it (Slint 1.18.1 closes the
    /// popup in the same `process_mouse_input` pass that dispatched the click, see
    /// `i-slint-core-1.18.1/window.rs`). `close-on-click-outside` still dismisses the popup on an
    /// outside click but leaves its own contents alone.
    #[test]
    fn output_popup_does_not_close_on_click_inside_itself() {
        let player_bar = extract_component(APP_SLINT, "PlayerBar");
        let popup_start = player_bar.find("output-popup := PopupWindow").expect("PlayerBar must declare output-popup");
        let popup = &player_bar[popup_start..];
        assert!(
            popup.contains("close-policy: close-on-click-outside;"),
            "output-popup must not use the default close-on-click policy, which breaks its own ComboBox"
        );
    }

    /// `§6` Stage 5b (unified title bar): `Sidebar` reserves its top 52 px for the traffic lights
    /// and `TopBar` shows the window title, both gated on `unified-title-bar`; both also carry a
    /// drag `TouchArea` over that empty space, and `TopBar`'s doubles as a zoom trigger, matching
    /// a native macOS title bar. `main.rs` only ever sets `unified-title-bar` to `true` on macOS
    /// (every other platform keeps the default `false`, so none of this reserves space there).
    /// The full-height right panel (`§6` layout pass) also forwards it, so its own top content
    /// gets the matching top clearance instead of sitting flush against the window's top edge.
    #[test]
    fn unified_title_bar_reserves_traffic_light_space_and_is_draggable() {
        let sidebar = extract_component(APP_SLINT, "Sidebar");
        assert!(
            sidebar.contains("padding-top: root.unified-title-bar ? 52px : 12px;"),
            "Sidebar must reserve exactly 52px for the traffic lights in unified mode"
        );
        assert!(
            sidebar.contains("callback window-drag-requested();"),
            "Sidebar must expose its own drag callback for the reserved top strip"
        );
        assert!(
            sidebar.contains("if root.unified-title-bar: TouchArea"),
            "Sidebar's drag TouchArea must only exist in unified mode"
        );

        let top_bar = extract_component(APP_SLINT, "TopBar");
        assert!(
            !top_bar.contains("\"Lime Player\""),
            "TopBar must not repeat the app name; the unified title bar shows no title text"
        );
        assert!(
            top_bar.contains("callback window-drag-requested();") && top_bar.contains("callback window-zoom-requested();"),
            "TopBar must expose both a drag and a zoom callback"
        );
        assert!(
            top_bar.contains("double-clicked => { root.window-zoom-requested(); }"),
            "TopBar's drag TouchArea must also zoom on a double click, like a native title bar"
        );

        let main_window = extract_component(APP_SLINT, "MainWindow");
        assert!(main_window.contains("in property <bool> unified-title-bar: false;"), "unified-title-bar must default to false");
        assert!(main_window.contains("callback window-drag-requested();"));
        assert!(main_window.contains("callback window-zoom-requested();"));
        let unified_bindings = main_window.matches("unified-title-bar: root.unified-title-bar;").count();
        assert_eq!(unified_bindings, 3, "Sidebar, TopBar and NowPlayingPanel must all forward MainWindow's unified-title-bar");
    }

    /// "Remove from Library" (`CLAUDE.md` "Library exclusions"): the album card's three-dot menu
    /// must use the dedicated `Theme.danger` token (declared and referenced, like the format-pill
    /// tokens above) for both its trash icon and its label, its own popup must not use the default
    /// `close-on-click` policy (same `ComboBox`-inside-a-popup trap as `output-popup`), and its
    /// click handler must be scoped to a TouchArea distinct from the whole-card one so opening the
    /// menu never also opens the album.
    #[test]
    fn remove_from_library_menu_contract() {
        let declaration_needle = "out property <color> danger:";
        assert!(THEME_SLINT.contains(declaration_needle), "theme.slint must declare `danger`");
        assert!(WIDGETS_SLINT.contains("Theme.danger"), "`danger` is declared but never referenced in widgets.slint");

        let album_tile = extract_component(WIDGETS_SLINT, "AlbumTile");
        assert!(album_tile.contains("Icons.more-vertical"), "AlbumTile must render the three-dot Icons.more-vertical icon");
        assert!(album_tile.contains("Icons.trash"), "AlbumTile's menu item must render Icons.trash");
        assert!(album_tile.contains("\"Remove from Library\""), "AlbumTile's menu must offer exactly \"Remove from Library\"");
        assert!(
            album_tile.contains("close-policy: close-on-click-outside;"),
            "AlbumTile's menu popup must not use the default close-on-click policy"
        );
        assert!(
            album_tile.contains("menu-touch := TouchArea"),
            "the three-dot button must have its own scoped TouchArea, distinct from the card's own"
        );
    }

    /// `PlayerBar`'s `output-popup` `ComboBox` and `NowPlayingPanel`'s own `ComboBox` both end up
    /// two-way bound to the same `MainWindow.selected-output-index` storage (chained `<=>`
    /// bindings), and Rust writes that index programmatically both at startup and on every
    /// `PlaybackEvent::Devices` refresh (`main.rs`). A `changed current-index` handler fires on
    /// those programmatic writes too, so round 1 guarded it on which control was currently visible
    /// — but that made whether a Rust-side index write sent `output-chosen` (and therefore
    /// `Command::SelectOutput`, a preference save/clear, and a possible `start_next`) depend on
    /// panel visibility/window width, which is wrong: e.g. the saved DAC being unplugged resets the
    /// index to 0 through `Devices`, and only a visible panel/open popup would then clear the saved
    /// preference and overwrite the "unavailable" status. `ComboBoxBase.selected(value)` fires only
    /// from `ComboBoxBase.select()` (a user click or arrow-key move), never from a plain
    /// `current-index` write, so both `ComboBox`es use it instead and no visibility guard is needed
    /// — each user selection still sends exactly one `output-chosen`, and Rust-side writes send
    /// none.
    #[test]
    fn output_selection_fires_output_chosen_exactly_once() {
        let player_bar = extract_component(APP_SLINT, "PlayerBar");
        let popup_start = player_bar.find("output-popup := PopupWindow").expect("PlayerBar must declare output-popup");
        let popup_combo = &player_bar[popup_start..];
        assert!(
            popup_combo.contains("selected(value) => {\n                                root.output-chosen(root.selected-output-index);\n                            }"),
            "PlayerBar's output-popup ComboBox must send output-chosen from the `selected` callback, not `changed current-index`"
        );
        assert!(
            !popup_combo.contains("changed current-index =>"),
            "PlayerBar's output-popup ComboBox must not use `changed current-index`, which also fires on Rust's programmatic index writes"
        );

        let now_playing_panel = extract_component(APP_SLINT, "NowPlayingPanel");
        assert!(
            now_playing_panel.contains("selected(value) => {\n                    root.output-chosen(root.selected-output-index);\n                }"),
            "NowPlayingPanel's own ComboBox must send output-chosen from the `selected` callback, not `changed current-index`"
        );
        assert!(
            !now_playing_panel.contains("changed current-index =>"),
            "NowPlayingPanel's own ComboBox must not use `changed current-index`, which also fires on Rust's programmatic index writes"
        );
    }

    /// The "now playing" indicator in `TrackRow`'s "#" column is an animated version of the app
    /// icon's bars (`PlayingBars`), not the old static waveform glyph (`Icons.now-playing`,
    /// `ui/icons/now-playing.svg` — removed once this landed, since nothing else referenced it).
    #[test]
    fn now_playing_indicator_uses_playing_bars_not_the_old_glyph() {
        assert!(
            !WIDGETS_SLINT.contains("Icons.now-playing") && !THEME_SLINT.contains("now-playing.svg"),
            "the old now-playing glyph icon must be gone from widgets.slint/theme.slint"
        );
        assert!(
            !std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("ui/icons/now-playing.svg").exists(),
            "ui/icons/now-playing.svg must be removed now that nothing references it"
        );
        assert!(WIDGETS_SLINT.contains("component PlayingBars"), "widgets.slint must declare PlayingBars");
        let track_row = extract_component(WIDGETS_SLINT, "TrackRow");
        assert!(
            track_row.contains("if root.is-playing: PlayingBars"),
            "TrackRow must render PlayingBars (not an Icon) for its current-track row"
        );
        assert!(
            track_row.contains("animating: root.playing;"),
            "TrackRow must drive PlayingBars.animating from real transport state, not just current-track status"
        );
    }
    /// Space (`CLAUDE.md` "Media controls" -> "Spacebar"): a window-wide `FocusScope` that is an
    /// ancestor of everything focusable (so a Space a focused control rejects still reaches it), given
    /// the initial focus by `forward-focus` (nothing has focus at startup, and key events only travel
    /// the focus item's ancestor chain). It must use the bubbling `key-pressed`: a `capture-key-pressed`
    /// runs before the focused search field and would swallow every space typed into it.
    #[test]
    fn window_wide_key_scope_handles_space_without_stealing_it_from_the_search_field() {
        let main_window = extract_component(APP_SLINT, "MainWindow");
        assert!(main_window.contains("forward-focus: key-root;"), "MainWindow must forward the initial focus to key-root");
        assert!(main_window.contains("callback space-pressed();"), "MainWindow must expose space-pressed for main.rs");
        let scope_start = main_window.find("key-root := FocusScope").expect("MainWindow must declare `key-root := FocusScope`");
        let scope = &main_window[scope_start..];
        let handler_start = scope.find("key-pressed(event) =>").expect("key-root must handle the bubbling key-pressed");
        let layout_start = scope.find("VerticalLayout {").expect("key-root must wrap the window's layout");
        assert!(handler_start < layout_start, "the key handler is declared before the wrapped layout");
        let handler = &scope[handler_start..layout_start];
        assert!(handler.contains("event.text == Key.Space"), "the handler must match Space");
        assert!(handler.contains("!event.repeat"), "holding Space must not flip play/pause on every auto-repeat");
        for modifier in ["control", "meta", "alt"] {
            assert!(handler.contains(&format!("!event.modifiers.{modifier}")), "Space with {modifier} is a shortcut, not playback");
        }
        assert!(handler.contains("root.space-pressed();") && handler.contains("return accept;") && handler.contains("return reject;"));
        for wrapped in ["Sidebar {", "TopBar {", "content-area := Rectangle", "NowPlayingPanel {", "PlayerBar {"] {
            assert!(scope.contains(wrapped), "`{wrapped}` must live inside key-root so its key events bubble to it");
        }
        assert!(
            !without_comments(APP_SLINT).contains("capture-key-pressed") && !without_comments(WIDGETS_SLINT).contains("capture-key-pressed"),
            "a capture handler would steal Space from the search field's TextInput"
        );
    }

    /// The player bar's now-playing block and the panel's cover both open the playing track's album.
    /// The zone is one `TouchArea` around cover, title, artist and badge (the heart/transport buttons
    /// are siblings outside it), gated by `now-playing-album-available`, and Rust resolves the album at
    /// click time: no album key string lives on the Slint side.
    #[test]
    fn now_playing_zones_open_the_playing_tracks_album() {
        let player_bar = extract_component(APP_SLINT, "PlayerBar");
        let zone_start = player_bar.find("clicked => { root.now-playing-album-requested(); }").expect("PlayerBar requests the album on a click");
        let heart_start = player_bar.find("IconButton { source: Icons.heart;").expect("PlayerBar still has the heart button");
        assert!(zone_start < heart_start, "the now-playing zone must come before, and not wrap, the heart button");
        let zone = &player_bar[..heart_start];
        assert!(zone.contains("enabled: root.now-playing-album-available;"), "a click only acts while an album is available");
        assert!(zone.contains("MouseCursor.pointer"), "the pointer cursor is the affordance");
        assert!(
            zone.contains("ArtworkView") && zone.contains("root.now-playing-title") && zone.contains("root.now-playing-artist"),
            "cover, title and artist are inside the one zone"
        );
        assert!(
            zone.contains("FormatPill { text: root.now-playing-badge; variant: root.now-playing-badge-variant;"),
            "the bar's format badge is a FormatPill colored by the projection's variant"
        );

        let panel = extract_component(APP_SLINT, "NowPlayingPanel");
        assert!(panel.contains("callback now-playing-album-requested();"));
        assert!(panel.contains("enabled: root.now-playing-album-available;"));
        assert!(panel.contains("clicked => { root.now-playing-album-requested(); }"), "the panel cover requests the album");

        let main_window = extract_component(APP_SLINT, "MainWindow");
        assert!(main_window.contains("in property <bool> now-playing-album-available: false;"));
        assert!(main_window.contains("in property <string> now-playing-badge-variant: \"\";"));
        assert_eq!(main_window.matches("now-playing-album-available: root.now-playing-album-available;").count(), 2, "bar and panel");
        assert!(!APP_SLINT.contains("album-key: root.now-playing"), "the album is resolved in Rust at click time");

        let main_rs = include_str!("main.rs");
        assert!(main_rs.contains("window.set_now_playing_badge_variant("), "the projection sets the badge variant");
        assert!(main_rs.contains("window.set_now_playing_album_available("), "the availability is kept in sync");
        assert!(main_rs.contains("now_playing_album_key("), "the click resolves the album key through the library");
        // The bar and panel keep showing the last track after `Stopped`/`Inactive`, so the zones resolve
        // from the track the panel shows (`PanelNowPlaying`), never from the live now-playing key, which
        // those events clear.
        assert!(main_rs.contains("now_playing_album_available("), "the availability asks the library without building a key");
        assert!(main_rs.contains("shown_now_playing_key"), "the zones resolve from the track the panel shows");
        assert!(!main_rs.contains("now_playing_album_playing"), "the click must not read the live key that Stopped clears");
        assert!(!main_rs.contains("now_playing_album_key(&event_app_state"), "the per-tick availability must not read the live key either");
    }

    /// The search field keeps the keyboard after a click elsewhere (Slint never clears focus on an
    /// outside click, and the app's buttons and rows are `TouchArea`s that never take it), so
    /// everything the user reaches by clicking something else hands it back to `key-root`; otherwise
    /// Space would keep typing into the search box.
    #[test]
    fn search_focus_returns_to_the_window_scope() {
        let top_bar = extract_component(APP_SLINT, "TopBar");
        assert!(top_bar.contains("callback search-dismissed();"));
        assert!(top_bar.contains("accepted => { root.search-dismissed(); }"), "Return in the search field must dismiss it");
        let escape = &top_bar[top_bar.find("event.text == Key.Escape").expect("TopBar must handle Esc")..];
        let escape = &escape[..escape.find("return accept;").expect("the Esc branch accepts the key")];
        assert!(escape.contains("root.search-edited(\"\");"), "Esc still clears the query first");
        assert!(escape.contains("root.search-dismissed();"), "Esc must also hand the keyboard back");

        let main_window = extract_component(APP_SLINT, "MainWindow");
        assert!(main_window.contains("search-dismissed => { root.release-search-focus(); }"));
        assert!(
            main_window.contains("function release-search-focus() {\n        key-root.focus();\n    }"),
            "release-search-focus must give the keyboard to key-root"
        );
        assert!(
            !without_comments(APP_SLINT).contains("clear-focus()"),
            "clearing focus leaves no focus item at all, and Space would then reach nobody"
        );
        // A click on a row is the only thing that reports a selection (a table appearing while the user
        // types a query reports nothing), so it always hands the keyboard back and records the key.
        assert!(
            main_window.contains("function row-selected(key: string) {\n        root.release-search-focus();\n        root.selected-track-key = key;\n    }"),
            "a row click must release the search focus and record the clicked track's key"
        );
        // Every click-driven handler, each call counted so a newly added duplicate cannot skip it.
        for call in [
            "root.nav-selected(v);",
            "root.nav-selected(View.albums);",
            "root.nav-selected(View.queue);",
            "root.back-requested();",
            "root.open-files();",
            "root.open-folder();",
            "root.album-opened(key);",
            "root.artist-opened(name);",
            "root.remove-album-requested(key);",
            "root.shuffle-all();",
            "root.album-action(root.album-header.key, action);",
            "root.play-pause-requested();",
            "root.previous-track-requested();",
            "root.next-track-requested();",
            "root.seek-requested(f);",
            "root.volume-requested(v, is-final);",
            "root.now-playing-album-requested();",
        ] {
            let all = main_window.matches(call).count();
            let releasing = main_window.matches(&format!("root.release-search-focus(); {call}")).count();
            assert!(all > 0, "`{call}` is no longer forwarded by MainWindow: update this list");
            assert_eq!(all, releasing, "every MainWindow forwarder of `{call}` must first call root.release-search-focus()");
        }
    }

    /// The row selection is a track KEY, not a position: `MainWindow.selected-track-key` is what every
    /// table highlights (`row.key == selected-key`) and what Space resolves against the visible rows.
    /// An index would name whatever sits at that position after every library re-projection (a scan, a
    /// `Started`), which replaces the tables' models under a table that stays on screen
    /// — and resetting the index whenever `rows` changes, the earlier fix, dropped a clicked row's
    /// highlight (and Space's target) within one re-projection.
    #[test]
    fn track_selection_is_keyed_by_track_key_and_survives_reprojection() {
        // `TrackTable inherits`, not `TrackTable`: `extract_component` finds the first `component
        // {name}` prefix, and `TrackTableHeader` is declared before `TrackTable`.
        for table in ["TrackTable inherits", "VirtualizedTrackTable"] {
            let body = extract_component(WIDGETS_SLINT, table);
            let code = without_comments(body);
            assert!(code.contains("in property <string> selected-key: \"\";"), "{table} must take the selected key from its owner");
            assert!(code.contains("callback selected(string);"), "{table} must report the clicked row's key");
            assert!(
                code.contains("selected: root.selected-key != \"\" && row.key == root.selected-key;"),
                "{table} must highlight the row whose key is selected, whatever position it is at"
            );
            assert!(code.contains("clicked => { root.selected(row.key); }"), "{table}: a click selects the row by its key");
            assert!(
                code.contains("activated(index) => { root.selected(row.key); root.activated(index); }"),
                "{table}: a double click selects it too"
            );
            for stale in ["selected-index", "changed rows", "init =>", "root.select("] {
                assert!(
                    !code.contains(stale),
                    "{table} must not keep a positional selection (`{stale}`): a reprojection would drop it or point it at another track"
                );
            }
        }
        let main_window = extract_component(APP_SLINT, "MainWindow");
        assert!(main_window.contains("in-out property <string> selected-track-key: \"\";"), "MainWindow owns the selected key");
        assert!(!without_comments(main_window).contains("track-selected"), "the key is read from the window when Space is pressed, not mirrored through a callback");
        for kind in ["search", "songs", "recently-added", "album", "queue"] {
            assert!(
                main_window.contains(&format!("root.track-activated(TrackListKind.{kind}, i);")),
                "the `{kind}` table's activation is still routed by kind"
            );
        }
        // Every view forwards the key down and the clicked key up, through the one `row-selected`.
        for (view, forwarded) in [
            ("SearchResultsView", 1),
            ("TrackListPage", 1),
            ("AlbumDetailView", 1),
            ("QueueView", 2),
        ] {
            let body = extract_component(APP_SLINT, view);
            assert!(body.contains("in property <string> selected-key: \"\";") && body.contains("callback selected(string);"), "{view} must take the key and report clicks");
            assert_eq!(body.matches("selected-key: root.selected-key;").count(), forwarded, "{view} must hand the key to each of its tables");
            assert_eq!(body.matches("selected(k) => { root.selected(k); }").count(), forwarded, "{view} must forward each table's clicks");
        }
        assert_eq!(
            main_window.matches("selected-key: root.selected-track-key;").count(),
            5,
            "the Search, Songs, Recently Added, album detail and Queue views must all show the same selection"
        );
        assert_eq!(main_window.matches("selected(k) => { root.row-selected(k); }").count(), 5);
        // The queue's single "Now Playing" row is a table of its own and reports through the same path.
        assert!(!without_comments(APP_SLINT).contains("current-selected"));
    }

    /// The selection belongs to the page it was made on: navigation callbacks (`sync_navigation`) and
    /// search edits that change the list on screen (`apply_search_edit`) clear it in `main.rs`, and only
    /// there — a re-projection of the same page (which never goes through either) must keep it, and so
    /// must a search edit that leaves the effective query alone (Esc in an empty field).
    #[test]
    fn navigation_and_search_edits_clear_the_selection_and_reprojection_does_not() {
        let main_rs = include_str!("main.rs");
        let body_of = |signature: &str| -> String {
            let start = main_rs.find(signature).unwrap_or_else(|| panic!("main.rs defines `{signature}`"));
            let body = &main_rs[start..];
            body[..body.find("\n}\n").expect("the function ends at a closing brace in column 0")].to_owned()
        };
        assert!(body_of("fn sync_navigation(").contains("clear_track_selection(window);"), "every navigation goes through sync_navigation");
        let search_edit = body_of("fn apply_search_edit(");
        assert!(search_edit.contains("clear_track_selection(window);"), "a new query is a new list");
        assert_eq!(
            search_edit.matches("navigation.effective_search_query()").count(),
            2,
            "the selection is cleared only when the effective query differs from what it was before the edit"
        );
        assert!(
            body_of("fn clear_track_selection(").contains("window.set_selected_track_key(SharedString::default());"),
            "clearing is writing the empty key"
        );
        let reprojection = body_of("fn project_and_set_library(");
        assert!(
            !reprojection.contains("selected_track_key") && !reprojection.contains("clear_track_selection"),
            "a re-projection replaces rows only; the selection follows its track through it"
        );
        assert!(
            main_rs.contains("apply_search_edit(&window, &mut navigation_for_search_edited.borrow_mut(), query.to_string());"),
            "the search-edited callback must go through apply_search_edit"
        );
    }

    /// An Output `ComboBox` that keeps the keyboard is a trap: Slint drops the focus item of a key press
    /// when it is invisible (the details panel hidden), and key bubbling stops at a popup's edge, so
    /// Space would reach nobody. Both forwarders of a choice hand the keyboard back to `key-root` once
    /// the dropdown has closed, and hiding the panel does it for a `ComboBox` that still holds it.
    #[test]
    fn output_combo_boxes_never_keep_the_keyboard() {
        let main_window = extract_component(APP_SLINT, "MainWindow");
        assert_eq!(
            main_window.matches("output-chosen(index) => { root.output-chosen(index); root.hand-focus-back-soon(); }").count(),
            2,
            "the panel's and the output popup's choice must both hand the keyboard back"
        );
        assert!(
            main_window.contains("focus-handback := Timer {") && main_window.contains("root.release-search-focus();"),
            "the hand-back is deferred by a Timer: the ComboBox takes the focus back after `selected` fires"
        );
        assert!(main_window.contains("function hand-focus-back-soon() {\n        focus-handback.running = true;\n    }"));
        assert!(
            main_window.contains("changed panel-shown => {\n        if (!root.panel-shown && now-playing-panel.output-focused) {"),
            "hiding the panel must free the keyboard from its Output ComboBox (and only from it)"
        );
        let panel = extract_component(APP_SLINT, "NowPlayingPanel");
        assert!(panel.contains("out property <bool> output-focused: output-combo.has-focus;"));
        assert!(panel.contains("output-combo := ComboBox {"), "the panel's ComboBox must carry the id output-focused reads");
    }

    /// `main.rs`'s Space handler and its OS media session ask `play_button_enabled` whether there is
    /// anything loaded or queued to act on; it must not drift from the expression the on-screen play
    /// button uses to enable itself. Both sides are pinned: the Slint expression as a string, and the
    /// helper's own truth table, which the same expression defines.
    #[test]
    fn space_mirrors_the_play_buttons_enabling_rule() {
        let player_bar = extract_component(APP_SLINT, "PlayerBar");
        assert!(
            player_bar.contains("TouchArea { enabled: root.now-playing-title != \"\" || root.queue-count > 0;"),
            "PlayerBar's play button enabling changed: update `play_button_enabled` in main.rs to match"
        );
        // `now-playing-title != "" || queue-count > 0`.
        assert!(!crate::play_button_enabled("", 0), "nothing has played and the queue is empty: the button does nothing");
        assert!(crate::play_button_enabled("Title", 0), "a track was loaded (the title is kept after Stopped)");
        assert!(crate::play_button_enabled("", 1), "a queued track can be started");
        assert!(crate::play_button_enabled("Title", 5));
        assert!(!crate::play_button_enabled("", -1), "a nonsensical count is not a queue");
        // No second copy of the rule in `main.rs`: the one place that reads the window's queue count
        // is the helper's window wrapper, which the Space handler and the media session both use.
        let main_rs = include_str!("main.rs");
        assert_eq!(main_rs.matches("get_queue_count()").count(), 1, "the enabling rule must live in `play_button_enabled` only");
        assert_eq!(
            main_rs.matches("window_play_button_enabled(&window)").count(),
            2,
            "both the Space handler and the OS media session must ask the shared helper"
        );
    }
}

//! UI-thread state: the session library and its decoded-artwork cache (Stage 3, `§3.1` partial),
//! `Navigation` (`§3.6`, Stage 5), and the now-playing/pending-volume pure helpers `§5.10`
//! describes. Pending seek stays in `main.rs`, since nothing outside `main.rs` needs it yet.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use slint::{Image, Rgb8Pixel, SharedPixelBuffer};

use crate::View;
use crate::audio::{AudioInfo, PreparedTrack};
use crate::library::format::{format_badge, format_file_size, format_line};
use crate::library::{
    ArtworkPixels, ArtworkSource, Library, TrackKey, TrackRecord, album_key, display_album, display_artist, display_title, normalize_for_search,
    resolution_group_key, track_key_string,
};

/// The back stack never grows past this many entries (`§3.6`): the oldest is dropped first.
const MAX_BACK_STACK: usize = 16;
/// "Jump back in" keeps at most this many recently played albums (`§5.10` "Recently played").
const MAX_RECENT_ALBUMS: usize = 12;

/// Who contributed a cached picture: the resolution group (`library::resolution_group_key`) of the
/// track whose scan carried it, `None` for a track without an album tag. Re-resolving a group's album
/// artist moves all of its tracks to another album key together, so the group is the unit its
/// pictures follow (`AppState::apply_scanned`).
type Contributor = (String, PathBuf);

/// One picture cached for an album key, plus the precedence tier it came from and who contributed
/// it, so a better picture can replace it (`AppState::cache_artwork`) and it can follow its
/// contributor to another key (`AppState::apply_scanned`).
struct CachedArtwork {
    /// Shares its pixel buffer with any other picture held under the same key that carries the very
    /// same pixels (`AppState::place_artwork`).
    image: Image,
    source: ArtworkSource,
    contributor: Option<Contributor>,
    /// Identifies this picture (`AppState::art_revision` at the time it was cached); it travels with
    /// the picture when `apply_scanned` moves it to another album key.
    revision: u64,
}

#[derive(Default)]
pub struct AppState {
    pub library: Library,
    /// Per album key, at most one picture per contributor (see `Contributor`); the one shown is the
    /// highest tier and, on a tie, the one whose contributor reached this key first (`shown_artwork`).
    /// Several contributors can share a key — two folders of one release, or one album's transient key
    /// while another's group is still resolving onto it — and keeping every contributor's own best
    /// picture is what lets each one leave again with the picture it brought, without taking the
    /// other's. Contributors whose pictures have identical pixels share one `Image`, so the usual case
    /// (the same cover in both folders) holds one decoded buffer; different pictures each keep their
    /// own.
    art_cache: HashMap<String, Vec<CachedArtwork>>,
    /// Counts the pictures ever cached; each `CachedArtwork` keeps the value it was cached under
    /// (`artwork_revision_of`).
    art_revision: u64,
    /// Album keys played this session, most recent first, deduped (`§5.10` "Recently played").
    /// Feeds Home's "Jump back in" shelf (`view_model::project_jump_back_albums`, `§6` Stage 6).
    recent_album_keys: Vec<String>,
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Caches a decoded album picture under `key` (`§3.3` "Artwork cache") on behalf of `contributor`.
    /// An album's artwork comes from whichever of its tracks the scanner reaches, in any order, so
    /// the picture shown is chosen by precedence and not by arrival (`shown_artwork`). A contributor
    /// keeps one picture per key: a new one replaces it unless it ranks lower (`ArtworkSource`'s
    /// `Ord`), so a later scan picks up a cover replaced or repaired on disc — an equal tier
    /// replaces — while a partial re-open that only sees a lower tier can never downgrade it. Between
    /// contributors the higher tier is shown and, on a tie, the one that reached `key` first (a
    /// contributor's replacement keeps its place).
    /// `slint::Image` never leaves the UI thread.
    pub fn cache_artwork(&mut self, key: &str, pixels: &ArtworkPixels, source: ArtworkSource, contributor: Option<&Contributor>) {
        let buffer = SharedPixelBuffer::<Rgb8Pixel>::clone_from_slice(&pixels.rgb, pixels.width, pixels.height);
        self.art_revision += 1;
        let picture = CachedArtwork { image: Image::from_rgb8(buffer), source, contributor: contributor.cloned(), revision: self.art_revision };
        self.place_artwork(key, picture, true);
    }

    /// Files `incoming` under `key`, next to the other contributors' pictures. Its own contributor's
    /// existing picture is replaced when `incoming` ranks higher, or equal and `replaces_equal`
    /// (a freshly scanned picture supersedes the previous scan's; one only moving in from another key
    /// does not override what the destination already had). When a picture already held under `key`
    /// carries the very same pixels, `incoming` takes over its `Image` (a reference-counted buffer)
    /// instead of keeping a second decoded copy: the same cover in the FLAC and the WavPack folder of
    /// one release is the usual case, and a decoded thumbnail is around 480 KB.
    fn place_artwork(&mut self, key: &str, mut incoming: CachedArtwork, replaces_equal: bool) {
        let held = self.art_cache.entry(key.to_owned()).or_default();
        let own = held.iter().position(|picture| picture.contributor == incoming.contributor);
        if let Some(index) = own
            && (held[index].source > incoming.source || (held[index].source == incoming.source && !replaces_equal))
        {
            return;
        }
        if let Some(twin) = held.iter().find(|picture| same_pixels(&picture.image, &incoming.image)) {
            incoming.image = twin.image.clone();
        }
        match own {
            Some(index) => held[index] = incoming,
            None => held.push(incoming),
        }
    }

    /// The picture shown for album `key`: the highest tier among its contributors' pictures and, on a
    /// tie, the one whose contributor reached `key` first — the first in `art_cache`'s order, where a
    /// picture that moved here counts from when it arrived and a contributor's replacement keeps the
    /// place its older picture had.
    fn shown_artwork(&self, key: &str) -> Option<&CachedArtwork> {
        self.art_cache.get(key)?.iter().reduce(|best, picture| if picture.source > best.source { picture } else { best })
    }

    /// Identifies the picture currently shown for the album of the library track `key`: `None` when
    /// the track is not in the library or its album has no picture yet, and a different value
    /// whenever that album's shown picture is first cached, replaced or swapped for another
    /// contributor's (a pure move of the same picture to the album's new key in `apply_scanned`
    /// keeps its value). The now-playing panel takes its picture only when a track starts or its own
    /// record is scanned, so `main.rs` compares this for the playing track before and after
    /// `apply_scanned` and re-projects the panel only then — when a SIBLING track's better picture
    /// replaced the one the panel is showing — and never for a picture of some unrelated album, or
    /// for a track that is no longer in the library.
    pub fn artwork_revision_of(&self, key: &TrackKey) -> Option<u64> {
        let album = self.library.get(key).map(album_key)?;
        self.shown_artwork(&album).map(|picture| picture.revision)
    }

    pub fn artwork_for_key(&self, key: &str) -> Option<Image> {
        self.shown_artwork(key).map(|picture| picture.image.clone())
    }

    /// "Remove from Library" for album `key` (`CLAUDE.md` "Library exclusions"): drops its tracks from
    /// the session library (`Library::remove_album`, returning their keys) and forgets its pictures
    /// too, so re-opening the album starts from what is on disc by then — a cover deleted in the
    /// meantime no longer outranks the embedded picture.
    pub fn remove_album(&mut self, key: &str) -> Vec<TrackKey> {
        self.art_cache.remove(key);
        self.library.remove_album(key)
    }

    /// Pushes `key` to the front of the recently played list, removing any earlier occurrence and
    /// keeping at most `MAX_RECENT_ALBUMS` (`§5.10` "Recently played"). Called on every `Started`.
    pub fn note_played_album(&mut self, key: &str) {
        self.recent_album_keys.retain(|existing| existing != key);
        self.recent_album_keys.insert(0, key.to_owned());
        self.recent_album_keys.truncate(MAX_RECENT_ALBUMS);
    }

    /// Looks up the album for `path` and records it as played, in the one exclusive borrow a
    /// caller needs (`§6` Stage 5 fix). A caller that instead does `self.library.get(path)` then
    /// `self.note_played_album(..)` as two separate statements is fine outside a `RefCell`, but an
    /// `if let Some(key) = state.borrow().library.get(path).map(album_key) { state.borrow_mut()...
    /// }` keeps the immutable `Ref` scrutinee alive through the whole then-block under edition
    /// 2024's `if let` rescoping, so the nested `borrow_mut()` panics with "already borrowed" the
    /// first time this runs. Folding both steps into one `&mut self` method removes the trap.
    pub fn note_played_key(&mut self, key: &TrackKey) {
        if let Some(album_key) = self.library.get(key).map(album_key) {
            self.note_played_album(&album_key);
        }
    }

    pub fn recent_album_keys(&self) -> &[String] {
        &self.recent_album_keys
    }

    /// Applies a `LibraryEvent::Probed` batch: inserts a minimal record only for paths the
    /// library does not already know. Re-opening an already-scanned file (to queue it again)
    /// must not wipe its tags, file size and artwork for the seconds it takes phase 2 to reach it
    /// again — or forever, if phase 2 then fails (`§3.3` "UI handling of scanner events").
    pub fn apply_probed(&mut self, tracks: &[PreparedTrack]) {
        for track in tracks {
            if self.library.get(&track.key).is_none() {
                self.library.upsert(TrackRecord::minimal(track.key.clone(), track.info.clone()));
            }
        }
    }

    /// Applies a freshly scanned record (`LibraryEvent::Scanned`): upserts it into the library
    /// (`§3.3`, `§3.4`) and caches its artwork, if any, under the resulting album key — tier-aware,
    /// see `cache_artwork`.
    ///
    /// Phase-2 tags can change a track's album key, from the scanner's phase-1 `dir:<scope>`
    /// fallback to a tagged `aa:`/`af:`/`va:` key (`§3.3` "Album grouping key") — and re-resolving a
    /// group (`Library::upsert`) can also silently change an already-stored SIBLING track's own key
    /// as a side effect (e.g. the group's majority flipping between a lone track's own `ARTIST` and
    /// `VariousArtists` as more, differently-tagged tracks join). `Library::upsert` now reports every
    /// such `(old_key, new_key)` transition it caused, not only the incoming record's own. For each
    /// one the pictures follow their tracks (`move_artwork`): the ones the moved group contributed
    /// always, and every picture once the old key is fully vacated
    /// (`Library::album(old_key).is_none()`). A vacated key's `recent_album_keys` entry is rewritten
    /// too, and the vacated transitions alone are returned so the caller does the same for
    /// `Navigation`'s current entry and back stack (`Navigation::rekey_album`).
    ///
    /// Left unmigrated, a stale key resolves to no album at all — album detail goes blank (empty
    /// title, no tracks, no-op action buttons), "Jump back in" silently drops the album (`§6` Stage 6
    /// fix) — and, for the artwork cache specifically, the image itself becomes permanently orphaned
    /// under a key nothing points to any more: the FIRST track of a group to get its embedded/folder
    /// art decoded caches it under whatever key was current at that moment, and the scanner's own
    /// best-tier dedup (`attach_artwork`, scoped to one scan batch) means no later track in the same
    /// group re-resolves a picture that could not improve on it — this was the real cause of a correctly-tagged,
    /// artwork-bearing disc showing the placeholder disc (`CLAUDE.md` "Album grouping"). The moves
    /// that leave the old key occupied matter as much: a compilation folder's first track (no
    /// `ALBUMARTIST`, say by "Queen") resolves alone onto `aa:queen|greatest hits`, next to Queen's own
    /// album of that name, and the compilation's cover must go back off that key once its group
    /// becomes "Various Artists" — without taking Queen's own picture with it or leaving it displaced.
    pub fn apply_scanned(&mut self, mut record: TrackRecord) -> Vec<(String, String)> {
        let pixels = record.artwork.take();
        let source = record.artwork_source;
        let path_key = record.key.clone();
        let group = resolution_group_key(&record);
        let transitions = self.library.upsert(record);
        let new_key = self.library.get(&path_key).map(album_key).unwrap_or_default();

        let mut applied: Vec<(String, String)> = Vec::new();
        for (old_key, moved_to) in transitions {
            if old_key == moved_to {
                continue;
            }
            let vacated = self.library.album(&old_key).is_none();
            self.move_artwork(&old_key, &moved_to, group.as_ref(), vacated);
            if vacated {
                applied.push((old_key, moved_to));
            }
        }
        // After the moves, so this record's own group has its earlier picture on `new_key` already
        // and the new one supersedes it.
        if let Some(pixels) = pixels {
            self.cache_artwork(&new_key, &pixels, source, group.as_ref());
        }
        for (old_key, new_key) in &applied {
            for existing in &mut self.recent_album_keys {
                if existing == old_key {
                    *existing = new_key.clone();
                }
            }
        }
        if !applied.is_empty() {
            // Rewriting can make two entries collide (the old and new key both already present);
            // keep only the first occurrence so "Jump back in" never shows the same album twice.
            let mut seen = std::collections::HashSet::new();
            self.recent_album_keys.retain(|key| seen.insert(key.clone()));
        }
        applied
    }

    /// Moves the pictures that follow the tracks which just went from `old_key` to `new_key`: those
    /// `group` (the group `apply_scanned` just re-resolved) contributed, and, when `old_key` is now
    /// `vacated`, every one — nothing is left there to own a picture, whoever brought it. Whatever
    /// else `old_key` holds stays: another album's own picture must neither travel with the moved
    /// tracks nor stay displaced by theirs. A picture only moving in never overrides its
    /// contributor's own picture already on `new_key`, at the same tier.
    fn move_artwork(&mut self, old_key: &str, new_key: &str, group: Option<&Contributor>, vacated: bool) {
        let Some(held) = self.art_cache.remove(old_key) else { return };
        let (moving, staying): (Vec<CachedArtwork>, Vec<CachedArtwork>) =
            held.into_iter().partition(|picture| vacated || (group.is_some() && picture.contributor.as_ref() == group));
        if !staying.is_empty() {
            self.art_cache.insert(old_key.to_owned(), staying);
        }
        for picture in moving {
            self.place_artwork(new_key, picture, false);
        }
    }
}

/// Whether two images carry the very same RGB pixels. Cheap on the cache's own images: `to_rgb8`
/// clones a reference-counted buffer, and the byte comparison stops at the first difference.
fn same_pixels(a: &Image, b: &Image) -> bool {
    match (a.to_rgb8(), b.to_rgb8()) {
        (Some(a), Some(b)) => a.width() == b.width() && a.height() == b.height() && a.as_bytes() == b.as_bytes(),
        _ => false,
    }
}

/// One entry in `Navigation`'s current-view/back-stack (`§3.6`): the view plus the two pieces of
/// state a stack entry needs to restore exactly — an artist filter (`artist-opened`) or an album
/// key (`album-opened`), never both.
///
/// `View` (generated by `slint_build`) derives `PartialEq` but not `Eq`, so this can't either.
///
/// `nav_view` (the sidebar highlight at the time this entry was current) lives here, not only on
/// `Navigation` itself, so `back_requested` restores it along with everything else (`§6` Stage 5
/// fix: a top-level-only `nav_view` survived a `back_requested` untouched, so after
/// `artist_opened` then Back the sidebar kept highlighting Artists while the restored view was
/// wherever Back came from).
#[derive(Clone, Debug, PartialEq)]
struct NavEntry {
    view: View,
    nav_view: View,
    artist_filter: Option<String>,
    album_key: Option<String>,
}

impl NavEntry {
    fn plain(view: View) -> Self {
        Self { view, nav_view: view, artist_filter: None, album_key: None }
    }
}

/// Rust-owned navigation state (`§3.6`): the current view (with an optional artist filter or album
/// key) and the sidebar highlight it was opened under, plus a back stack capped at
/// `MAX_BACK_STACK` of the same pairs, and the live search box text (`§3.4` "Search").
///
/// The search query lives here, not as a separate piece of `main.rs` state, so that "leaving" the
/// Search view — by sidebar navigation or by opening an album/artist from its results — and
/// "returning to the view the user was on before searching" are both provably one thing: clearing
/// `search_query` never touches `current`, and every navigation that pushes a new `current` also
/// clears `search_query`, tested together below instead of relying on `main.rs` to keep two Rcs in
/// step by hand.
pub struct Navigation {
    current: NavEntry,
    back: Vec<NavEntry>,
    search_query: String,
}

impl Default for Navigation {
    fn default() -> Self {
        Self { current: NavEntry::plain(View::Home), back: Vec::new(), search_query: String::new() }
    }
}

impl Navigation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn view(&self) -> View {
        self.current.view
    }

    pub fn nav_view(&self) -> View {
        self.current.nav_view
    }

    pub fn artist_filter(&self) -> Option<&str> {
        self.current.artist_filter.as_deref()
    }

    pub fn album_key(&self) -> Option<&str> {
        self.current.album_key.as_deref()
    }

    pub fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub fn search_query(&self) -> &str {
        &self.search_query
    }

    /// Whether the dedicated Search view should be showing (`§3.3`/`§3.4` "Search"): a non-empty
    /// query once whitespace is trimmed off both ends, so pressing Space alone does not switch the
    /// middle content away from whatever view was already current.
    pub fn search_active(&self) -> bool {
        !self.search_query.trim().is_empty()
    }

    /// What the search projections actually match on: the query trimmed and folded exactly as
    /// `Library` does (`normalize_for_search(query.trim())`), empty exactly when `search_active()` is
    /// false. Two query strings with the same effective query show the same lists — a lone Space in an
    /// empty box, a trailing space, another letter case — so an edit between them changes nothing on
    /// screen (`main.rs` `apply_search_edit` keeps the row selection across such an edit).
    pub fn effective_search_query(&self) -> String {
        normalize_for_search(self.search_query.trim())
    }

    /// Sets the live search box text (`search-edited`, `§3.4`). Never touches `current`: the view
    /// underneath the Search view is exactly whatever it already was, so clearing the query later
    /// (Esc, the pill's "x", or editing back down to empty) needs no separate "previous view" to
    /// restore.
    pub fn set_search_query(&mut self, query: String) {
        self.search_query = query;
    }

    /// A sidebar click: clears the back stack, any artist filter and the search query, and moves
    /// both the current view and the sidebar highlight (`§3.6`, `§3.4` "Search").
    pub fn nav_selected(&mut self, view: View) {
        self.back.clear();
        self.current = NavEntry::plain(view);
        self.search_query.clear();
    }

    /// Opening an album: pushes the current entry and switches to album detail. `nav_view` is
    /// carried over unchanged, so the sidebar keeps highlighting wherever the album was opened
    /// from, both now and after a later `back_requested` restores this entry (`§3.6`). Also clears
    /// the search query (`§3.4` "Search"): opening an album from the Search view's results must
    /// actually navigate there, not leave the Search view covering it because a non-empty query is
    /// still active.
    pub fn album_opened(&mut self, key: &str) {
        let nav_view = self.current.nav_view;
        self.push_current();
        self.current = NavEntry { view: View::AlbumDetail, nav_view, artist_filter: None, album_key: Some(key.to_owned()) };
        self.search_query.clear();
    }

    /// Opening an artist: pushes the current entry, switches to Albums filtered by `name`, and
    /// highlights Artists in the sidebar (`§3.6`). Also clears the search query, for the same
    /// reason `album_opened` does (`§3.4` "Search").
    pub fn artist_opened(&mut self, name: &str) {
        self.push_current();
        self.current = NavEntry { view: View::Albums, nav_view: View::Artists, artist_filter: Some(name.to_owned()), album_key: None };
        self.search_query.clear();
    }

    /// Restores the previous entry, sidebar highlight included (see the `NavEntry` doc comment).
    pub fn back_requested(&mut self) {
        if let Some(entry) = self.back.pop() {
            self.current = entry;
        }
    }

    /// "Remove from Library" (`CLAUDE.md` "Library exclusions"): if `key`'s album-detail page is
    /// the one currently showing, navigates back automatically, since it would otherwise be left
    /// stranded on an empty header/track list. A no-op on `current` if the removed album is not the
    /// one currently open (e.g. removed from a card in a different, still-valid view).
    ///
    /// Also purges every `self.back` entry that points at the removed album — the same both-
    /// `current`-and-`back` walk `rekey_album` (right below) already does. Without this, an album
    /// can be opened, navigated away from onto the back stack (e.g. via its artist credit), removed
    /// from a card in that other view, and then a later Back press would land on the now-removed
    /// album's stale detail page instead of a sane view. Purged before the `current` check so a
    /// Back triggered by removing the *currently open* album's own page never lands on another
    /// stale entry for the same album.
    pub fn remove_album(&mut self, key: &str) {
        self.back.retain(|entry| entry.album_key.as_deref() != Some(key));
        if self.current.album_key.as_deref() == Some(key) {
            self.back_requested();
        }
    }

    fn push_current(&mut self) {
        if self.back.len() >= MAX_BACK_STACK {
            self.back.remove(0);
        }
        self.back.push(self.current.clone());
    }

    /// Rewrites every stored reference to `old_key` — the current entry and the whole back stack —
    /// to `new_key`. Called after `AppState::apply_scanned` reports that the scanner's phase 2 moved
    /// an open album off its phase-1 `dir:` key, so an album-detail page or a back-stack entry
    /// opened before the scan finished keeps pointing at a real album instead of going stale
    /// (`§6` Stage 6 fix).
    pub fn rekey_album(&mut self, old_key: &str, new_key: &str) {
        if self.current.album_key.as_deref() == Some(old_key) {
            self.current.album_key = Some(new_key.to_owned());
        }
        for entry in &mut self.back {
            if entry.album_key.as_deref() == Some(old_key) {
                entry.album_key = Some(new_key.to_owned());
            }
        }
    }
}

/// The label `UnavailableView` shows for a nav item marked `available: false` (`§3.6`
/// "`unavailable-title` comes from a `View` → label map"). Empty for every other view: those
/// render their own heading (`§5.7`) and never read `unavailable-title`.
pub fn unavailable_label(view: View) -> &'static str {
    match view {
        View::Browser => "Browser",
        View::Folders => "Folders",
        View::Liked => "Liked Songs",
        View::Statistics => "Statistics",
        View::RecentlyOpened => "Recently Opened",
        View::Scripts => "Scripts",
        View::Favorites => "Favorites",
        View::NewPlaylist => "New Playlist",
        _ => "",
    }
}

/// The title/artist/album shown for the playing path, from the library record when one exists,
/// otherwise the `Started` event's own fallback title/album and "Unknown Artist" (`§5.10` "Now-
/// playing projection").
pub fn now_playing_identity(
    library: &Library,
    key: &TrackKey,
    fallback_title: &str,
    fallback_album: &str,
) -> (String, String, String) {
    match library.get(key) {
        Some(record) => (display_title(record), display_artist(record), display_album(record)),
        None => (fallback_title.to_owned(), "Unknown Artist".to_owned(), fallback_album.to_owned()),
    }
}

/// The format/size line for the playing path: built from what is actually playing (`info`) plus
/// the library record's file size, when known (`§5.10`). Feeds the right panel's `format-line`.
pub fn now_playing_format_line(library: &Library, key: &TrackKey, info: &AudioInfo) -> String {
    let file_size = library.get(key).and_then(|record| record.file_size);
    format_line(&info.format, info.bits_per_sample, info.sample_rate, info.is_float, info.source_channels, file_size, info.duration_ms)
}

/// Every now-playing field the right panel and player bar need (`§3.5` "Now playing", `§5.8`,
/// `§5.10` "Now-playing projection"). Identity and the format line reuse `now_playing_identity`/
/// `now_playing_format_line`; year, genre, artwork and lyrics come from the library record and are
/// empty/absent until the scanner's second pass reaches this path (`Scanned` re-projects, `§5.10`).
pub struct NowPlayingProjection {
    pub key: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub year: String,
    pub genre: String,
    pub badge: String,
    pub format_line: String,
    pub file_size_line: String,
    pub art: Option<Image>,
    pub lyrics: String,
    pub has_lyrics: bool,
}

pub fn project_now_playing(state: &AppState, key: &TrackKey, info: &AudioInfo, fallback: &(String, String)) -> NowPlayingProjection {
    let (title, artist, album) = now_playing_identity(&state.library, key, &fallback.0, &fallback.1);
    let record = state.library.get(key);
    let year = record.and_then(|r| r.tags.year).map(|year| year.to_string()).unwrap_or_default();
    let genre = record.and_then(|r| r.tags.genre.clone()).unwrap_or_default();
    let badge = format_badge(&info.format, info.bits_per_sample, info.sample_rate, info.is_float);
    let format_line = now_playing_format_line(&state.library, key, info);
    let file_size_line = record.and_then(|r| r.file_size).map(format_file_size).unwrap_or_default();
    let art = record.map(album_key).and_then(|key| state.artwork_for_key(&key));
    let lyrics = record.and_then(|r| r.tags.lyrics.clone()).unwrap_or_default();
    let has_lyrics = !lyrics.is_empty();
    NowPlayingProjection {
        key: track_key_string(key),
        title,
        artist,
        album,
        year,
        genre,
        badge,
        format_line,
        file_size_line,
        art,
        lyrics,
        has_lyrics,
    }
}

/// Optimistic UI state for an in-flight volume change (`§5.10` "Pending volume"). Set on every
/// `volume-requested`, cleared once the worker's own `Volume` echo is accepted or availability
/// turns false.
pub struct PendingVolume {
    pub level: f32,
    pub changed_at: Instant,
}

/// Whether a `Volume` event's level should be applied over an optimistic pending value: either it
/// echoes back close enough to what the UI already shows, or the pending state has been waiting
/// long enough that it should stop holding the slider back (`§5.10`).
pub fn accept_volume(pending: Option<&PendingVolume>, level: f32, now: Instant) -> bool {
    pending.is_none_or(|p| (level - p.level).abs() <= 0.02 || now.duration_since(p.changed_at) >= Duration::from_millis(500))
}

/// The `§5.10` "Pending volume" snap-back rule for a `Volume` event: clears the pending state and
/// says the level should be applied whenever the device is unavailable (nothing is left to hold
/// back for) or `accept_volume` accepts the echo; otherwise leaves the pending state untouched and
/// says to keep showing the optimistic level (`§6` Stage 4 fix — previously inlined with no direct
/// test, so removing the `!available ||` half silently regressed the snap-back).
pub fn resolve_volume_event(pending: &mut Option<PendingVolume>, available: bool, level: f32, now: Instant) -> bool {
    if !available || accept_volume(pending.as_ref(), level, now) {
        *pending = None;
        true
    } else {
        false
    }
}

/// A rejected `Volume` echo (a coarse device quantizing to a different level than requested, or a
/// poll/knob landing inside the 500 ms hold window) otherwise never gets re-applied: the worker
/// only emits again on a real change, so the slider can show a level the device is not at
/// indefinitely (`§6` Stage 4 fix). Called on every timer tick; once the pending state has been
/// held for at least 500 ms, expires it and returns the last level the worker actually reported
/// (accepted or not) so the caller can snap the slider to the truth.
pub fn expire_pending_volume(pending: &mut Option<PendingVolume>, last_worker_level: Option<f32>, now: Instant) -> Option<f32> {
    let changed_at = pending.as_ref()?.changed_at;
    if now.duration_since(changed_at) < Duration::from_millis(500) {
        return None;
    }
    *pending = None;
    last_worker_level
}

/// Throttles outgoing `SetVolume` commands to at most one per 50 ms while dragging, but never
/// holds back the final value of a gesture (`§5.10`).
pub fn should_send_volume(last: Option<Instant>, now: Instant, is_final: bool) -> bool {
    is_final || last.is_none_or(|t| now.duration_since(t) >= Duration::from_millis(50))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::TrackRecord;
    use std::path::PathBuf;

    fn info(format: &str, bits: u32, rate: u32, is_float: bool, channels: u16, duration_ms: Option<u64>) -> AudioInfo {
        AudioInfo {
            sample_rate: rate,
            duration_ms,
            source_channels: channels,
            bits_per_sample: bits,
            is_float,
            integer_pcm: !is_float,
            format: format.to_owned(),
        }
    }

    #[test]
    fn apply_probed_does_not_replace_an_existing_tagged_record() {
        let mut state = AppState::new();
        let path = TrackKey::whole_file(PathBuf::from("/nas/album/track.flac"));
        let mut tagged = TrackRecord::minimal(path.clone(), info("FLAC", 24, 44_100, false, 2, Some(10_000)));
        tagged.tags.title = Some("Real Title".into());
        tagged.file_size = Some(1_703_750);
        state.library.upsert(tagged);

        // Re-opening the same file (queuing it again) probes it a second time; that must not wipe
        // the tags and file size already known from the first, full scan.
        state.apply_probed(&[PreparedTrack { key: path.clone(), info: info("FLAC", 24, 44_100, false, 2, Some(10_000)) }]);

        let record = state.library.get(&path).unwrap();
        assert_eq!(record.tags.title.as_deref(), Some("Real Title"));
        assert_eq!(record.file_size, Some(1_703_750));
    }

    #[test]
    fn apply_probed_inserts_a_minimal_record_for_an_unknown_path() {
        let mut state = AppState::new();
        let path = TrackKey::whole_file(PathBuf::from("/nas/album/new.flac"));

        state.apply_probed(&[PreparedTrack { key: path.clone(), info: info("FLAC", 16, 44_100, false, 2, None) }]);

        let record = state.library.get(&path).expect("a minimal record should have been inserted");
        assert!(record.tags.title.is_none());
    }

    #[test]
    fn now_playing_identity_falls_back_when_no_record_exists() {
        let library = Library::new();
        let path = TrackKey::whole_file(PathBuf::from("/nas/album/track.flac"));

        let (title, artist, album) = now_playing_identity(&library, &path, "Track", "Album Folder");

        assert_eq!(title, "Track");
        assert_eq!(artist, "Unknown Artist");
        assert_eq!(album, "Album Folder");
    }

    #[test]
    fn now_playing_identity_prefers_the_library_record() {
        let mut library = Library::new();
        let path = TrackKey::whole_file(PathBuf::from("/nas/album/track.flac"));
        let mut record = TrackRecord::minimal(path.clone(), info("FLAC", 24, 44_100, false, 2, Some(1_000)));
        record.tags.title = Some("Real Title".into());
        record.tags.artist = Some("Real Artist".into());
        record.tags.album = Some("Real Album".into());
        library.upsert(record);

        let (title, artist, album) = now_playing_identity(&library, &path, "Fallback", "Fallback Album");

        assert_eq!(title, "Real Title");
        assert_eq!(artist, "Real Artist");
        assert_eq!(album, "Real Album");
    }

    #[test]
    fn now_playing_format_line_uses_playing_info_and_record_file_size() {
        let mut library = Library::new();
        let path = TrackKey::whole_file(PathBuf::from("/nas/album/track.flac"));
        // The record's own `info` deliberately differs from what is passed as "playing": the line
        // must reflect the latter (`Started.info`, what is actually playing), not the former.
        let mut record = TrackRecord::minimal(path.clone(), info("FLAC", 16, 48_000, false, 2, Some(5_000)));
        record.file_size = Some(1_703_750);
        library.upsert(record);

        let line = now_playing_format_line(&library, &path, &info("FLAC", 24, 44_100, false, 2, Some(10_000)));

        assert_eq!(line, "FLAC · 24-bit / 44.1\u{00A0}kHz\nStereo · 1,363\u{00A0}kbps");
    }

    fn contributor(album: &str, folder: &str) -> Contributor {
        (album.to_owned(), PathBuf::from(folder))
    }

    /// An equal tier is settled by who contributed: one contributor's newer picture supersedes its
    /// older one (a cover replaced on disc, keeping its place), while between contributors the one
    /// that reached the key first stays.
    #[test]
    fn cache_artwork_ties_go_to_the_same_contributors_newer_picture_and_otherwise_the_first_contributor() {
        let mut state = AppState::new();
        let first = ArtworkPixels { width: 2, height: 2, rgb: vec![255; 12] };
        let second = ArtworkPixels { width: 4, height: 4, rgb: vec![0; 48] };
        let newer = ArtworkPixels { width: 6, height: 6, rgb: vec![128; 108] };
        let (flac, wavpack) = (contributor("album", "/nas/flac"), contributor("album", "/nas/wavpack"));

        state.cache_artwork("album-1", &first, ArtworkSource::Embedded, Some(&flac));
        state.cache_artwork("album-1", &second, ArtworkSource::Embedded, Some(&wavpack));
        assert_eq!(cached_side(&state, "album-1"), 2, "another contributor's picture of the same tier does not displace the first");

        state.cache_artwork("album-1", &newer, ArtworkSource::Embedded, Some(&flac));
        assert_eq!(cached_side(&state, "album-1"), 6, "the same contributor's newer picture of the same tier replaces its own");

        state.cache_artwork("album-1", &first, ArtworkSource::Folder, Some(&wavpack));
        assert_eq!(cached_side(&state, "album-1"), 2, "a higher tier from any contributor is shown");
        assert!(state.artwork_for_key("album-2").is_none());
    }

    fn cached_side(state: &AppState, key: &str) -> u32 {
        state.artwork_for_key(key).expect("an image should be cached").size().width
    }

    /// Where each contributor's picture under `key` keeps its pixels, in `art_cache`'s order.
    fn buffer_addresses(state: &AppState, key: &str) -> Vec<*const u8> {
        state.art_cache[key].iter().map(|picture| picture.image.to_rgb8().unwrap().as_bytes().as_ptr()).collect()
    }

    /// The same cover cached for two contributors of one key (the FLAC and the WavPack folder of one
    /// release) is one decoded buffer, however it got there — scanned for both, or moved in from
    /// another key — while every contributor still holds its own picture and pixels that differ, in
    /// bytes or in dimensions, keep a buffer of their own.
    #[test]
    fn identical_pictures_of_two_contributors_share_one_buffer() {
        let mut state = AppState::new();
        let cover = ArtworkPixels { width: 4, height: 4, rgb: vec![10; 48] };
        let recolored = ArtworkPixels { width: 4, height: 4, rgb: vec![11; 48] };
        let reshaped = ArtworkPixels { width: 2, height: 8, rgb: vec![10; 48] };
        let (flac, wavpack, mp3, aac, ogg) = (
            contributor("album", "/nas/flac"),
            contributor("album", "/nas/wavpack"),
            contributor("album", "/nas/mp3"),
            contributor("album", "/nas/aac"),
            contributor("album", "/nas/ogg"),
        );

        state.cache_artwork("album-1", &cover, ArtworkSource::Embedded, Some(&flac));
        state.cache_artwork("album-1", &cover.clone(), ArtworkSource::Folder, Some(&wavpack));
        let held = buffer_addresses(&state, "album-1");
        assert_eq!(held.len(), 2, "each contributor still holds its own picture");
        assert_eq!(held[0], held[1], "identical pixels are decoded once");

        state.cache_artwork("album-1", &recolored, ArtworkSource::Embedded, Some(&mp3));
        state.cache_artwork("album-1", &reshaped, ArtworkSource::Embedded, Some(&aac));
        let held = buffer_addresses(&state, "album-1");
        assert_eq!(held.len(), 4);
        assert!(held[2] != held[0] && held[3] != held[0] && held[2] != held[3], "other bytes or other dimensions never share a buffer");

        state.cache_artwork("album-2", &cover, ArtworkSource::Embedded, Some(&ogg));
        state.move_artwork("album-2", "album-1", Some(&ogg), true);
        let held = buffer_addresses(&state, "album-1");
        assert_eq!(held.len(), 5, "the moved picture is filed next to the others");
        assert_eq!(held[4], held[0], "a picture moving in shares the buffer of an identical one already there");

        // A contributor replacing its own picture with a different one leaves the others' untouched.
        state.cache_artwork("album-1", &recolored, ArtworkSource::Folder, Some(&flac));
        let held = buffer_addresses(&state, "album-1");
        assert_eq!(held.len(), 5);
        assert_eq!(held[0], held[2], "the replacement shares with the identical picture the mp3 contributor holds");
        assert_eq!(held[1], held[4], "the other contributors of the old cover keep it");
    }

    #[test]
    fn cache_artwork_replaces_a_lower_tier_image_with_a_higher_one() {
        let mut state = AppState::new();
        let fallback = ArtworkPixels { width: 2, height: 2, rgb: vec![255; 12] };
        let embedded = ArtworkPixels { width: 4, height: 4, rgb: vec![0; 48] };
        let folder = ArtworkPixels { width: 6, height: 6, rgb: vec![128; 108] };

        state.cache_artwork("album-1", &fallback, ArtworkSource::FolderFallback, None);
        state.cache_artwork("album-1", &embedded, ArtworkSource::Embedded, None);
        assert_eq!(cached_side(&state, "album-1"), 4, "an embedded picture replaces the legacy folder.jpg");

        state.cache_artwork("album-1", &folder, ArtworkSource::Folder, None);
        assert_eq!(cached_side(&state, "album-1"), 6, "a named folder cover replaces the embedded picture");
    }

    #[test]
    fn cache_artwork_never_replaces_a_higher_tier_image_with_a_lower_one() {
        let mut state = AppState::new();
        let folder = ArtworkPixels { width: 6, height: 6, rgb: vec![128; 108] };
        let embedded = ArtworkPixels { width: 4, height: 4, rgb: vec![0; 48] };
        let fallback = ArtworkPixels { width: 2, height: 2, rgb: vec![255; 12] };

        state.cache_artwork("album-1", &folder, ArtworkSource::Folder, None);
        state.cache_artwork("album-1", &embedded, ArtworkSource::Embedded, None);
        state.cache_artwork("album-1", &fallback, ArtworkSource::FolderFallback, None);
        state.cache_artwork("album-1", &fallback, ArtworkSource::None, None);

        assert_eq!(cached_side(&state, "album-1"), 6, "the named folder cover must survive every lower tier");
    }

    #[test]
    fn should_send_volume_throttles_to_50ms_but_always_sends_final() {
        let last = Instant::now();

        assert!(!should_send_volume(Some(last), last, false), "a drag tick right after the last send is throttled");
        assert!(
            should_send_volume(Some(last), last + Duration::from_millis(50), false),
            "a drag tick at the 50 ms mark is allowed through"
        );
        assert!(
            should_send_volume(Some(last), last + Duration::from_millis(10), true),
            "the final value of a gesture is never throttled"
        );
        assert!(should_send_volume(None, last, false), "the very first send has no prior send to throttle against");
    }

    #[test]
    fn accept_volume_ignores_stale_echo_for_500ms() {
        let changed_at = Instant::now();
        let pending = PendingVolume { level: 0.6, changed_at };

        assert!(accept_volume(Some(&pending), 0.6, changed_at), "an exact echo is accepted immediately");
        assert!(
            accept_volume(Some(&pending), 0.615, changed_at),
            "within the 0.02 tolerance is accepted immediately"
        );
        assert!(
            !accept_volume(Some(&pending), 0.2, changed_at),
            "a stale echo far from the pending value is held back before the timeout"
        );
        assert!(
            accept_volume(Some(&pending), 0.2, changed_at + Duration::from_millis(500)),
            "a stale echo is accepted once 500 ms have passed, so the UI never sticks forever"
        );
        assert!(accept_volume(None, 0.2, changed_at), "with nothing pending, every echo is accepted");
    }

    #[test]
    fn resolve_volume_event_clears_pending_on_unavailable_or_accepted_echo() {
        let changed_at = Instant::now();

        let mut pending = Some(PendingVolume { level: 0.6, changed_at });
        assert!(
            resolve_volume_event(&mut pending, false, 0.6, changed_at),
            "unavailable always clears the pending optimistic level and applies the reported one"
        );
        assert!(pending.is_none());

        let mut pending = Some(PendingVolume { level: 0.6, changed_at });
        assert!(
            !resolve_volume_event(&mut pending, true, 0.2, changed_at),
            "a stale echo under 500 ms with the device still available keeps the pending state"
        );
        assert!(pending.is_some());

        let mut pending = Some(PendingVolume { level: 0.6, changed_at });
        assert!(
            resolve_volume_event(&mut pending, true, 0.6, changed_at),
            "a matching echo clears the pending state and applies"
        );
        assert!(pending.is_none());
    }

    #[test]
    fn expire_pending_volume_snaps_back_after_500ms_to_the_last_worker_level() {
        let changed_at = Instant::now();
        let mut pending = Some(PendingVolume { level: 0.6, changed_at });

        assert_eq!(
            expire_pending_volume(&mut pending, Some(0.25), changed_at + Duration::from_millis(499)),
            None,
            "still within the hold window: nothing to snap back to yet"
        );
        assert!(pending.is_some(), "an unexpired pending state must not be cleared");

        assert_eq!(
            expire_pending_volume(&mut pending, Some(0.25), changed_at + Duration::from_millis(500)),
            Some(0.25),
            "past the hold window, the last level the worker actually reported wins"
        );
        assert!(pending.is_none(), "expiring the pending state must clear it");

        let mut nothing_pending = None;
        assert_eq!(
            expire_pending_volume(&mut nothing_pending, Some(0.9), changed_at + Duration::from_secs(1)),
            None,
            "nothing pending means nothing to expire"
        );
    }

    #[test]
    fn nav_select_clears_back_stack_and_filter() {
        let mut nav = Navigation::new();
        nav.artist_opened("Nina");
        assert!(nav.can_go_back());

        nav.nav_selected(View::Songs);

        assert!(!nav.can_go_back());
        assert_eq!(nav.view(), View::Songs);
        assert_eq!(nav.nav_view(), View::Songs);
        assert_eq!(nav.artist_filter(), None);
    }

    #[test]
    fn album_open_pushes_and_back_pops() {
        let mut nav = Navigation::new();
        nav.nav_selected(View::Albums);

        nav.album_opened("aa:band\u{1f}double album");

        assert_eq!(nav.view(), View::AlbumDetail);
        assert_eq!(nav.album_key(), Some("aa:band\u{1f}double album"));
        assert_eq!(nav.nav_view(), View::Albums, "album-opened must not move the sidebar highlight");
        assert!(nav.can_go_back());

        nav.back_requested();

        assert_eq!(nav.view(), View::Albums);
        assert_eq!(nav.nav_view(), View::Albums);
        assert!(!nav.can_go_back());
    }

    #[test]
    fn back_requested_restores_the_sidebar_highlight_too() {
        // Latent in Stage 5 (no view calls `artist_opened` yet), reachable once Stage 6 wires
        // Artists: a top-level-only `nav_view` used to survive `back_requested` untouched, so the
        // sidebar kept highlighting Artists after Back even though the restored view was Home.
        let mut nav = Navigation::new();
        nav.nav_selected(View::Home);

        nav.artist_opened("Nina");
        assert_eq!(nav.nav_view(), View::Artists);

        nav.back_requested();

        assert_eq!(nav.view(), View::Home);
        assert_eq!(nav.nav_view(), View::Home, "back must restore the sidebar highlight, not just the view");
    }

    /// "Remove from Library" (`CLAUDE.md` "Library exclusions"): removing the album whose detail
    /// page is currently open navigates back automatically; removing a different album (from a
    /// card in some other still-valid view) leaves navigation untouched.
    #[test]
    fn remove_album_navigates_back_only_when_its_own_detail_page_is_open() {
        let mut nav = Navigation::new();
        nav.nav_selected(View::Albums);
        nav.album_opened("album-a");
        assert_eq!(nav.view(), View::AlbumDetail);

        // Removing a different album (e.g. from a card in the Albums grid underneath) must not
        // navigate away from the open album-detail page.
        nav.remove_album("album-b");
        assert_eq!(nav.view(), View::AlbumDetail);
        assert_eq!(nav.album_key(), Some("album-a"));

        // Removing the album whose page is actually open goes back automatically.
        nav.remove_album("album-a");
        assert_eq!(nav.view(), View::Albums);
        assert!(!nav.can_go_back());
    }

    /// Regression test for the WARNING fix: `remove_album` must purge stale `self.back` entries
    /// too, not just check `current` — mirroring the code review's repro: open Album A, navigate
    /// away via its artist credit (which pushes `AlbumDetail(album-a)` onto the back stack), then
    /// remove Album A while looking at that artist's albums view (a still-valid view, so the old
    /// `current`-only check never caught this). A later Back press must not land on the stale,
    /// now-removed album page.
    #[test]
    fn remove_album_purges_stale_back_stack_entries() {
        let mut nav = Navigation::new();
        nav.nav_selected(View::Albums);
        nav.album_opened("album-a");
        nav.artist_opened("Nina"); // pushes AlbumDetail(album-a) onto the back stack

        nav.remove_album("album-a");

        // The earlier `Albums` entry `album_opened` itself pushed is unrelated and must remain;
        // only the stale `AlbumDetail(album-a)` entry `artist_opened` pushed is purged.
        assert!(nav.can_go_back(), "an unrelated back-stack entry must not be purged along with the stale one");
        nav.back_requested();
        assert_ne!(nav.album_key(), Some("album-a"), "the stale back-stack entry for the removed album must have been purged");
        assert!(!nav.can_go_back(), "the back stack must now be empty");
    }

    /// Removing an album must clear every reference to it at once: the current entry (via the
    /// existing back-navigation) and every stale `self.back` entry, even when both exist together.
    #[test]
    fn remove_album_clears_both_current_and_back_stack_references() {
        let mut nav = Navigation::new();
        nav.nav_selected(View::Albums);
        nav.album_opened("album-a");
        nav.artist_opened("Nina"); // back: [.., AlbumDetail(album-a)]
        nav.album_opened("album-a"); // current: AlbumDetail(album-a) again, a second reference

        nav.remove_album("album-a");

        assert_ne!(nav.album_key(), Some("album-a"), "current must no longer point at the removed album");
        while nav.can_go_back() {
            nav.back_requested();
            assert_ne!(nav.album_key(), Some("album-a"), "no back-stack entry may still reference the removed album");
        }
    }

    #[test]
    fn artist_open_sets_filter_and_nav_highlight() {
        let mut nav = Navigation::new();

        nav.artist_opened("Nina");

        assert_eq!(nav.view(), View::Albums);
        assert_eq!(nav.artist_filter(), Some("Nina"));
        assert_eq!(nav.nav_view(), View::Artists);
        assert!(nav.can_go_back());
    }

    #[test]
    fn setting_and_clearing_the_search_query_does_not_change_the_current_view() {
        let mut nav = Navigation::new();
        nav.nav_selected(View::Albums);

        nav.set_search_query("blue".to_owned());
        assert!(nav.search_active());
        assert_eq!(nav.view(), View::Albums, "typing a search must not itself change the view");

        nav.set_search_query(String::new());
        assert!(!nav.search_active());
        assert_eq!(
            nav.view(),
            View::Albums,
            "clearing the search (Esc or the pill's \"x\") must land back on exactly the view that was already current"
        );
    }

    #[test]
    fn search_active_ignores_a_whitespace_only_query() {
        let mut nav = Navigation::new();

        nav.set_search_query("   ".to_owned());

        assert!(!nav.search_active(), "a whitespace-only query must not switch the content area to the Search view");
    }

    #[test]
    fn the_effective_search_query_is_what_the_projections_match_on() {
        let mut nav = Navigation::new();
        assert_eq!(nav.effective_search_query(), "");

        for same_as_empty in [" ", "   ", "\t"] {
            nav.set_search_query(same_as_empty.to_owned());
            assert_eq!(nav.effective_search_query(), "", "{same_as_empty:?} matches everything, like no query");
            assert!(!nav.search_active());
        }
        for same_as_ab in ["ab", " ab", "ab ", "AB", "  Ab  "] {
            nav.set_search_query(same_as_ab.to_owned());
            assert_eq!(nav.effective_search_query(), "ab", "{same_as_ab:?} is trimmed and case-folded like the library's own matching");
            assert!(nav.search_active());
        }
        nav.set_search_query("Música".to_owned());
        assert_eq!(nav.effective_search_query(), "musica", "accents fold as they do in `Library`");
        nav.set_search_query("abc".to_owned());
        assert_ne!(nav.effective_search_query(), "ab", "a refined query is a different list");
    }

    #[test]
    fn nav_selected_clears_the_search_query() {
        let mut nav = Navigation::new();
        nav.set_search_query("blue".to_owned());
        assert!(nav.search_active());

        nav.nav_selected(View::Songs);

        assert!(!nav.search_active(), "a sidebar click must clear the search box");
        assert_eq!(nav.search_query(), "");
    }

    #[test]
    fn album_opened_and_artist_opened_clear_the_search_query() {
        let mut nav = Navigation::new();
        nav.set_search_query("blue".to_owned());

        nav.album_opened("aa:band\u{1f}double album");

        assert!(
            !nav.search_active(),
            "opening an album from the Search view's own results must clear the search, or the Search view would keep \
             covering the album it just navigated to"
        );

        nav.set_search_query("blue".to_owned());
        nav.artist_opened("Nina");

        assert!(!nav.search_active(), "opening an artist from search results must clear the search for the same reason");
    }

    #[test]
    fn rekey_album_rewrites_the_current_entry() {
        let mut nav = Navigation::new();
        nav.album_opened("dir:/nas/album");

        nav.rekey_album("dir:/nas/album", "aa:band\u{1f}double album");

        assert_eq!(nav.album_key(), Some("aa:band\u{1f}double album"));
    }

    #[test]
    fn rekey_album_rewrites_a_back_stack_entry() {
        let mut nav = Navigation::new();
        nav.album_opened("dir:/nas/album");
        nav.album_opened("dir:/nas/other");

        nav.rekey_album("dir:/nas/album", "aa:band\u{1f}double album");
        nav.back_requested();

        assert_eq!(nav.album_key(), Some("aa:band\u{1f}double album"), "a back-stack entry naming the old key must be rewritten");
    }

    #[test]
    fn rekey_album_leaves_unrelated_keys_untouched() {
        let mut nav = Navigation::new();
        nav.album_opened("dir:/nas/other");

        nav.rekey_album("dir:/nas/album", "aa:band\u{1f}double album");

        assert_eq!(nav.album_key(), Some("dir:/nas/other"));
    }

    #[test]
    fn unavailable_label_covers_every_unavailable_nav_item_and_nothing_else() {
        for view in [
            View::Browser,
            View::Folders,
            View::Liked,
            View::Statistics,
            View::RecentlyOpened,
            View::Scripts,
            View::Favorites,
            View::NewPlaylist,
        ] {
            assert!(!unavailable_label(view).is_empty(), "{view:?} must have a label");
        }
        for view in [View::Home, View::Artists, View::Albums, View::Songs, View::RecentlyAdded, View::Queue, View::AlbumDetail] {
            assert_eq!(unavailable_label(view), "", "{view:?} draws its own heading and never reads unavailable-title");
        }
    }

    #[test]
    fn note_played_path_looks_up_the_album_in_one_borrow_and_records_it() {
        let mut state = AppState::new();
        let path = TrackKey::whole_file(PathBuf::from("/nas/album/track.flac"));
        let record = TrackRecord::minimal(path.clone(), info("FLAC", 24, 44_100, false, 2, Some(10_000)));
        let key = album_key(&record);
        state.library.upsert(record);

        state.note_played_key(&path);

        assert_eq!(state.recent_album_keys(), [key]);
    }

    #[test]
    fn note_played_path_does_nothing_for_a_path_with_no_library_record() {
        let mut state = AppState::new();

        state.note_played_key(&TrackKey::whole_file(PathBuf::from("/nas/unknown.flac")));

        assert!(state.recent_album_keys().is_empty());
    }

    #[test]
    fn apply_scanned_rekeys_recent_album_when_the_dir_fallback_key_is_replaced() {
        let mut state = AppState::new();
        let path = TrackKey::whole_file(PathBuf::from("/nas/album/track.flac"));
        // Phase 1: a minimal, untagged record groups under the `dir:` fallback key.
        state.apply_probed(&[PreparedTrack { key: path.clone(), info: info("FLAC", 24, 44_100, false, 2, None) }]);
        let old_key = album_key(state.library.get(&path).unwrap());
        state.note_played_key(&path);
        assert_eq!(state.recent_album_keys(), std::slice::from_ref(&old_key));

        // Phase 2: real tags arrive and move the same path onto an `aa:` key — resolved by the
        // library after `apply_scanned`, not computable from the incoming record alone, since only
        // this one track shares its folder here and `Library` resolves its `effective_album_artist`
        // via the shared-`ARTIST` fallback once it is actually upserted (`EffectiveAlbumArtist`).
        let mut scanned = TrackRecord::minimal(path.clone(), info("FLAC", 24, 44_100, false, 2, Some(10_000)));
        scanned.tags.title = Some("Real Title".into());
        scanned.tags.artist = Some("Real Artist".into());
        scanned.tags.album = Some("Real Album".into());

        let rekey = state.apply_scanned(scanned);
        let new_key = album_key(state.library.get(&path).unwrap());
        assert_ne!(old_key, new_key, "the scan must actually change the album key for this test to be meaningful");

        assert_eq!(rekey, vec![(old_key, new_key.clone())]);
        assert_eq!(
            state.recent_album_keys(),
            [new_key],
            "the stale dir: key in recent_album_keys must be rewritten, not left dangling"
        );
    }

    #[test]
    fn apply_scanned_does_not_rekey_while_sibling_tracks_still_hold_the_old_key() {
        let mut state = AppState::new();
        let path_a = TrackKey::whole_file(PathBuf::from("/nas/album/a.flac"));
        let path_b = TrackKey::whole_file(PathBuf::from("/nas/album/b.flac"));
        state.apply_probed(&[
            PreparedTrack { key: path_a.clone(), info: info("FLAC", 24, 44_100, false, 2, None) },
            PreparedTrack { key: path_b.clone(), info: info("FLAC", 24, 44_100, false, 2, None) },
        ]);
        let old_key = album_key(state.library.get(&path_a).unwrap());
        state.note_played_key(&path_a);

        // Only `a.flac` gets scanned; `b.flac` still holds the old `dir:` key, so the group is not
        // empty yet and the recent-album entry must not be rewritten out from under it.
        let mut scanned = TrackRecord::minimal(path_a.clone(), info("FLAC", 24, 44_100, false, 2, Some(10_000)));
        scanned.tags.album = Some("Real Album".into());

        let rekey = state.apply_scanned(scanned);

        assert!(rekey.is_empty());
        assert_eq!(state.recent_album_keys(), [old_key]);
    }

    /// Reproduces the real "a correctly-tagged, artwork-bearing disc shows the placeholder disc"
    /// bug (`CLAUDE.md` "Album grouping"): the scanner decodes a track's art once, caches it under
    /// whatever album key is current at that moment, and never revisits it — but `Library::upsert`
    /// re-resolving a GROUP can silently move an already-cached SIBLING track onto a different key
    /// as a side effect of a later track's tags arriving, orphaning the cached image under the
    /// abandoned key unless `apply_scanned` migrates it.
    #[test]
    fn apply_scanned_migrates_cached_artwork_when_group_resolution_flips_a_sibling_key() {
        let mut state = AppState::new();
        let track_a = TrackKey::whole_file(PathBuf::from("/nas/album/a.flac"));
        let track_b = TrackKey::whole_file(PathBuf::from("/nas/album/b.flac"));

        // Track A arrives alone: a single-track group resolves trivially via its own shared ARTIST,
        // and its embedded artwork is cached under that key.
        let mut scanned_a = TrackRecord::minimal(track_a.clone(), info("FLAC", 24, 44_100, false, 2, Some(10_000)));
        scanned_a.tags.artist = Some("Solo".into());
        scanned_a.tags.album = Some("Album".into());
        scanned_a.artwork = Some(ArtworkPixels { width: 2, height: 2, rgb: vec![255; 12] });
        scanned_a.artwork_source = ArtworkSource::Embedded;
        state.apply_scanned(scanned_a);
        let key_a = album_key(state.library.get(&track_a).unwrap());
        assert_eq!(key_a, "aa:solo\u{1f}album");
        assert!(state.artwork_for_key(&key_a).is_some(), "track A's own art must be cached under its own key");

        // Track B arrives with an explicit ALBUMARTIST that disagrees with A's ARTIST: the group
        // re-resolves via the majority-ALBUMARTIST step, silently moving A (a SIBLING, not the record
        // being upserted) onto a different key too.
        let mut scanned_b = TrackRecord::minimal(track_b.clone(), info("FLAC", 24, 44_100, false, 2, Some(10_000)));
        scanned_b.tags.artist = Some("Someone Else".into());
        scanned_b.tags.album_artist = Some("Compilation".into());
        scanned_b.tags.album = Some("Album".into());
        let rekeys = state.apply_scanned(scanned_b);

        let final_key_a = album_key(state.library.get(&track_a).unwrap());
        assert_ne!(final_key_a, key_a, "track A's own key must have moved as a side effect of B's upsert");
        assert!(rekeys.contains(&(key_a.clone(), final_key_a.clone())), "the sibling transition must be reported");

        assert!(state.artwork_for_key(&key_a).is_none(), "the abandoned key must no longer serve stale art");
        assert!(state.artwork_for_key(&final_key_a).is_some(), "the cached image must follow track A onto its final, correct key");
    }

    /// The same sibling-key migration, but with a picture already cached under the destination key:
    /// the higher-precedence image of the two must survive (the destination's own on a tie), instead
    /// of the migrated one blindly losing or winning.
    #[test]
    fn apply_scanned_key_migration_keeps_the_higher_tier_image() {
        // (tier already cached under the destination key, width of the image expected to survive).
        // The migrated image is the 2 px, `Embedded` one; the destination's is 6 px.
        let cases = [
            (ArtworkSource::Folder, 6, "a named folder cover already on the destination beats the migrated embedded picture"),
            (ArtworkSource::Embedded, 6, "on a tie the destination's own image stays"),
            (ArtworkSource::FolderFallback, 2, "the migrated embedded picture beats a legacy folder.jpg on the destination"),
        ];
        for (destination_source, expected_width, why) in cases {
            let mut state = AppState::new();
            let track_a = TrackKey::whole_file(PathBuf::from("/nas/album/a.flac"));
            let track_b = TrackKey::whole_file(PathBuf::from("/nas/album/b.flac"));

            let mut scanned_a = TrackRecord::minimal(track_a.clone(), info("FLAC", 24, 44_100, false, 2, Some(10_000)));
            scanned_a.tags.artist = Some("Solo".into());
            scanned_a.tags.album = Some("Album".into());
            scanned_a.artwork = Some(ArtworkPixels { width: 2, height: 2, rgb: vec![255; 12] });
            scanned_a.artwork_source = ArtworkSource::Embedded;
            state.apply_scanned(scanned_a);
            let key_a = album_key(state.library.get(&track_a).unwrap());

            // B's ALBUMARTIST moves A onto `destination` (see the test above); something is already there.
            let destination = "aa:compilation\u{1f}album";
            state.cache_artwork(destination, &ArtworkPixels { width: 6, height: 6, rgb: vec![128; 108] }, destination_source, None);

            let mut scanned_b = TrackRecord::minimal(track_b, info("FLAC", 24, 44_100, false, 2, Some(10_000)));
            scanned_b.tags.artist = Some("Someone Else".into());
            scanned_b.tags.album_artist = Some("Compilation".into());
            scanned_b.tags.album = Some("Album".into());
            state.apply_scanned(scanned_b);

            assert_eq!(album_key(state.library.get(&track_a).unwrap()), destination);
            assert!(state.artwork_for_key(&key_a).is_none(), "the abandoned key must no longer serve art");
            assert_eq!(cached_side(&state, destination), expected_width, "{why}");
        }
    }

    /// A record of the one album "Solo" / "Album" (one key, whatever the file) carrying a `side` px
    /// square picture of `source` tier.
    fn scanned_with_art(path: &str, source: ArtworkSource, side: u32) -> TrackRecord {
        let mut record = TrackRecord::minimal(TrackKey::whole_file(PathBuf::from(path)), info("FLAC", 24, 44_100, false, 2, Some(10_000)));
        record.tags.artist = Some("Solo".into());
        record.tags.album = Some("Album".into());
        record.artwork = Some(ArtworkPixels { width: side, height: side, rgb: vec![128; (side * side * 3) as usize] });
        record.artwork_source = source;
        record
    }

    /// Scan-order independence on the UI side depends on `apply_scanned` handing the record's own
    /// tier to `cache_artwork`: a later folder cover replaces an earlier embedded picture, and a
    /// lower tier arriving after a higher one never does, in either arrival order (an equal tier
    /// from the same group is a newer scan of it, which replaces the older picture).
    #[test]
    fn apply_scanned_caches_artwork_by_the_tier_each_record_carries() {
        // (arrival order as (tier, picture width), width of the surviving picture, why)
        let cases: [(&[(ArtworkSource, u32)], u32, &str); 3] = [
            (&[(ArtworkSource::Embedded, 2), (ArtworkSource::Folder, 6), (ArtworkSource::FolderFallback, 4)], 6, "the folder cover wins over an earlier embedded and a later legacy picture"),
            (&[(ArtworkSource::Folder, 6), (ArtworkSource::Embedded, 2)], 6, "a later embedded picture never replaces the folder cover"),
            (&[(ArtworkSource::FolderFallback, 4), (ArtworkSource::Embedded, 2), (ArtworkSource::Embedded, 8)], 8, "embedded beats legacy, then the album's newer picture of an equal tier replaces its older one"),
        ];
        for (arrivals, expected_width, why) in cases {
            let mut state = AppState::new();
            for (index, (source, side)) in arrivals.iter().enumerate() {
                state.apply_scanned(scanned_with_art(&format!("/nas/album/{index}.flac"), *source, *side));
            }

            let key = album_key(state.library.get(&TrackKey::whole_file(PathBuf::from("/nas/album/0.flac"))).unwrap());
            assert_eq!(cached_side(&state, &key), expected_width, "{why}");
        }
    }

    /// The now-playing panel reads the album's cached picture, so a better picture cached through a
    /// SIBLING track must show up in its projection (`main.rs` re-projects when the playing track's
    /// `artwork_revision_of` moves), while a picture that does not replace anything must leave the
    /// revision alone.
    #[test]
    fn a_sibling_tracks_higher_tier_picture_changes_the_now_playing_art_and_the_revision() {
        let mut state = AppState::new();
        let playing = TrackKey::whole_file(PathBuf::from("/nas/album/cd1/01.flac"));
        let no_fallback = (String::new(), String::new());
        let playing_info = info("FLAC", 24, 44_100, false, 2, Some(10_000));
        state.apply_scanned(scanned_with_art("/nas/album/cd1/01.flac", ArtworkSource::Embedded, 2));
        let embedded_revision = state.artwork_revision_of(&playing);
        assert!(embedded_revision.is_some());
        assert_eq!(project_now_playing(&state, &playing, &playing_info, &no_fallback).art.unwrap().size().width, 2);

        // A lower tier, and an equal one from another folder of the same album (another contributor:
        // the first one cached stays), replace nothing.
        state.apply_scanned(scanned_with_art("/nas/album/cd1/03.flac", ArtworkSource::FolderFallback, 5));
        state.apply_scanned(scanned_with_art("/nas/album-wavpack/01.wv", ArtworkSource::Embedded, 4));
        assert_eq!(state.artwork_revision_of(&playing), embedded_revision, "neither changes the picture the panel shows");

        state.apply_scanned(scanned_with_art("/nas/album/cd2/01.flac", ArtworkSource::Folder, 6));

        assert_ne!(state.artwork_revision_of(&playing), embedded_revision, "the replacement must be observable");
        assert_eq!(
            project_now_playing(&state, &playing, &playing_info, &no_fallback).art.unwrap().size().width,
            6,
            "the playing track's album now shows the folder cover another track brought"
        );
    }

    /// The panel is only refreshed for the playing track's OWN album: a picture cached for another
    /// album never moves its revision, and a track that is no longer in the library (removed while it
    /// plays) has none even if its album's picture is still in the cache (`Library::remove_album`
    /// alone leaves the cache untouched; `AppState::remove_album` is what forgets it).
    #[test]
    fn artwork_revision_of_ignores_other_albums_and_tracks_removed_from_the_library() {
        let mut state = AppState::new();
        let playing = TrackKey::whole_file(PathBuf::from("/nas/album/01.flac"));
        assert_eq!(state.artwork_revision_of(&playing), None, "not in the library yet");
        state.apply_scanned(scanned_with_art("/nas/album/01.flac", ArtworkSource::Embedded, 2));
        let before = state.artwork_revision_of(&playing);
        assert!(before.is_some());

        let mut other = scanned_with_art("/nas/other/01.flac", ArtworkSource::Folder, 6);
        other.tags.album = Some("Another Album".into());
        state.apply_scanned(other);
        assert_eq!(state.artwork_revision_of(&playing), before, "another album's picture is not this track's business");

        let album = album_key(state.library.get(&playing).unwrap());
        assert_eq!(state.library.remove_album(&album), [playing.clone()]);
        assert!(state.artwork_for_key(&album).is_some(), "the removed album's picture stays cached");
        assert_eq!(state.artwork_revision_of(&playing), None, "a track outside the library has no album picture to refresh");
    }

    /// A picture that merely follows its album onto another key (`apply_scanned`'s migration) is the
    /// same picture, so the playing track's revision stays put; landing on a destination that already
    /// has a different picture is a change.
    #[test]
    fn artwork_revision_of_survives_a_key_migration_but_not_a_different_destination_picture() {
        for destination in [None, Some(ArtworkSource::Folder)] {
            let mut state = AppState::new();
            let track_a = TrackKey::whole_file(PathBuf::from("/nas/album/a.flac"));
            let track_b = TrackKey::whole_file(PathBuf::from("/nas/album/b.flac"));
            let mut scanned_a = TrackRecord::minimal(track_a.clone(), info("FLAC", 24, 44_100, false, 2, Some(10_000)));
            scanned_a.tags.artist = Some("Solo".into());
            scanned_a.tags.album = Some("Album".into());
            scanned_a.artwork = Some(ArtworkPixels { width: 2, height: 2, rgb: vec![255; 12] });
            scanned_a.artwork_source = ArtworkSource::Embedded;
            state.apply_scanned(scanned_a);
            let before = state.artwork_revision_of(&track_a);
            if let Some(source) = destination {
                state.cache_artwork("aa:compilation\u{1f}album", &ArtworkPixels { width: 6, height: 6, rgb: vec![128; 108] }, source, None);
            }

            // B's ALBUMARTIST moves A onto `aa:compilation\u{1f}album`.
            let mut scanned_b = TrackRecord::minimal(track_b, info("FLAC", 24, 44_100, false, 2, Some(10_000)));
            scanned_b.tags.artist = Some("Someone Else".into());
            scanned_b.tags.album_artist = Some("Compilation".into());
            scanned_b.tags.album = Some("Album".into());
            state.apply_scanned(scanned_b);

            assert_eq!(album_key(state.library.get(&track_a).unwrap()), "aa:compilation\u{1f}album");
            let after = state.artwork_revision_of(&track_a);
            assert!(after.is_some());
            assert_eq!(after == before, destination.is_none(), "destination picture: {destination:?}");
        }
    }

    /// A record of "Greatest Hits" in `folder`, by `artist`, with an optional `ALBUMARTIST` and an
    /// optional `side` px square picture of the given tier.
    fn greatest_hits(folder: &str, file: &str, artist: &str, album_artist: Option<&str>, art: Option<(ArtworkSource, u32)>) -> TrackRecord {
        let mut record = TrackRecord::minimal(TrackKey::whole_file(PathBuf::from(format!("{folder}/{file}"))), info("FLAC", 24, 44_100, false, 2, Some(10_000)));
        record.tags.artist = Some(artist.into());
        record.tags.album_artist = album_artist.map(str::to_owned);
        record.tags.album = Some("Greatest Hits".into());
        if let Some((source, side)) = art {
            record.artwork = Some(ArtworkPixels { width: side, height: side, rgb: vec![128; (side * side * 3) as usize] });
            record.artwork_source = source;
        }
        record
    }

    /// A compilation folder with no `ALBUMARTIST` starts as a one-track group, which resolves to that
    /// track's own artist and so, for a moment, to another album's `aa:artist|title` key. Its cover
    /// must not stay on that key once the group turns into "Various Artists": the other album gets its
    /// own picture back and the compilation keeps its cover, whatever order the tracks arrive in.
    #[test]
    fn a_compilations_transient_key_merge_never_takes_another_albums_picture() {
        for compilation_first_is_queen in [true, false] {
            let mut state = AppState::new();
            let queens_own = TrackKey::whole_file(PathBuf::from("/m/Queen/01.flac"));
            let compilation_queen = TrackKey::whole_file(PathBuf::from("/m/Compilation/01.flac"));
            state.apply_scanned(greatest_hits("/m/Queen", "01.flac", "Queen", Some("Queen"), Some((ArtworkSource::Embedded, 2))));

            let queen_track = greatest_hits("/m/Compilation", "01.flac", "Queen", None, Some((ArtworkSource::Folder, 6)));
            let abba_track = greatest_hits("/m/Compilation", "02.flac", "ABBA", None, None);
            if compilation_first_is_queen {
                state.apply_scanned(queen_track);
                state.apply_scanned(abba_track);
            } else {
                // Same folder, same approximate scanner key: the cover rides on whichever track came first.
                let mut abba_track = abba_track;
                abba_track.artwork = queen_track.artwork.clone();
                abba_track.artwork_source = queen_track.artwork_source;
                state.apply_scanned(abba_track);
                state.apply_scanned(TrackRecord { artwork: None, artwork_source: ArtworkSource::None, ..queen_track });
            }

            let queens_key = album_key(state.library.get(&queens_own).unwrap());
            let compilation_key = album_key(state.library.get(&compilation_queen).unwrap());
            assert_eq!(queens_key, "aa:queen\u{1f}greatest hits");
            assert!(compilation_key.starts_with("va:"), "the compilation must end up as Various Artists: {compilation_key}");
            assert_eq!(cached_side(&state, &queens_key), 2, "Queen's own album shows its own embedded picture again (queen first: {compilation_first_is_queen})");
            assert_eq!(cached_side(&state, &compilation_key), 6, "the compilation keeps its folder cover (queen first: {compilation_first_is_queen})");
        }
    }

    /// While a compilation's cover follows it off the shared key, the picture a track of the
    /// compilation is playing with must not go away: its revision never goes back to `None`.
    #[test]
    fn a_partial_key_move_never_blanks_the_playing_tracks_picture() {
        let mut state = AppState::new();
        let playing = TrackKey::whole_file(PathBuf::from("/m/Compilation/01.flac"));
        state.apply_scanned(greatest_hits("/m/Queen", "01.flac", "Queen", Some("Queen"), Some((ArtworkSource::Embedded, 2))));
        state.apply_scanned(greatest_hits("/m/Compilation", "01.flac", "Queen", None, Some((ArtworkSource::Folder, 6))));
        let before = state.artwork_revision_of(&playing);
        assert!(before.is_some());

        state.apply_scanned(greatest_hits("/m/Compilation", "02.flac", "ABBA", None, None));

        assert_eq!(state.artwork_revision_of(&playing), before, "the picture followed the playing track to its final key");
    }

    /// A cover replaced on disc with another image of the same tier (or a repaired `cover.jpg` taking
    /// over from the `front.jpg` that stood in for it) is picked up by the next scan of that album.
    #[test]
    fn a_later_scan_replaces_the_same_albums_picture_of_an_equal_tier() {
        let mut state = AppState::new();
        let track = TrackKey::whole_file(PathBuf::from("/m/Album/01.flac"));
        state.apply_scanned(greatest_hits("/m/Album", "01.flac", "Queen", Some("Queen"), Some((ArtworkSource::Folder, 6))));
        let key = album_key(state.library.get(&track).unwrap());
        let first = state.artwork_revision_of(&track);

        state.apply_scanned(greatest_hits("/m/Album", "01.flac", "Queen", Some("Queen"), Some((ArtworkSource::Folder, 8))));
        assert_eq!(cached_side(&state, &key), 8, "the newer cover of the same tier replaces the older one");
        assert_ne!(state.artwork_revision_of(&track), first);

        state.apply_scanned(greatest_hits("/m/Album", "01.flac", "Queen", Some("Queen"), Some((ArtworkSource::Embedded, 2))));
        assert_eq!(cached_side(&state, &key), 8, "a lower tier never replaces it, so a partial re-open cannot downgrade the album");
    }

    /// "Remove from Library" forgets the album's picture, so re-opening it starts from what is on disc
    /// now: a cover deleted meanwhile falls back to the embedded picture.
    #[test]
    fn removing_an_album_forgets_its_picture_so_a_reopen_starts_fresh() {
        let mut state = AppState::new();
        let track = TrackKey::whole_file(PathBuf::from("/m/Album/01.flac"));
        state.apply_scanned(greatest_hits("/m/Album", "01.flac", "Queen", Some("Queen"), Some((ArtworkSource::Folder, 6))));
        let key = album_key(state.library.get(&track).unwrap());

        assert_eq!(state.remove_album(&key), [track.clone()]);
        assert!(state.artwork_for_key(&key).is_none());

        state.apply_scanned(greatest_hits("/m/Album", "01.flac", "Queen", Some("Queen"), Some((ArtworkSource::Embedded, 2))));
        assert_eq!(cached_side(&state, &key), 2, "the deleted cover no longer outranks the embedded picture");
    }

    #[test]
    fn now_playing_projection_fills_panel_fields_and_has_lyrics_only_when_present() {
        let mut state = AppState::new();
        let path = TrackKey::whole_file(PathBuf::from("/nas/album/track.flac"));
        let mut record = TrackRecord::minimal(path.clone(), info("FLAC", 24, 44_100, false, 2, Some(10_000)));
        record.tags.title = Some("Real Title".into());
        record.tags.artist = Some("Real Artist".into());
        record.tags.album = Some("Real Album".into());
        record.tags.year = Some(2021);
        record.tags.genre = Some("Ambient".into());
        record.tags.lyrics = Some("La la la".into());
        record.file_size = Some(1_703_750);
        state.library.upsert(record);
        // The library-resolved key, not one computed from the record before it was upserted: only
        // the stored record's `effective_album_artist` is final (`EffectiveAlbumArtist`).
        let key = album_key(state.library.get(&path).unwrap());
        state.cache_artwork(&key, &ArtworkPixels { width: 2, height: 2, rgb: vec![255; 12] }, ArtworkSource::Embedded, None);
        let now_playing_info = info("FLAC", 24, 44_100, false, 2, Some(10_000));

        let projection = project_now_playing(&state, &path, &now_playing_info, &(String::new(), String::new()));

        assert_eq!(projection.title, "Real Title");
        assert_eq!(projection.artist, "Real Artist");
        assert_eq!(projection.album, "Real Album");
        assert_eq!(projection.year, "2021");
        assert_eq!(projection.genre, "Ambient");
        assert_eq!(projection.file_size_line, "1.7 MB");
        assert!(projection.art.is_some(), "cached artwork for the record's album key must be returned");
        assert_eq!(projection.lyrics, "La la la");
        assert!(projection.has_lyrics);
    }

    #[test]
    fn now_playing_projection_reports_no_lyrics_when_none_are_tagged() {
        let mut state = AppState::new();
        let path = TrackKey::whole_file(PathBuf::from("/nas/album/track.flac"));
        let record = TrackRecord::minimal(path.clone(), info("FLAC", 16, 44_100, false, 2, Some(5_000)));
        state.library.upsert(record);
        let now_playing_info = info("FLAC", 16, 44_100, false, 2, Some(5_000));

        let projection = project_now_playing(&state, &path, &now_playing_info, &(String::new(), String::new()));

        assert_eq!(projection.lyrics, "");
        assert!(!projection.has_lyrics);
    }

    #[test]
    fn now_playing_projection_falls_back_to_filename_fields_with_no_record() {
        let state = AppState::new();
        let path = TrackKey::whole_file(PathBuf::from("/nas/album/unknown.flac"));
        let fallback = ("Fallback Title".to_owned(), "Fallback Album".to_owned());
        let now_playing_info = info("FLAC", 16, 44_100, false, 2, Some(5_000));

        let projection = project_now_playing(&state, &path, &now_playing_info, &fallback);

        assert_eq!(projection.title, "Fallback Title");
        assert_eq!(projection.artist, "Unknown Artist");
        assert_eq!(projection.album, "Fallback Album");
        assert_eq!(projection.year, "");
        assert_eq!(projection.genre, "");
        assert_eq!(projection.file_size_line, "");
        assert!(projection.art.is_none());
        assert!(!projection.has_lyrics);
    }

    #[test]
    fn note_played_album_dedupes_most_recent_first_and_caps_at_twelve() {
        let mut state = AppState::new();
        for i in 0..14 {
            state.note_played_album(&format!("album-{i}"));
        }
        state.note_played_album("album-5");

        let recent = state.recent_album_keys();
        assert_eq!(recent.len(), 12, "the list is capped at 12");
        assert_eq!(recent[0], "album-5", "a re-played album moves back to the front");
        assert!(!recent[1..].contains(&"album-5".to_owned()), "no duplicate entry is kept");
    }
}

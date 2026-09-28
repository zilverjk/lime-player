//! UI-thread state: the session library and its decoded-artwork cache (Stage 3, `§3.1` partial),
//! `Navigation` (`§3.6`, Stage 5), and the now-playing/pending-volume pure helpers `§5.10`
//! describes. Pending seek stays in `main.rs`, since nothing outside `main.rs` needs it yet.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use slint::{Image, Rgb8Pixel, SharedPixelBuffer};

use crate::View;
use crate::audio::{AudioInfo, PreparedTrack};
use crate::library::format::{format_badge, format_file_size, format_line};
use crate::library::{ArtworkPixels, Library, TrackKey, TrackRecord, album_key, display_album, display_artist, display_title, track_key_string};

/// The back stack never grows past this many entries (`§3.6`): the oldest is dropped first.
const MAX_BACK_STACK: usize = 16;
/// "Jump back in" keeps at most this many recently played albums (`§5.10` "Recently played").
const MAX_RECENT_ALBUMS: usize = 12;

#[derive(Default)]
pub struct AppState {
    pub library: Library,
    art_cache: HashMap<String, Image>,
    /// Album keys played this session, most recent first, deduped (`§5.10` "Recently played").
    /// Feeds Home's "Jump back in" shelf (`view_model::project_jump_back_albums`, `§6` Stage 6).
    recent_album_keys: Vec<String>,
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Caches a decoded album picture under `key`, the first time one arrives for that album
    /// (`§3.3` "Artwork cache"). `slint::Image` never leaves the UI thread.
    pub fn cache_artwork(&mut self, key: &str, pixels: &ArtworkPixels) {
        if self.art_cache.contains_key(key) {
            return;
        }
        let buffer = SharedPixelBuffer::<Rgb8Pixel>::clone_from_slice(&pixels.rgb, pixels.width, pixels.height);
        self.art_cache.insert(key.to_owned(), Image::from_rgb8(buffer));
    }

    pub fn artwork_for_key(&self, key: &str) -> Option<Image> {
        self.art_cache.get(key).cloned()
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

    /// Applies a freshly scanned record (`LibraryEvent::Scanned`): caches its artwork, if any,
    /// under the album key, then upserts the record into the library (`§3.3`, `§3.4`).
    ///
    /// Phase-2 tags can change the path's album key, from the scanner's phase-1 `dir:<parent>`
    /// fallback to a tagged `aa:`/`af:` key (`§3.3` "Album grouping key"). Once every track that
    /// shared the old key has moved off it, this returns `Some((old_key, new_key))` so the caller
    /// can rewrite any stored reference to the old key — `Navigation`'s current entry and back
    /// stack (`Navigation::rekey_album`), and `recent_album_keys` — before it goes stale. Left
    /// unrewritten, a stale key resolves to no album at all: album detail goes blank (empty title,
    /// no tracks, no-op action buttons) and "Jump back in" silently drops the album (`§6` Stage 6
    /// fix).
    pub fn apply_scanned(&mut self, mut record: TrackRecord) -> Option<(String, String)> {
        let old_key = self.library.get(&record.key).map(album_key);
        let new_key = album_key(&record);
        if let Some(pixels) = record.artwork.take() {
            self.cache_artwork(&new_key, &pixels);
        }
        self.library.upsert(record);
        let rekey = match old_key {
            Some(old_key) if old_key != new_key && self.library.album(&old_key).is_none() => Some((old_key, new_key)),
            _ => None,
        };
        if let Some((old_key, new_key)) = &rekey {
            for existing in &mut self.recent_album_keys {
                if existing == old_key {
                    *existing = new_key.clone();
                }
            }
            // Rewriting can make two entries collide (the old and new key both already present);
            // keep only the first occurrence so "Jump back in" never shows the same album twice.
            let mut seen = std::collections::HashSet::new();
            self.recent_album_keys.retain(|key| seen.insert(key.clone()));
        }
        rekey
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

    #[test]
    fn cache_artwork_keeps_the_first_image_for_a_key() {
        let mut state = AppState::new();
        let first = ArtworkPixels { width: 2, height: 2, rgb: vec![255; 12] };
        let second = ArtworkPixels { width: 4, height: 4, rgb: vec![0; 48] };

        state.cache_artwork("album-1", &first);
        state.cache_artwork("album-1", &second);

        let cached = state.artwork_for_key("album-1").expect("the first image should be cached");
        let size = cached.size();
        assert_eq!((size.width, size.height), (2, 2), "a later image for the same key must not replace the first");
        assert!(state.artwork_for_key("album-2").is_none());
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

        // Phase 2: real tags arrive and move the same path onto an `af:`/`aa:` key.
        let mut scanned = TrackRecord::minimal(path.clone(), info("FLAC", 24, 44_100, false, 2, Some(10_000)));
        scanned.tags.title = Some("Real Title".into());
        scanned.tags.artist = Some("Real Artist".into());
        scanned.tags.album = Some("Real Album".into());
        let new_key = album_key(&scanned);
        assert_ne!(old_key, new_key, "the scan must actually change the album key for this test to be meaningful");

        let rekey = state.apply_scanned(scanned);

        assert_eq!(rekey, Some((old_key, new_key.clone())));
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

        assert_eq!(rekey, None);
        assert_eq!(state.recent_album_keys(), [old_key]);
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
        let key = album_key(&record);
        state.cache_artwork(&key, &ArtworkPixels { width: 2, height: 2, rgb: vec![255; 12] });
        state.library.upsert(record);
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

//! Persisted library sources (`CLAUDE.md` "Persistent library"): the set of individually opened
//! files and added folders, stored as `library.json` next to `settings.json` in the same
//! `ProjectDirs` config directory. Loaded at startup and re-scanned through the existing scanner
//! thread (`scanner::LibraryScanner`) to rebuild the session library — the *decoded* library
//! (`Library`/`TrackRecord`, tags, artwork) itself is still never persisted, only which paths to
//! re-scan (`main.rs`'s `BatchKind::StartupRestore`).

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use super::TrackKey;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct LibrarySources {
    /// Individually opened files ("Open Files…"), one entry per file.
    pub files: Vec<PathBuf>,
    /// Added folders ("Open Folder…"), re-walked at every startup so new files placed in them
    /// since the last run show up automatically.
    pub folders: Vec<PathBuf>,
    /// Individually opened `.cue` sheets ("Open Files…", a `.cue` picked directly): re-scanned
    /// (never re-enqueued) at the next startup so its own folder's cue expansion runs again,
    /// exactly like a saved `files`/`folders` entry. `#[serde(default)]` (on the whole struct
    /// above) so a `library.json` written before this field existed still loads: only the *source*
    /// paths persist here, never the cue-derived `TrackKey`/`TrackRecord`s themselves, which are
    /// always rebuilt by rescanning.
    pub cue_files: Vec<PathBuf>,
    /// "Remove from Library" exclusions (`CLAUDE.md` "Library exclusions"): every `TrackKey` a user
    /// has explicitly removed from the session library, so a later rescan of `files`/`folders`/
    /// `cue_files` (chiefly the startup restore) skips them instead of silently bringing them back.
    /// Keyed on the exact `TrackKey` (path + sample-frame sub-range), not a bare path, so removing
    /// one CUE sub-range track never excludes its siblings that share the same physical file.
    /// `#[serde(default)]` (on the whole struct above) so a `library.json` written before this field
    /// existed still loads, defaulting to an empty set. Removal itself never touches a file on
    /// disk — only this in-memory/persisted set of keys to skip on the next scan.
    pub excluded_tracks: HashSet<TrackKey>,
}

impl LibrarySources {
    /// Adds `path` if it is not already present (exact-path de-dup — the same file opened twice,
    /// or a file also covered by an already-added folder, must not duplicate this list;
    /// `Library::upsert` separately de-dupes the *scanned* library by path, so a path present in
    /// both `files` and a folder's eventual walk results still ends up as a single library entry).
    pub fn add_file(&mut self, path: PathBuf) {
        if !self.files.contains(&path) {
            self.files.push(path);
        }
    }

    /// Adds `path` if it is not already present (same de-dup as `add_file`).
    pub fn add_folder(&mut self, path: PathBuf) {
        if !self.folders.contains(&path) {
            self.folders.push(path);
        }
    }

    /// Adds a directly-opened `.cue` sheet path if it is not already present (same de-dup as
    /// `add_file`).
    pub fn add_cue_file(&mut self, path: PathBuf) {
        if !self.cue_files.contains(&path) {
            self.cue_files.push(path);
        }
    }

    /// Records `keys` as excluded ("Remove from Library", `CLAUDE.md` "Library exclusions") so a
    /// later rescan of an already-persisted `files`/`folders`/`cue_files` entry skips them.
    pub fn exclude_tracks(&mut self, keys: impl IntoIterator<Item = TrackKey>) {
        self.excluded_tracks.extend(keys);
    }

    pub fn is_excluded(&self, key: &TrackKey) -> bool {
        self.excluded_tracks.contains(key)
    }

    /// Lifts the exclusion for every `TrackKey` whose path is exactly `path` — called when the user
    /// explicitly re-opens that same file via "Open Files…", which must override a prior removal
    /// (`CLAUDE.md` "Library exclusions"): an explicit user action always wins over a past removal.
    /// A no-op if nothing under `path` was excluded. Returns whether anything was actually lifted,
    /// purely for tests; callers that don't need it can ignore the result.
    pub fn include_path(&mut self, path: &Path) -> bool {
        let before = self.excluded_tracks.len();
        self.excluded_tracks.retain(|key| key.path != path);
        self.excluded_tracks.len() != before
    }

    /// Lifts the exclusion for every `TrackKey` whose path is `folder` or lives under it — called
    /// when the user explicitly re-adds that folder via "Open Folder…", the same override rule as
    /// `include_path` above.
    pub fn include_folder(&mut self, folder: &Path) -> bool {
        let before = self.excluded_tracks.len();
        self.excluded_tracks.retain(|key| !key.path.starts_with(folder));
        self.excluded_tracks.len() != before
    }

    /// Lifts the exclusion for every `TrackKey` a re-opened `.cue` sheet at `cue_path` resolves to
    /// (`CLAUDE.md` "Library exclusions"). A CUE-derived `TrackKey.path` is the underlying *audio*
    /// file the sheet points to (e.g. `album.flac`), never the `.cue` path itself — so re-opening
    /// `album.cue` through "Open Files…" and calling plain `include_path(cue_path)` on it can never
    /// match those keys, permanently stranding previously-removed CUE tracks even though the user
    /// explicitly re-opened the sheet that names them.
    ///
    /// Fix approach: parse and resolve the sheet the same way the scanner does
    /// (`library::cue::{parse_cue, resolve_cue_files}`), then lift exactly the resolved audio
    /// path(s) *this* sheet claims. This was chosen over a cheaper "lift everything under
    /// `cue_path.parent()`" heuristic because that heuristic is imprecise: two different `.cue`
    /// sheets commonly live in the same folder (e.g. two rips of the same session), and a
    /// parent-directory sweep would also lift an unrelated sheet's legitimately-still-excluded
    /// tracks. Reusing the existing parse/resolve code keeps this exact and consistent with how the
    /// scanner itself maps a `.cue` to its `TrackKey`s.
    ///
    /// On any read/parse/resolve failure (unreadable, malformed, or unresolvable sheet) this is a
    /// no-op returning `false` — a corrupt re-opened cue sheet must not panic here; the scanner
    /// separately reports the same failure when it (re)scans the path.
    pub fn include_cue_sheet(&mut self, cue_path: &Path) -> bool {
        let Ok(bytes) = fs::read(cue_path) else { return false };
        let Ok(sheet) = super::cue::parse_cue(&bytes) else { return false };
        let Ok(resolved) = super::cue::resolve_cue_files(cue_path, &sheet) else { return false };
        let mut lifted = false;
        for file in &resolved {
            if self.include_path(&file.resolved_path) {
                lifted = true;
            }
        }
        lifted
    }

    /// Loads `library.json`. A missing file is not an error (first run): `Ok(Self::default())`. A
    /// present-but-corrupt file returns `Err` so the caller can report it — and, crucially, the
    /// caller must not then save a fresh default over it, or whatever was still readable in it is
    /// lost for good.
    pub fn load() -> Result<Self, String> {
        let Some(path) = library_path() else { return Ok(Self::default()) };
        load_from(&path)
    }

    /// Writes `library.json` atomically: a temp file in the same directory, then `rename` — a
    /// crash or power loss mid-write can never leave a half-written `library.json` behind, since a
    /// same-filesystem `rename` is atomic and lands either the old or the new content.
    pub fn save(&self) -> Result<(), String> {
        let path = library_path().ok_or_else(|| "could not determine the settings directory".to_owned())?;
        save_to(&path, self)
    }
}

fn library_path() -> Option<PathBuf> {
    ProjectDirs::from("com", "Lime Player", "Lime Player").map(|project| project.config_dir().join("library.json"))
}

fn load_from(path: &Path) -> Result<LibrarySources, String> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| format!("{}: {error}", path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(LibrarySources::default()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn save_to(path: &Path, sources: &LibrarySources) -> Result<(), String> {
    let parent = path.parent().ok_or_else(|| "could not determine the settings directory".to_owned())?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let bytes = serde_json::to_vec_pretty(sources).map_err(|error| error.to_string())?;
    // Unique per-process temp name: two instances (or two rapid saves) racing would otherwise
    // clobber each other's in-progress temp file before either `rename` runs.
    let tmp_path = parent.join(format!(".library.json.{}.tmp", std::process::id()));
    fs::write(&tmp_path, &bytes).map_err(|error| error.to_string())?;
    fs::rename(&tmp_path, path).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let suffix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("lime-player-library-store-{name}-{}-{suffix}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn round_trips_through_save_and_load() {
        let dir = temp_dir("round-trip");
        let path = dir.join("library.json");
        let mut sources = LibrarySources::default();
        sources.add_file(PathBuf::from("/Volumes/nas/song.flac"));
        sources.add_folder(PathBuf::from("/Volumes/nas/Album"));
        sources.add_cue_file(PathBuf::from("/Volumes/nas/Album/album.cue"));

        save_to(&path, &sources).unwrap();
        let loaded = load_from(&path).unwrap();

        assert_eq!(loaded, sources);
        assert_eq!(loaded.cue_files, vec![PathBuf::from("/Volumes/nas/Album/album.cue")]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn add_cue_file_deduplicates_exact_paths() {
        let mut sources = LibrarySources::default();
        sources.add_cue_file(PathBuf::from("/music/Album/album.cue"));
        sources.add_cue_file(PathBuf::from("/music/Album/album.cue"));

        assert_eq!(sources.cue_files, vec![PathBuf::from("/music/Album/album.cue")]);
    }

    /// `#[serde(default)]` on `LibrarySources` must let a `library.json` written before
    /// `cue_files` existed still load, with `cue_files` defaulting to empty instead of failing to
    /// parse (`CLAUDE.md` CUE-sheet playback support, §7).
    #[test]
    fn a_library_json_without_cue_files_still_loads() {
        let dir = temp_dir("pre-cue-files");
        let path = dir.join("library.json");
        std::fs::write(&path, br#"{"files":["/music/a.flac"],"folders":["/music/Album"]}"#).unwrap();

        let loaded = load_from(&path).unwrap();

        assert_eq!(loaded.files, vec![PathBuf::from("/music/a.flac")]);
        assert_eq!(loaded.folders, vec![PathBuf::from("/music/Album")]);
        assert!(loaded.cue_files.is_empty(), "an old library.json with no cue_files field must default to empty, not fail to load");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_is_atomic_and_leaves_no_temp_file_behind() {
        let dir = temp_dir("atomic");
        let path = dir.join("library.json");
        let mut sources = LibrarySources::default();
        sources.add_file(PathBuf::from("/music/a.flac"));

        save_to(&path, &sources).unwrap();

        let leftover: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name())
            .filter(|name| name.to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftover.is_empty(), "no .tmp file must remain after a successful save");
        assert!(path.exists());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_file_loads_as_default_not_an_error() {
        let dir = temp_dir("missing-file");
        let path = dir.join("library.json");

        let loaded = load_from(&path).unwrap();

        assert_eq!(loaded, LibrarySources::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_path_missing_on_disk_is_retained_across_a_round_trip() {
        // The whole point of persisting sources separately from the scanned library: a file or
        // folder that is temporarily unreachable (NAS not mounted) must not be dropped just
        // because it could not be scanned this run — `LibrarySources` never checks path existence.
        let dir = temp_dir("missing-path");
        let path = dir.join("library.json");
        let mut sources = LibrarySources::default();
        sources.add_file(PathBuf::from("/Volumes/not-currently-mounted/song.flac"));

        save_to(&path, &sources).unwrap();
        let loaded = load_from(&path).unwrap();

        assert_eq!(loaded.files, vec![PathBuf::from("/Volumes/not-currently-mounted/song.flac")]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn add_file_and_add_folder_deduplicate_exact_paths() {
        let mut sources = LibrarySources::default();
        sources.add_file(PathBuf::from("/music/a.flac"));
        sources.add_file(PathBuf::from("/music/a.flac"));
        sources.add_folder(PathBuf::from("/music/Album"));
        sources.add_folder(PathBuf::from("/music/Album"));

        assert_eq!(sources.files, vec![PathBuf::from("/music/a.flac")]);
        assert_eq!(sources.folders, vec![PathBuf::from("/music/Album")]);
    }

    /// Exclusion persistence round-trip (`CLAUDE.md` "Library exclusions"): a saved set of excluded
    /// `TrackKey`s (including a CUE sub-range key) survives a save/load cycle intact.
    #[test]
    fn excluded_tracks_round_trip_through_save_and_load() {
        let dir = temp_dir("excluded-round-trip");
        let path = dir.join("library.json");
        let mut sources = LibrarySources::default();
        sources.exclude_tracks([
            TrackKey::whole_file(PathBuf::from("/music/removed.flac")),
            TrackKey { path: PathBuf::from("/music/album.wv"), start_frame: 1_000, end_frame: Some(2_000) },
        ]);

        save_to(&path, &sources).unwrap();
        let loaded = load_from(&path).unwrap();

        assert_eq!(loaded, sources);
        assert_eq!(loaded.excluded_tracks.len(), 2);
        assert!(loaded.is_excluded(&TrackKey::whole_file(PathBuf::from("/music/removed.flac"))));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `#[serde(default)]` must also cover `excluded_tracks`: a `library.json` written before this
    /// field existed (no `excluded_tracks` key at all) still loads, defaulting to an empty set
    /// (`CLAUDE.md` "Library exclusions").
    #[test]
    fn a_library_json_without_excluded_tracks_still_loads() {
        let dir = temp_dir("pre-excluded-tracks");
        let path = dir.join("library.json");
        std::fs::write(&path, br#"{"files":["/music/a.flac"],"folders":[],"cue_files":[]}"#).unwrap();

        let loaded = load_from(&path).unwrap();

        assert_eq!(loaded.files, vec![PathBuf::from("/music/a.flac")]);
        assert!(loaded.excluded_tracks.is_empty(), "an old library.json with no excluded_tracks field must default to empty, not fail to load");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Explicitly re-opening the same file lifts its exclusion (`CLAUDE.md` "Library exclusions"):
    /// an unrelated excluded track (a different path) is left untouched.
    #[test]
    fn include_path_lifts_exclusion_for_that_exact_path_only() {
        let mut sources = LibrarySources::default();
        let removed = TrackKey::whole_file(PathBuf::from("/music/a.flac"));
        let other = TrackKey::whole_file(PathBuf::from("/music/b.flac"));
        sources.exclude_tracks([removed.clone(), other.clone()]);

        let lifted = sources.include_path(Path::new("/music/a.flac"));

        assert!(lifted);
        assert!(!sources.is_excluded(&removed));
        assert!(sources.is_excluded(&other), "an unrelated excluded path must stay excluded");
    }

    /// Re-adding a folder lifts the exclusion for every track under it, including a CUE sub-range
    /// track of a file inside that folder, but not a track in a sibling folder.
    #[test]
    fn include_folder_lifts_exclusion_for_every_track_under_it() {
        let mut sources = LibrarySources::default();
        let inside_whole = TrackKey::whole_file(PathBuf::from("/music/Album/a.flac"));
        let inside_cue_range = TrackKey { path: PathBuf::from("/music/Album/live.wv"), start_frame: 500, end_frame: Some(900) };
        let outside = TrackKey::whole_file(PathBuf::from("/music/Other/b.flac"));
        sources.exclude_tracks([inside_whole.clone(), inside_cue_range.clone(), outside.clone()]);

        let lifted = sources.include_folder(Path::new("/music/Album"));

        assert!(lifted);
        assert!(!sources.is_excluded(&inside_whole));
        assert!(!sources.is_excluded(&inside_cue_range));
        assert!(sources.is_excluded(&outside), "a track outside the re-added folder must stay excluded");
    }

    #[test]
    fn include_path_and_include_folder_are_no_ops_when_nothing_matches() {
        let mut sources = LibrarySources::default();
        sources.exclude_tracks([TrackKey::whole_file(PathBuf::from("/music/a.flac"))]);

        assert!(!sources.include_path(Path::new("/music/does-not-exist.flac")));
        assert!(!sources.include_folder(Path::new("/music/DoesNotExist")));
        assert_eq!(sources.excluded_tracks.len(), 1);
    }

    /// Regression test for the CRITICAL fix: re-opening a `.cue` file must lift the exclusion for
    /// the CUE-derived `TrackKey`s it resolves to, even though `key.path` is the underlying audio
    /// file, never the `.cue` path itself (`include_cue_sheet`'s doc comment). Before this fix,
    /// `main.rs` called plain `include_path(cue_path)`, which could never match a CUE-derived key
    /// and left the tracks excluded forever.
    #[test]
    fn include_cue_sheet_lifts_exclusion_for_the_tracks_it_resolves_to() {
        let dir = temp_dir("include-cue-sheet");
        let audio_path = dir.join("album.flac");
        std::fs::write(&audio_path, b"x").unwrap();
        let cue_path = dir.join("album.cue");
        std::fs::write(
            &cue_path,
            b"FILE \"album.flac\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n  TRACK 02 AUDIO\n    INDEX 01 03:00:00\n",
        )
        .unwrap();

        let mut sources = LibrarySources::default();
        let removed = TrackKey { path: audio_path.clone(), start_frame: 0, end_frame: Some(1_000) };
        let other = TrackKey::whole_file(PathBuf::from("/music/unrelated.flac"));
        sources.exclude_tracks([removed.clone(), other.clone()]);

        let lifted = sources.include_cue_sheet(&cue_path);

        assert!(lifted);
        assert!(!sources.is_excluded(&removed), "the CUE-derived track must no longer be excluded after re-opening its .cue");
        assert!(sources.is_excluded(&other), "an unrelated excluded track must stay excluded");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `include_cue_sheet` must not panic or lift anything for a cue path that can't be read (e.g.
    /// already deleted from disk since it was picked in the file dialog).
    #[test]
    fn include_cue_sheet_is_a_no_op_when_the_cue_file_cannot_be_read() {
        let mut sources = LibrarySources::default();
        let removed = TrackKey::whole_file(PathBuf::from("/music/album.flac"));
        sources.exclude_tracks([removed.clone()]);

        let lifted = sources.include_cue_sheet(Path::new("/music/does-not-exist.cue"));

        assert!(!lifted);
        assert!(sources.is_excluded(&removed));
    }

    #[test]
    fn corrupt_file_reports_an_error_instead_of_silently_defaulting() {
        let dir = temp_dir("corrupt");
        let path = dir.join("library.json");
        std::fs::write(&path, b"{ not valid json").unwrap();

        let result = load_from(&path);

        assert!(result.is_err(), "corrupt JSON must be reported, not silently defaulted");
        // `load_from` never writes on failure; the corrupt file must be untouched.
        let bytes_after = std::fs::read(&path).unwrap();
        assert_eq!(bytes_after, b"{ not valid json");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

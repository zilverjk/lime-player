//! Persisted library sources (`CLAUDE.md` "Persistent library"): the set of individually opened
//! files and added folders, stored as `library.json` next to `settings.json` in the same
//! `ProjectDirs` config directory. Loaded at startup and re-scanned through the existing scanner
//! thread (`scanner::LibraryScanner`) to rebuild the session library — the *decoded* library
//! (`Library`/`TrackRecord`, tags, artwork) itself is still never persisted, only which paths to
//! re-scan (`main.rs`'s `BatchKind::StartupRestore`).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

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

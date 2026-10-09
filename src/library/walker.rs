//! Recursive folder walk for "Open Folder…" and the startup library-restore rescan of a saved
//! folder (`CLAUDE.md` "Persistent library"). Runs entirely off the UI thread — the caller
//! (`scanner::LibraryScanner::scan_folder`) spawns a dedicated thread for it — and is
//! cancellation-safe: the walk polls a shared flag between directories so a shutdown does not wait
//! for a slow NAS walk to finish (best-effort only, matching the decoder-thread shutdown note in
//! `audio/player.rs`: a walk blocked in a single non-interruptible `read_dir`/`metadata` syscall
//! cannot be interrupted).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// The same extensions "Open Files…" accepts (`main.rs`'s `pick_audio_files`), case-insensitive.
pub const AUDIO_EXTENSIONS: [&str; 5] = ["flac", "wv", "wavpack", "wav", "mp3"];

pub fn has_audio_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| AUDIO_EXTENSIONS.iter().any(|candidate| candidate.eq_ignore_ascii_case(ext)))
}

/// The `.cue` file extension, case-insensitive (`has_cue_extension`). Kept apart from
/// `AUDIO_EXTENSIONS` deliberately: a `.cue` sheet is never itself a decodable audio stream
/// (`audio::probe_file` must never be asked to probe one; this recursive walk itself also never
/// includes cue files in the audio list it returns), it only *describes* one or more real audio
/// files elsewhere in the same directory. `library::scanner`'s own per-directory cue detection
/// (`CLAUDE.md` CUE-sheet playback support) does a separate, non-recursive `read_dir` for these,
/// keyed off the directories its requested audio paths already live in — so a folder scan need not
/// carry cue paths through this walker's own results for cue expansion to find them.
pub const CUE_EXTENSION: &str = "cue";

/// Whether `path`'s extension is `.cue`, case-insensitive — the same convention as
/// `has_audio_extension`.
pub fn has_cue_extension(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| ext.eq_ignore_ascii_case(CUE_EXTENSION))
}

/// A hidden dotfile/dotdir, or a macOS AppleDouble sidecar (`._Foo.flac`) that a NAS share copied
/// from an HFS+/APFS volume commonly carries next to the real file — both are named starting with
/// `.`, so one check covers both.
fn is_hidden_or_appledouble(name: &str) -> bool {
    name.starts_with('.')
}

/// A directory whose name ends in `.wv` (case-insensitive) is skipped entirely, never descended
/// into — real NAS libraries can contain one alongside ordinary `.wv` files, and it must not be
/// walked as if it were a WavPack file or an ordinary folder.
fn is_skipped_dir_name(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".wv")
}

/// Recursively walks `root`, returning every file with a recognized audio extension in a stable
/// (name-sorted) order. `cancel` is polled before each directory and each entry; `on_progress(n)`
/// is called with the running total every time a file is added, so the caller can report live
/// progress without waiting for the whole walk to finish. A missing or unreadable `root` simply
/// yields an empty result rather than an error or a panic.
#[cfg(test)]
pub fn walk_folder(root: &Path, cancel: &AtomicBool, on_progress: impl FnMut(usize)) -> Vec<PathBuf> {
    walk_folder_checked(root, cancel, on_progress).files
}

/// What a walk found, and whether it can be trusted to be the folder's COMPLETE contents.
pub struct WalkOutcome {
    pub files: Vec<PathBuf>,
    /// `true` only when every directory was listed without an I/O error and the walk was not
    /// cancelled. A missing file may be treated as deleted (`cache` pruning) only if this holds: a
    /// directory that failed to list (a NAS share timing out or reconnecting mid-walk) silently
    /// drops its whole subtree from `files`.
    pub complete: bool,
    /// Directories whose listing came back with no entries at all and no error. A flaky share can
    /// answer a directory it cannot serve with an empty listing, so the library cache never treats
    /// the files it holds below such a directory as proven gone.
    pub empty_dirs: Vec<PathBuf>,
}

/// `walk_folder` plus the completeness flag the persistent library cache needs.
pub fn walk_folder_checked(root: &Path, cancel: &AtomicBool, mut on_progress: impl FnMut(usize)) -> WalkOutcome {
    let mut found = Vec::new();
    let mut visited: HashSet<PathBuf> = HashSet::new();
    visited.insert(root.canonicalize().unwrap_or_else(|_| root.to_path_buf()));
    let mut complete = true;
    let mut empty_dirs = Vec::new();
    walk_dir(root, &mut visited, cancel, &mut found, &mut on_progress, &mut complete, &mut empty_dirs);
    WalkOutcome { files: found, complete: complete && !cancel.load(Ordering::Acquire), empty_dirs }
}

/// Whether `walk_folder(root)` would have visited `path` had it existed: an audio-extension file
/// below `root` with no hidden component and no directory named `*.wv` on the way. A cached track
/// outside this set (a hidden file opened individually, a file inside a `*.wv` directory reached
/// through a cue sheet) must never be judged missing because a walk did not list it.
pub fn is_reachable_by_walk(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else { return false };
    if !has_audio_extension(path) {
        return false;
    }
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let Some(name) = component.as_os_str().to_str() else { return false };
        if is_hidden_or_appledouble(name) {
            return false;
        }
        let is_directory = components.peek().is_some();
        if is_directory && is_skipped_dir_name(name) {
            return false;
        }
    }
    true
}

fn walk_dir(
    dir: &Path,
    visited: &mut HashSet<PathBuf>,
    cancel: &AtomicBool,
    found: &mut Vec<PathBuf>,
    on_progress: &mut impl FnMut(usize),
    complete: &mut bool,
    empty_dirs: &mut Vec<PathBuf>,
) {
    if cancel.load(Ordering::Acquire) {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        *complete = false;
        return;
    };
    let mut entries: Vec<_> = entries
        .filter_map(|entry| match entry {
            Ok(entry) => Some(entry),
            Err(_) => {
                *complete = false;
                None
            }
        })
        .collect();
    if entries.is_empty() && *complete {
        empty_dirs.push(dir.to_path_buf());
    }
    // Deterministic order: reproducible tests and a stable-looking progress count, not a
    // correctness requirement.
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if cancel.load(Ordering::Acquire) {
            return;
        }
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else { continue };
        if is_hidden_or_appledouble(name_str) {
            continue;
        }
        let path = entry.path();
        // `fs::metadata` follows symlinks (unlike `DirEntry::file_type()`, which reports the link
        // itself), so a symlinked file or directory is classified by what it actually points to.
        // An unreadable or broken entry (a dangling symlink, a permission error) is skipped rather
        // than failing the whole walk.
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                // A dangling symlink is just not there; anything else (a timeout, a permission
                // error) means this entry's presence is unknown.
                if error.kind() != std::io::ErrorKind::NotFound {
                    *complete = false;
                }
                continue;
            }
        };
        if metadata.is_dir() {
            if is_skipped_dir_name(name_str) {
                continue;
            }
            let real_path = path.canonicalize().unwrap_or_else(|_| path.clone());
            // Already on this walk's path (a symlink loop, direct or indirect): skip instead of
            // recursing forever.
            if !visited.insert(real_path) {
                continue;
            }
            walk_dir(&path, visited, cancel, found, on_progress, complete, empty_dirs);
        } else if metadata.is_file() && has_audio_extension(&path) {
            found.push(path);
            on_progress(found.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let suffix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("lime-player-walker-{name}-{}-{suffix}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(path: &Path) {
        std::fs::write(path, b"x").unwrap();
    }

    fn walk(root: &Path) -> Vec<PathBuf> {
        let cancel = AtomicBool::new(false);
        walk_folder(root, &cancel, |_| {})
    }

    #[test]
    fn finds_nested_files_with_recognized_extensions_case_insensitively() {
        let dir = temp_dir("nested");
        std::fs::create_dir_all(dir.join("Artist/Album")).unwrap();
        touch(&dir.join("a.flac"));
        touch(&dir.join("Artist/Album/b.WV"));
        touch(&dir.join("Artist/Album/c.MP3"));
        touch(&dir.join("Artist/Album/readme.txt"));

        let found = walk(&dir);

        let names: Vec<String> = found.iter().map(|p| p.strip_prefix(&dir).unwrap().display().to_string()).collect();
        assert_eq!(found.len(), 3, "found {names:?}");
        assert!(names.contains(&"a.flac".to_string()));
        assert!(names.contains(&"Artist/Album/b.WV".to_string()));
        assert!(names.contains(&"Artist/Album/c.MP3".to_string()));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn skips_hidden_files_and_appledouble_sidecars() {
        let dir = temp_dir("hidden");
        touch(&dir.join("real.flac"));
        touch(&dir.join("._real.flac"));
        touch(&dir.join(".hidden.flac"));
        std::fs::create_dir_all(dir.join(".hidden-dir")).unwrap();
        touch(&dir.join(".hidden-dir/inside.flac"));

        let found = walk(&dir);

        assert_eq!(found, vec![dir.join("real.flac")]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn skips_directories_named_like_a_wavpack_file() {
        let dir = temp_dir("wv-dir");
        std::fs::create_dir_all(dir.join("weird.wv")).unwrap();
        touch(&dir.join("weird.wv/inside.flac"));
        touch(&dir.join("normal.wv")); // a real file named like a WavPack track: must still count

        let found = walk(&dir);

        assert_eq!(found, vec![dir.join("normal.wv")]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn extension_matching_is_case_insensitive_for_every_accepted_format() {
        let dir = temp_dir("ext-case");
        for (i, ext) in ["FLAC", "Wv", "WAVPACK", "Wav", "Mp3"].iter().enumerate() {
            touch(&dir.join(format!("track{i}.{ext}")));
        }

        let found = walk(&dir);

        assert_eq!(found.len(), 5);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cue_extension_is_recognized_case_insensitively_and_never_as_audio() {
        for name in ["album.cue", "Album.CUE", "album.Cue"] {
            let path = Path::new(name);
            assert!(has_cue_extension(path), "{name} should be recognized as a cue sheet");
            assert!(!has_audio_extension(path), "a .cue file must never be treated as a decodable audio file");
        }
        assert!(!has_cue_extension(Path::new("track.flac")), "an audio file must never be treated as a cue sheet");
    }

    #[test]
    #[cfg(unix)]
    fn does_not_follow_a_symlink_loop() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("symlink-loop");
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        touch(&dir.join("a/song.flac"));
        // `a/b/loop` points back to `a`, a directory already on this walk's path.
        symlink(dir.join("a"), dir.join("a/b/loop")).unwrap();

        // Must terminate (the test itself would hang otherwise) and still find the one real file
        // exactly once, not once per loop iteration.
        let found = walk(&dir);

        assert_eq!(found, vec![dir.join("a/song.flac")]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cancellation_stops_the_walk() {
        let dir = temp_dir("cancel");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        touch(&dir.join("a.flac"));
        touch(&dir.join("sub/b.flac"));

        let cancel = AtomicBool::new(true); // already cancelled before the walk starts
        let found = walk_folder(&dir, &cancel, |_| {});

        assert!(found.is_empty(), "a pre-cancelled walk must find nothing");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_root_returns_empty_without_panicking() {
        let dir = temp_dir("missing").join("does-not-exist");
        let found = walk(&dir);
        assert!(found.is_empty());
    }

    #[test]
    fn a_complete_walk_is_flagged_complete_and_an_unreadable_root_is_not() {
        let dir = temp_dir("complete");
        touch(&dir.join("a.flac"));
        let cancel = AtomicBool::new(false);
        let outcome = walk_folder_checked(&dir, &cancel, |_| {});
        assert!(outcome.complete);
        assert_eq!(outcome.files.len(), 1);

        let missing = walk_folder_checked(&dir.join("not-there"), &cancel, |_| {});
        assert!(!missing.complete, "a root that cannot be listed must not count as an empty folder");

        cancel.store(true, Ordering::Release);
        assert!(!walk_folder_checked(&dir, &cancel, |_| {}).complete, "a cancelled walk is never complete");
    }

    #[test]
    fn reachability_matches_what_the_walk_would_visit() {
        let root = Path::new("/m");
        assert!(is_reachable_by_walk(root, Path::new("/m/Artist/Album/01.FLAC")));
        assert!(!is_reachable_by_walk(root, Path::new("/m/Artist/.hidden/01.flac")));
        assert!(!is_reachable_by_walk(root, Path::new("/m/Artist/._01.flac")));
        assert!(!is_reachable_by_walk(root, Path::new("/m/Disc.wv/01.flac")));
        assert!(is_reachable_by_walk(root, Path::new("/m/Artist/track.wv")), "a .wv FILE is walked");
        assert!(!is_reachable_by_walk(root, Path::new("/m/Artist/notes.txt")));
        assert!(!is_reachable_by_walk(root, Path::new("/elsewhere/01.flac")));
    }

    #[test]
    fn directories_that_list_empty_are_reported() {
        let dir = temp_dir("empty-dirs");
        std::fs::create_dir_all(dir.join("Empty")).unwrap();
        std::fs::create_dir_all(dir.join("Full")).unwrap();
        touch(&dir.join("Full/a.flac"));
        let outcome = walk_folder_checked(&dir, &AtomicBool::new(false), |_| {});
        assert_eq!(outcome.empty_dirs, vec![dir.join("Empty")]);
    }
}

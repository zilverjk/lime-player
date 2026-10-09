//! Dev-only diagnosis harness: rebuilds the session library from a real `library.json` WITHOUT the UI
//! and prints every album the Albums grid would show.
//!
//! It follows the app's startup restore (`main.rs`): `files` and `cue_files` go through
//! `LibraryScanner::scan`, every folder through `LibraryScanner::scan_folder`, all with
//! `explicit_selection = false` and the persisted exclusion set; `Probed`/`Scanned` events are
//! filtered against that set exactly like `main.rs` does, then fed to `AppState::apply_probed` /
//! `apply_scanned` (which call `Library::upsert`) in arrival order.
//!
//! Safety: uses `LibraryScanner::new()` only (never repairs or writes a file), never constructs an
//! `AudioPlayer` or `MediaSession`, and only READS `library.json`.
//!
//! ```sh
//! LIME_LIBRARY_DUMP=<library.json> [LIME_LIBRARY_DUMP_OUT=<dir>] \
//!   cargo test --bin lime-player dump_real_library -- --ignored --nocapture
//! ```
//!
//! With `LIME_LIBRARY_DUMP_OUT` set it also writes `albums.tsv` (one row per album), `tracks.tsv`
//! (one row per visible track with its raw tags and album key) and `hidden.tsv` (one row per hidden
//! duplicate copy and the track it hides behind) into that directory, for diffing before/after a
//! grouping change. The console output ends with the albums that still share a normalized title and
//! the hidden copies, for review.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::app_state::AppState;
use crate::library::scanner::{LibraryEvent, LibraryScanner};
use crate::library::store::LibrarySources;
use crate::library::{TrackKey, album_title_key};

/// Idle limit: abort if no scanner event arrives for this long (a hung NAS read).
const IDLE_TIMEOUT: Duration = Duration::from_secs(600);

/// What two libraries must agree on to be "the same library" for the user: every record's album key,
/// visibility and the winner it hides behind, plus which albums have a picture.
fn library_fingerprint(app: &AppState) -> (Vec<String>, Vec<String>) {
    let mut records: Vec<String> = app
        .library
        .records()
        .iter()
        .map(|record| format!("{:?}|{}|{:?}", record.key, crate::library::album_key(record), record.hidden_copy_of))
        .collect();
    records.sort();
    let mut art: Vec<String> =
        app.library.albums("", None).iter().filter(|album| app.artwork_for_key(&album.key).is_some()).map(|album| album.key.clone()).collect();
    art.sort();
    (records, art)
}

/// Writes the scanned library to a cache in a throwaway temp directory (never the user's real cache
/// directory), reads it back into a fresh `AppState` the way the startup restore does and compares it
/// with the scanned one.
fn check_cache_round_trip(scanned: &AppState) {
    use crate::library::cache::{self, CacheSnapshot, LoadOutcome};
    let dir = std::env::temp_dir().join(format!("lime-dump-cache-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let snapshot = CacheSnapshot::of(scanned, &HashSet::new());
    cache::write(&dir, &snapshot).expect("write the throwaway cache");
    let LoadOutcome::Loaded(loaded) = cache::load(&dir, &HashSet::new()) else { panic!("the throwaway cache did not load") };
    let mut restored = AppState::new();
    let started = Instant::now();
    restored.restore_cache(loaded);
    let (scanned_records, scanned_art) = library_fingerprint(scanned);
    let (restored_records, restored_art) = library_fingerprint(&restored);
    println!(
        "cache round trip: restore of {} records took {:.1?}; records equal: {}, albums with art {} vs {} (equal: {})",
        restored_records.len(),
        started.elapsed(),
        scanned_records == restored_records,
        scanned_art.len(),
        restored_art.len(),
        scanned_art == restored_art
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(scanned_records, restored_records, "restoring the cache must produce the scanned library");
    assert_eq!(scanned_art, restored_art, "restoring the cache must restore the same pictures");
}

fn tsv(value: &str) -> String {
    value.replace(['\t', '\n', '\r'], " ")
}

#[test]
#[ignore = "dev-only: needs LIME_LIBRARY_DUMP=<library.json> and a mounted library"]
fn dump_real_library() {
    let Ok(sources_path) = std::env::var("LIME_LIBRARY_DUMP") else {
        eprintln!("LIME_LIBRARY_DUMP is unset; skipping (set it to a library.json path)");
        return;
    };
    let out_dir = std::env::var("LIME_LIBRARY_DUMP_OUT").ok().map(PathBuf::from);
    let started = Instant::now();

    let sources: LibrarySources =
        serde_json::from_str(&std::fs::read_to_string(&sources_path).expect("read library.json")).expect("parse library.json");
    eprintln!(
        "sources: {} files, {} folders, {} cue files, {} excluded tracks",
        sources.files.len(),
        sources.folders.len(),
        sources.cue_files.len(),
        sources.excluded_tracks.len()
    );

    let scanner = LibraryScanner::new();
    let events = scanner.events();
    let excluded: HashSet<TrackKey> = sources.excluded_tracks.clone();
    let mut pending: HashSet<u64> = HashSet::new();
    if !sources.files.is_empty() {
        pending.insert(scanner.scan(sources.files.clone(), false, excluded.clone()));
    }
    if !sources.cue_files.is_empty() {
        pending.insert(scanner.scan(sources.cue_files.clone(), false, excluded.clone()));
    }
    for folder in &sources.folders {
        pending.insert(scanner.scan_folder(folder.clone(), excluded.clone()));
    }

    let mut app = AppState::new();
    let mut failures: Vec<String> = Vec::new();
    let mut dropped_excluded = 0usize;
    let mut scanned = 0usize;
    while !pending.is_empty() {
        let event = events.recv_timeout(IDLE_TIMEOUT).expect("scanner went silent");
        match event {
            LibraryEvent::Probed { tracks, .. } => {
                let before = tracks.len();
                let tracks: Vec<_> = tracks.into_iter().filter(|t| !sources.is_excluded(&t.key)).collect();
                dropped_excluded += before - tracks.len();
                app.apply_probed(&tracks);
            }
            LibraryEvent::Scanned(record) => {
                if sources.is_excluded(&record.key) {
                    continue;
                }
                scanned += 1;
                app.apply_scanned(*record);
            }
            LibraryEvent::Failed { path, error, .. } => failures.push(format!("{}: {error}", path.display())),
            LibraryEvent::BatchDone { batch, .. } => {
                pending.remove(&batch);
            }
            LibraryEvent::FolderScanProgress { .. }
            | LibraryEvent::FolderWalked { .. }
            | LibraryEvent::RateRepaired { .. } | LibraryEvent::RateRepairFailed { .. } => {}
        }
    }
    eprintln!("scan finished in {:.1?}: {scanned} scanned records, {dropped_excluded} probed tracks dropped as excluded, {} failures", started.elapsed(), failures.len());
    for failure in &failures {
        eprintln!("FAILED {failure}");
    }

    check_cache_round_trip(&app);

    let library = &app.library;
    let mut albums_out = String::from("key\ttitle\tartist\tyear\ttracks\tformat\tsource_folders\n");
    let mut tracks_out = String::from("album_key\tpath\tstart_frame\talbum\talbum_artist\tartist\ttitle\tdisc\ttrack\tyear\tformat\tbits\trate\n");
    let albums = library.albums("", None);
    println!("=== {} albums ===", albums.len());
    for album in &albums {
        let tracks = library.album_tracks(&album.key);
        let folders: BTreeSet<String> = tracks
            .iter()
            .map(|t| t.key.path.parent().map(Path::to_string_lossy).unwrap_or_default().into_owned())
            .collect();
        let year = album.year.map(|y| y.to_string()).unwrap_or_default();
        println!(
            "[{}] {:?} | {} | {} | {} tracks | {} | {} folder(s)",
            album.key.split('\u{1f}').next().unwrap_or(""),
            album.title,
            album.artist,
            year,
            album.track_count,
            album.format_label,
            folders.len()
        );
        println!("    key: {}", album.key.replace('\u{1f}', " | "));
        for folder in &folders {
            println!("    folder: {folder}");
        }
        let _ = writeln!(
            albums_out,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            tsv(&album.key.replace('\u{1f}', "|")),
            tsv(&album.title),
            tsv(&album.artist),
            year,
            album.track_count,
            album.format_label,
            tsv(&folders.iter().cloned().collect::<Vec<_>>().join(" ; "))
        );
        for t in tracks {
            let _ = writeln!(
                tracks_out,
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                tsv(&album.key.replace('\u{1f}', "|")),
                tsv(&t.key.path.to_string_lossy()),
                t.key.start_frame,
                tsv(t.tags.album.as_deref().unwrap_or("")),
                tsv(t.tags.album_artist.as_deref().unwrap_or("")),
                tsv(t.tags.artist.as_deref().unwrap_or("")),
                tsv(t.tags.title.as_deref().unwrap_or("")),
                t.tags.disc_number.map(|n| n.to_string()).unwrap_or_default(),
                t.tags.track_number.map(|n| n.to_string()).unwrap_or_default(),
                t.tags.year.map(|n| n.to_string()).unwrap_or_default(),
                t.info.format,
                t.info.bits_per_sample,
                t.info.sample_rate,
            );
        }
    }

    // Albums sharing a normalized title (ignoring artist): the quickest list of duplicate-looking
    // cards that survived the grouping rules.
    let mut by_title: BTreeMap<String, Vec<&crate::library::AlbumSummary>> = BTreeMap::new();
    for album in &albums {
        by_title.entry(album_title_key(&album.title)).or_default().push(album);
    }
    println!("=== album titles that appear on more than one card ===");
    for (norm, cards) in by_title.iter().filter(|(_, cards)| cards.len() > 1) {
        println!("{norm}:");
        for card in cards {
            println!("    {:?} by {} ({} tracks) key={}", card.title, card.artist, card.track_count, card.key.replace('\u{1f}', " | "));
        }
    }

    // Hidden duplicate copies: album -> hidden file -> the visible file it hides behind.
    let mut hidden_out = String::from("album_key\talbum\thidden_path\thidden_start\thidden_format\thidden_bits\thidden_rate\twinner_path\twinner_format\twinner_bits\twinner_rate\n");
    let mut hidden = library.hidden_copies();
    hidden.sort_by(|a, b| a.0.cmp(b.0).then_with(|| a.1.key.cmp(&b.1.key)));
    println!("=== {} hidden duplicate copies ===", hidden.len());
    for (key, copy) in &hidden {
        let winner = copy.hidden_copy_of.as_ref().and_then(|winner| library.get(winner)).expect("a hidden copy points at a stored track");
        let album = crate::library::display_album(winner);
        println!(
            "{album:?}: {} [{} {}/{}] hidden behind {} [{} {}/{}]",
            copy.key.path.display(),
            copy.info.format,
            copy.info.bits_per_sample,
            copy.info.sample_rate,
            winner.key.path.display(),
            winner.info.format,
            winner.info.bits_per_sample,
            winner.info.sample_rate
        );
        let _ = writeln!(
            hidden_out,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            tsv(&key.replace('\u{1f}', "|")),
            tsv(&album),
            tsv(&copy.key.path.to_string_lossy()),
            copy.key.start_frame,
            copy.info.format,
            copy.info.bits_per_sample,
            copy.info.sample_rate,
            tsv(&winner.key.path.to_string_lossy()),
            winner.info.format,
            winner.info.bits_per_sample,
            winner.info.sample_rate
        );
    }

    if let Some(dir) = out_dir {
        std::fs::create_dir_all(&dir).expect("create output dir");
        std::fs::write(dir.join("albums.tsv"), albums_out).expect("write albums.tsv");
        std::fs::write(dir.join("tracks.tsv"), tracks_out).expect("write tracks.tsv");
        std::fs::write(dir.join("hidden.tsv"), hidden_out).expect("write hidden.tsv");
        std::fs::write(dir.join("failures.txt"), failures.join("\n")).expect("write failures.txt");
        eprintln!("wrote albums.tsv, tracks.tsv, hidden.tsv, failures.txt to {}", dir.display());
    }
    eprintln!("total {:.1?}", started.elapsed());
}

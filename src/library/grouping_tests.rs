//! `Library`-level tests of the grouping rules in `grouping.rs`: artist/title normalization, the
//! folder-majority album title, hidden duplicate copies, disc numbers from subfolders, key
//! transitions, and scan-order independence. The pure helpers have their own tests next to them.

use std::path::PathBuf;

use super::*;
use crate::audio::{AudioInfo, TrackTags};

fn info(format: &str, bits: u32, rate: u32, duration_ms: Option<u64>) -> AudioInfo {
    AudioInfo { sample_rate: rate, duration_ms, source_channels: 2, bits_per_sample: bits, is_float: false, integer_pcm: true, format: format.into() }
}

/// A FLAC 16/44.1 track with the given tags; `album` / `album_artist` `None` means untagged.
fn flac(path: &str, title: &str, artist: &str, album: Option<&str>, album_artist: Option<&str>) -> TrackRecord {
    let mut record = TrackRecord::minimal(TrackKey::whole_file(PathBuf::from(path)), info("FLAC", 16, 44_100, Some(200_000)));
    record.tags = TrackTags {
        title: Some(title.into()),
        artist: Some(artist.into()),
        album: album.map(str::to_owned),
        album_artist: album_artist.map(str::to_owned),
        ..TrackTags::default()
    };
    record
}

fn float_info(format: &str, bits: u32, rate: u32, duration_ms: Option<u64>) -> AudioInfo {
    AudioInfo { sample_rate: rate, duration_ms, source_channels: 2, bits_per_sample: bits, is_float: true, integer_pcm: false, format: format.into() }
}

/// A CUE sub-range track `number` (1-based) of the physical file `file`.
fn cue_track(file: &str, number: u64, title: &str, artist: &str, album: &str, album_artist: Option<&str>) -> TrackRecord {
    let mut record = flac(file, title, artist, Some(album), album_artist);
    record.key.start_frame = (number - 1) * 10_000_000;
    record.key.end_frame = Some(number * 10_000_000);
    record
}

fn with_info(mut record: TrackRecord, format: &str, bits: u32, rate: u32, duration_ms: Option<u64>) -> TrackRecord {
    record.info = info(format, bits, rate, duration_ms);
    record
}

fn library_of(records: Vec<TrackRecord>) -> Library {
    let mut library = Library::new();
    for record in records {
        library.upsert(record);
    }
    library
}

fn titles(library: &Library, album: &AlbumSummary) -> Vec<String> {
    library.album_tracks(&album.key).iter().map(|track| display_title(track)).collect()
}

/// Everything observable about a library, order-independently, to compare two insertion orders.
fn signature(library: &Library) -> Vec<(PathBuf, String, Option<TrackKey>, String)> {
    let mut rows: Vec<_> = library
        .tracks
        .iter()
        .zip(&library.keys)
        .map(|(track, key)| (track.key.path.clone(), key.clone(), track.hidden_copy_of.clone(), display_album(track)))
        .collect();
    rows.sort();
    rows
}

fn album_with_tracks(folder: &str, album: &str, count: usize) -> Vec<TrackRecord> {
    (1..=count).map(|n| flac(&format!("{folder}/{n:02}.flac"), &format!("{album} song {n}"), "Band", Some(album), Some("Band"))).collect()
}

#[test]
fn album_artist_spellings_that_only_differ_in_punctuation_share_one_album_and_show_the_majority_spelling() {
    let library = library_of(vec![
        flac("/m/a/1.flac", "One", "Daryl Hall  John Oates", Some("Private Eyes"), Some("Daryl Hall  John Oates")),
        flac("/m/b/2.flac", "Two", "Daryl Hall & John Oates", Some("Private Eyes"), Some("Daryl Hall & John Oates")),
        flac("/m/b/3.flac", "Three", "Daryl Hall & John Oates", Some("Private Eyes"), Some("Daryl Hall & John Oates")),
    ]);
    let albums = library.albums("", None);
    assert_eq!(albums.len(), 1, "{albums:?}");
    assert_eq!(albums[0].artist, "Daryl Hall & John Oates", "the displayed artist is the majority spelling");
    assert_eq!(albums[0].track_count, 3);

    let earth = library_of(vec![
        flac("/m/a/1.flac", "One", "Earth Wind & Fire", Some("I Am"), Some("Earth Wind & Fire")),
        flac("/m/b/1.flac", "Two", "Earth, Wind & Fire", Some("I Am"), Some("Earth, Wind & Fire")),
    ]);
    assert_eq!(earth.albums("", None).len(), 1);
}

#[test]
fn disc_and_media_markers_in_the_album_title_do_not_split_an_album() {
    let library = library_of(vec![
        flac("/m/Space Cowboy/CD1/01.flac", "One", "Jamiroquai", Some("Return Of The Space Cowboy (CD1)"), None),
        flac("/m/Space Cowboy/CD2/01.flac", "Two", "Jamiroquai", Some("The Return Of The Space Cowboy (CD2)"), None),
        flac("/m/Off The Wall/01.flac", "Three", "Michael Jackson", Some("Off The Wall (LP)"), None),
        flac("/m/Off The Wall FLAC/01.flac", "Four", "Michael Jackson", Some("Off The Wall"), None),
    ]);
    let albums = library.albums("", None);
    assert_eq!(albums.len(), 2, "{albums:?}");
    let cowboy = albums.iter().find(|album| album.artist == "Jamiroquai").unwrap();
    assert_eq!(cowboy.track_count, 2);
    assert!(!cowboy.title.contains("CD"), "{}", cowboy.title);
    let wall = albums.iter().find(|album| album.artist == "Michael Jackson").unwrap();
    assert_eq!(wall.title, "Off The Wall");
}

#[test]
fn edition_words_keep_releases_apart() {
    let library = library_of(vec![
        flac("/m/Dynamite/01.flac", "Dynamite", "BTS", Some("Dynamite"), Some("BTS")),
        flac("/m/Dynamite Remastered/01.flac", "Dynamite", "BTS", Some("Dynamite (Remastered)"), Some("BTS")),
    ]);
    assert_eq!(library.albums("", None).len(), 2);
}

#[test]
fn folder_majority_folds_a_minority_title_and_untagged_tracks_into_the_dominant_album() {
    let mut records = Vec::new();
    for n in 1..=4 {
        records.push(flac(&format!("/m/Avicii/{n}.flac"), &format!("Song {n}"), "Avicii", Some("The Days / The Nights EP"), None));
    }
    for n in 5..=6 {
        records.push(flac(&format!("/m/Avicii/{n}.flac"), &format!("Song {n}"), "Avicii", Some("The Days & The Nights"), None));
    }
    records.push(flac("/m/Avicii/7.wav", "Song 7", "Avicii", None, None));
    let library = library_of(records);
    let albums = library.albums("", None);
    assert_eq!(albums.len(), 1, "{albums:?}");
    assert_eq!(albums[0].title, "The Days / The Nights EP");
    assert_eq!(albums[0].track_count, 7);
}

#[test]
fn an_untagged_track_inherits_the_album_artist_the_group_resolves_to() {
    let mut records = album_with_tracks("/m/Grease", "Grease", 5);
    let mut stray = TrackRecord::minimal(TrackKey::whole_file(PathBuf::from("/m/Grease/stray.wav")), info("WAV", 16, 44_100, Some(150_000)));
    stray.tags = TrackTags::default();
    records.push(stray);
    let library = library_of(records);
    let albums = library.albums("", None);
    assert_eq!(albums.len(), 1, "{albums:?}");
    assert_eq!(albums[0].artist, "Band");
    assert_eq!(albums[0].track_count, 6);
}

#[test]
fn a_flat_folder_holding_two_full_albums_stays_two_albums() {
    let mut records = album_with_tracks("/m/flat", "First Album", 12);
    records.extend((1..=5).map(|n| flac(&format!("/m/flat/b{n}.flac"), &format!("Other {n}"), "Band", Some("Second Album"), Some("Band"))));
    let library = library_of(records);
    let mut counts: Vec<usize> = library.albums("", None).iter().map(|album| album.track_count).collect();
    counts.sort();
    assert_eq!(counts, vec![5, 12]);
}

#[test]
fn the_folder_majority_fold_reports_the_minority_key_transition_and_is_order_independent() {
    let minority = flac("/m/F/z.flac", "Stray", "Band", Some("Minority Title"), Some("Band"));
    let majority: Vec<TrackRecord> = album_with_tracks("/m/F", "Main Title", 4);

    let mut library = Library::new();
    assert!(library.upsert(minority.clone()).is_empty());
    let own_key = album_key(library.get(&minority.key).unwrap());
    let mut transitions = Vec::new();
    for record in majority.clone() {
        transitions.extend(library.upsert(record));
    }
    let folded_key = album_key(library.get(&minority.key).unwrap());
    assert_ne!(folded_key, own_key);
    assert!(transitions.contains(&(own_key, folded_key.clone())), "the fold must report the minority's key move: {transitions:?}");
    assert_eq!(library.albums("", None).len(), 1);

    let mut reversed = Library::new();
    for record in majority.into_iter().rev().chain(std::iter::once(minority)) {
        reversed.upsert(record);
    }
    assert_eq!(signature(&library), signature(&reversed));
}

#[test]
fn transitions_name_the_resolution_groups_whose_pictures_move() {
    let mut library = Library::new();
    let minority = flac("/m/F/z.flac", "Stray", "Band", Some("Minority Title"), Some("Band"));
    let minority_group = resolution_group_key(&minority).unwrap();
    library.upsert_detailed(minority);
    let mut moved = Vec::new();
    for record in album_with_tracks("/m/F", "Main Title", 4) {
        moved.extend(library.upsert_detailed(record));
    }
    assert!(moved.iter().any(|transition| transition.groups.contains(&minority_group)), "{moved:?}");
}

// -----------------------------------------------------------------------------------------------
// Duplicate copies
// -----------------------------------------------------------------------------------------------

fn flac_and_wavpack_copies() -> Vec<TrackRecord> {
    let mut records = Vec::new();
    for (n, song) in ["Feels Just Like It Should", "Dynamite", "Starchild"].into_iter().enumerate() {
        records.push(flac(&format!("/m/flac/{:02} - Jamiroquai - {song}.flac", n + 1), song, "Jamiroquai", Some("Dynamite"), Some("Jamiroquai")));
        let side = ["A1", "A2", "B1"][n];
        records.push(with_info(
            flac(&format!("/m/wv/{side}-{song}.wv"), &format!("{side}-{song}"), "Jamiroquai", Some("Dynamite"), Some("Jamiroquai")),
            "WavPack",
            32,
            192_000,
            Some(201_000),
        ));
    }
    records
}

#[test]
fn the_best_copy_of_each_song_stays_visible_and_the_rest_are_hidden_from_every_view() {
    let library = library_of(flac_and_wavpack_copies());
    let albums = library.albums("", None);
    assert_eq!(albums.len(), 1);
    assert_eq!(albums[0].track_count, 3, "one entry per song");
    assert_eq!(albums[0].format_label, "WavPack", "the pill reflects the visible copies only");
    assert!(library.album_tracks(&albums[0].key).iter().all(|track| track.info.format == "WavPack"));
    assert_eq!(library.songs("").len(), 3);
    assert_eq!(library.songs_recent("").len(), 3);
    assert_eq!(library.songs("starchild").len(), 1);
    let artists = library.artists("");
    assert_eq!(artists.len(), 1);
    assert_eq!(artists[0].track_count, 3);
    assert_eq!(library.hidden_copies().len(), 3);
    // Hidden copies stay reachable for playback and queue actions.
    let hidden = library.hidden_copies()[0].1.key.clone();
    assert!(library.get(&hidden).is_some() && library.prepared(&hidden).is_some());
}

#[test]
fn lossless_beats_lossy_then_bit_depth_then_rate_then_format() {
    let library = library_of(vec![
        with_info(flac("/m/a/01.mp3", "Song", "Band", Some("Album"), Some("Band")), "MP3", 16, 44_100, Some(200_000)),
        with_info(flac("/m/b/01.flac", "Song", "Band", Some("Album"), Some("Band")), "FLAC", 16, 44_100, Some(200_000)),
        with_info(flac("/m/c/01.flac", "Song", "Band", Some("Album"), Some("Band")), "FLAC", 24, 96_000, Some(200_000)),
        with_info(flac("/m/d/01.wv", "Song", "Band", Some("Album"), Some("Band")), "WavPack", 24, 96_000, Some(200_000)),
    ]);
    let album = library.albums("", None).remove(0);
    let visible = library.album_tracks(&album.key);
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].key.path, PathBuf::from("/m/c/01.flac"), "24/96 FLAC beats 24/96 WavPack by format preference");
}

#[test]
fn a_flac_and_an_mp3_of_the_same_song_in_one_folder_are_copies() {
    let library = library_of(vec![
        flac("/m/Album/01 - Song.flac", "Song", "Band", Some("Album"), Some("Band")),
        with_info(flac("/m/Album/01 - Song.mp3", "Song", "Band", Some("Album"), Some("Band")), "MP3", 16, 44_100, Some(200_300)),
    ]);
    assert_eq!(library.songs("").len(), 1);
    assert_eq!(library.songs("")[0].info.format, "FLAC");
}

#[test]
fn copy_detection_never_hides_what_could_be_a_different_song() {
    // Same title, same folder and format: two songs.
    let same_folder = library_of(vec![
        flac("/m/Album/a.flac", "Intro", "Band", Some("Album"), Some("Band")),
        flac("/m/Album/b.flac", "Intro", "Band", Some("Album"), Some("Band")),
    ]);
    assert_eq!(same_folder.songs("").len(), 2);

    // Same title in two folders, durations far apart: two songs.
    let long_short = library_of(vec![
        flac("/m/a/x.flac", "Intro", "Band", Some("Album"), Some("Band")),
        with_info(flac("/m/b/x.wv", "Intro", "Band", Some("Album"), Some("Band")), "WavPack", 24, 96_000, Some(60_000)),
    ]);
    assert_eq!(long_short.songs("").len(), 2);

    // Different DISCNUMBERs: two songs.
    let mut disc1 = flac("/m/a/x.flac", "Intro", "Band", Some("Album"), Some("Band"));
    disc1.tags.disc_number = Some(1);
    let mut disc2 = with_info(flac("/m/b/x.wv", "Intro", "Band", Some("Album"), Some("Band")), "WavPack", 24, 96_000, Some(200_000));
    disc2.tags.disc_number = Some(2);
    assert_eq!(library_of(vec![disc1, disc2]).songs("").len(), 2);

    // Two CUE tracks of one physical file: never copies of each other.
    let mut first = flac("/m/img/album.flac", "Same Title", "Band", Some("Album"), Some("Band"));
    first.key.end_frame = Some(1_000_000);
    let mut second = flac("/m/img/album.flac", "Same Title", "Band", Some("Album"), Some("Band"));
    second.key.start_frame = 1_000_000;
    assert_eq!(library_of(vec![first, second]).songs("").len(), 2);

    // Two songs of one folder and format stay two even when a copy exists elsewhere for both.
    let mut records = vec![
        flac("/m/a/1.flac", "Intro", "Band", Some("Album"), Some("Band")),
        flac("/m/a/2.flac", "Intro", "Band", Some("Album"), Some("Band")),
    ];
    records.push(with_info(flac("/m/b/1.wv", "Intro", "Band", Some("Album"), Some("Band")), "WavPack", 24, 96_000, Some(200_000)));
    let library = library_of(records);
    assert_eq!(library.songs("").len(), 2, "one WavPack winner plus the second FLAC, which shares folder and format with the hidden one");
}

#[test]
fn an_unknown_duration_leaves_the_title_match_alone_to_decide() {
    let library = library_of(vec![
        with_info(flac("/m/a/01.flac", "Song", "Band", Some("Album"), Some("Band")), "FLAC", 16, 44_100, None),
        with_info(flac("/m/b/A1-Song.wv", "A1-Song", "Band", Some("Album"), Some("Band")), "WavPack", 24, 96_000, Some(200_000)),
    ]);
    assert_eq!(library.songs("").len(), 1);
}

#[test]
fn removing_an_album_returns_every_copy_and_a_replaced_winner_promotes_the_hidden_copy() {
    let mut library = library_of(flac_and_wavpack_copies());
    let key = library.albums("", None)[0].key.clone();

    // The winner stops being a copy (retagged): the hidden FLAC of that song becomes visible again.
    let winner = library.songs("starchild")[0].key.clone();
    let mut retagged = library.get(&winner).unwrap().clone();
    retagged.tags.title = Some("Something Else Entirely".into());
    library.upsert(retagged);
    assert_eq!(library.songs("").len(), 4);
    assert!(library.songs("starchild").iter().all(|track| track.info.format == "FLAC"));

    let removed = library.remove_album(&key);
    assert_eq!(removed.len(), 6, "visible tracks and hidden copies are all excluded together");
    assert!(library.is_empty());
}

#[test]
fn copy_visibility_does_not_depend_on_scan_order() {
    let records = flac_and_wavpack_copies();
    let forward = library_of(records.clone());
    let reversed = library_of(records.iter().cloned().rev().collect());
    let mut rotated = records.clone();
    rotated.rotate_left(2);
    assert_eq!(signature(&forward), signature(&reversed));
    assert_eq!(signature(&forward), signature(&library_of(rotated)));
}

#[test]
fn a_hidden_copy_still_resolves_its_album_for_the_click_to_album_helper() {
    let library = library_of(flac_and_wavpack_copies());
    let (album_key_of_hidden, hidden) = library.hidden_copies().remove(0);
    assert_eq!(album_key(hidden), album_key_of_hidden);
    assert!(library.album(album_key_of_hidden).is_some());
}

// -----------------------------------------------------------------------------------------------
// Disc numbers
// -----------------------------------------------------------------------------------------------

#[test]
fn missing_disc_numbers_come_from_the_disc_subfolder_so_discs_sort_one_after_another() {
    let library = library_of(vec![
        flac("/m/Please/CD2/01.flac", "Disc two first", "Pet Shop Boys", Some("Please"), Some("Pet Shop Boys")),
        flac("/m/Please/CD1/02.flac", "Disc one second", "Pet Shop Boys", Some("Please"), Some("Pet Shop Boys")),
        flac("/m/Please/Disc One/01.flac", "Disc one first", "Pet Shop Boys", Some("Please"), Some("Pet Shop Boys")),
        flac("/m/Please/CD2/02.flac", "Disc two second", "Pet Shop Boys", Some("Please"), Some("Pet Shop Boys")),
    ]);
    let album = library.albums("", None).remove(0);
    let mut records: Vec<(u32, u32, String)> = Vec::new();
    for track in library.album_tracks(&album.key) {
        records.push((effective_disc_number(track).unwrap(), track.tags.track_number.unwrap_or(0), display_title(track)));
    }
    let order: Vec<u32> = records.iter().map(|(disc, _, _)| *disc).collect();
    assert_eq!(order, vec![1, 1, 2, 2], "{records:?}");
    assert!(titles(&library, &album)[0].starts_with("Disc one"));
}

// -----------------------------------------------------------------------------------------------
// Folder-majority fold: false merges
// -----------------------------------------------------------------------------------------------

#[test]
fn twelve_album_tracks_beside_eight_singles_by_other_artists_stay_nine_albums() {
    let mut records = album_with_tracks("/m/flat", "Big Album", 12);
    for n in 1..=8 {
        records.push(flac(&format!("/m/flat/single{n}.flac"), &format!("Single {n}"), &format!("Artist {n}"), Some(&format!("Single {n}")), None));
    }
    let albums = library_of(records).albums("", None);
    assert_eq!(albums.len(), 9, "{albums:?}");
    assert_eq!(albums.iter().map(|album| album.track_count).max(), Some(12));
}

#[test]
fn a_five_track_ep_by_another_artist_beside_fifteen_tracks_is_not_merged() {
    let mut records = album_with_tracks("/m/flat", "Full Length", 15);
    records.extend((1..=5).map(|n| flac(&format!("/m/flat/ep{n}.flac"), &format!("EP song {n}"), "Someone Else", Some("The EP"), Some("Someone Else"))));
    let mut counts: Vec<usize> = library_of(records).albums("", None).iter().map(|album| album.track_count).collect();
    counts.sort();
    assert_eq!(counts, vec![5, 15]);
}

#[test]
fn the_total_of_folded_minority_tracks_is_capped_not_each_title_judged_alone() {
    // 12 tracks of Main plus five one-track stray titles by the same artist: every stray alone is
    // within max(2, 25%), but the TOTAL folded may not exceed max(2, 25% of 17) = 4.25, so four
    // strays fold into Main (16 tracks) and the fifth stays on its own.
    let mut records = album_with_tracks("/m/flat", "Main", 12);
    for n in 1..=5 {
        records.push(flac(&format!("/m/flat/s{n}.flac"), &format!("Stray {n}"), "Band", Some(&format!("Stray Title {n}")), Some("Band")));
    }
    let mut counts: Vec<usize> = library_of(records).albums("", None).iter().map(|album| album.track_count).collect();
    counts.sort();
    assert_eq!(counts, vec![1, 16]);
}

#[test]
fn a_real_minority_of_one_artist_still_folds_for_america_and_hall_and_oates() {
    let mut america: Vec<TrackRecord> =
        (1..=3).map(|n| flac(&format!("/m/America/{n}.flac"), &format!("Song {n}"), "America", Some("History: America's Greatest Hits"), None)).collect();
    america.push(flac("/m/America/4.flac", "Song 4", "America", Some("Time-Life AM Gold 1974"), None));
    let albums = library_of(america).albums("", None);
    assert_eq!(albums.len(), 1, "{albums:?}");
    assert_eq!(albums[0].track_count, 4);

    let performer = "Daryl Hall & John Oates";
    let mut records: Vec<TrackRecord> =
        (1..=13).map(|n| cue_track("/m/HO/Big Bam Boom.flac", n, &format!("Song {n}"), performer, "Big Bam Boom", Some(performer))).collect();
    records.push(flac("/m/HO/Out Of Touch.flac", "Out Of Touch Alt", performer, Some("1984 Big Bam Boom (Sony Legacy 2011)"), None));
    let albums = library_of(records).albums("", None);
    assert_eq!(albums.len(), 1, "{albums:?}");
    assert_eq!(albums[0].track_count, 14);
}

#[test]
fn compilation_tracks_fold_their_own_artists_but_a_stranger_does_not() {
    let artists = ["Queen", "ABBA", "Blondie", "Madonna", "Prince", "Sting", "Cher", "Bjork"];
    let mut records: Vec<TrackRecord> = artists
        .iter()
        .enumerate()
        .map(|(n, artist)| flac(&format!("/m/Soundtrack/{n}.flac"), &format!("Song {n}"), artist, Some("Movie Soundtrack"), None))
        .collect();
    records.push(flac("/m/Soundtrack/x.flac", "Bonus", "ABBA", Some("Movie Soundtrack (Bonus)"), None));
    records.push(flac("/m/Soundtrack/y.flac", "Stranger", "Nobody Here", Some("Another Title"), None));
    let mut counts: Vec<usize> = library_of(records).albums("", None).iter().map(|album| album.track_count).collect();
    counts.sort();
    assert_eq!(counts, vec![1, 9]);
}

#[test]
fn an_untagged_track_by_another_artist_does_not_join_the_dominant_album() {
    let mut records = album_with_tracks("/m/flat", "Main", 6);
    records.push(flac("/m/flat/stranger.wav", "Stranger", "Not The Band", None, None));
    let mut counts: Vec<usize> = library_of(records).albums("", None).iter().map(|album| album.track_count).collect();
    counts.sort();
    assert_eq!(counts, vec![1, 6]);
}

// -----------------------------------------------------------------------------------------------
// Copies per source
// -----------------------------------------------------------------------------------------------

fn live_disc(folder: &str, songs: &[&str]) -> Vec<TrackRecord> {
    songs.iter().enumerate().map(|(n, song)| flac(&format!("{folder}/{:02}.flac", n + 1), song, "Band", Some("Live"), Some("Band"))).collect()
}

#[test]
fn an_intro_on_each_of_two_unrecognised_disc_folders_hides_nothing() {
    let mut small = live_disc("/m/Live/Disc A", &["Intro", "Song A"]);
    small.extend(live_disc("/m/Live/Disc B", &["Intro", "Song B"]));
    assert_eq!(library_of(small).songs("").len(), 4);

    let mut large = live_disc("/m/Live/Disc A", &["Intro", "A2", "A3", "A4", "A5"]);
    large.extend(live_disc("/m/Live/Disc B", &["Intro", "B2", "B3", "B4", "B5"]));
    assert_eq!(library_of(large).songs("").len(), 10);
}

#[test]
fn flac_and_wavpack_sibling_folders_of_a_ten_track_album_hide_ten() {
    let songs: Vec<String> = (1..=10).map(|n| format!("Track {n}")).collect();
    let names: Vec<&str> = songs.iter().map(String::as_str).collect();
    let mut records = live_disc("/m/Album FLAC", &names);
    records.extend(live_disc("/m/Album WV", &names).into_iter().map(|record| with_info(record, "WavPack", 24, 96_000, Some(200_000))));
    let library = library_of(records);
    assert_eq!(library.songs("").len(), 10);
    assert_eq!(library.hidden_copies().len(), 10);
}

#[test]
fn a_cue_track_and_an_extracted_single_of_the_same_song_are_copies_of_each_other() {
    let performer = "Daryl Hall & John Oates";
    let mut records: Vec<TrackRecord> =
        (1..=13).map(|n| cue_track("/m/HO/Big Bam Boom.flac", n, &format!("Song {n}"), performer, "Big Bam Boom", Some(performer))).collect();
    records.push(cue_track("/m/HO/Big Bam Boom.flac", 14, "Out Of Touch", performer, "Big Bam Boom", Some(performer)));
    records.push(flac("/m/HO/Out Of Touch.flac", "Out Of Touch", performer, Some("Big Bam Boom"), Some(performer)));
    let library = library_of(records);
    assert_eq!(library.songs("").len(), 14);
    let hidden = library.hidden_copies();
    assert_eq!(hidden.len(), 1);
    assert_eq!(hidden[0].1.key.path, PathBuf::from("/m/HO/Out Of Touch.flac"));
}

#[test]
fn two_cue_tracks_of_one_file_and_two_whole_files_of_one_folder_are_never_copies() {
    let both_cue = library_of(vec![
        cue_track("/m/x/a.flac", 1, "Reprise", "Band", "Album", Some("Band")),
        cue_track("/m/x/a.flac", 2, "Reprise", "Band", "Album", Some("Band")),
    ]);
    assert_eq!(both_cue.songs("").len(), 2);
    let whole = library_of(vec![
        flac("/m/x/1.flac", "Reprise", "Band", Some("Album"), Some("Band")),
        flac("/m/x/2.flac", "Reprise", "Band", Some("Album"), Some("Band")),
    ]);
    assert_eq!(whole.songs("").len(), 2);
}

// -----------------------------------------------------------------------------------------------
// Best copy ranking and version suffixes
// -----------------------------------------------------------------------------------------------

#[test]
fn a_24_bit_integer_flac_beats_a_32_bit_float_wavpack_and_integer_beats_float_at_equal_depth() {
    let mut vinyl = flac("/m/b/01.wv", "Girlfriend", "Michael Jackson", Some("Off The Wall"), None);
    vinyl.info = float_info("WavPack", 32, 192_000, Some(200_000));
    let library = library_of(vec![
        with_info(flac("/m/a/01.flac", "Girlfriend", "Michael Jackson", Some("Off The Wall"), None), "FLAC", 24, 192_000, Some(200_000)),
        vinyl,
    ]);
    let visible = library.songs("");
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].info.format, "FLAC");

    let mut float = flac("/m/a/01.wv", "Song", "Band", Some("Album"), Some("Band"));
    float.info = float_info("WavPack", 24, 96_000, Some(200_000));
    let equal_depth = library_of(vec![
        float,
        with_info(flac("/m/b/01.wv", "Song", "Band", Some("Album"), Some("Band")), "WavPack", 24, 96_000, Some(200_000)),
    ]);
    let visible = equal_depth.songs("");
    assert_eq!(visible.len(), 1);
    assert!(visible[0].info.integer_pcm, "integer PCM keeps the strict bit-perfect route");

    // Float precision is 24 bits, so a 32-bit float source still beats a 16-bit integer one.
    let mut float = flac("/m/b/01.wv", "Song", "Band", Some("Album"), Some("Band"));
    float.info = float_info("WavPack", 32, 44_100, Some(200_000));
    let deeper = library_of(vec![flac("/m/a/01.flac", "Song", "Band", Some("Album"), Some("Band")), float]);
    assert!(deeper.songs("")[0].info.is_float);
}

#[test]
fn a_remastered_suffix_on_one_side_does_not_stop_two_copies_matching() {
    for suffix in ["(Remastered)", "(Remastered 2013)", "(2013 Remaster)", "- Remastered 2013", "(Remastered Version)", "[Remastered]"] {
        let library = library_of(vec![
            flac("/m/a/01.flac", &format!("Dynamite {suffix}"), "Band", Some("Album"), Some("Band")),
            with_info(flac("/m/b/01.wv", "Dynamite", "Band", Some("Album"), Some("Band")), "WavPack", 24, 96_000, Some(200_000)),
        ]);
        assert_eq!(library.songs("").len(), 1, "{suffix}");
    }
    // Durations still have to agree.
    let different = library_of(vec![
        flac("/m/a/01.flac", "Dynamite (Remastered)", "Band", Some("Album"), Some("Band")),
        with_info(flac("/m/b/01.wv", "Dynamite", "Band", Some("Album"), Some("Band")), "WavPack", 24, 96_000, Some(90_000)),
    ]);
    assert_eq!(different.songs("").len(), 2);
}

// -----------------------------------------------------------------------------------------------
// Incremental regrouping and load performance
// -----------------------------------------------------------------------------------------------

fn mixed_records() -> Vec<TrackRecord> {
    let mut records = album_with_tracks("/m/one", "First", 8);
    records.extend((1..=3).map(|n| flac(&format!("/m/one/second{n}.flac"), &format!("Second song {n}"), "Band", Some("Second"), Some("Band"))));
    records.push(flac("/m/one/stray.flac", "Stray", "Band", Some("Tail"), Some("Band")));
    records.push(flac("/m/one/free.wav", "Free", "Band", None, None));
    records.extend(album_with_tracks("/m/two/CD1", "Box", 3));
    records.extend(album_with_tracks("/m/two/CD2", "Box", 3));
    records.extend(flac_and_wavpack_copies());
    records.extend(live_disc("/m/Live/Disc A", &["Intro", "A2", "A3"]));
    records.extend(live_disc("/m/Live/Disc B", &["Intro", "B2", "B3"]));
    records.push(cue_track("/m/three/img.flac", 1, "Cue One", "Duo", "Image", Some("Duo")));
    records.push(cue_track("/m/three/img.flac", 2, "Cue Two", "Duo", "Image", Some("Duo")));
    records.push(flac("/m/three/Cue Two.flac", "Cue Two", "Duo", Some("Image"), Some("Duo")));
    records
}

#[test]
fn a_library_built_by_probe_then_scan_upserts_in_any_order_equals_one_built_from_the_final_records() {
    let records = mixed_records();
    let fresh = library_of(records.clone());
    let probe_then_scan = |order: Vec<usize>| {
        let mut library = Library::new();
        for &index in &order {
            library.upsert(TrackRecord::minimal(records[index].key.clone(), records[index].info.clone()));
        }
        for &index in &order {
            library.upsert(records[index].clone());
        }
        library
    };
    let forward: Vec<usize> = (0..records.len()).collect();
    let mut shuffled = forward.clone();
    shuffled.reverse();
    shuffled.rotate_left(7);
    assert_eq!(signature(&fresh), signature(&probe_then_scan(forward)));
    assert_eq!(signature(&fresh), signature(&probe_then_scan(shuffled)));
}

fn flat_folder(tagged: bool, count: usize) -> Vec<TrackRecord> {
    (0..count)
        .map(|n| {
            let album = tagged.then(|| format!("Album {}", n / 12));
            let artist = if tagged { format!("Artist {}", n / 12) } else { "Artist".to_owned() };
            flac(&format!("/m/flat/{n:05}.flac"), &format!("Song {n}"), &artist, album.as_deref(), None)
        })
        .collect()
}

/// Wall time of loading a flat folder the way the startup restore does (probed, then scanned).
fn load_flat_folder(tagged: bool, count: usize) -> (std::time::Duration, Library) {
    let records = flat_folder(tagged, count);
    let started = std::time::Instant::now();
    let mut library = Library::new();
    for record in &records {
        library.upsert(TrackRecord::minimal(record.key.clone(), record.info.clone()));
    }
    for record in records {
        library.upsert(record);
    }
    (started.elapsed(), library)
}

#[test]
#[ignore = "timing measurement: cargo test --bin lime-player flat_folder_load_time -- --ignored --nocapture"]
fn flat_folder_load_time() {
    for (tagged, label) in [(true, "tagged (~12 tracks/album)"), (false, "untagged (one dir: album)")] {
        let (elapsed, library) = load_flat_folder(tagged, 2_000);
        eprintln!("2000 flat tracks, {label}: {elapsed:?}, {} albums", library.albums("", None).len());
    }
}

#[test]
fn a_flat_folder_loads_into_the_expected_albums_with_the_indexed_regrouping() {
    let (_, tagged) = load_flat_folder(true, 240);
    assert_eq!(tagged.albums("", None).len(), 20);
    let (_, untagged) = load_flat_folder(false, 240);
    let albums = untagged.albums("", None);
    assert_eq!(albums.len(), 1);
    assert_eq!(albums[0].track_count, 240);
}

// -----------------------------------------------------------------------------------------------
// Removal re-resolves the survivors
// -----------------------------------------------------------------------------------------------

/// Five "Main" tracks, two "Bonus" tracks and two "Other" tracks, all by Band in one folder: no title
/// reaches 60% (5 of 9), so three albums. Removing "Other" lifts Main to 5 of 7, and Bonus (2) then
/// folds into it.
fn main_bonus_other() -> Vec<TrackRecord> {
    let mut records = album_with_tracks("/m/F", "Main", 5);
    records.extend((1..=2).map(|n| flac(&format!("/m/F/bonus{n}.flac"), &format!("Bonus song {n}"), "Band", Some("Bonus"), Some("Band"))));
    records.extend((1..=2).map(|n| flac(&format!("/m/F/other{n}.flac"), &format!("Other song {n}"), "Band", Some("Other"), Some("Band"))));
    records
}

#[test]
fn removing_an_album_reports_the_survivors_whose_key_changed() {
    let mut library = library_of(main_bonus_other());
    assert_eq!(library.albums("", None).len(), 3);
    let key_of = |library: &Library, title: &str| library.albums("", None).into_iter().find(|album| album.title == title).unwrap().key;
    let (bonus_key, other_key) = (key_of(&library, "Bonus"), key_of(&library, "Other"));

    let (removed, transitions) = library.remove_album_detailed(&other_key);
    assert_eq!(removed.len(), 2);
    let albums = library.albums("", None);
    assert_eq!(albums.len(), 1, "Bonus folded into Main: {albums:?}");
    let main_key = albums[0].key.clone();
    assert!(
        transitions.iter().any(|transition| transition.old_key == bonus_key && transition.new_key == main_key),
        "the survivors' move must be reported: {transitions:?}"
    );
}

//! Pure, I/O-free rules behind album grouping (`CLAUDE.md` "Album grouping"): the text
//! normalization used for album-key components, the folder-majority album title, and the planner
//! that hides duplicate copies of one song inside a final album. `Library` (`mod.rs`) owns the state
//! and calls these on every insert/remove, so everything here is a pure function of its inputs and
//! never depends on arrival order.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

use super::{EffectiveAlbum, TrackKey, TrackRecord, display_artist, display_title, strip_feat_suffix, strip_latin_diacritic};

/// A scope's dominant album title must cover at least this share of the scope's ALBUM-tagged tracks.
const FOLDER_MAJORITY_THRESHOLD: f64 = 0.6;
/// Minority titles fold into the dominant one only while the TOTAL number of folded tracks stays at
/// most `max(2, 25% of the scope's tracks)`, so a flat folder holding two full albums (12 + 5) is
/// never merged.
const MINORITY_FOLD_SHARE: f64 = 0.25;
const MINORITY_FOLD_FLOOR: usize = 2;

/// What the grouping rules read from one track, normalized once when the record is stored (never
/// per regroup, which would re-normalize a whole folder on every insert). A pure function of the
/// record (`derive`) plus the `Library`-assigned `source`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Derived {
    /// `(normalized, cleaned)` form of the track's own ALBUM tag; `None` without one.
    pub album: Option<(String, String)>,
    /// `(normalized key, trimmed raw text)` of the ALBUMARTIST tag.
    pub album_artist: Option<(String, String)>,
    /// Whether the track carries an ARTIST or ALBUMARTIST tag at all (an untagged file that joined an
    /// album has no vote in its artist).
    pub has_artist: bool,
    /// `(normalized key, raw text)` of the primary artist: `display_artist` with a trailing
    /// featured-guest credit stripped.
    pub primary: (String, String),
    /// `normalize_copy_title(display_title)`.
    pub copy_title: String,
    /// Interned `SourceKey` id (`Library` assigns it; 0 for a value built outside a `Library`).
    pub source: u32,
}

pub(super) fn derive(record: &TrackRecord) -> Derived {
    let album = record.tags.album.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(|raw| (normalize_album_title(raw), clean_album_title(raw)));
    let album_artist =
        record.tags.album_artist.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(|raw| (normalize_artist_key(raw), raw.to_owned()));
    let has_artist = [&record.tags.artist, &record.tags.album_artist].into_iter().any(|tag| tag.as_deref().is_some_and(|s| !s.trim().is_empty()));
    let artist = display_artist(record);
    let primary_raw = strip_feat_suffix(&artist).to_owned();
    Derived {
        album,
        album_artist,
        has_artist,
        primary: (normalize_artist_key(&primary_raw), primary_raw),
        copy_title: normalize_copy_title(&display_title(record)),
        source: 0,
    }
}

/// Where a track comes from for copy detection: its directory, its format and whether it is a CUE
/// sub-range of an image file. Copies are judged between sources, never inside one.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct SourceKey {
    dir: PathBuf,
    format: String,
    cue_range: bool,
}

pub(super) fn source_key(record: &TrackRecord) -> SourceKey {
    SourceKey {
        dir: record.key.path.parent().map(std::path::Path::to_path_buf).unwrap_or_default(),
        format: record.info.format.to_ascii_lowercase(),
        cue_range: record.key.start_frame != 0 || record.key.end_frame.is_some(),
    }
}

/// The words of `input` folded for key comparison: Unicode-decomposed with combining marks dropped
/// (accent-insensitive), lowercased, apostrophes removed (`don't` -> `dont`), every other
/// non-alphanumeric character a separator, and `&` / `+` / `and` all dropped as one filler word so
/// `Earth, Wind & Fire`, `Earth Wind and Fire` and `Earth Wind Fire` agree.
fn folded_words(input: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    let flush = |current: &mut String, words: &mut Vec<String>| {
        if !current.is_empty() {
            words.push(std::mem::take(current));
        }
    };
    for ch in input.nfd() {
        if is_combining_mark(ch) {
            continue;
        }
        for lower in ch.to_lowercase() {
            let lower = strip_latin_diacritic(lower);
            match lower {
                '\'' | '\u{2019}' | '\u{2018}' | '`' => {}
                '&' | '+' => {
                    flush(&mut current, &mut words);
                }
                c if c.is_alphanumeric() => current.push(c),
                _ => flush(&mut current, &mut words),
            }
        }
    }
    flush(&mut current, &mut words);
    words.retain(|word| word != "and");
    words
}

/// Lowercased, whitespace-collapsed, NFC text: the fallback key for a string that folds to nothing
/// (an album called `!!!`).
fn plain_key(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ").nfc().collect::<String>().to_lowercase()
}

/// The grouping-key form of an artist name: case/accent-insensitive, punctuation stripped, `&` /
/// `and` / `+` treated alike, commas and whitespace collapsed. Never used for display.
pub(super) fn normalize_artist_key(artist: &str) -> String {
    let words = folded_words(artist);
    if words.is_empty() { plain_key(artist) } else { words.join(" ") }
}

/// The grouping-key form of an album title: `clean_album_title` first (trailing disc/media markers
/// gone), then the artist-style folding, with a leading `The` ignored. Edition words such as
/// `Remastered` or `Deluxe` are deliberately kept: those releases can have different track lists.
pub(super) fn normalize_album_title(title: &str) -> String {
    let mut words = folded_words(&clean_album_title(title));
    if words.len() > 1 && words[0] == "the" {
        words.remove(0);
    }
    if words.is_empty() { plain_key(title) } else { words.join(" ") }
}

/// `title` with whitespace collapsed and trailing disc markers (`(CD1)`, `[CD 2]`, `(Disc 1)`,
/// `- Disc 2`, `CD01`, `(Disc One)`) and media markers (`(LP)`, `[LP]`, `(Vinyl)`, `(12")`,
/// `(12'')`) removed, case preserved. This is the album title shown to the user. A title that would
/// be left empty is returned unchanged.
pub(super) fn clean_album_title(title: &str) -> String {
    let mut current = title.split_whitespace().collect::<Vec<_>>().join(" ");
    while let Some(shorter) = strip_one_trailing_marker(&current) {
        if shorter.is_empty() {
            break;
        }
        current = shorter;
    }
    current
}

fn strip_one_trailing_marker(title: &str) -> Option<String> {
    // `to_ascii_lowercase` keeps byte offsets, so offsets found in `lower` slice `title` safely.
    let lower = title.to_ascii_lowercase();
    for (open, close) in [('(', ')'), ('[', ']')] {
        if lower.ends_with(close)
            && let Some(start) = lower.rfind(open)
        {
            let inner = lower[start + 1..lower.len() - 1].trim();
            if is_marker(inner) {
                return Some(trim_separators(&title[..start]).to_owned());
            }
        }
    }
    let tokens: Vec<(usize, &str)> = lower.split(' ').scan(0usize, |offset, token| {
        let start = *offset;
        *offset += token.len() + 1;
        Some((start, token))
    }).collect();
    for take in [1usize, 2] {
        if tokens.len() > take {
            let start = tokens[tokens.len() - take].0;
            if matches!(parse_disc_designator(&lower[start..]), Some((_, ""))) {
                return Some(trim_separators(&title[..start]).to_owned());
            }
        }
    }
    None
}

fn trim_separators(text: &str) -> &str {
    text.trim_end_matches([' ', '-', '\u{2013}', '\u{2014}', ':', ',', '/', '|'])
}

/// Whether `inner` (lowercased, the inside of a bracket pair) names a disc or a physical medium.
fn is_marker(inner: &str) -> bool {
    const MEDIA: [&str; 9] = ["lp", "vinyl", "12\"", "12''", "12\u{201d}", "12\u{2033}", "12 inch", "12-inch", "12in"];
    MEDIA.contains(&inner) || is_multi_lp(inner) || matches!(parse_disc_designator(inner), Some((_, "")))
}

/// `2 LP`, `2LP`, `2xLP`, `3 LPs`: a count of vinyl records.
fn is_multi_lp(inner: &str) -> bool {
    let rest = inner.trim_start_matches(|c: char| c.is_ascii_digit());
    rest.len() < inner.len() && matches!(rest.trim_start_matches(['x', ' ']), "lp" | "lps")
}

const SPELLED_NUMBERS: [&str; 10] = ["one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten"];

/// Parses a leading disc designator from lowercased `text`: a `cd`/`disc`/`disk` word, optional
/// `[ .-_]` separators, then digits or a spelled-out `one`..`ten`. Returns the disc number and
/// whatever follows it. A bare word with no number (`cd`, `disco`, `discography`) is `None`.
pub(super) fn parse_disc_designator(text: &str) -> Option<(u32, &str)> {
    let text = text.trim();
    let rest = ["disc", "disk", "cd"].into_iter().find_map(|word| text.strip_prefix(word))?;
    let rest = rest.trim_start_matches([' ', '.', '-', '_']);
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 {
        return Some((rest[..digits].parse().unwrap_or(u32::MAX), &rest[digits..]));
    }
    // A spelled number must end at a word boundary: `Disc Tentacles` is not disc ten.
    SPELLED_NUMBERS.iter().enumerate().find_map(|(index, word)| {
        let after = rest.strip_prefix(*word)?;
        after.chars().next().is_none_or(|c| !c.is_alphanumeric()).then_some((index as u32 + 1, after))
    })
}

/// The disc a disc-subfolder name stands for (`CD1` -> 1, `Disc One` -> 1); `None` for any other name.
pub(super) fn disc_number_from_folder(name: &str) -> Option<u32> {
    parse_disc_designator(&name.to_ascii_lowercase()).map(|(number, _)| number)
}

/// The albums one grouping scope resolves to: `groups` are the distinct albums, `assignment[i]` the
/// group index of the scope's track `i` (`None` = no album at all).
pub(super) struct ScopeAlbums {
    pub groups: Vec<EffectiveAlbum>,
    pub assignment: Vec<Option<usize>>,
}

/// Whether `track`'s artist is one the dominant album already has (a primary artist or album artist
/// of a dominant-title track), or it has no artist at all.
fn artist_agrees(track: &Derived, dominant_artists: &HashSet<&str>) -> bool {
    !track.has_artist
        || dominant_artists.contains(track.primary.0.as_str())
        || track.album_artist.as_ref().is_some_and(|(norm, _)| dominant_artists.contains(norm.as_str()))
}

/// Resolves, for one grouping scope, the album each track belongs to, from the tracks' own album
/// titles and artists (`tracks[i]` = the derived values of the scope's track `i`).
///
/// Real folders are tagged inconsistently, so a single tag is never trusted in isolation: when one
/// normalized title covers at least 60% of the scope's ALBUM-tagged tracks (the dominant title),
///
/// - a track with no ALBUM tag joins it when its artist agrees (or it has none), and
/// - minority titles fold into it, but only (i) while the TOTAL number of folded tracks stays within
///   `max(2, 25% of the scope's tracks)` (smallest titles first, ties by title), and (ii) a title
///   folds only when EVERY one of its tracks has a primary artist (or album artist) that appears on
///   at least one dominant-title track, or no artist at all. So a compilation's own tracks still fold,
///   while a different artist's single or EP in the same flat folder never does.
///
/// Otherwise every track keeps its own title (untagged tracks stay out of any album). The displayed
/// title of a resulting album is the most common cleaned raw title among its members, ties broken by
/// the smallest string.
pub(super) fn resolve_scope_albums(tracks: &[&Derived]) -> ScopeAlbums {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for track in tracks {
        if let Some((norm, _)) = &track.album {
            *counts.entry(norm.as_str()).or_default() += 1;
        }
    }
    let tagged: usize = counts.values().sum();
    let dominant: Option<&str> = counts
        .iter()
        .max_by(|(norm_a, count_a), (norm_b, count_b)| count_a.cmp(count_b).then_with(|| norm_b.cmp(norm_a)))
        .filter(|(_, count)| tagged > 0 && (**count as f64) / (tagged as f64) >= FOLDER_MAJORITY_THRESHOLD)
        .map(|(norm, _)| *norm);

    let mut folded: HashSet<&str> = HashSet::new();
    let mut dominant_artists: HashSet<&str> = HashSet::new();
    if let Some(dominant) = dominant {
        for track in tracks {
            if track.album.as_ref().is_some_and(|(norm, _)| norm == dominant) {
                if track.has_artist {
                    dominant_artists.insert(track.primary.0.as_str());
                }
                if let Some((norm, _)) = &track.album_artist {
                    dominant_artists.insert(norm.as_str());
                }
            }
        }
        // A minority title is out of the question when any of its tracks has a foreign artist.
        let mut disagreeing: HashSet<&str> = HashSet::new();
        for track in tracks {
            if let Some((norm, _)) = &track.album
                && norm != dominant
                && !artist_agrees(track, &dominant_artists)
            {
                disagreeing.insert(norm.as_str());
            }
        }
        // Smallest first, so the cap is spent on the most plausible strays and never depends on order.
        let mut candidates: Vec<(usize, &str)> =
            counts.iter().filter(|(norm, _)| **norm != dominant && !disagreeing.contains(**norm)).map(|(norm, count)| (*count, *norm)).collect();
        candidates.sort();
        let fold_limit = (tracks.len() as f64 * MINORITY_FOLD_SHARE).max(MINORITY_FOLD_FLOOR as f64);
        let mut total = 0usize;
        for (count, norm) in candidates {
            if (total + count) as f64 > fold_limit {
                break;
            }
            total += count;
            folded.insert(norm);
        }
    }

    let mut group_of: HashMap<&str, usize> = HashMap::new();
    let mut group_norms: Vec<&str> = Vec::new();
    let mut assignment: Vec<Option<usize>> = Vec::with_capacity(tracks.len());
    for track in tracks {
        let norm: Option<&str> = match (&track.album, dominant) {
            (None, Some(dominant)) if artist_agrees(track, &dominant_artists) => Some(dominant),
            (None, _) => None,
            (Some((norm, _)), Some(dominant)) if norm != dominant && folded.contains(norm.as_str()) => Some(dominant),
            (Some((norm, _)), _) => Some(norm.as_str()),
        };
        assignment.push(norm.map(|norm| {
            *group_of.entry(norm).or_insert_with(|| {
                group_norms.push(norm);
                group_norms.len() - 1
            })
        }));
    }

    // Display title per group: the most common cleaned raw title among the members' own tags.
    let mut titles: HashMap<(usize, &str), usize> = HashMap::new();
    for (track, group) in tracks.iter().zip(&assignment) {
        if let (Some(group), Some((_, clean))) = (group, &track.album) {
            *titles.entry((*group, clean.as_str())).or_default() += 1;
        }
    }
    let mut best: Vec<Option<(&str, usize)>> = vec![None; group_norms.len()];
    for ((group, title), count) in titles {
        let better = match best[group] {
            None => true,
            Some((current, current_count)) => count > current_count || (count == current_count && title < current),
        };
        if better {
            best[group] = Some((title, count));
        }
    }
    let groups = group_norms
        .iter()
        .zip(&best)
        .map(|(norm, best)| EffectiveAlbum { norm: (*norm).to_owned(), title: best.map_or(*norm, |(title, _)| title).to_owned() })
        .collect();
    ScopeAlbums { groups, assignment }
}

// ---------------------------------------------------------------------------------------------
// Duplicate copies
// ---------------------------------------------------------------------------------------------

/// Tracks of one album count as copies of each other when their durations agree within
/// `max(5 s, 3%)`; an unknown duration on either side leaves the decision to the title.
const COPY_DURATION_FLOOR_MS: u64 = 5_000;
const COPY_DURATION_SHARE: f64 = 0.03;
/// Two sources are copies of each other only when at least this share of the smaller source's tracks
/// has a title+duration match in the other (a lone "Intro" on each of two live discs is not enough).
const SOURCE_LINK_SHARE: f64 = 0.5;

/// The comparison form of a track title for copy detection: a leading side/track prefix (`A1-`,
/// `A1 `, `01 - `, `1-01 `), a trailing featured-guest credit, a trailing remaster/version qualifier
/// (`(Remastered 2013)`, `- Remastered`, `[2011 Remaster]`) and all case, accent and punctuation
/// differences are ignored.
pub(super) fn normalize_copy_title(title: &str) -> String {
    let title = strip_track_prefix(title.trim());
    let title = strip_feat_credit(title);
    let title = strip_version_suffix(title);
    let words = folded_words(title);
    if words.is_empty() { plain_key(title) } else { words.join(" ") }
}

fn strip_track_prefix(title: &str) -> &str {
    let bytes = title.as_bytes();
    let is_separator = |c: u8| matches!(c, b' ' | b'-' | b'.' | b')' | b':' | b'_');
    let skip_separators = |from: usize| {
        let mut end = from;
        while end < bytes.len() && is_separator(bytes[end]) {
            end += 1;
        }
        end
    };
    let digits_at = |from: usize| bytes[from.min(bytes.len())..].iter().take_while(|b| b.is_ascii_digit()).count();

    // Vinyl side + position: `A1-Title`, `B2 Title`.
    if bytes.len() > 2 && matches!(bytes[0].to_ascii_lowercase(), b'a'..=b'h') {
        let n = digits_at(1);
        if (1..=2).contains(&n) && bytes.get(1 + n).is_some_and(|&c| matches!(c, b'-' | b'.' | b' ' | b'\t')) {
            let end = skip_separators(1 + n);
            if end < bytes.len() {
                return &title[end..];
            }
        }
    }
    // `1-01 Title` (disc-track) and `01 - Title` / `01. Title` (track). A real separator is required
    // (a space alone is not one), so a title that merely starts with a number (`99 Problems`,
    // `1979`) is kept.
    let first = digits_at(0);
    if (1..=3).contains(&first) {
        if matches!(bytes.get(first), Some(b'-' | b'.')) {
            let second = digits_at(first + 1);
            let after = first + 1 + second;
            if (2..=3).contains(&second) && bytes.get(after).is_some_and(|&c| is_separator(c)) {
                let end = skip_separators(after);
                if end < bytes.len() {
                    return &title[end..];
                }
            }
        }
        let end = skip_separators(first);
        let run = &bytes[first..end];
        // Two or more digits need a real separator; a single digit (`2-Step`, `3.14`) additionally
        // needs the separator spaced (`1 - Title`, `1. Title`).
        let spaced = first >= 2 || run.contains(&b' ');
        if end < bytes.len() && spaced && run.iter().any(|&c| c != b' ') {
            return &title[end..];
        }
    }
    title
}

fn strip_feat_credit(title: &str) -> &str {
    let lower = title.to_ascii_lowercase();
    const MARKERS: [&str; 15] =
        [" feat. ", " feat ", " ft. ", " ft ", " featuring ", "(feat.", "(feat ", "(ft.", "(ft ", "(featuring ", "[feat.", "[feat ", "[ft.", "[ft ", "[featuring "];
    match MARKERS.into_iter().filter_map(|marker| lower.find(marker)).min() {
        Some(position) if position > 0 => title[..position].trim(),
        _ => title,
    }
}

/// `title` without trailing remaster qualifiers, bracketed (`(Remastered 2013)`, `[Remastered]`) or
/// after a dash (`- Remastered 2013`). Never strips the whole title.
fn strip_version_suffix(title: &str) -> &str {
    let mut current = title.trim();
    while let Some(shorter) = strip_one_version_suffix(current) {
        if shorter.is_empty() {
            break;
        }
        current = shorter;
    }
    current
}

fn strip_one_version_suffix(title: &str) -> Option<&str> {
    for (open, close) in [('(', ')'), ('[', ']')] {
        if title.ends_with(close)
            && let Some(start) = title.rfind(open)
            && is_remaster_qualifier(&title[start + 1..title.len() - 1])
        {
            return Some(title[..start].trim_end());
        }
    }
    for separator in [" - ", " \u{2013} ", " \u{2014} "] {
        if let Some(position) = title.rfind(separator)
            && is_remaster_qualifier(&title[position + separator.len()..])
        {
            return Some(title[..position].trim_end());
        }
    }
    None
}

/// Whether `text` is only a remaster qualifier: the word `remaster(ed)` plus optional years and the
/// words `version` / `digital(ly)`.
fn is_remaster_qualifier(text: &str) -> bool {
    let mut remaster = false;
    for word in text.split(|c: char| !c.is_alphanumeric()).filter(|word| !word.is_empty()) {
        match word.to_lowercase().as_str() {
            "remaster" | "remastered" => remaster = true,
            "version" | "digital" | "digitally" => {}
            year if year.len() == 4 && year.bytes().all(|b| b.is_ascii_digit()) && (year.starts_with("19") || year.starts_with("20")) => {}
            _ => return false,
        }
    }
    remaster
}

/// The disc a track belongs to: its `DISCNUMBER` tag, else the number of the disc subfolder it sits
/// in (`CD2/` -> 2), else unknown. Drives the album's disc ordering and the copy-detection guard.
pub fn effective_disc_number(record: &TrackRecord) -> Option<u32> {
    record.tags.disc_number.or_else(|| {
        record.key.path.parent().and_then(|parent| parent.file_name()).and_then(|name| name.to_str()).and_then(disc_number_from_folder)
    })
}

fn is_lossy(record: &TrackRecord) -> bool {
    record.info.format.eq_ignore_ascii_case("mp3")
}

fn format_preference(record: &TrackRecord) -> u8 {
    match record.info.format.to_ascii_lowercase().as_str() {
        "flac" => 0,
        "wavpack" => 1,
        "wav" => 2,
        _ => 3,
    }
}

/// Float samples carry 24 bits of precision, so a 32-bit float source is not "deeper" than a 24-bit
/// integer one.
fn effective_bits(record: &TrackRecord) -> u32 {
    if record.info.is_float { record.info.bits_per_sample.min(24) } else { record.info.bits_per_sample }
}

/// Best copy first: lossless over lossy, then higher effective bit depth, higher sample rate, integer
/// PCM (the strict bit-perfect route) over float, FLAC over WavPack over WAV, then the path and key
/// as a deterministic tie-break.
fn quality_cmp(a: &TrackRecord, b: &TrackRecord) -> Ordering {
    is_lossy(a)
        .cmp(&is_lossy(b))
        .then_with(|| effective_bits(b).cmp(&effective_bits(a)))
        .then_with(|| b.info.sample_rate.cmp(&a.info.sample_rate))
        .then_with(|| (!a.info.integer_pcm).cmp(&!b.info.integer_pcm))
        .then_with(|| format_preference(a).cmp(&format_preference(b)))
        .then_with(|| a.key.path.cmp(&b.key.path))
        .then_with(|| a.key.cmp(&b.key))
}

fn durations_agree(a: Option<u64>, b: Option<u64>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            let tolerance = ((a.max(b) as f64 * COPY_DURATION_SHARE) as u64).max(COPY_DURATION_FLOOR_MS);
            a.abs_diff(b) <= tolerance
        }
        _ => true,
    }
}

/// One track of an album as the copy planner sees it.
pub(super) struct CopyTrack<'a> {
    pub record: &'a TrackRecord,
    pub derived: &'a Derived,
}

/// Whether tracks `a` and `b` of different sources (same copy title assumed) can be one song: not the
/// same physical file, durations agree, and not two known different discs.
fn could_be_copies(a: &CopyTrack, b: &CopyTrack) -> bool {
    if a.record.key.path == b.record.key.path {
        return false;
    }
    if let (Some(disc_a), Some(disc_b)) = (effective_disc_number(a.record), effective_disc_number(b.record))
        && disc_a != disc_b
    {
        return false;
    }
    durations_agree(a.record.info.duration_ms, b.record.info.duration_ms)
}

/// For the tracks of one album, the winning copy each track hides behind (`None` = visible).
///
/// Copies are judged per SOURCE (`SourceKey`: directory + format family, CUE sub-ranges apart from
/// whole files): two tracks of one source are never copies (two songs of one folder and format,
/// whatever their titles say; two CUE tracks of one image), and two sources are linked only when the
/// title+duration matches between them cover at least half of the smaller source's tracks (and at
/// least two, unless the smaller source is a single track) — one "Intro" on each of two live discs
/// links nothing. Within linked sources, tracks are taken best quality first and one joins the first
/// earlier winner it is a copy of, provided it shares a source with no member of that winner's
/// cluster. The result depends only on the current set of tracks.
pub(super) fn plan_copies(tracks: &[CopyTrack]) -> Vec<Option<TrackKey>> {
    let mut hidden: Vec<Option<TrackKey>> = vec![None; tracks.len()];
    let Some(first) = tracks.first() else { return hidden };
    if tracks.iter().all(|track| track.derived.source == first.derived.source) {
        return hidden;
    }
    let mut buckets: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, track) in tracks.iter().enumerate() {
        if !track.derived.copy_title.is_empty() {
            buckets.entry(track.derived.copy_title.as_str()).or_default().push(index);
        }
    }
    buckets.retain(|_, members| {
        let source = tracks[members[0]].derived.source;
        members.len() >= 2 && members.iter().any(|&index| tracks[index].derived.source != source)
    });
    if buckets.is_empty() {
        return hidden;
    }

    let mut sizes: HashMap<u32, usize> = HashMap::new();
    for track in tracks {
        *sizes.entry(track.derived.source).or_default() += 1;
    }
    // (track, other source) pairs where the track has a title+duration match in that source.
    let mut partnered: HashSet<(usize, u32)> = HashSet::new();
    for members in buckets.values() {
        for (position, &x) in members.iter().enumerate() {
            for &y in &members[position + 1..] {
                let (source_x, source_y) = (tracks[x].derived.source, tracks[y].derived.source);
                if source_x != source_y && could_be_copies(&tracks[x], &tracks[y]) {
                    partnered.insert((x, source_y));
                    partnered.insert((y, source_x));
                }
            }
        }
    }
    let mut covered: HashMap<(u32, u32), usize> = HashMap::new();
    for &(index, other) in &partnered {
        *covered.entry((tracks[index].derived.source, other)).or_default() += 1;
    }
    let mut linked_cache: HashMap<(u32, u32), bool> = HashMap::new();
    let mut linked = |a: u32, b: u32| -> bool {
        *linked_cache.entry((a.min(b), a.max(b))).or_insert_with(|| {
            let matched = |from: u32, to: u32| covered.get(&(from, to)).copied().unwrap_or(0);
            let (size_a, size_b) = (sizes[&a], sizes[&b]);
            let found = match size_a.cmp(&size_b) {
                Ordering::Less => matched(a, b),
                Ordering::Greater => matched(b, a),
                Ordering::Equal => matched(a, b).max(matched(b, a)),
            };
            let smaller = size_a.min(size_b);
            found as f64 >= smaller as f64 * SOURCE_LINK_SHARE && (found >= 2 || smaller == 1)
        })
    };

    for members in buckets.values() {
        let mut order = members.clone();
        order.sort_by(|&a, &b| quality_cmp(tracks[a].record, tracks[b].record));
        let mut clusters: Vec<Vec<usize>> = Vec::new();
        for index in order {
            let source = tracks[index].derived.source;
            let home = clusters.iter().position(|cluster| {
                let winner = cluster[0];
                cluster.iter().all(|&member| tracks[member].derived.source != source)
                    && linked(tracks[winner].derived.source, source)
                    && could_be_copies(&tracks[winner], &tracks[index])
            });
            match home {
                Some(position) => {
                    hidden[index] = Some(tracks[clusters[position][0]].record.key.clone());
                    clusters[position].push(index);
                }
                None => clusters.push(vec![index]),
            }
        }
    }
    hidden
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn album_titles_drop_disc_and_media_markers_but_keep_edition_words() {
        for (raw, expected) in [
            ("Return Of The Space Cowboy (CD1)", "Return Of The Space Cowboy"),
            ("Return Of The Space Cowboy [CD 2]", "Return Of The Space Cowboy"),
            ("Album (Disc 1)", "Album"),
            ("Album - Disc 2", "Album"),
            ("Album CD01", "Album"),
            ("Album (Disc One)", "Album"),
            ("Off The Wall (LP)", "Off The Wall"),
            ("Off The Wall [LP]", "Off The Wall"),
            ("Off The Wall (Vinyl)", "Off The Wall"),
            ("Off The Wall (12\")", "Off The Wall"),
            ("Future Nostalgia (2 LP)", "Future Nostalgia"),
            ("Future Nostalgia [2xLP]", "Future Nostalgia"),
            ("Off The Wall (12'')", "Off The Wall"),
            ("Album (CD1) (LP)", "Album"),
            ("Dynamite (Remastered)", "Dynamite (Remastered)"),
            ("Greatest Hits (Complete Edition)", "Greatest Hits (Complete Edition)"),
            ("CD1", "CD1"),
            ("Discography", "Discography"),
        ] {
            assert_eq!(clean_album_title(raw), expected, "{raw}");
        }
    }

    #[test]
    fn album_title_keys_ignore_case_accents_punctuation_articles_and_markers() {
        assert_eq!(normalize_album_title("The Return Of The Space Cowboy (CD2)"), normalize_album_title("Return of the Space Cowboy (CD1)"));
        assert_eq!(normalize_album_title("Caf\u{e9} del Mar"), normalize_album_title("cafe del mar"));
        assert_eq!(normalize_album_title("Don't Stop!"), normalize_album_title("Dont Stop"));
        assert_eq!(normalize_album_title("Rock & Roll"), normalize_album_title("Rock and Roll"));
        assert_ne!(normalize_album_title("Dynamite"), normalize_album_title("Dynamite (Remastered)"));
        assert_ne!(normalize_album_title("Greatest Hits"), normalize_album_title("Greatest Hits (Special Edition)"));
        assert!(!normalize_album_title("!!!").is_empty(), "a title that folds to nothing keeps a fallback key");
        assert_eq!(normalize_album_title("The"), "the");
    }

    #[test]
    fn artist_keys_treat_ampersand_and_plus_alike_and_ignore_commas_and_whitespace() {
        let wind = normalize_artist_key("Earth, Wind & Fire");
        assert_eq!(normalize_artist_key("Earth Wind & Fire"), wind);
        assert_eq!(normalize_artist_key("EARTH WIND AND FIRE"), wind);
        assert_eq!(normalize_artist_key("Daryl Hall & John Oates"), normalize_artist_key("Daryl Hall  John Oates"));
        assert_eq!(normalize_artist_key("Florence + The Machine"), normalize_artist_key("Florence and the Machine"));
        assert_eq!(normalize_artist_key("Bj\u{f6}rk"), normalize_artist_key("bjork"));
        assert_ne!(normalize_artist_key("Simon & Garfunkel"), normalize_artist_key("Simon"));
    }

    fn derived(album: Option<&str>, artist: Option<&str>) -> Derived {
        let mut record = TrackRecord::minimal(
            TrackKey::whole_file(PathBuf::from("/m/x.flac")),
            crate::audio::AudioInfo { sample_rate: 44_100, duration_ms: None, source_channels: 2, bits_per_sample: 16, is_float: false, integer_pcm: true, format: "FLAC".into() },
        );
        record.tags.album = album.map(str::to_owned);
        record.tags.artist = artist.map(str::to_owned);
        derive(&record)
    }

    /// Resolves a scope of `(album, artist)` pairs into one `Option<EffectiveAlbum>` per track.
    fn resolve(tracks: &[(Option<&str>, Option<&str>)]) -> Vec<Option<EffectiveAlbum>> {
        let derived: Vec<Derived> = tracks.iter().map(|(album, artist)| derived(*album, *artist)).collect();
        let refs: Vec<&Derived> = derived.iter().collect();
        let resolved = resolve_scope_albums(&refs);
        resolved.assignment.iter().map(|group| group.map(|group| resolved.groups[group].clone())).collect()
    }

    /// `resolve` with every track by the same artist.
    fn own(titles: &[Option<&str>]) -> Vec<Option<EffectiveAlbum>> {
        resolve(&titles.iter().map(|title| (*title, Some("Band"))).collect::<Vec<_>>())
    }

    fn titles_of(resolved: &[Option<EffectiveAlbum>]) -> Vec<Option<&str>> {
        resolved.iter().map(|album| album.as_ref().map(|album| album.title.as_str())).collect()
    }

    #[test]
    fn folder_majority_folds_a_small_minority_and_untagged_tracks() {
        let mut titles = vec![Some("The Days / The Nights EP"); 4];
        titles.extend([Some("The Days & The Nights"); 2]);
        titles.push(None);
        let resolved = own(&titles);
        assert!(titles_of(&resolved).iter().all(|title| *title == Some("The Days / The Nights EP")), "{resolved:?}");
    }

    #[test]
    fn folder_majority_keeps_two_full_albums_in_a_flat_folder_apart() {
        let mut titles = vec![Some("First Album"); 12];
        titles.extend([Some("Second Album"); 5]);
        let resolved = own(&titles);
        assert_eq!(resolved.iter().flatten().filter(|album| album.title == "First Album").count(), 12);
        assert_eq!(resolved.iter().flatten().filter(|album| album.title == "Second Album").count(), 5);
    }

    #[test]
    fn folder_majority_needs_sixty_percent_and_never_adopts_untagged_tracks_without_it() {
        let resolved = own(&[Some("A"), Some("A"), Some("B"), Some("B"), None]);
        assert_eq!(titles_of(&resolved), vec![Some("A"), Some("A"), Some("B"), Some("B"), None]);
        assert_eq!(own(&[None, None]), vec![None, None]);
    }

    #[test]
    fn folder_majority_displays_the_most_common_cleaned_title_with_a_stable_tie_break() {
        let resolved = own(&[Some("The Return (CD1)"), Some("Return (CD2)"), Some("The Return"), Some("The Return (CD2)")]);
        assert!(resolved.iter().flatten().all(|album| album.title == "The Return"), "{resolved:?}");
        let tie = own(&[Some("The Album"), Some("Album")]);
        assert!(tie.iter().flatten().all(|album| album.title == "Album"));
    }

    #[test]
    fn a_minority_title_folds_only_for_the_dominant_albums_artists_and_within_the_total_cap() {
        let mut tracks: Vec<(Option<&str>, Option<&str>)> = vec![(Some("Big Album"), Some("Band")); 12];
        tracks.extend((0..8).map(|n| (Some(["S1", "S2", "S3", "S4", "S5", "S6", "S7", "S8"][n]), Some(["A", "B", "C", "D", "E", "F", "G", "H"][n]))));
        let resolved = resolve(&tracks);
        assert_eq!(resolved.iter().flatten().filter(|album| album.title == "Big Album").count(), 12, "other artists never fold");

        // A guest feat. credit on the stray track is stripped before comparing artists.
        let mut guest: Vec<(Option<&str>, Option<&str>)> = vec![(Some("Big Album"), Some("Band")); 5];
        guest.push((Some("Bonus"), Some("Band feat. Guest")));
        assert!(own_titles(&resolve(&guest)).iter().all(|title| title == "Big Album"));

        // An untagged track by a stranger stays out; one with no artist at all joins.
        let mut untagged: Vec<(Option<&str>, Option<&str>)> = vec![(Some("Big Album"), Some("Band")); 5];
        untagged.extend([(None, Some("Stranger")), (None, None)]);
        let resolved = resolve(&untagged);
        assert_eq!(resolved[5], None);
        assert_eq!(resolved[6].as_ref().map(|album| album.title.as_str()), Some("Big Album"));
    }

    fn own_titles(resolved: &[Option<EffectiveAlbum>]) -> Vec<String> {
        resolved.iter().flatten().map(|album| album.title.clone()).collect()
    }

    #[test]
    fn copy_titles_drop_side_and_track_prefixes_and_feat_credits() {
        let song = normalize_copy_title("Feels Just Like It Should");
        for raw in ["A1-Feels Just Like It Should", "A1 Feels Just Like It Should", "01 - Feels Just Like It Should", "1-01 Feels Just Like It Should", "01. Feels Just Like It Should", "feels just like it should (feat. Someone)"] {
            assert_eq!(normalize_copy_title(raw), song, "{raw}");
        }
        assert_eq!(normalize_copy_title("99 Problems"), "99 problems");
        assert_eq!(normalize_copy_title("1979"), "1979");
        assert_eq!(normalize_copy_title("7 Rings"), "7 rings");
        assert_eq!(normalize_copy_title("A Song"), "a song");
        assert_eq!(normalize_copy_title("Caf\u{e9}"), normalize_copy_title("cafe"));
    }

    #[test]
    fn feat_markers_need_a_real_marker_and_digit_titles_keep_their_number() {
        assert_eq!(normalize_copy_title("Song (Feather Mix)"), "song feather mix");
        assert_eq!(normalize_copy_title("Song (Ftp Edit)"), "song ftp edit");
        assert_eq!(normalize_copy_title("Song (feat. X)"), "song");
        assert_eq!(normalize_copy_title("Song [ft. X]"), "song");
        assert_eq!(normalize_copy_title("Song (featuring X)"), "song");
        assert_eq!(normalize_copy_title("2-Step"), "2 step");
        assert_eq!(normalize_copy_title("3.14"), "3 14");
        assert_eq!(normalize_copy_title("B12 Song"), "song");
        assert_eq!(normalize_copy_title("4 - Song"), "song");
        assert_eq!(normalize_copy_title("7 Rings"), "7 rings");
    }

    #[test]
    fn disc_numbers_come_from_folder_names() {
        assert_eq!(disc_number_from_folder("CD1"), Some(1));
        assert_eq!(disc_number_from_folder("Disc 2"), Some(2));
        assert_eq!(disc_number_from_folder("Disc One"), Some(1));
        assert_eq!(disc_number_from_folder("cd 03 - Bonus"), Some(3));
        assert_eq!(disc_number_from_folder("Disco"), None);
        assert_eq!(disc_number_from_folder("CD"), None);
    }

    #[test]
    fn spelled_disc_numbers_need_a_word_boundary() {
        for name in ["Disc Tentacles", "CD Tenor", "Disc Onerous", "Disk Fivefold", "CD Threesome"] {
            assert_eq!(disc_number_from_folder(name), None, "{name}");
        }
        assert_eq!(disc_number_from_folder("Disc Ten"), Some(10));
        assert_eq!(disc_number_from_folder("CD one"), Some(1));
        assert_eq!(disc_number_from_folder("Disc Ten - Bonus"), Some(10));
        assert_eq!(disc_number_from_folder("Disc Two (Remastered)"), Some(2));
        assert_eq!(clean_album_title("Album (Disc Tentacles)"), "Album (Disc Tentacles)");
    }

    #[test]
    fn copy_titles_ignore_remaster_and_version_suffixes() {
        let song = normalize_copy_title("Dynamite");
        for raw in [
            "Dynamite (Remastered)",
            "Dynamite (Remastered 2013)",
            "Dynamite (2013 Remaster)",
            "Dynamite - Remastered 2013",
            "Dynamite (Remastered Version)",
            "Dynamite [Remastered]",
            "Dynamite - 2011 Remaster",
            "A1-Dynamite (Remastered) (feat. Someone)",
        ] {
            assert_eq!(normalize_copy_title(raw), song, "{raw}");
        }
        assert_ne!(normalize_copy_title("Dynamite (Live)"), song);
        assert_ne!(normalize_copy_title("Dynamite (Remix)"), song);
        assert_ne!(normalize_copy_title("Dynamite (Acoustic Version)"), song);
        assert_eq!(normalize_copy_title("Remastered"), "remastered", "a title that is only the word is kept");
    }
}

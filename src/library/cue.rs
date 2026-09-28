//! CUE sheet parsing and audio-file resolution. Pure and I/O-free for parsing/time math
//! (`parse_cue`, `cue_time_to_sample_frame`), following the style of `format.rs`; file resolution
//! and dedup (`resolve_cue_files`, `dedupe_cue_sheets`) genuinely need `Path::exists`/`read_dir`
//! filesystem checks, documented at each call site, but never write anything.
//!
//! Wired into `library::scanner`'s per-directory cue detection/expansion (`CLAUDE.md` CUE-sheet
//! playback support): `parse_cue`, `resolve_cue_files`, `dedupe_cue_sheets` and
//! `cue_time_to_sample_frame` are all called from `scanner::expand_cue_sheets_for_paths`/
//! `scanner::expand_one_cue_sheet`.

use std::fmt;
use std::path::{Path, PathBuf};

use super::walker::{AUDIO_EXTENSIONS, has_audio_extension};

/// A parsed `.cue` sheet: global album-level fields plus one or more `FILE` blocks, each holding
/// its own tracks (a multi-file rip, e.g. one `.wv` per LP side, has more than one `CueFile`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CueSheet {
    /// Global `PERFORMER "..."`.
    pub performer: Option<String>,
    /// Global `TITLE "..."` (album title).
    pub title: Option<String>,
    /// `REM GENRE ...`.
    pub genre: Option<String>,
    /// `REM DATE ...`.
    pub date: Option<String>,
    pub files: Vec<CueFile>,
}

/// One `FILE "..." <FORMAT>` block and the tracks inside it.
#[derive(Clone, Debug, PartialEq)]
pub struct CueFile {
    /// The raw `FILE` value exactly as written in the cue sheet, e.g. `"Thriller.wv"` — not yet
    /// resolved to a real path on disk (see `resolve_cue_files`).
    pub file_field: String,
    pub tracks: Vec<CueTrack>,
}

/// One `TRACK nn AUDIO` block.
///
/// The `AUDIO` track-type keyword itself is not modeled: this app only ever plays back the PCM
/// stream of an already-decoded WAVE/WavPack/etc. file, so a non-`AUDIO` track type (e.g. a CD+G
/// `MODE1/2352` data track) would refer to a medium this player has no decoder for regardless —
/// every `TRACK` command is treated as audio, and the type keyword is ignored rather than causing
/// a parse error, so an unusual/mislabeled cue sheet still yields its track boundaries.
#[derive(Clone, Debug, PartialEq)]
pub struct CueTrack {
    pub number: u32,
    pub title: Option<String>,
    pub performer: Option<String>,
    /// `INDEX 00` (pregap start), optional.
    pub index00: Option<CueTime>,
    /// `INDEX 01` (track start). Required by the CUE spec; `parse_cue` errors if a `TRACK` block
    /// never sets it.
    pub index01: CueTime,
}

/// A `mm:ss:ff` CUE timestamp — 75 frames per second (the CD-audio/CUE timing base), independent
/// of any file's actual sample rate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct CueTime {
    pub minutes: u32,
    pub seconds: u32,
    pub frames: u32,
}

/// Where and why `parse_cue` gave up. `line` is the 1-based source line the problem was found on,
/// or `0` when the error isn't tied to one line (currently unused, reserved for whole-sheet
/// failures). Intended for a later diagnostic/status message, not for programmatic matching.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CueParseError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for CueParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line > 0 {
            write!(f, "cue sheet line {}: {}", self.line, self.message)
        } else {
            write!(f, "cue sheet: {}", self.message)
        }
    }
}

impl std::error::Error for CueParseError {}

/// A `CueTrack` still being assembled: `index01` is optional here and only promoted to
/// `CueTrack::index01` (required) once the block closes without error.
struct PendingTrack {
    number: u32,
    title: Option<String>,
    performer: Option<String>,
    index00: Option<CueTime>,
    index01: Option<CueTime>,
    /// The line the `TRACK` command started on, for `index01`-missing error messages.
    line: usize,
}

/// Parses a `.cue` sheet from raw file bytes.
///
/// Decoding: tries UTF-8 first, stripping a leading BOM (`EF BB BF`) if present. If the bytes
/// aren't valid UTF-8, falls back to a direct byte-to-codepoint (Latin-1/ISO-8859-1) mapping —
/// exact for Latin-1, but real-world cue sheets in this byte-range are more often CP1252, which
/// differs from Latin-1 only in the `0x80..=0x9F` control-code range (there, CP1252 has printable
/// characters like curly quotes and em dash where Latin-1 has C1 control codes). No encoding crate
/// (e.g. `encoding_rs`) is a dependency of this crate, so that narrow gap is a known limitation:
/// a non-UTF-8 cue sheet using a `0x80..=0x9F` CP1252 character (typically a smart quote/dash in a
/// title) decodes to the wrong (control-code) character instead. UTF-8 and plain Latin-1 sheets —
/// the common cases seen on real NAS libraries — are unaffected.
///
/// Line endings: both CRLF and LF are handled via `str::lines()`, which strips a trailing `\r`.
///
/// Command values may be quoted (`TITLE "Foo Bar"`) or bare (`TITLE Foo Bar`); both are accepted.
/// Unknown/unhandled commands are silently ignored (forward compatible). A `TRACK` block that
/// never sets `INDEX 01` is a parse error, since the CUE format requires it.
pub fn parse_cue(bytes: &[u8]) -> Result<CueSheet, CueParseError> {
    let text = decode_cue_bytes(bytes);

    let mut sheet = CueSheet::default();
    let mut current_file: Option<CueFile> = None;
    let mut pending_track: Option<PendingTrack> = None;

    for (index, raw_line) in text.lines().enumerate() {
        let line_no = index + 1;
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let (command, rest) = split_command(line);
        match command.to_ascii_uppercase().as_str() {
            "PERFORMER" => {
                let value = parse_value(rest);
                match pending_track.as_mut() {
                    Some(track) => track.performer = Some(value),
                    None => sheet.performer = Some(value),
                }
            }
            "TITLE" => {
                let value = parse_value(rest);
                match pending_track.as_mut() {
                    Some(track) => track.title = Some(value),
                    None => sheet.title = Some(value),
                }
            }
            "REM" => {
                let (sub_command, sub_rest) = split_command(rest);
                match sub_command.to_ascii_uppercase().as_str() {
                    "GENRE" => sheet.genre = Some(parse_value(sub_rest)),
                    "DATE" => sheet.date = Some(parse_value(sub_rest)),
                    // Other REM fields (COMMENT, DISCID, ...) carry nothing this player needs.
                    _ => {}
                }
            }
            "FILE" => {
                finalize_pending_track(&mut current_file, &mut pending_track)?;
                if let Some(file) = current_file.take() {
                    sheet.files.push(file);
                }
                let file_field = parse_file_field(rest)
                    .ok_or_else(|| CueParseError { line: line_no, message: "FILE command is missing a filename".to_owned() })?;
                current_file = Some(CueFile { file_field, tracks: Vec::new() });
            }
            "TRACK" => {
                finalize_pending_track(&mut current_file, &mut pending_track)?;
                if current_file.is_none() {
                    return Err(CueParseError { line: line_no, message: "TRACK command appears before any FILE command".to_owned() });
                }
                let mut parts = rest.split_whitespace();
                let number_str = parts
                    .next()
                    .ok_or_else(|| CueParseError { line: line_no, message: "TRACK command is missing a track number".to_owned() })?;
                let number: u32 = number_str
                    .parse()
                    .map_err(|_| CueParseError { line: line_no, message: format!("invalid TRACK number '{number_str}'") })?;
                pending_track = Some(PendingTrack { number, title: None, performer: None, index00: None, index01: None, line: line_no });
            }
            "INDEX" => {
                let mut parts = rest.split_whitespace();
                let index_kind = parts
                    .next()
                    .ok_or_else(|| CueParseError { line: line_no, message: "INDEX command is missing its index number".to_owned() })?;
                let time_str = parts
                    .next()
                    .ok_or_else(|| CueParseError { line: line_no, message: "INDEX command is missing its mm:ss:ff time".to_owned() })?;
                let time = parse_cue_time(time_str, line_no)?;
                let track = pending_track
                    .as_mut()
                    .ok_or_else(|| CueParseError { line: line_no, message: "INDEX command appears before any TRACK command".to_owned() })?;
                match index_kind {
                    "00" => track.index00 = Some(time),
                    "01" => track.index01 = Some(time),
                    // Secondary indexes (02+) exist in the spec but aren't modeled by `CueTrack`;
                    // ignored rather than erroring.
                    _ => {}
                }
            }
            // Forward-compatible: any other command (CATALOG, FLAGS, CDTEXTFILE, ...) is ignored.
            _ => {}
        }
    }

    finalize_pending_track(&mut current_file, &mut pending_track)?;
    if let Some(file) = current_file.take() {
        sheet.files.push(file);
    }

    Ok(sheet)
}

/// Decodes cue-sheet bytes to text (see `parse_cue`'s doc comment for the encoding policy).
fn decode_cue_bytes(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.strip_prefix('\u{feff}').unwrap_or(text).to_owned(),
        // Latin-1: every byte 0x00-0xFF maps directly to the Unicode scalar of the same value.
        Err(_) => bytes.iter().map(|&byte| byte as char).collect(),
    }
}

/// Splits `line` into its leading command word (uppercased comparison happens at the call site)
/// and the rest of the line with leading whitespace trimmed. `"INDEX 01 00:00:00"` ->
/// `("INDEX", "01 00:00:00")`.
fn split_command(line: &str) -> (&str, &str) {
    match line.find(char::is_whitespace) {
        Some(split_at) => (&line[..split_at], line[split_at..].trim_start()),
        None => (line, ""),
    }
}

/// A command value, quoted (`"Foo Bar"` -> `Foo Bar`) or bare (`Foo Bar` -> `Foo Bar`, just
/// trimmed).
fn parse_value(rest: &str) -> String {
    let trimmed = rest.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        trimmed[1..trimmed.len() - 1].to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// The filename out of a `FILE` command's value, which is followed by a format keyword
/// (`WAVE`/`WAVPACK`/`MP3`/...) that this parser ignores. Quoted: takes everything between the
/// first pair of quotes, so a quoted filename may itself contain spaces. Unquoted: the format
/// keyword is the last whitespace-separated token, dropped; the rest (rejoined with single spaces)
/// is the filename. Returns `None` for an empty value.
fn parse_file_field(rest: &str) -> Option<String> {
    let trimmed = rest.trim();
    if let Some(after_quote) = trimmed.strip_prefix('"') {
        let end = after_quote.find('"')?;
        return Some(after_quote[..end].to_owned());
    }
    let mut tokens: Vec<&str> = trimmed.split_whitespace().collect();
    if tokens.is_empty() {
        return None;
    }
    if tokens.len() > 1 {
        tokens.pop(); // the trailing format keyword
    }
    let name = tokens.join(" ");
    if name.is_empty() { None } else { Some(name) }
}

fn parse_cue_time(value: &str, line: usize) -> Result<CueTime, CueParseError> {
    let parts: Vec<&str> = value.split(':').collect();
    let [minutes_str, seconds_str, frames_str] = parts.as_slice() else {
        return Err(CueParseError { line, message: format!("invalid INDEX time '{value}': expected mm:ss:ff") });
    };
    let minutes = minutes_str
        .parse()
        .map_err(|_| CueParseError { line, message: format!("invalid INDEX minutes '{minutes_str}' in '{value}'") })?;
    let seconds = seconds_str
        .parse()
        .map_err(|_| CueParseError { line, message: format!("invalid INDEX seconds '{seconds_str}' in '{value}'") })?;
    let frames = frames_str
        .parse()
        .map_err(|_| CueParseError { line, message: format!("invalid INDEX frames '{frames_str}' in '{value}'") })?;
    Ok(CueTime { minutes, seconds, frames })
}

/// Closes out `pending`, if any: requires `index01` to have been set (a CUE spec requirement) and
/// that a `FILE` block is currently open, then appends the finished `CueTrack` to it. A no-op when
/// `pending` is `None` (called at every point a track block could end: a new `FILE`, a new
/// `TRACK`, or end of input).
fn finalize_pending_track(current_file: &mut Option<CueFile>, pending: &mut Option<PendingTrack>) -> Result<(), CueParseError> {
    let Some(track) = pending.take() else { return Ok(()) };
    let index01 = track
        .index01
        .ok_or_else(|| CueParseError { line: track.line, message: format!("TRACK {:02} is missing a required INDEX 01", track.number) })?;
    let file = current_file.as_mut().ok_or_else(|| CueParseError {
        line: track.line,
        message: format!("TRACK {:02} appears before any FILE command", track.number),
    })?;
    file.tracks.push(CueTrack { number: track.number, title: track.title, performer: track.performer, index00: track.index00, index01 });
    Ok(())
}

/// Converts a CUE `mm:ss:ff` time to an absolute sample-frame count at `sample_rate`:
/// `frame = (minutes*60 + seconds) * sample_rate + frames * sample_rate / 75`.
///
/// 75 frames/second is the fixed CD-audio/CUE timing base (independent of `sample_rate`, which is
/// the *decoded* file's own rate — e.g. a 96 kHz WavPack rip of a CD source). The division is
/// truncating integer division, not rounding, matching how a 1/75 s CUE frame quantizes onto a
/// sample grid it isn't a whole multiple of. This is the final sample-frame value; a caller
/// compares it directly against the decoder's frame counter, nothing further is applied.
pub fn cue_time_to_sample_frame(time: CueTime, sample_rate: u32) -> u64 {
    let whole_seconds = time.minutes as u64 * 60 + time.seconds as u64;
    let frame_samples = (time.frames as u64 * sample_rate as u64) / 75;
    whole_seconds * sample_rate as u64 + frame_samples
}

/// A `CueFile` paired with a real path resolved on disk (see `resolve_cue_files`).
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedCueFile {
    pub file_field: String,
    pub resolved_path: PathBuf,
    pub tracks: Vec<CueTrack>,
    /// `true` when `file_field` resolved by a literal join against the cue's directory; `false`
    /// when a stem/extension or single-file-in-folder fallback was needed. Read by
    /// `dedupe_cue_sheets`'s tie-break policy.
    pub resolved_directly: bool,
}

/// Resolves every `CueFile` in `sheet` to a real file on disk, relative to `cue_path`'s directory.
/// For each `FILE`, tries in order:
/// 1. `file_field` joined literally onto the cue's directory.
/// 2. The same directory, same file stem as `file_field`, tried against each supported audio
///    extension (`walker::AUDIO_EXTENSIONS`) — covers a cue that names `Album.wav` next to the
///    actual `Album.flac`.
/// 3. Only when `sheet.files` has exactly one entry: if the cue's directory contains exactly one
///    audio file (by the same extension list), that file — covers a renamed/retagged single-file
///    rip whose cue still names the old filename.
///
/// This is an all-or-nothing resolution: if any `FILE` in `sheet` can't be resolved by the above,
/// the whole call fails (a cue sheet that can only partially find its audio isn't safely playable
/// as a unit). Needs real filesystem `Path::is_file`/`read_dir` calls; otherwise side-effect-free.
pub fn resolve_cue_files(cue_path: &Path, sheet: &CueSheet) -> Result<Vec<ResolvedCueFile>, String> {
    let dir = cue_path.parent().unwrap_or_else(|| Path::new("."));
    let is_only_file = sheet.files.len() == 1;
    sheet.files.iter().map(|file| resolve_one_cue_file(dir, file, is_only_file)).collect()
}

fn resolve_one_cue_file(dir: &Path, file: &CueFile, is_only_file: bool) -> Result<ResolvedCueFile, String> {
    let literal = dir.join(&file.file_field);
    if literal.is_file() {
        return Ok(ResolvedCueFile { file_field: file.file_field.clone(), resolved_path: literal, tracks: file.tracks.clone(), resolved_directly: true });
    }

    if let Some(stem) = Path::new(&file.file_field).file_stem().and_then(|stem| stem.to_str()) {
        for ext in AUDIO_EXTENSIONS {
            let candidate = dir.join(format!("{stem}.{ext}"));
            if candidate.is_file() {
                return Ok(ResolvedCueFile { file_field: file.file_field.clone(), resolved_path: candidate, tracks: file.tracks.clone(), resolved_directly: false });
            }
        }
    }

    if is_only_file && let Some(only) = single_audio_file_in_dir(dir) {
        return Ok(ResolvedCueFile { file_field: file.file_field.clone(), resolved_path: only, tracks: file.tracks.clone(), resolved_directly: false });
    }

    Err(format!("could not resolve FILE \"{}\" referenced by cue sheet in {}", file.file_field, dir.display()))
}

/// `Some(path)` when `dir` contains exactly one audio file (`walker::has_audio_extension`);
/// `None` otherwise (zero, or more than one — ambiguous).
fn single_audio_file_in_dir(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut found: Option<PathBuf> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && has_audio_extension(&path) {
            if found.is_some() {
                return None;
            }
            found = Some(path);
        }
    }
    found
}

/// Given several already-resolved cue sheets found in the same folder (`(cue_path,
/// resolved_files)` pairs, `resolved_files` from `resolve_cue_files`), returns the indices into
/// `sheets` to keep. Two sheets are duplicates when their resolved audio-file-path sets are equal,
/// compared via `std::fs::canonicalize` (falling back to the already-joined path when
/// canonicalization fails, e.g. a path that doesn't exist).
///
/// Deduplication policy, in order: prefer the sheet whose files *all* resolved directly
/// (`ResolvedCueFile::resolved_directly`, i.e. no stem/folder fallback needed) over one that
/// needed a fallback for any of its files; if still tied, prefer whichever cue file name sorts
/// earlier. This deliberately deviates from a `&[(PathBuf, CueSheet)]` signature — dedup needs the
/// *resolved* paths (step 3's output), not the raw unresolved `CueSheet`, so callers are expected
/// to resolve first via `resolve_cue_files`.
pub fn dedupe_cue_sheets(sheets: &[(PathBuf, Vec<ResolvedCueFile>)]) -> Vec<usize> {
    let signatures: Vec<Vec<PathBuf>> = sheets
        .iter()
        .map(|(_, resolved)| {
            let mut paths: Vec<PathBuf> =
                resolved.iter().map(|file| std::fs::canonicalize(&file.resolved_path).unwrap_or_else(|_| file.resolved_path.clone())).collect();
            paths.sort();
            paths
        })
        .collect();
    let all_direct: Vec<bool> = sheets.iter().map(|(_, resolved)| resolved.iter().all(|file| file.resolved_directly)).collect();

    let mut by_filename: Vec<usize> = (0..sheets.len()).collect();
    by_filename.sort_by_key(|&index| sheets[index].0.file_name().map(|name| name.to_owned()));

    let mut kept: Vec<usize> = Vec::new();
    for index in by_filename {
        match kept.iter().position(|&kept_index| signatures[kept_index] == signatures[index]) {
            Some(position) if all_direct[index] && !all_direct[kept[position]] => kept[position] = index,
            Some(_) => {} // duplicate of an already-kept sheet that ties or wins on directness
            None => kept.push(index),
        }
    }
    kept.sort_unstable();
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_test_dir(name: &str) -> PathBuf {
        let suffix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("lime-player-cue-{name}-{}-{suffix}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(path: &Path) {
        std::fs::write(path, b"x").unwrap();
    }

    fn time(minutes: u32, seconds: u32, frames: u32) -> CueTime {
        CueTime { minutes, seconds, frames }
    }

    const SIMPLE_CUE: &str = "PERFORMER \"Bruno Mars\"\r\n\
TITLE \"Doo-Wops\"\r\n\
REM GENRE Pop\r\n\
REM DATE 2010\r\n\
FILE \"track.wav\" WAVE\r\n\
  TRACK 01 AUDIO\r\n\
    TITLE \"Grenade\"\r\n\
    PERFORMER \"Bruno Mars\"\r\n\
    INDEX 00 00:00:00\r\n\
    INDEX 01 00:02:00\r\n\
  TRACK 02 AUDIO\r\n\
    TITLE \"Talking to the Moon\"\r\n\
    INDEX 01 03:42:43\r\n";

    #[test]
    fn bom_and_utf8_parses() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(SIMPLE_CUE.as_bytes());

        let sheet = parse_cue(&bytes).unwrap();

        assert_eq!(sheet.performer.as_deref(), Some("Bruno Mars"));
        assert_eq!(sheet.title.as_deref(), Some("Doo-Wops"));
    }

    #[test]
    fn crlf_line_endings_parse() {
        let sheet = parse_cue(SIMPLE_CUE.as_bytes()).unwrap();

        assert_eq!(sheet.files.len(), 1);
        assert_eq!(sheet.files[0].tracks.len(), 2);
        assert_eq!(sheet.files[0].tracks[0].index01, time(0, 2, 0));
    }

    #[test]
    fn latin1_cp1252_fallback_decodes_non_utf8_bytes() {
        // 0xE9 alone (not followed by a valid UTF-8 continuation byte) is invalid UTF-8, so this
        // forces the Latin-1 fallback; 0xE9 is 'é' in both Latin-1 and CP1252.
        let bytes = b"TITLE \"Caf\xE9\"\nFILE \"a.wav\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n".to_vec();
        assert!(std::str::from_utf8(&bytes).is_err(), "test fixture must actually be invalid UTF-8");

        let sheet = parse_cue(&bytes).unwrap();

        assert_eq!(sheet.title.as_deref(), Some("Café"));
    }

    #[test]
    fn quoted_and_unquoted_values_both_parse() {
        let cue = "TITLE \"Quoted Title\"\nPERFORMER Unquoted Performer\nFILE \"a.wav\" WAVE\n  TRACK 01 AUDIO\n    TITLE Bare Track Title\n    INDEX 01 00:00:00\n";

        let sheet = parse_cue(cue.as_bytes()).unwrap();

        assert_eq!(sheet.title.as_deref(), Some("Quoted Title"));
        assert_eq!(sheet.performer.as_deref(), Some("Unquoted Performer"));
        assert_eq!(sheet.files[0].tracks[0].title.as_deref(), Some("Bare Track Title"));
    }

    #[test]
    fn rem_genre_and_date_are_captured() {
        let sheet = parse_cue(SIMPLE_CUE.as_bytes()).unwrap();

        assert_eq!(sheet.genre.as_deref(), Some("Pop"));
        assert_eq!(sheet.date.as_deref(), Some("2010"));
    }

    #[test]
    fn index00_pregap_is_optional() {
        let sheet = parse_cue(SIMPLE_CUE.as_bytes()).unwrap();

        let with_pregap = &sheet.files[0].tracks[0];
        assert_eq!(with_pregap.index00, Some(time(0, 0, 0)));
        assert_eq!(with_pregap.index01, time(0, 2, 0));

        let without_pregap = &sheet.files[0].tracks[1];
        assert_eq!(without_pregap.index00, None);
        assert_eq!(without_pregap.index01, time(3, 42, 43));
    }

    #[test]
    fn multiple_file_blocks_each_get_their_own_tracks() {
        let cue = "TITLE \"Double LP\"\n\
FILE \"Side A.wv\" WAVPACK\n\
  TRACK 01 AUDIO\n    TITLE \"A1\"\n    INDEX 01 00:00:00\n\
  TRACK 02 AUDIO\n    TITLE \"A2\"\n    INDEX 01 05:00:00\n\
FILE \"Side B.wv\" WAVPACK\n\
  TRACK 03 AUDIO\n    TITLE \"B1\"\n    INDEX 01 00:00:00\n";

        let sheet = parse_cue(cue.as_bytes()).unwrap();

        assert_eq!(sheet.files.len(), 2);
        assert_eq!(sheet.files[0].file_field, "Side A.wv");
        assert_eq!(sheet.files[0].tracks.len(), 2);
        assert_eq!(sheet.files[1].file_field, "Side B.wv");
        assert_eq!(sheet.files[1].tracks.len(), 1);
        assert_eq!(sheet.files[1].tracks[0].number, 3);
    }

    #[test]
    fn malformed_track_missing_index01_is_an_error_not_a_panic() {
        let cue = "FILE \"a.wav\" WAVE\n  TRACK 01 AUDIO\n    TITLE \"No Index\"\n";

        let result = parse_cue(cue.as_bytes());

        let err = result.unwrap_err();
        assert!(err.message.contains("INDEX 01"), "unexpected message: {}", err.message);
    }

    #[test]
    fn cue_time_to_sample_frame_exact_and_truncating() {
        assert_eq!(cue_time_to_sample_frame(time(0, 0, 0), 44_100), 0);
        // Whole seconds only: no fractional-frame remainder to worry about.
        assert_eq!(cue_time_to_sample_frame(time(0, 2, 0), 44_100), 88_200);
        // 44100 and 48000 are both multiples of 75, so a single frame divides evenly at either
        // rate (44100/75 = 588, 48000/75 = 640) — these confirm the non-remainder case at two
        // different, real WavPack/FLAC-capable rates.
        assert_eq!(cue_time_to_sample_frame(time(0, 0, 1), 44_100), 588);
        assert_eq!(cue_time_to_sample_frame(time(0, 0, 1), 48_000), 640);
        assert_eq!(cue_time_to_sample_frame(time(0, 0, 1), 96_000), 1_280);
        // 44,000 Hz is NOT a multiple of 75: 37 * 44000 = 1,628,000, and 1,628,000 / 75 =
        // 21,706 remainder 50. Truncating integer division must yield 21,706, not 21,707.
        assert_eq!(cue_time_to_sample_frame(time(0, 0, 37), 44_000), 21_706);
        // Minutes and seconds combine correctly at a hi-res rate.
        assert_eq!(cue_time_to_sample_frame(time(1, 1, 0), 96_000), 61 * 96_000);
    }

    #[test]
    fn resolve_literal_match() {
        let dir = temp_test_dir("literal");
        touch(&dir.join("Thriller.wv"));
        let cue_path = dir.join("Thriller.cue");
        let sheet = CueSheet { files: vec![CueFile { file_field: "Thriller.wv".to_owned(), tracks: vec![] }], ..Default::default() };

        let resolved = resolve_cue_files(&cue_path, &sheet).unwrap();

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].resolved_path, dir.join("Thriller.wv"));
        assert!(resolved[0].resolved_directly);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_stem_fallback_match() {
        let dir = temp_test_dir("stem-fallback");
        touch(&dir.join("Thriller.flac")); // cue names a .wav, real file is a .flac with same stem
        let cue_path = dir.join("Thriller.cue");
        let sheet = CueSheet { files: vec![CueFile { file_field: "Thriller.wav".to_owned(), tracks: vec![] }], ..Default::default() };

        let resolved = resolve_cue_files(&cue_path, &sheet).unwrap();

        assert_eq!(resolved[0].resolved_path, dir.join("Thriller.flac"));
        assert!(!resolved[0].resolved_directly);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_single_file_in_folder_fallback() {
        let dir = temp_test_dir("single-file-fallback");
        touch(&dir.join("Actual Rip.wv")); // unrelated name; the only audio file in the folder
        let cue_path = dir.join("Thriller.cue");
        let sheet = CueSheet { files: vec![CueFile { file_field: "Thriller.wav".to_owned(), tracks: vec![] }], ..Default::default() };

        let resolved = resolve_cue_files(&cue_path, &sheet).unwrap();

        assert_eq!(resolved[0].resolved_path, dir.join("Actual Rip.wv"));
        assert!(!resolved[0].resolved_directly);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_failure_when_nothing_matches() {
        let dir = temp_test_dir("resolve-failure");
        touch(&dir.join("Unrelated.wv"));
        touch(&dir.join("AlsoUnrelated.flac")); // two audio files: single-file fallback can't apply
        let cue_path = dir.join("Thriller.cue");
        let sheet = CueSheet { files: vec![CueFile { file_field: "Thriller.wav".to_owned(), tracks: vec![] }], ..Default::default() };

        let result = resolve_cue_files(&cue_path, &sheet);

        assert!(result.is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dedupe_keeps_the_direct_match_over_the_folder_fallback() {
        // The worked example this policy is built for: two cue sheets in one folder, one whose
        // FILE resolves directly, one whose FILE doesn't exist and falls back to "the only audio
        // file in the folder" — which happens to be the same file the other cue resolved to
        // directly.
        let dir = temp_test_dir("thriller-dedupe");
        touch(&dir.join("Thriller (LP) WV.wv")); // the only real audio file in the folder

        let direct_cue_path = dir.join("Thriller (LP) WV.cue");
        let direct_sheet =
            CueSheet { files: vec![CueFile { file_field: "Thriller (LP) WV.wv".to_owned(), tracks: vec![] }], ..Default::default() };
        let direct_resolved = resolve_cue_files(&direct_cue_path, &direct_sheet).unwrap();
        assert!(direct_resolved[0].resolved_directly);

        let fallback_cue_path = dir.join("Thriller (LP).cue");
        let fallback_sheet = CueSheet { files: vec![CueFile { file_field: "Thriller (LP).wav".to_owned(), tracks: vec![] }], ..Default::default() };
        let fallback_resolved = resolve_cue_files(&fallback_cue_path, &fallback_sheet).unwrap();
        assert!(!fallback_resolved[0].resolved_directly);
        // Both cue sheets must have resolved to the exact same real file for this to be a
        // meaningful dedupe test.
        assert_eq!(direct_resolved[0].resolved_path, fallback_resolved[0].resolved_path);

        let sheets = vec![(fallback_cue_path.clone(), fallback_resolved), (direct_cue_path.clone(), direct_resolved)];

        let kept = dedupe_cue_sheets(&sheets);

        assert_eq!(kept, vec![1], "must keep the direct match (index 1) and drop the fallback (index 0)");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dedupe_keeps_both_when_they_resolve_to_different_files() {
        let dir = temp_test_dir("no-duplicates");
        touch(&dir.join("Side A.wv"));
        touch(&dir.join("Side B.wv"));

        let cue_a_path = dir.join("Side A.cue");
        let sheet_a = CueSheet { files: vec![CueFile { file_field: "Side A.wv".to_owned(), tracks: vec![] }], ..Default::default() };
        let resolved_a = resolve_cue_files(&cue_a_path, &sheet_a).unwrap();

        let cue_b_path = dir.join("Side B.cue");
        let sheet_b = CueSheet { files: vec![CueFile { file_field: "Side B.wv".to_owned(), tracks: vec![] }], ..Default::default() };
        let resolved_b = resolve_cue_files(&cue_b_path, &sheet_b).unwrap();

        let sheets = vec![(cue_a_path, resolved_a), (cue_b_path, resolved_b)];

        let mut kept = dedupe_cue_sheets(&sheets);
        kept.sort_unstable();

        assert_eq!(kept, vec![0, 1], "two cue sheets resolving to different files must both be kept");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

//! Pure display-string formatting for the session library (`§5.9`). No I/O, no Slint types: this
//! module is unit-tested in isolation and consumed by both `app_state.rs` and the (future) view
//! projections.

/// `44100 -> "44.1\u{00A0}kHz"`, `48000 -> "48\u{00A0}kHz"`: `rate / 1000` to one decimal, with a
/// trailing `.0` dropped. Joined with a no-break space so the panel's word-wrap can never split the
/// value from its unit (`§7`, matches the Orange reference wrapping "1,363 kbps" as one unit).
pub fn format_khz(sample_rate_hz: u32) -> String {
    format!("{}\u{00A0}kHz", format_khz_bare(sample_rate_hz))
}

fn format_khz_bare(sample_rate_hz: u32) -> String {
    let khz = sample_rate_hz as f64 / 1000.0;
    let rounded = (khz * 10.0).round() / 10.0;
    let text = format!("{rounded:.1}");
    text.strip_suffix(".0").map(str::to_owned).unwrap_or(text)
}

/// The small chip badge shown on track rows: `"FLAC 24/44.1"`, `"WavPack 32f/48"`, `"MP3"`.
pub fn format_badge(format: &str, bits_per_sample: u32, sample_rate: u32, is_float: bool) -> String {
    if format.eq_ignore_ascii_case("MP3") {
        return "MP3".to_owned();
    }
    let khz = format_khz_bare(sample_rate);
    if is_float {
        format!("{format} {bits_per_sample}f/{khz}")
    } else {
        format!("{format} {bits_per_sample}/{khz}")
    }
}

/// `!lossy && (bits >= 24 || rate > 48 kHz)`; MP3 is the only lossy format in this milestone.
pub fn is_hi_res(bits_per_sample: u32, sample_rate: u32, lossy: bool) -> bool {
    !lossy && (bits_per_sample >= 24 || sample_rate > 48_000)
}

/// The now-playing format/size line: `"FLAC · 24-bit / 44.1 kHz\nStereo · 1,363 kbps"`. MP3 omits
/// the bit depth; the kbps segment is dropped when the size or the duration is unknown.
///
/// The two halves are joined with an explicit `\n`, not `" · "` (Orange's own two-line layout):
/// the U+00A0 no-break space between a kbps value and its unit keeps that pair together within a
/// line, but Slint 1.18.1's renderer does not treat U+00A0 as unbreakable and still wraps a long
/// line between them (e.g. splitting "2,304" from "kbps" at 1440x900). An explicit `\n` is a hard
/// line break the renderer always honors, so the second line only ever wraps within itself.
pub fn format_line(
    format: &str,
    bits_per_sample: u32,
    sample_rate: u32,
    is_float: bool,
    channels: u16,
    file_size: Option<u64>,
    duration_ms: Option<u64>,
) -> String {
    let khz = format_khz(sample_rate);
    let mut first_line = vec![format.to_owned()];
    if format.eq_ignore_ascii_case("MP3") {
        first_line.push(khz);
    } else if is_float {
        first_line.push(format!("{bits_per_sample}-bit float / {khz}"));
    } else {
        first_line.push(format!("{bits_per_sample}-bit / {khz}"));
    }
    let mut second_line = vec![if channels == 1 { "Mono".to_owned() } else { "Stereo".to_owned() }];
    if let Some(kbps) = format_kbps(file_size, duration_ms) {
        // No-break space: keeps the value glued to its unit within the second line itself.
        second_line.push(format!("{kbps}\u{00A0}kbps"));
    }
    format!("{}\n{}", first_line.join(" · "), second_line.join(" · "))
}

/// The average bitrate in kbit/s, including container and tags: `round(file_size_bytes * 8 /
/// duration_ms)`. `None` when either input is unknown or the duration is zero.
fn format_kbps(file_size: Option<u64>, duration_ms: Option<u64>) -> Option<String> {
    let size = file_size?;
    let duration = duration_ms.filter(|ms| *ms > 0)?;
    let kbps = ((size as f64 * 8.0) / duration as f64).round() as u64;
    Some(thousands(kbps))
}

/// Decimal file size units: `"6.6 MB"`, `"850 KB"`, `"1.2 GB"`. The unit is chosen from the
/// *rounded* value, not the raw one, so a value that rounds up into the next unit (e.g. 999,999
/// bytes) prints in that unit instead of overflowing its own (`"1000 KB"`).
pub fn format_file_size(bytes: u64) -> String {
    const KB: f64 = 1_000.0;
    const MB: f64 = KB * 1_000.0;
    const GB: f64 = MB * 1_000.0;
    let value = bytes as f64;
    if value >= GB || (value / MB).round() >= 1_000.0 {
        format!("{:.1} GB", value / GB)
    } else if value >= MB || (value / KB).round() >= 1_000.0 {
        format!("{:.1} MB", value / MB)
    } else if value >= KB {
        format!("{:.0} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

/// `"2021 · 12 tracks · 48:12"`. The year is dropped when unknown; the duration is `H:MM:SS` once
/// it reaches an hour. Feeds the album-detail header (`§5.7`).
pub fn format_album_meta(year: Option<u16>, track_count: usize, total_duration_ms: u64) -> String {
    let mut segments = Vec::new();
    if let Some(year) = year {
        segments.push(year.to_string());
    }
    segments.push(track_word(track_count));
    segments.push(format_album_duration(total_duration_ms));
    segments.join(" · ")
}

fn format_album_duration(total_duration_ms: u64) -> String {
    let total_seconds = total_duration_ms / 1_000;
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let seconds = total_seconds % 60;
    if hours >= 1 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn track_word(track_count: usize) -> String {
    if track_count == 1 { "1 track".to_owned() } else { format!("{track_count} tracks") }
}

/// `(Some(2021), 12) -> "2021 · 12 tracks"`, `(None, 12) -> "12 tracks"`. The year is printed
/// plainly (never with a thousands separator). Feeds album cards (`§5.5` `AlbumTile`).
pub fn format_album_card_subtitle(year: Option<u16>, track_count: usize) -> String {
    match year {
        Some(year) => format!("{year} · {}", track_word(track_count)),
        None => track_word(track_count),
    }
}

/// `(128, 33_120_000) -> "128 songs · 9 h 12 min"`, `(0, 0) -> "0 songs"`. Minutes are rounded
/// half-up with a minimum of 1 min once `total_ms > 0`; the sum is expected to include only known
/// durations. Feeds the Songs/Recently Added summary line (`§5.7`).
pub fn format_library_summary(count: usize, total_duration_ms: u64) -> String {
    let song_word = if count == 1 { "1 song".to_owned() } else { format!("{} songs", thousands(count as u64)) };
    if total_duration_ms == 0 {
        return song_word;
    }
    let total_minutes = ((total_duration_ms as f64) / 60_000.0).round().max(1.0) as u64;
    let hours = total_minutes / 60;
    let minutes = total_minutes % 60;
    let time_part = match (hours, minutes) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    };
    format!("{song_word} · {time_part}")
}

/// A dedicated Search view section header (`§3.3` "Search"): `"Songs (12)"` once every match is
/// shown, or `"Songs — Showing first 200 of 532"` once the section's cap (`SEARCH_SONGS_LIMIT`/
/// `SEARCH_ALBUMS_LIMIT` in `view_model.rs`) truncates the match count — the count is never
/// silently dropped.
pub fn format_search_section_header(label: &str, shown: usize, total: usize) -> String {
    if shown < total { format!("{label} \u{2014} Showing first {shown} of {total}") } else { format!("{label} ({total})") }
}

/// `mm:ss` (or `h:mm:ss` past an hour is not needed for playback position in this milestone;
/// matches the pre-Stage-3 `format_playback_time` behavior exactly, tests included).
pub fn format_clock(position_ms: u64) -> String {
    let total_seconds = position_ms / 1_000;
    format!("{}:{:02}", total_seconds / 60, total_seconds % 60)
}

/// `0.0..=1.0`, clamped, with `None`/zero duration reporting `0.0` (matches the pre-Stage-3
/// `playback_progress` behavior exactly, tests included).
pub fn playback_progress(position_ms: u64, duration_ms: Option<u64>) -> f32 {
    let Some(duration_ms) = duration_ms.filter(|duration| *duration > 0) else {
        return 0.0;
    };
    (position_ms.min(duration_ms) as f64 / duration_ms as f64) as f32
}

/// The sticky status shown when a scan batch (`§6` "Open Files…") had failures, either while
/// probing (phase 1) or while reading tags/artwork for an already-probed, already-added track
/// (phase 2): `"Added 18 of 20 files · 2 could not be opened (first: foo.wv: hybrid WavPack is
/// unsupported)"`, `"Added 20 of 20 files · 1 track's tags could not be read (bar.flac: ...)"`, or
/// both clauses together. `probe_failures` are files that never reached the library or queue at
/// all, so they *do* reduce the "Added N of {requested}" count; `metadata_failures` are files that
/// were already probed, enqueued and added — only their tags/artwork could not be read — so they
/// must never be counted as "not added" (`§1.4`, "refuse rather than silently degrade" — reporting
/// a tag-read failure as if the track itself was refused would misinform the user about what is
/// actually in the queue). Each list names only its first entry, so a multi-file failure still says
/// which one to look at. Never called with both lists empty — the caller restores the ordinary
/// route status instead when a batch had neither.
pub fn format_scan_failures_summary(requested: usize, probe_failures: &[(String, String)], metadata_failures: &[(String, String)]) -> String {
    let opened = requested.saturating_sub(probe_failures.len());
    let mut summary = format!("Added {opened} of {requested} files");
    if let Some((first_name, first_error)) = probe_failures.first() {
        if probe_failures.len() == 1 {
            summary.push_str(&format!(" · 1 could not be opened ({first_name}: {first_error})"));
        } else {
            summary.push_str(&format!(" · {} could not be opened (first: {first_name}: {first_error})", probe_failures.len()));
        }
    }
    if let Some((first_name, first_error)) = metadata_failures.first() {
        if metadata_failures.len() == 1 {
            summary.push_str(&format!(" · 1 track's tags could not be read ({first_name}: {first_error})"));
        } else {
            summary.push_str(&format!(
                " · {} tracks' tags could not be read (first: {first_name}: {first_error})",
                metadata_failures.len()
            ));
        }
    }
    summary
}

/// An album's format, aggregated across all of its tracks (`§5.5`/`§5.7` album format pill). The
/// per-track input is whatever `AudioInfo.format` string `probe_file` set in
/// `src/audio/decoder.rs` — today always exactly `"FLAC"`, `"WAV"`, `"MP3"` or `"WavPack"`, never
/// anything else, since every other extension is refused before a `TrackRecord` exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlbumFormat {
    Mp3,
    Flac,
    WavPack,
    Wav,
    Mix,
}

impl AlbumFormat {
    /// The pill's display label: `"MP3"`, `"FLAC"`, `"WavPack"`, `"WAV"`, `"Mix Formats"`.
    pub fn label(self) -> &'static str {
        match self {
            AlbumFormat::Mp3 => "MP3",
            AlbumFormat::Flac => "FLAC",
            AlbumFormat::WavPack => "WavPack",
            AlbumFormat::Wav => "WAV",
            AlbumFormat::Mix => "Mix Formats",
        }
    }

    /// The Slint-side `FormatPill.variant` string that picks the pill's color.
    pub fn variant(self) -> &'static str {
        match self {
            AlbumFormat::Mp3 => "mp3",
            AlbumFormat::Flac => "flac",
            AlbumFormat::WavPack => "wavpack",
            AlbumFormat::Wav => "wav",
            AlbumFormat::Mix => "mix",
        }
    }
}

/// Aggregates an album's per-track format strings into one album-level `AlbumFormat`: `None` for
/// an empty track list (an album whose tracks have not resolved yet), the matching single format
/// when every track shares the same one (matched case-insensitively — `probe_file` itself only
/// ever emits one fixed case per format, but nothing here should rely on that staying true), or
/// `AlbumFormat::Mix` once two or more distinct formats are present, however many. A format string
/// this module does not recognize also falls back to `Mix` rather than panicking or silently
/// picking a label for it — `probe_file` today never produces one, since every unsupported
/// extension is refused before a `TrackRecord` exists, but this keeps the function total.
pub fn aggregate_album_format<'a>(track_formats: impl Iterator<Item = &'a str>) -> Option<AlbumFormat> {
    let mut distinct: Vec<String> = Vec::new();
    for format in track_formats {
        let lower = format.to_ascii_lowercase();
        if !distinct.contains(&lower) {
            distinct.push(lower);
        }
    }
    match distinct.as_slice() {
        [] => None,
        [only] => Some(match only.as_str() {
            "mp3" => AlbumFormat::Mp3,
            "flac" => AlbumFormat::Flac,
            "wavpack" => AlbumFormat::WavPack,
            "wav" => AlbumFormat::Wav,
            _ => AlbumFormat::Mix,
        }),
        _ => Some(AlbumFormat::Mix),
    }
}

fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_khz_and_sizes() {
        assert_eq!(format_khz(44_100), "44.1\u{00A0}kHz");
        assert_eq!(format_khz(48_000), "48\u{00A0}kHz");
        assert_eq!(format_khz(88_200), "88.2\u{00A0}kHz");
        assert_eq!(format_khz(352_800), "352.8\u{00A0}kHz");

        assert_eq!(format_file_size(850_000), "850 KB");
        assert_eq!(format_file_size(6_600_000), "6.6 MB");
        assert_eq!(format_file_size(1_200_000_000), "1.2 GB");
        assert_eq!(format_file_size(500), "500 B");
        // Just below a unit boundary: the rounded display value reaches the next unit, so the
        // printed unit must too, rather than showing "1000 KB" or "1000.0 MB".
        assert_eq!(format_file_size(999_999), "1.0 MB");
        assert_eq!(format_file_size(999_950_000), "1.0 GB");
    }

    #[test]
    fn format_line_matches_orange_pattern() {
        // Two lines, split with an explicit `\n` before the channels/kbps segment (`§7`; see
        // `format_line`'s doc comment for why a no-break space alone is not enough in Slint 1.18.1).
        let line = format_line("FLAC", 24, 44_100, false, 2, Some(1_703_750), Some(10_000));
        assert_eq!(line, "FLAC · 24-bit / 44.1\u{00A0}kHz\nStereo · 1,363\u{00A0}kbps");
    }

    #[test]
    fn format_line_omits_bit_depth_for_mp3() {
        let line = format_line("MP3", 16, 44_100, false, 2, Some(1_320_000), Some(33_000));
        assert_eq!(line, "MP3 · 44.1\u{00A0}kHz\nStereo · 320\u{00A0}kbps");
    }

    #[test]
    fn format_line_drops_kbps_without_size_or_duration() {
        assert_eq!(format_line("FLAC", 16, 44_100, false, 1, None, Some(10_000)), "FLAC · 16-bit / 44.1\u{00A0}kHz\nMono");
        assert_eq!(
            format_line("WavPack", 32, 48_000, true, 2, Some(100), None),
            "WavPack · 32-bit float / 48\u{00A0}kHz\nStereo"
        );
    }

    #[test]
    fn format_badge_matches_orange_examples() {
        assert_eq!(format_badge("FLAC", 24, 44_100, false), "FLAC 24/44.1");
        assert_eq!(format_badge("WAV", 16, 44_100, false), "WAV 16/44.1");
        assert_eq!(format_badge("WavPack", 24, 96_000, false), "WavPack 24/96");
        assert_eq!(format_badge("WavPack", 32, 48_000, true), "WavPack 32f/48");
        assert_eq!(format_badge("MP3", 16, 44_100, false), "MP3");
    }

    #[test]
    fn hi_res_rules() {
        assert!(is_hi_res(24, 44_100, false));
        assert!(is_hi_res(16, 96_000, false));
        assert!(!is_hi_res(16, 44_100, false));
        assert!(!is_hi_res(24, 44_100, true), "MP3-like lossy formats are never hi-res");
    }

    #[test]
    fn album_meta_formats_counts_and_hours() {
        assert_eq!(format_album_meta(Some(2021), 12, 48 * 60_000 + 12_000), "2021 · 12 tracks · 48:12");
        assert_eq!(format_album_meta(Some(2021), 1, 3 * 60_000), "2021 · 1 track · 3:00");
        assert_eq!(format_album_meta(None, 12, 2 * 3_600_000), "12 tracks · 2:00:00");
    }

    #[test]
    fn album_card_subtitle_formats_year_and_count() {
        assert_eq!(format_album_card_subtitle(Some(2021), 12), "2021 · 12 tracks");
        assert_eq!(format_album_card_subtitle(Some(2021), 1), "2021 · 1 track");
        assert_eq!(format_album_card_subtitle(None, 12), "12 tracks");
    }

    #[test]
    fn library_summary_formats_counts_and_durations() {
        assert_eq!(format_library_summary(128, 33_120_000), "128 songs · 9 h 12 min");
        assert_eq!(format_library_summary(1, 2_700_000), "1 song · 45 min");
        assert_eq!(format_library_summary(3, 7_200_000), "3 songs · 2 h");
        assert_eq!(format_library_summary(2, 20_000), "2 songs · 1 min");
        assert_eq!(format_library_summary(0, 0), "0 songs");
        assert_eq!(format_library_summary(1_234, 60_000), "1,234 songs · 1 min");
    }

    #[test]
    fn scan_failures_summary_names_the_first_refusal_and_counts_the_rest() {
        assert_eq!(
            format_scan_failures_summary(12, &[("foo.wv".to_owned(), "hybrid WavPack is unsupported".to_owned())], &[]),
            "Added 11 of 12 files · 1 could not be opened (foo.wv: hybrid WavPack is unsupported)"
        );
        assert_eq!(
            format_scan_failures_summary(
                12,
                &[
                    ("foo.wv".to_owned(), "hybrid WavPack is unsupported".to_owned()),
                    ("bar.flac".to_owned(), "corrupt file".to_owned()),
                ],
                &[]
            ),
            "Added 10 of 12 files · 2 could not be opened (first: foo.wv: hybrid WavPack is unsupported)"
        );
    }

    #[test]
    fn scan_failures_summary_never_counts_metadata_failures_as_not_added() {
        // A phase-2 (tags/artwork) failure happens after the track was already probed, enqueued
        // and added to the library — it must not reduce the "Added N of {requested}" count the way
        // a phase-1 probe failure does (`§1.4`).
        assert_eq!(
            format_scan_failures_summary(5, &[], &[("bar.flac".to_owned(), "unreadable tag block".to_owned())]),
            "Added 5 of 5 files · 1 track's tags could not be read (bar.flac: unreadable tag block)"
        );
        assert_eq!(
            format_scan_failures_summary(
                5,
                &[],
                &[
                    ("bar.flac".to_owned(), "unreadable tag block".to_owned()),
                    ("baz.flac".to_owned(), "corrupt artwork".to_owned()),
                ]
            ),
            "Added 5 of 5 files · 2 tracks' tags could not be read (first: bar.flac: unreadable tag block)"
        );
    }

    #[test]
    fn scan_failures_summary_reports_both_probe_and_metadata_failures_together() {
        assert_eq!(
            format_scan_failures_summary(
                12,
                &[("foo.wv".to_owned(), "hybrid WavPack is unsupported".to_owned())],
                &[("bar.flac".to_owned(), "unreadable tag block".to_owned())]
            ),
            "Added 11 of 12 files · 1 could not be opened (foo.wv: hybrid WavPack is unsupported) \
             · 1 track's tags could not be read (bar.flac: unreadable tag block)"
        );
    }

    #[test]
    fn search_section_header_reports_the_count_or_the_cap() {
        assert_eq!(format_search_section_header("Songs", 12, 12), "Songs (12)");
        assert_eq!(format_search_section_header("Songs", 200, 532), "Songs \u{2014} Showing first 200 of 532");
        assert_eq!(format_search_section_header("Albums", 0, 0), "Albums (0)");
    }

    #[test]
    fn aggregate_album_format_matches_single_shared_formats() {
        assert_eq!(aggregate_album_format(["MP3", "MP3", "MP3"].into_iter()), Some(AlbumFormat::Mp3));
        assert_eq!(aggregate_album_format(["FLAC", "FLAC"].into_iter()), Some(AlbumFormat::Flac));
        assert_eq!(aggregate_album_format(["WavPack", "WavPack"].into_iter()), Some(AlbumFormat::WavPack));
        assert_eq!(aggregate_album_format(["WAV", "WAV"].into_iter()), Some(AlbumFormat::Wav));
    }

    #[test]
    fn aggregate_album_format_handles_single_track_and_empty_albums() {
        assert_eq!(aggregate_album_format(["FLAC"].into_iter()), Some(AlbumFormat::Flac));
        assert_eq!(aggregate_album_format(std::iter::empty()), None);
    }

    #[test]
    fn aggregate_album_format_is_mix_once_two_or_more_formats_are_present() {
        assert_eq!(aggregate_album_format(["FLAC", "MP3"].into_iter()), Some(AlbumFormat::Mix));
        assert_eq!(aggregate_album_format(["FLAC", "WAV", "WavPack"].into_iter()), Some(AlbumFormat::Mix));
        assert_eq!(aggregate_album_format(["MP3", "MP3", "WAV"].into_iter()), Some(AlbumFormat::Mix));
    }

    #[test]
    fn aggregate_album_format_matches_case_insensitively() {
        assert_eq!(aggregate_album_format(["flac", "FLAC", "Flac"].into_iter()), Some(AlbumFormat::Flac));
        assert_eq!(aggregate_album_format(["wavpack", "WAVPACK"].into_iter()), Some(AlbumFormat::WavPack));
    }

    #[test]
    fn aggregate_album_format_labels_and_variants() {
        assert_eq!(AlbumFormat::Mp3.label(), "MP3");
        assert_eq!(AlbumFormat::Flac.label(), "FLAC");
        assert_eq!(AlbumFormat::WavPack.label(), "WavPack");
        assert_eq!(AlbumFormat::Wav.label(), "WAV");
        assert_eq!(AlbumFormat::Mix.label(), "Mix Formats");
        assert_eq!(AlbumFormat::Mp3.variant(), "mp3");
        assert_eq!(AlbumFormat::Flac.variant(), "flac");
        assert_eq!(AlbumFormat::WavPack.variant(), "wavpack");
        assert_eq!(AlbumFormat::Wav.variant(), "wav");
        assert_eq!(AlbumFormat::Mix.variant(), "mix");
    }

    #[test]
    fn playback_time_and_progress_are_derived_from_timeline_values() {
        assert_eq!(format_clock(0), "0:00");
        assert_eq!(format_clock(65_999), "1:05");
        assert_eq!(format_clock(3_600_000), "60:00");
        assert_eq!(playback_progress(25, None), 0.0);
        assert_eq!(playback_progress(25, Some(0)), 0.0);
        assert_eq!(playback_progress(500, Some(1_000)), 0.5);
        assert_eq!(playback_progress(2_000, Some(1_000)), 1.0);
    }
}

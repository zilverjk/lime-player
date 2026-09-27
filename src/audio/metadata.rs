//! Tag, embedded-picture and lyrics extraction (`§4.4`).
//!
//! These functions run only on the library scanner thread (`lime-library-scanner`), never on the
//! UI thread or the audio controller thread: they do file I/O and, for pictures, hold onto
//! multi-megabyte byte buffers, neither of which belongs on a real-time-adjacent path.
//!
//! FLAC, WAV and MP3 go through symphonia's own metadata readers (Vorbis comments, RIFF INFO and
//! ID3v2 respectively). WavPack does not: `wavpack-0.4.0`'s tag API is unsound on malformed input
//! (`§2.6`), so its APEv2 tags are parsed here by a small in-repo, bounds-checked, safe-Rust
//! reader instead.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use symphonia::core::meta::{MetadataRevision, StandardTag, StandardVisualKey, Tag, Visual};

use super::decoder::{DecoderError, open_symphonia};

/// Embedded pictures larger than this are never copied into memory, in either extraction path
/// (`§4.4`).
const MAX_EMBEDDED_PICTURE_BYTES: usize = 20 * 1024 * 1024;

/// APEv2 tag size ceiling (items + footer); guards against a corrupt or hostile `size` field
/// asking for an implausibly large read (`§4.4` step 4).
const MAX_APEV2_TAG_BYTES: u64 = 32 * 1024 * 1024;

/// APEv2 item-count ceiling; guards against a corrupt `item_count` field (`§4.4` step 4).
const MAX_APEV2_ITEM_COUNT: u32 = 1024;

/// A key ASCII byte range allowed in an APEv2 item key (`§4.4` step 5).
const APEV2_KEY_MIN: u8 = 0x20;
const APEV2_KEY_MAX: u8 = 0x7E;
/// An APEv2 item key must be NUL-terminated within this many bytes (`§4.4` step 5).
const APEV2_KEY_MAX_LEN: usize = 255;
/// A binary APEv2 item's embedded filename, when present, must end in a NUL within this many
/// bytes of the value's start (`§4.4` step 6).
const APEV2_BINARY_FILENAME_SEARCH_LEN: usize = 256;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrackTags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub year: Option<u16>,
    pub genre: Option<String>,
    pub track_number: Option<u32>,
    pub track_total: Option<u32>,
    pub disc_number: Option<u32>,
    pub disc_total: Option<u32>,
    pub lyrics: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmbeddedPicture {
    pub media_type: Option<String>,
    pub front_cover: bool,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
pub struct TrackMetadata {
    pub tags: TrackTags,
    pub picture: Option<EmbeddedPicture>,
}

/// Extracts tags, an embedded picture (if any) and lyrics for `path`. Never blocks adding a track
/// to the library: callers treat an `Err` as "no tags", not as a reason to skip the file (`§4.4`
/// "Normalization").
pub fn read_metadata(path: &Path) -> Result<TrackMetadata, DecoderError> {
    if is_wavpack_path(path) {
        read_wavpack_metadata(path)
    } else {
        read_symphonia_metadata(path)
    }
}

fn is_wavpack_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()).map(str::to_ascii_lowercase).as_deref(),
        Some("wv") | Some("wavpack")
    )
}

// ---------------------------------------------------------------------------------------------
// Symphonia (FLAC, MP3, WAV)
// ---------------------------------------------------------------------------------------------

fn read_symphonia_metadata(path: &Path) -> Result<TrackMetadata, DecoderError> {
    let (mut format, _track_id, _codec_params) = open_symphonia(path)?;

    let mut revisions = Vec::new();
    {
        let mut md = format.metadata();
        while let Some(rev) = md.pop() {
            revisions.push(rev);
        }
        if let Some(cur) = md.current() {
            revisions.push(cur.clone());
        }
    }

    let mut tags = TrackTags::default();
    let mut year_tier = u8::MAX;
    for revision in &revisions {
        apply_symphonia_tags(&mut tags, &mut year_tier, &revision.media.tags);
        for per_track in &revision.per_track {
            apply_symphonia_tags(&mut tags, &mut year_tier, &per_track.metadata.tags);
        }
    }
    normalize_tags(&mut tags);

    let picture = select_embedded_picture(&revisions);

    Ok(TrackMetadata { tags, picture })
}

/// Applies every standard tag in `list` to `tags`, honoring "first non-empty value per field
/// wins" (`§4.4`), with the year field additionally tiered so a `RecordingYear`/`RecordingDate`
/// always outranks a `ReleaseYear`/`ReleaseDate`, which in turn outranks `OriginalReleaseYear`,
/// regardless of the order tags happen to appear in.
fn apply_symphonia_tags(tags: &mut TrackTags, year_tier: &mut u8, list: &[Tag]) {
    for tag in list {
        let Some(std) = &tag.std else { continue };
        match std {
            StandardTag::TrackTitle(v) => set_if_empty(&mut tags.title, v.as_str()),
            StandardTag::Artist(v) => set_if_empty(&mut tags.artist, v.as_str()),
            StandardTag::Album(v) => set_if_empty(&mut tags.album, v.as_str()),
            StandardTag::AlbumArtist(v) => set_if_empty(&mut tags.album_artist, v.as_str()),
            StandardTag::RecordingYear(y) => apply_year(tags, year_tier, 0, Some(*y)),
            StandardTag::RecordingDate(s) => apply_year(tags, year_tier, 0, year_from_date_str(s)),
            StandardTag::ReleaseYear(y) => apply_year(tags, year_tier, 1, Some(*y)),
            StandardTag::ReleaseDate(s) => apply_year(tags, year_tier, 1, year_from_date_str(s)),
            StandardTag::OriginalReleaseYear(y) => apply_year(tags, year_tier, 2, Some(*y)),
            StandardTag::TrackNumber(n) => set_if_none_u32(&mut tags.track_number, *n),
            StandardTag::TrackTotal(n) => set_if_none_u32(&mut tags.track_total, *n),
            StandardTag::DiscNumber(n) => set_if_none_u32(&mut tags.disc_number, *n),
            StandardTag::DiscTotal(n) => set_if_none_u32(&mut tags.disc_total, *n),
            StandardTag::Genre(v) => set_if_empty(&mut tags.genre, v.as_str()),
            StandardTag::Lyrics(v) => set_lyrics_if_empty(&mut tags.lyrics, v.as_str()),
            _ => {}
        }
    }
}

fn select_embedded_picture(revisions: &[MetadataRevision]) -> Option<EmbeddedPicture> {
    let mut candidates: Vec<&Visual> = Vec::new();
    for revision in revisions {
        candidates.extend(revision.media.visuals.iter().filter(|v| v.data.len() <= MAX_EMBEDDED_PICTURE_BYTES));
        for per_track in &revision.per_track {
            candidates
                .extend(per_track.metadata.visuals.iter().filter(|v| v.data.len() <= MAX_EMBEDDED_PICTURE_BYTES));
        }
    }
    let front_index = candidates.iter().position(|v| v.usage == Some(StandardVisualKey::FrontCover));
    let chosen = match front_index {
        Some(index) => candidates[index],
        None => *candidates.first()?,
    };
    Some(EmbeddedPicture {
        media_type: chosen.media_type.clone(),
        front_cover: front_index.is_some(),
        data: chosen.data.to_vec(),
    })
}

// ---------------------------------------------------------------------------------------------
// WavPack: safe-Rust, bounds-checked APEv2 parser (`§2.6`, `§4.4`)
// ---------------------------------------------------------------------------------------------

#[derive(Debug)]
pub(crate) struct ApeItem {
    pub key: String,
    pub value: ApeValue,
}

#[derive(Debug)]
pub(crate) enum ApeValue {
    Text(String),
    Binary(Vec<u8>),
}

struct ApeFooter {
    size: u32,
    item_count: u32,
}

fn read_wavpack_metadata(path: &Path) -> Result<TrackMetadata, DecoderError> {
    let mut file = std::fs::File::open(path)?;
    let file_len = file.metadata()?.len();

    let Some(tail) = locate_apev2_tail(&mut file, file_len)? else {
        return Ok(TrackMetadata::default());
    };
    let items = parse_apev2_tail(&tail, file_len).unwrap_or_default();

    let mut tags = TrackTags::default();
    for item in &items {
        apply_ape_item(&mut tags, item);
    }
    normalize_tags(&mut tags);

    let picture = select_ape_picture(&items);

    Ok(TrackMetadata { tags, picture })
}

/// Locates the APEv2 footer (directly at the end of the file, or just before a trailing ID3v1
/// tag), validates it, and returns the `size` bytes of items+footer it describes. `Ok(None)`
/// means "no tags", not an error (`§4.4` steps 1-4).
fn locate_apev2_tail(file: &mut std::fs::File, file_len: u64) -> Result<Option<Vec<u8>>, DecoderError> {
    if file_len < 32 {
        return Ok(None);
    }

    let mut footer_bytes = [0u8; 32];
    file.seek(SeekFrom::End(-32))?;
    file.read_exact(&mut footer_bytes)?;

    let footer_end = if &footer_bytes[0..8] == b"APETAGEX" {
        file_len
    } else if file_len >= 160 {
        let mut marker = [0u8; 3];
        file.seek(SeekFrom::Start(file_len - 128))?;
        file.read_exact(&mut marker)?;
        if &marker != b"TAG" {
            return Ok(None);
        }
        file.seek(SeekFrom::Start(file_len - 128 - 32))?;
        file.read_exact(&mut footer_bytes)?;
        if &footer_bytes[0..8] != b"APETAGEX" {
            return Ok(None);
        }
        file_len - 128
    } else {
        return Ok(None);
    };

    let Some(footer) = parse_apev2_footer(&footer_bytes) else { return Ok(None) };
    if footer.size < 32
        || u64::from(footer.size) > MAX_APEV2_TAG_BYTES
        || u64::from(footer.size) > footer_end
        || footer.item_count > MAX_APEV2_ITEM_COUNT
    {
        return Ok(None);
    }

    let tail_start = footer_end - u64::from(footer.size);
    let mut tail = vec![0u8; footer.size as usize];
    file.seek(SeekFrom::Start(tail_start))?;
    file.read_exact(&mut tail)?;
    Ok(Some(tail))
}

/// Parses the 32-byte APEv2 header/footer fields. Only the fields this reader needs (`size`,
/// `item_count`) are kept; the version is checked but not retained, and flags/reserved bytes are
/// not interpreted because this reader never consults the optional header block (`size` already
/// excludes it).
fn parse_apev2_footer(footer: &[u8; 32]) -> Option<ApeFooter> {
    if &footer[0..8] != b"APETAGEX" {
        return None;
    }
    let version = u32::from_le_bytes(footer[8..12].try_into().unwrap());
    if version != 1000 && version != 2000 {
        return None;
    }
    let size = u32::from_le_bytes(footer[12..16].try_into().unwrap());
    let item_count = u32::from_le_bytes(footer[16..20].try_into().unwrap());
    Some(ApeFooter { size, item_count })
}

/// Walks the APEv2 items in `tail` (which ends with the 32-byte footer itself) with checked
/// arithmetic throughout. Any malformed item stops the walk and returns the items parsed so far,
/// never a panic and never an out-of-bounds read (`§4.4` step 5).
///
/// `item_count`, read from the footer already present at the end of `tail` and clamped to
/// `MAX_APEV2_ITEM_COUNT`, caps how many items the walk will ever visit. `locate_apev2_tail`
/// already rejects a footer whose own `item_count` exceeds the ceiling, but nothing otherwise
/// stops a hostile tag from packing far more (tiny) items into the tail than it declares, growing
/// the parsed `Vec` well past what the byte-size cap alone allows.
pub(crate) fn parse_apev2_tail(tail: &[u8], file_len: u64) -> Result<Vec<ApeItem>, String> {
    if tail.len() < 32 {
        return Ok(Vec::new());
    }
    let items_end = tail.len() - 32;
    let item_count = u32::from_le_bytes(tail[items_end + 16..items_end + 20].try_into().unwrap()).min(MAX_APEV2_ITEM_COUNT);
    let mut items = Vec::new();
    let mut offset = 0usize;
    let mut seen = 0u32;

    while offset < items_end && seen < item_count {
        let Some(header_end) = offset.checked_add(8) else { break };
        if header_end > items_end {
            break;
        }
        let value_len = u32::from_le_bytes(tail[offset..offset + 4].try_into().unwrap());
        let item_flags = u32::from_le_bytes(tail[offset + 4..offset + 8].try_into().unwrap());
        if u64::from(value_len) > file_len {
            break;
        }

        let key_start = header_end;
        let Some(key_end) = find_apev2_key_end(tail, key_start, items_end) else { break };
        let key = String::from_utf8_lossy(&tail[key_start..key_end]).into_owned();

        let Some(value_start) = key_end.checked_add(1) else { break };
        let Some(value_end) = value_start.checked_add(value_len as usize) else { break };
        if value_end > items_end {
            break;
        }
        let value_bytes = &tail[value_start..value_end];

        // Counted even for a skipped locator/reserved item: `item_count` bounds how many item
        // slots are walked, not how many end up in `items`.
        seen += 1;
        match (item_flags >> 1) & 3 {
            0 => items.push(ApeItem { key, value: ApeValue::Text(String::from_utf8_lossy(value_bytes).into_owned()) }),
            1 => items.push(ApeItem { key, value: ApeValue::Binary(apev2_binary_data(value_bytes)) }),
            _ => {} // 2 = locator, 3 = reserved: skipped (`§4.4` step 6).
        }

        offset = value_end;
    }

    Ok(items)
}

/// Finds the end of a NUL-terminated APEv2 item key starting at `key_start`: ASCII 0x20-0x7E,
/// terminated by NUL within `APEV2_KEY_MAX_LEN` bytes. Returns `None` (stop the walk) on an
/// invalid character, a missing terminator, or running past `items_end`.
fn find_apev2_key_end(tail: &[u8], key_start: usize, items_end: usize) -> Option<usize> {
    let mut pos = key_start;
    while pos < items_end && pos - key_start < APEV2_KEY_MAX_LEN {
        match tail[pos] {
            0 => return Some(pos),
            byte if (APEV2_KEY_MIN..=APEV2_KEY_MAX).contains(&byte) => pos += 1,
            _ => return None,
        }
    }
    None
}

/// A binary APEv2 value conventionally starts with `filename\0`; when a NUL shows up within the
/// first `APEV2_BINARY_FILENAME_SEARCH_LEN` bytes, the filename is dropped and the data is
/// everything after it, otherwise the whole value is the data (`§4.4` step 6).
fn apev2_binary_data(value: &[u8]) -> Vec<u8> {
    let search_len = value.len().min(APEV2_BINARY_FILENAME_SEARCH_LEN);
    match value[..search_len].iter().position(|&b| b == 0) {
        Some(nul_index) => value[nul_index + 1..].to_vec(),
        None => value.to_vec(),
    }
}

fn apply_ape_item(tags: &mut TrackTags, item: &ApeItem) {
    let ApeValue::Text(value) = &item.value else { return };
    let key = item.key.to_ascii_lowercase();
    match key.as_str() {
        "title" => set_if_empty(&mut tags.title, value.as_str()),
        "artist" => set_if_empty(&mut tags.artist, value.as_str()),
        "album" => set_if_empty(&mut tags.album, value.as_str()),
        "album artist" | "albumartist" => set_if_empty(&mut tags.album_artist, value.as_str()),
        "year" | "date" => {
            if tags.year.is_none() {
                tags.year = year_from_date_str(value);
            }
        }
        "track" | "tracknumber" => {
            let (number, total) = parse_number_pair(value);
            set_if_none(&mut tags.track_number, number);
            set_if_none(&mut tags.track_total, total);
        }
        "disc" | "discnumber" => {
            let (number, total) = parse_number_pair(value);
            set_if_none(&mut tags.disc_number, number);
            set_if_none(&mut tags.disc_total, total);
        }
        "genre" => set_if_empty(&mut tags.genre, value.as_str()),
        "lyrics" | "unsyncedlyrics" | "unsynced lyrics" => set_lyrics_if_empty(&mut tags.lyrics, value.as_str()),
        _ => {}
    }
}

/// Prefers the binary item "cover art (front)"; otherwise the first binary item whose key starts
/// with "cover art" (`§4.4` step 8). The 20 MB cap is the same as the other formats.
fn select_ape_picture(items: &[ApeItem]) -> Option<EmbeddedPicture> {
    let mut front_cover: Option<&ApeItem> = None;
    let mut first_cover_art: Option<&ApeItem> = None;
    for item in items {
        if !matches!(item.value, ApeValue::Binary(_)) {
            continue;
        }
        let key = item.key.to_ascii_lowercase();
        if key == "cover art (front)" && front_cover.is_none() {
            front_cover = Some(item);
        }
        if key.starts_with("cover art") && first_cover_art.is_none() {
            first_cover_art = Some(item);
        }
    }
    let chosen = front_cover.or(first_cover_art)?;
    let ApeValue::Binary(data) = &chosen.value else { unreachable!("filtered to binary items above") };
    if data.len() > MAX_EMBEDDED_PICTURE_BYTES {
        return None;
    }
    Some(EmbeddedPicture {
        media_type: None,
        front_cover: front_cover.is_some(),
        data: data.clone(),
    })
}

// ---------------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------------

/// Sets `field` to `value`, trimmed, unless `field` is already set or `value` is blank
/// ("first non-empty value per field wins", `§4.4` "Normalization").
fn set_if_empty(field: &mut Option<String>, value: &str) {
    if field.is_some() {
        return;
    }
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return;
    }
    *field = Some(trimmed.to_owned());
}

fn set_lyrics_if_empty(field: &mut Option<String>, raw: &str) {
    set_if_empty(field, &strip_lrc_timestamps(raw));
}

fn set_if_none<T>(field: &mut Option<T>, value: Option<T>) {
    if field.is_none() {
        *field = value;
    }
}

fn set_if_none_u32(field: &mut Option<u32>, value: u64) {
    set_if_none(field, Some(saturating_u32(value)));
}

fn saturating_u32(value: u64) -> u32 {
    value.min(u64::from(u32::MAX)) as u32
}

fn apply_year(tags: &mut TrackTags, current_tier: &mut u8, tier: u8, value: Option<u16>) {
    let Some(value) = value else { return };
    if tags.year.is_none() || tier < *current_tier {
        tags.year = Some(value);
        *current_tier = tier;
    }
}

/// The year from the first run of 4 consecutive ASCII digits in `value` (handles "2021",
/// "2021-05-04", etc.).
fn year_from_date_str(value: &str) -> Option<u16> {
    let bytes = value.as_bytes();
    if bytes.len() < 4 {
        return None;
    }
    for start in 0..=(bytes.len() - 4) {
        let candidate = &bytes[start..start + 4];
        if candidate.iter().all(u8::is_ascii_digit) {
            return std::str::from_utf8(candidate).ok()?.parse().ok();
        }
    }
    None
}

/// `"3/12"` -> `(Some(3), Some(12))`. A missing or non-numeric part is `None`; garbage input never
/// panics.
pub fn parse_number_pair(value: &str) -> (Option<u32>, Option<u32>) {
    let mut parts = value.trim().splitn(2, '/');
    let number = parts.next().and_then(|s| s.trim().parse::<u32>().ok());
    let total = parts.next().and_then(|s| s.trim().parse::<u32>().ok());
    (number, total)
}

/// Removes a leading `"[mm:ss]"` or `"[mm:ss.xx]"`-style LRC timestamp tag from the start of each
/// line (repeated tags on one line are all removed); non-timestamp bracketed text (e.g. a
/// `"[Chorus]"` section marker) is left alone.
pub fn strip_lrc_timestamps(lyrics: &str) -> String {
    lyrics.lines().map(strip_leading_lrc_tags).collect::<Vec<_>>().join("\n")
}

fn strip_leading_lrc_tags(line: &str) -> &str {
    let mut rest = line;
    while let Some(after_bracket) = rest.strip_prefix('[') {
        let Some(close) = after_bracket.find(']') else { break };
        if !is_lrc_timestamp(&after_bracket[..close]) {
            break;
        }
        rest = &after_bracket[close + 1..];
    }
    rest
}

fn is_lrc_timestamp(tag: &str) -> bool {
    let mut parts = tag.splitn(2, ':');
    let Some(minutes) = parts.next() else { return false };
    let Some(seconds_and_rest) = parts.next() else { return false };
    if minutes.is_empty() || !minutes.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let seconds = seconds_and_rest.split(['.', ':']).next().unwrap_or("");
    !seconds.is_empty() && seconds.bytes().all(|b| b.is_ascii_digit())
}

fn normalize_tags(tags: &mut TrackTags) {
    trim_or_clear(&mut tags.title);
    trim_or_clear(&mut tags.artist);
    trim_or_clear(&mut tags.album);
    trim_or_clear(&mut tags.album_artist);
    trim_or_clear(&mut tags.genre);
    trim_or_clear(&mut tags.lyrics);
}

fn trim_or_clear(field: &mut Option<String>) {
    if let Some(value) = field {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            *field = None;
        } else if trimmed.len() != value.len() {
            *value = trimmed.to_owned();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    use image::ImageFormat;

    fn fixture_path(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
    }

    fn temp_path(name: &str) -> std::path::PathBuf {
        let suffix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("lime-player-metadata-{name}-{}-{suffix}", std::process::id()))
    }

    fn tiny_png() -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(4, 4, image::Rgba([200, 100, 50, 255]));
        let mut bytes = Vec::new();
        image.write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png).unwrap();
        bytes
    }

    // -----------------------------------------------------------------------------------------
    // FLAC (Vorbis comments + PICTURE block)
    // -----------------------------------------------------------------------------------------

    /// Builds a copy of `strict-24-bit-reference.flac` with a synthesized VORBIS_COMMENT block
    /// (and a trailing PICTURE block) inserted before the audio frames, following the FLAC
    /// metadata-block layout byte-for-byte (`§6` Stage 3 test list).
    fn build_tagged_flac() -> std::path::PathBuf {
        let source = std::fs::read(fixture_path("strict-24-bit-reference.flac")).unwrap();
        assert_eq!(&source[0..4], b"fLaC");

        let mut blocks = Vec::new(); // (type, last, body)
        let mut pos = 4usize;
        loop {
            let header = source[pos];
            let block_type = header & 0x7f;
            let last = header & 0x80 != 0;
            let len = u32::from_be_bytes([0, source[pos + 1], source[pos + 2], source[pos + 3]]) as usize;
            let body = source[pos + 4..pos + 4 + len].to_vec();
            pos += 4 + len;
            let keep = block_type == 0 || block_type == 3; // STREAMINFO, SEEKTABLE
            if keep {
                blocks.push((block_type, body));
            }
            if last {
                break;
            }
        }
        let audio = source[pos..].to_vec();

        let mut vorbis = Vec::new();
        let vendor = b"lime-player-test";
        vorbis.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        vorbis.extend_from_slice(vendor);
        let comments: &[(&str, &str)] = &[
            ("TITLE", "Test Title"),
            ("ARTIST", "Test Artist"),
            ("ALBUM", "Test Album"),
            ("ALBUMARTIST", "Test Album Artist"),
            ("DATE", "2021-05-04"),
            ("TRACKNUMBER", "3"),
            ("TRACKTOTAL", "12"),
            ("DISCNUMBER", "1"),
            ("GENRE", "Test Genre"),
            ("LYRICS", "[00:01.00]Hello"),
        ];
        vorbis.extend_from_slice(&(comments.len() as u32).to_le_bytes());
        for (key, value) in comments {
            let entry = format!("{key}={value}");
            vorbis.extend_from_slice(&(entry.len() as u32).to_le_bytes());
            vorbis.extend_from_slice(entry.as_bytes());
        }

        let png = tiny_png();
        let mime = b"image/png";
        let mut picture = Vec::new();
        picture.extend_from_slice(&3u32.to_be_bytes()); // type 3 = front cover
        picture.extend_from_slice(&(mime.len() as u32).to_be_bytes());
        picture.extend_from_slice(mime);
        picture.extend_from_slice(&0u32.to_be_bytes()); // description length
        picture.extend_from_slice(&4u32.to_be_bytes()); // width
        picture.extend_from_slice(&4u32.to_be_bytes()); // height
        picture.extend_from_slice(&32u32.to_be_bytes()); // depth
        picture.extend_from_slice(&0u32.to_be_bytes()); // colors (0 = non-indexed)
        picture.extend_from_slice(&(png.len() as u32).to_be_bytes());
        picture.extend_from_slice(&png);

        let mut out = Vec::new();
        out.extend_from_slice(b"fLaC");
        for (index, (block_type, body)) in blocks.iter().enumerate() {
            let _ = index;
            out.push(*block_type); // never last here; more blocks follow
            out.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
            out.extend_from_slice(body);
        }
        out.push(4); // VORBIS_COMMENT, not last
        out.extend_from_slice(&(vorbis.len() as u32).to_be_bytes()[1..]);
        out.extend_from_slice(&vorbis);
        out.push(0x80 | 6); // PICTURE, last block
        out.extend_from_slice(&(picture.len() as u32).to_be_bytes()[1..]);
        out.extend_from_slice(&picture);
        out.extend_from_slice(&audio);

        let path = temp_path("tagged.flac");
        std::fs::write(&path, out).unwrap();
        path
    }

    #[test]
    fn reads_flac_vorbis_comments_and_front_cover() {
        let path = build_tagged_flac();

        let metadata = read_metadata(&path).unwrap();

        std::fs::remove_file(&path).unwrap();

        assert_eq!(metadata.tags.title.as_deref(), Some("Test Title"));
        assert_eq!(metadata.tags.artist.as_deref(), Some("Test Artist"));
        assert_eq!(metadata.tags.album.as_deref(), Some("Test Album"));
        assert_eq!(metadata.tags.album_artist.as_deref(), Some("Test Album Artist"));
        assert_eq!(metadata.tags.year, Some(2021));
        assert_eq!(metadata.tags.track_number, Some(3));
        assert_eq!(metadata.tags.track_total, Some(12));
        assert_eq!(metadata.tags.disc_number, Some(1));
        assert_eq!(metadata.tags.genre.as_deref(), Some("Test Genre"));
        assert_eq!(metadata.tags.lyrics.as_deref(), Some("Hello"));

        let picture = metadata.picture.expect("front cover should be present");
        assert!(picture.front_cover);
        let decoded = image::load_from_memory(&picture.data).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (4, 4));
    }

    // -----------------------------------------------------------------------------------------
    // MP3 (ID3v2.3 text frames)
    // -----------------------------------------------------------------------------------------

    fn strip_leading_id3v2(bytes: &[u8]) -> &[u8] {
        if bytes.len() < 10 || &bytes[0..3] != b"ID3" {
            return bytes;
        }
        let size = syncsafe_decode(&bytes[6..10]);
        &bytes[10 + size..]
    }

    fn syncsafe_decode(bytes: &[u8]) -> usize {
        bytes.iter().fold(0usize, |acc, &b| (acc << 7) | (b & 0x7f) as usize)
    }

    fn syncsafe_encode(value: usize) -> [u8; 4] {
        [
            ((value >> 21) & 0x7f) as u8,
            ((value >> 14) & 0x7f) as u8,
            ((value >> 7) & 0x7f) as u8,
            (value & 0x7f) as u8,
        ]
    }

    fn id3v2_text_frame(id: &[u8; 4], text: &str) -> Vec<u8> {
        let mut body = vec![0u8]; // encoding 0 = ISO-8859-1/ASCII
        body.extend_from_slice(text.as_bytes());
        let mut frame = Vec::new();
        frame.extend_from_slice(id);
        frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
        frame.extend_from_slice(&[0, 0]); // frame flags
        frame.extend_from_slice(&body);
        frame
    }

    fn build_tagged_mp3() -> std::path::PathBuf {
        let source = std::fs::read(fixture_path("decoder-tone.mp3")).unwrap();
        let audio = strip_leading_id3v2(&source);

        let mut frames = Vec::new();
        frames.extend_from_slice(&id3v2_text_frame(b"TIT2", "Test Title"));
        frames.extend_from_slice(&id3v2_text_frame(b"TPE1", "Test Artist"));
        frames.extend_from_slice(&id3v2_text_frame(b"TALB", "Test Album"));
        frames.extend_from_slice(&id3v2_text_frame(b"TPE2", "Test Album Artist"));
        frames.extend_from_slice(&id3v2_text_frame(b"TRCK", "3/12"));
        frames.extend_from_slice(&id3v2_text_frame(b"TYER", "2021"));
        frames.extend_from_slice(&id3v2_text_frame(b"TCON", "Jazz"));

        let mut out = Vec::new();
        out.extend_from_slice(b"ID3");
        out.extend_from_slice(&[3, 0]); // version 2.3.0
        out.push(0); // flags
        out.extend_from_slice(&syncsafe_encode(frames.len()));
        out.extend_from_slice(&frames);
        out.extend_from_slice(audio);

        let path = temp_path("tagged.mp3");
        std::fs::write(&path, out).unwrap();
        path
    }

    #[test]
    fn reads_mp3_id3v2_text_frames() {
        let path = build_tagged_mp3();

        let metadata = read_metadata(&path).unwrap();

        std::fs::remove_file(&path).unwrap();

        assert_eq!(metadata.tags.title.as_deref(), Some("Test Title"));
        assert_eq!(metadata.tags.artist.as_deref(), Some("Test Artist"));
        assert_eq!(metadata.tags.album.as_deref(), Some("Test Album"));
        assert_eq!(metadata.tags.album_artist.as_deref(), Some("Test Album Artist"));
        assert_eq!(metadata.tags.track_number, Some(3));
        assert_eq!(metadata.tags.track_total, Some(12));
        assert_eq!(metadata.tags.year, Some(2021));
        assert_eq!(metadata.tags.genre.as_deref(), Some("Jazz"));
    }

    // -----------------------------------------------------------------------------------------
    // WavPack (APEv2)
    // -----------------------------------------------------------------------------------------

    fn build_wavpack_audio(path: &Path) {
        let mut writer = wavpack::WavpackWriter::create(path)
            .unwrap()
            .add_bytes_per_sample(2)
            .add_bits_per_sample(16)
            .add_num_channels(2)
            .add_channel_mask(3)
            .add_sample_rate(44_100)
            .build()
            .unwrap();
        let mut samples = vec![0i32, 0i32, 1i32, -1i32];
        writer.pack_samples(&mut samples).unwrap();
    }

    fn ape_item_bytes(key: &str, flags: u32, value: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(key.as_bytes());
        out.push(0);
        out.extend_from_slice(value);
        out
    }

    /// Builds a full APEv2 block (header + items + footer), matching real WavPack files (`§4.4`
    /// test list). Only the trailing `size` bytes (items + footer) are ever read back by this
    /// reader; the header is present for realism but is never consulted.
    fn build_apev2_block(item_bytes: &[u8], item_count: u32) -> Vec<u8> {
        let size = item_bytes.len() as u32 + 32;
        let mut header = Vec::new();
        header.extend_from_slice(b"APETAGEX");
        header.extend_from_slice(&2000u32.to_le_bytes());
        header.extend_from_slice(&size.to_le_bytes());
        header.extend_from_slice(&item_count.to_le_bytes());
        header.extend_from_slice(&((1u32 << 31) | (1u32 << 29)).to_le_bytes());
        header.extend_from_slice(&[0u8; 8]);

        let mut footer = Vec::new();
        footer.extend_from_slice(b"APETAGEX");
        footer.extend_from_slice(&2000u32.to_le_bytes());
        footer.extend_from_slice(&size.to_le_bytes());
        footer.extend_from_slice(&item_count.to_le_bytes());
        footer.extend_from_slice(&(1u32 << 31).to_le_bytes());
        footer.extend_from_slice(&[0u8; 8]);

        let mut out = Vec::new();
        out.extend_from_slice(&header);
        out.extend_from_slice(item_bytes);
        out.extend_from_slice(&footer);
        out
    }

    #[test]
    fn reads_wavpack_apev2_text_and_binary_cover() {
        let path = temp_path("tagged.wv");
        build_wavpack_audio(&path);

        let png = tiny_png();
        let mut cover_value = b"cover.png\0".to_vec();
        cover_value.extend_from_slice(&png);

        let mut items = Vec::new();
        items.extend_from_slice(&ape_item_bytes("Title", 0, b"Test Title"));
        items.extend_from_slice(&ape_item_bytes("Artist", 0, b"Test Artist"));
        items.extend_from_slice(&ape_item_bytes("Album", 0, b"Test Album"));
        items.extend_from_slice(&ape_item_bytes("Album Artist", 0, b"Test Album Artist"));
        items.extend_from_slice(&ape_item_bytes("Year", 0, b"2021"));
        items.extend_from_slice(&ape_item_bytes("Track", 0, b"3/12"));
        items.extend_from_slice(&ape_item_bytes("Genre", 0, b"Test Genre"));
        items.extend_from_slice(&ape_item_bytes("Lyrics", 0, b"[00:01.00]Hello"));
        items.extend_from_slice(&ape_item_bytes("Cover Art (Front)", 2, &cover_value));

        let block = build_apev2_block(&items, 9);
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&block).unwrap();
        drop(file);

        let metadata = read_metadata(&path).unwrap();

        std::fs::remove_file(&path).unwrap();

        assert_eq!(metadata.tags.title.as_deref(), Some("Test Title"));
        assert_eq!(metadata.tags.artist.as_deref(), Some("Test Artist"));
        assert_eq!(metadata.tags.album.as_deref(), Some("Test Album"));
        assert_eq!(metadata.tags.album_artist.as_deref(), Some("Test Album Artist"));
        assert_eq!(metadata.tags.year, Some(2021));
        assert_eq!(metadata.tags.track_number, Some(3));
        assert_eq!(metadata.tags.track_total, Some(12));
        assert_eq!(metadata.tags.genre.as_deref(), Some("Test Genre"));
        assert_eq!(metadata.tags.lyrics.as_deref(), Some("Hello"));

        let picture = metadata.picture.expect("cover art should be present");
        assert!(picture.front_cover);
        let decoded = image::load_from_memory(&picture.data).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (4, 4));
    }

    /// Builds a direct-call `parse_apev2_tail` tail: the raw item bytes followed by a minimal
    /// 32-byte footer carrying the real `APETAGEX` magic and `item_count` (the only two fields
    /// this reader consults from a tail's trailing footer).
    fn tail_with_footer(item_bytes: &[u8], item_count: u32) -> Vec<u8> {
        let items_len = item_bytes.len();
        let mut tail = item_bytes.to_vec();
        tail.resize(items_len + 32, 0);
        tail[items_len..items_len + 8].copy_from_slice(b"APETAGEX");
        tail[items_len + 16..items_len + 20].copy_from_slice(&item_count.to_le_bytes());
        tail
    }

    #[test]
    fn apev2_binary_item_without_nul_is_whole_value() {
        let value = [1u8, 2, 3, 4, 5]; // deliberately free of NUL bytes
        let items = ape_item_bytes("Cover Art (Front)", 2, &value);
        let tail = tail_with_footer(&items, 1);

        let result = parse_apev2_tail(&tail, 1_000_000).unwrap();

        assert_eq!(result.len(), 1);
        match &result[0].value {
            ApeValue::Binary(data) => assert_eq!(data, &value),
            ApeValue::Text(_) => panic!("expected a binary item"),
        }
    }

    #[test]
    fn apev2_non_utf8_text_is_lossy_per_item() {
        let mut items = Vec::new();
        items.extend_from_slice(&ape_item_bytes("Bad", 0, &[0xFF, 0xFE]));
        items.extend_from_slice(&ape_item_bytes("Good", 0, b"fine"));
        let tail = tail_with_footer(&items, 2);

        let result = parse_apev2_tail(&tail, 1_000_000).unwrap();

        assert_eq!(result.len(), 2);
        match &result[0].value {
            ApeValue::Text(text) => assert!(text.contains('\u{FFFD}'), "expected a lossy replacement, got {text:?}"),
            ApeValue::Binary(_) => panic!("expected a text item"),
        }
        match &result[1].value {
            ApeValue::Text(text) => assert_eq!(text, "fine"),
            ApeValue::Binary(_) => panic!("expected a text item"),
        }
    }

    #[test]
    fn apev2_rejects_oversized_or_truncated_tags() {
        // A `size` larger than the file itself: rejected at the footer level, empty result.
        let path = temp_path("oversized.wv");
        build_wavpack_audio(&path);
        let file_len = std::fs::metadata(&path).unwrap().len();
        let mut footer = Vec::new();
        footer.extend_from_slice(b"APETAGEX");
        footer.extend_from_slice(&2000u32.to_le_bytes());
        footer.extend_from_slice(&(file_len as u32 * 4).to_le_bytes()); // absurdly large size
        footer.extend_from_slice(&1u32.to_le_bytes());
        footer.extend_from_slice(&(1u32 << 31).to_le_bytes());
        footer.extend_from_slice(&[0u8; 8]);
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&footer).unwrap();
        drop(file);

        let metadata = read_metadata(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(metadata.tags, TrackTags::default());

        // `item_count` above the 1024 ceiling: rejected at the footer level, empty result.
        let path = temp_path("too-many-items.wv");
        build_wavpack_audio(&path);
        let items = ape_item_bytes("Title", 0, b"x");
        let block = build_apev2_block(&items, 1025);
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&block).unwrap();
        drop(file);

        let metadata = read_metadata(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(metadata.tags, TrackTags::default());

        // A `value_len` reaching past the tail: the walk stops, keeping nothing parsed so far,
        // never a panic or an out-of-bounds read.
        let mut truncated = Vec::new();
        truncated.extend_from_slice(&500u32.to_le_bytes()); // value_len far past the buffer
        truncated.extend_from_slice(&0u32.to_le_bytes());
        truncated.extend_from_slice(b"Title\0short");
        let tail = tail_with_footer(&truncated, 1);
        let result = parse_apev2_tail(&tail, 1_000_000).unwrap();
        assert!(result.is_empty(), "a value_len past the end must stop the walk with no items, got {result:?}");

        // A valid item followed by a malformed one: the walk stops at the malformed item but
        // keeps what it already parsed (`§4.4` step 5 "empty or partial results").
        let mut partial = ape_item_bytes("Title", 0, b"ok");
        partial.extend_from_slice(&500u32.to_le_bytes()); // second item's value_len far past the buffer
        partial.extend_from_slice(&0u32.to_le_bytes());
        partial.extend_from_slice(b"Bad\0short");
        let tail = tail_with_footer(&partial, 2);
        let result = parse_apev2_tail(&tail, 1_000_000).unwrap();
        assert_eq!(result.len(), 1);
        match &result[0].value {
            ApeValue::Text(text) => assert_eq!(text, "ok"),
            ApeValue::Binary(_) => panic!("expected a text item"),
        }

        // `item_count` under the 1024 ceiling but far below how many items the tail actually
        // holds: the walk stops once `item_count` items have been visited, even though more
        // well-formed items follow (a hostile tag could otherwise grow the parsed `Vec` well past
        // the byte-size cap by packing in many more items than it declares).
        let mut many_items = Vec::new();
        many_items.extend_from_slice(&ape_item_bytes("First", 0, b"1"));
        many_items.extend_from_slice(&ape_item_bytes("Second", 0, b"2"));
        many_items.extend_from_slice(&ape_item_bytes("Third", 0, b"3"));
        let tail = tail_with_footer(&many_items, 1);
        let result = parse_apev2_tail(&tail, 1_000_000).unwrap();
        assert_eq!(result.len(), 1, "item_count must cap the walk even when more items follow in the tail");
        assert_eq!(result[0].key, "First");
    }

    #[test]
    fn apev2_footer_before_id3v1_is_found() {
        let path = temp_path("id3v1.wv");
        build_wavpack_audio(&path);

        let items = ape_item_bytes("Title", 0, b"Before ID3v1");
        let block = build_apev2_block(&items, 1);
        let mut id3v1 = vec![0u8; 128];
        id3v1[0..3].copy_from_slice(b"TAG");

        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&block).unwrap();
        file.write_all(&id3v1).unwrap();
        drop(file);

        let metadata = read_metadata(&path).unwrap();
        std::fs::remove_file(&path).unwrap();

        assert_eq!(metadata.tags.title.as_deref(), Some("Before ID3v1"));
    }

    // -----------------------------------------------------------------------------------------
    // Small pure helpers
    // -----------------------------------------------------------------------------------------

    #[test]
    fn strip_lrc_timestamps_removes_leading_tags() {
        let input = "[00:01.00]Hello\n[00:05.20][00:07.00]World\n[Chorus] stays\nPlain line";
        let expected = "Hello\nWorld\n[Chorus] stays\nPlain line";
        assert_eq!(strip_lrc_timestamps(input), expected);
    }

    #[test]
    fn parse_number_pair_handles_total_and_garbage() {
        assert_eq!(parse_number_pair("3/12"), (Some(3), Some(12)));
        assert_eq!(parse_number_pair("5"), (Some(5), None));
        assert_eq!(parse_number_pair(""), (None, None));
        assert_eq!(parse_number_pair("abc"), (None, None));
        assert_eq!(parse_number_pair("3/abc"), (Some(3), None));
        assert_eq!(parse_number_pair(" 3 / 12 "), (Some(3), Some(12)));
    }
}

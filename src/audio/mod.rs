mod coreaudio;
mod decoder;
pub mod metadata;
mod player;
pub mod rate_repair;

pub use decoder::{AudioInfo, probe_file};
// `TrackMetadata` itself is only ever named inside `metadata.rs` today (callers bind
// `read_metadata`'s result and destructure it); re-exported anyway so the whole metadata API
// (`§3.1`) is reachable as `audio::TrackMetadata`, not just `audio::metadata::TrackMetadata`.
#[allow(unused_imports)]
pub use metadata::{EmbeddedPicture, TrackMetadata, TrackTags, read_metadata};
pub use player::{
    AudioPlayer, OutputDevice, PlaybackEvent, PlayerSettings, PreparedTrack, QueueTrackSnapshot, enumerate_outputs,
};

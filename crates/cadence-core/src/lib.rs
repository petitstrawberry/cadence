//! Portable, streaming playback and transport for Cadence.
//!
//! Playback and device ownership live on one worker; a bounded decoder worker
//! prepares the next track for continuous output. UI commands use a
//! bounded, nonblocking channel; audio callbacks never acquire the UI snapshot lock.
pub mod decoder;
mod gapless;
pub mod output;
mod player;

pub use decoder::{
    AudioDecoder, DecoderFactory, DecoderRegistry, EXTENSIONS, MediaInput, MediaSource, Resampler,
    TrackMetadata,
};
pub use output::{AudioOutput, Frame, OutputFactory, Result};
pub use player::{
    AudioProcessor, Command, PlaybackState, Player, ProcessorFactory, RepeatMode, Snapshot, Track,
};

//! Portable, streaming playback and transport for Cadence.
//!
//! File I/O, decoding and device ownership live on one worker. UI commands use a
//! bounded, nonblocking channel; audio callbacks never acquire the UI snapshot lock.
pub mod decoder;
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

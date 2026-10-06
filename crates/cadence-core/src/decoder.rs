//! Streaming audio decoding and a small, replaceable sample-rate conversion stage.

use std::collections::VecDeque;
use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use symphonia::core::codecs::audio::{AudioDecoder as SymphoniaAudioDecoder, AudioDecoderOptions};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatReader, SeekMode, SeekTo, TrackType};
pub use symphonia::core::io::MediaSource;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataRevision, StandardTag, Tag};
use symphonia::core::units::{Time, TimeBase, Timestamp};

use crate::Track;
use crate::output::{Frame, Result};

type MediaOpener = dyn Fn(Arc<AtomicBool>) -> Result<Box<dyn MediaSource>> + Send + Sync;

/// Reopenable media owned by a source adapter, without embedding network details in
/// the playback engine. The ID must be stable and must not contain credentials.
/// Openers and readers must use bounded I/O timeouts and observe cancellation.
#[derive(Clone)]
pub struct MediaInput {
    id: String,
    extension_hint: Option<String>,
    opener: Arc<MediaOpener>,
}

impl MediaInput {
    pub fn new(
        id: impl Into<String>,
        extension_hint: Option<String>,
        opener: impl Fn(Arc<AtomicBool>) -> Result<Box<dyn MediaSource>> + Send + Sync + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            extension_hint,
            opener: Arc::new(opener),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn extension_hint(&self) -> Option<&str> {
        self.extension_hint.as_deref()
    }

    fn open(&self, cancelled: Arc<AtomicBool>) -> Result<Box<dyn MediaSource>> {
        check_cancelled(&cancelled)?;
        let source = (self.opener)(cancelled.clone())?;
        check_cancelled(&cancelled)?;
        Ok(source)
    }
}

impl fmt::Debug for MediaInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // An adapter may accidentally place a private URL in its ID. Keep even
        // that out of snapshots and debug logs; credentials remain in the closure.
        formatter
            .debug_struct("MediaInput")
            .field("extension_hint", &self.extension_hint)
            .finish_non_exhaustive()
    }
}

impl PartialEq for MediaInput {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for MediaInput {}

fn check_cancelled(cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::Acquire) {
        Err("Media input was cancelled".into())
    } else {
        Ok(())
    }
}

#[derive(Clone)]
enum DecoderInput {
    File(PathBuf),
    Stream {
        source: MediaInput,
        title: String,
        cancelled: Arc<AtomicBool>,
    },
}

impl DecoderInput {
    fn open(&self) -> Result<Box<dyn MediaSource>> {
        match self {
            Self::File(path) => File::open(path)
                .map(|file| Box::new(file) as Box<dyn MediaSource>)
                .map_err(|error| error.to_string()),
            Self::Stream {
                source, cancelled, ..
            } => source.open(cancelled.clone()),
        }
    }

    fn extension_hint(&self) -> Option<&str> {
        match self {
            Self::File(path) => path.extension().and_then(|extension| extension.to_str()),
            Self::Stream { source, .. } => source.extension_hint(),
        }
    }

    fn title(&self) -> String {
        match self {
            Self::File(path) => path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            Self::Stream { title, .. } => title.clone(),
        }
    }

    fn check_cancelled(&self) -> Result<()> {
        match self {
            Self::File(_) => Ok(()),
            Self::Stream { cancelled, .. } => check_cancelled(cancelled),
        }
    }
}

/// File extensions recognized by the bundled Symphonia container readers.
/// The codec inside a container still needs to be one of the enabled codecs.
pub const EXTENSIONS: &[&str] = &[
    "wav", "wave", "aif", "aiff", "aifc", "caf", "flac", "mp3", "ogg", "oga", "m4a", "mp4", "m4b",
    "aac",
];

const MAX_READ_FRAMES: usize = 8192;
const MAX_PACKET_FRAMES: usize = 262_144;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackMetadata {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration: Option<Duration>,
    pub sample_rate: u32,
}

pub trait AudioDecoder: Send {
    fn metadata(&self) -> &TrackMetadata;
    /// Whether arbitrary seeks are supported by the opened media source.
    fn is_seekable(&self) -> bool {
        true
    }
    /// Return at most `max_frames` stereo frames. An empty result means EOF, except
    /// when `max_frames` is zero. Implementations may return a shorter block.
    fn read_frames(&mut self, max_frames: usize) -> Result<Vec<Frame>>;
    /// Seek and return the position actually reached (within a source sample when
    /// supported). A decoder may report a different position when it cannot land there.
    fn seek(&mut self, position: Duration) -> Result<Duration>;
}

/// Extension point for additional codecs or media sources backed by a local path.
pub trait DecoderFactory: Send + Sync {
    fn name(&self) -> &str;
    fn open(&self, path: &Path) -> Result<Box<dyn AudioDecoder>>;
}

pub struct DecoderRegistry {
    factories: Vec<Box<dyn DecoderFactory>>,
}

impl Default for DecoderRegistry {
    fn default() -> Self {
        Self {
            factories: vec![Box::new(SymphoniaDecoderFactory)],
        }
    }
}

impl DecoderRegistry {
    /// Custom factories take priority over previously registered factories.
    pub fn register(&mut self, factory: Box<dyn DecoderFactory>) {
        self.factories.insert(0, factory);
    }

    pub fn open(&self, path: &Path) -> Result<Box<dyn AudioDecoder>> {
        let mut failures = Vec::new();
        for factory in &self.factories {
            match factory.open(path) {
                Ok(decoder) => return Ok(decoder),
                Err(error) => failures.push(format!("{}: {error}", factory.name())),
            }
        }
        Err(format!(
            "Unable to open {} ({})",
            path.display(),
            failures.join("; ")
        ))
    }

    /// Open a source-backed track incrementally, or retain registered local path
    /// factories for ordinary files. Reopening after EOF uses the same source.
    pub fn open_track(
        &self,
        track: &Track,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Box<dyn AudioDecoder>> {
        match &track.media_input {
            Some(source) => Ok(Box::new(SymphoniaDecoder::from_input(
                DecoderInput::Stream {
                    source: source.clone(),
                    title: track.title.clone(),
                    cancelled,
                },
            )?)),
            None => self.open(&track.path),
        }
    }
}

pub struct SymphoniaDecoderFactory;

impl DecoderFactory for SymphoniaDecoderFactory {
    fn name(&self) -> &str {
        "Symphonia"
    }

    fn open(&self, path: &Path) -> Result<Box<dyn AudioDecoder>> {
        Ok(Box::new(SymphoniaDecoder::open(path)?))
    }
}

struct SymphoniaDecoder {
    input: DecoderInput,
    format: Box<dyn FormatReader>,
    decoder: Box<dyn SymphoniaAudioDecoder>,
    track_id: u32,
    time_base: TimeBase,
    start_ts: Timestamp,
    timeline_initialized: bool,
    metadata: TrackMetadata,
    pending: VecDeque<Frame>,
    pending_frame: u64,
    num_frames: Option<u64>,
    finished: bool,
    seekable: bool,
}

impl SymphoniaDecoder {
    fn open(path: &Path) -> Result<Self> {
        Self::from_input(DecoderInput::File(path.to_owned()))
    }

    fn from_input(input: DecoderInput) -> Result<Self> {
        let source = input.open()?;
        let seekable = source.is_seekable();
        let stream = MediaSourceStream::new(source, Default::default());
        let mut hint = Hint::new();
        if let Some(extension) = input.extension_hint() {
            hint.with_extension(extension);
        }
        let format = symphonia::default::get_probe()
            .probe(&hint, stream, Default::default(), Default::default())
            .map_err(|e| e.to_string())?;
        let track = format
            .default_track(TrackType::Audio)
            .ok_or("No audio track found")?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or("Audio codec parameters are missing")?;
        if let Some(channels) = &params.channels {
            validate_channels(channels.count())?;
        }
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map_err(|e| e.to_string())?;
        let rate = params.sample_rate.unwrap_or(0);
        let track_id = track.id;
        let start_ts = track.start_ts;
        let num_frames = track.num_frames;
        let declared_time_base = track.time_base;
        let duration = declared_time_base
            .zip(track.duration)
            .and_then(|(base, length)| base.calc_duration(length))
            .and_then(time_to_duration);
        let metadata = TrackMetadata {
            title: input.title(),
            artist: None,
            album: None,
            duration,
            sample_rate: rate,
        };
        let mut result = Self {
            input,
            format,
            decoder,
            track_id,
            time_base: declared_time_base
                .unwrap_or_else(|| TimeBase::try_from_recip(rate.max(1)).unwrap()),
            start_ts,
            timeline_initialized: false,
            metadata,
            pending: VecDeque::new(),
            pending_frame: 0,
            num_frames,
            finished: false,
            seekable,
        };
        result.update_metadata();
        // Prime one packet so codecs that discover their specification during decoding
        // expose the correct sample rate before an output/resampler is constructed.
        result.decode_packet()?;
        if result.metadata.sample_rate == 0 {
            return Err("Audio stream has no valid sample rate".into());
        }
        if declared_time_base.is_none() {
            result.time_base = TimeBase::try_from_recip(result.metadata.sample_rate).unwrap();
        }
        if result.metadata.duration.is_none() {
            result.metadata.duration = num_frames.map(|frames| {
                Duration::from_secs_f64(frames as f64 / f64::from(result.metadata.sample_rate))
            });
        }
        Ok(result)
    }

    fn update_metadata(&mut self) {
        let mut metadata = self.format.metadata();
        loop {
            if let Some(revision) = metadata.current() {
                apply_revision(&mut self.metadata, revision, self.track_id);
            }
            if metadata.is_latest() {
                break;
            }
            metadata.pop();
        }
    }

    fn decode_packet(&mut self) -> Result<()> {
        while !self.finished && self.pending.is_empty() {
            self.input.check_cancelled()?;
            let packet = self.format.next_packet().map_err(|e| e.to_string())?;
            self.input.check_cancelled()?;
            self.update_metadata();
            let Some(packet) = packet else {
                self.finished = true;
                return Ok(());
            };
            if packet.track_id != self.track_id {
                continue;
            }
            let audio = self.decoder.decode(&packet).map_err(|e| e.to_string())?;
            let channels = audio.spec().channels().count();
            validate_channels(channels)?;
            let rate = audio.spec().rate();
            if rate == 0 {
                return Err("Audio stream has no valid sample rate".into());
            }
            if self.metadata.sample_rate != 0 && self.metadata.sample_rate != rate {
                return Err("Changing sample rates within a track are not supported".into());
            }
            self.metadata.sample_rate = rate;
            if audio.frames() > MAX_PACKET_FRAMES {
                return Err("Audio packet exceeds the streaming decoder's frame limit".into());
            }
            let mut samples = vec![0.0_f32; audio.samples_interleaved()];
            audio.copy_to_slice_interleaved(&mut samples);
            self.pending.extend(
                samples
                    .chunks_exact(channels)
                    .map(|s| [s[0], if channels == 1 { s[0] } else { s[1] }]),
            );
            // Packet PTS includes any encoder delay; the decoder has already removed
            // trim_start. A reset Vorbis decoder can emit no frames for a priming
            // packet, so seek positioning must use the next packet that emits audio.
            let first_valid_ts = packet.pts.saturating_add(packet.trim_start);
            if !self.timeline_initialized && !self.pending.is_empty() {
                // Ogg's declared start can include encoder delay while MP3's does
                // not. Establish playback zero from the first actual audio sample.
                self.start_ts = first_valid_ts;
                self.timeline_initialized = true;
            }
            self.pending_frame = first_valid_ts
                .duration_from(self.start_ts)
                .and_then(|duration| self.time_base.calc_duration(duration))
                .and_then(time_to_duration)
                .map(|time| {
                    ((time.as_nanos() * u128::from(rate) + 500_000_000) / 1_000_000_000) as u64
                })
                .unwrap_or(0);
            // FLAC 0.6.1 may report UnexpectedEof after its last complete frame.
            // Stop only at the declared frame count, preserving errors on truncation.
            if self
                .num_frames
                .is_some_and(|total| self.pending_frame + self.pending.len() as u64 >= total)
            {
                self.finished = true;
            }
        }
        Ok(())
    }
}

impl AudioDecoder for SymphoniaDecoder {
    fn metadata(&self) -> &TrackMetadata {
        &self.metadata
    }

    fn is_seekable(&self) -> bool {
        self.seekable
    }

    fn read_frames(&mut self, max_frames: usize) -> Result<Vec<Frame>> {
        self.input.check_cancelled()?;
        let target = max_frames.min(MAX_READ_FRAMES);
        let mut output = Vec::with_capacity(target);
        while output.len() < target {
            if self.pending.is_empty() {
                self.decode_packet()?;
                if self.pending.is_empty() {
                    break;
                }
            }
            let count = (target - output.len()).min(self.pending.len());
            output.extend(self.pending.drain(..count));
            self.pending_frame += count as u64;
        }
        Ok(output)
    }

    fn seek(&mut self, position: Duration) -> Result<Duration> {
        self.input.check_cancelled()?;
        if !self.seekable {
            return Err("This source does not support seeking".into());
        }
        let position = self.metadata.duration.map_or(position, |d| position.min(d));
        if self
            .metadata
            .duration
            .is_some_and(|duration| position == duration)
        {
            // Many container readers reject the one-past-last timestamp. Logical EOF
            // is still a useful seek target for a progress slider.
            self.decoder.reset();
            self.pending.clear();
            self.finished = true;
            return Ok(position);
        }
        if self.finished {
            // Re-open exhausted demuxers: Symphonia's MP4 atom reader cannot seek
            // after consuming its terminal atom. This also restores stream indexes.
            *self = Self::from_input(self.input.clone())?;
        }
        // Decode a short pre-roll to rebuild transform overlap and MP3 reservoir
        // state, then discard using packet timestamps rather than guessed lengths.
        let preroll = position.saturating_sub(Duration::from_millis(250));
        let offset = Time::try_from_nanos_u128(preroll.as_nanos())
            .and_then(|t| self.time_base.calc_timestamp(t))
            .ok_or("Seek position is out of range")?;
        let offset = offset
            .duration_from(Timestamp::new(0))
            .ok_or("Invalid seek position")?;
        let timestamp = self
            .start_ts
            .checked_add(offset)
            .ok_or("Seek position is out of range")?;
        self.format
            .seek(
                SeekMode::Accurate,
                SeekTo::Timestamp {
                    ts: timestamp,
                    track_id: self.track_id,
                },
            )
            .map_err(|e| e.to_string())?;
        let params = self
            .format
            .tracks()
            .iter()
            .find(|track| track.id == self.track_id)
            .and_then(|track| track.codec_params.as_ref())
            .and_then(|params| params.audio())
            .ok_or("Audio codec parameters are missing after seek")?;
        // Some codecs retain synthesis/RNG state in reset(); reconstruct the decoder
        // so replaying from the beginning is equivalent to opening the file again.
        self.decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map_err(|e| e.to_string())?;
        self.pending.clear();
        self.finished = false;
        let target = position.as_nanos() * u128::from(self.metadata.sample_rate) / 1_000_000_000;
        loop {
            self.decode_packet()?;
            if self.pending.is_empty() || u128::from(self.pending_frame) >= target {
                break;
            }
            let count =
                (target - u128::from(self.pending_frame)).min(self.pending.len() as u128) as usize;
            self.pending.drain(..count);
            self.pending_frame += count as u64;
        }
        Ok(Duration::from_secs_f64(
            self.pending_frame as f64 / f64::from(self.metadata.sample_rate),
        ))
    }
}

fn validate_channels(channels: usize) -> Result<()> {
    if !(1..=2).contains(&channels) {
        return Err(format!(
            "Unsupported channel count: {channels}; only mono and stereo are supported"
        ));
    }
    Ok(())
}

fn time_to_duration(time: Time) -> Option<Duration> {
    let nanos = u128::try_from(time.as_nanos()).ok()?;
    Some(Duration::new(
        (nanos / 1_000_000_000).try_into().ok()?,
        (nanos % 1_000_000_000) as u32,
    ))
}

fn apply_revision(metadata: &mut TrackMetadata, revision: &MetadataRevision, track_id: u32) {
    apply_tags(metadata, &revision.media.tags);
    for track in &revision.per_track {
        if track.track_id == u64::from(track_id) {
            apply_tags(metadata, &track.metadata.tags);
        }
    }
}

fn apply_tags(metadata: &mut TrackMetadata, tags: &[Tag]) {
    for tag in tags {
        match tag.std.as_ref() {
            Some(StandardTag::TrackTitle(value)) if !value.trim().is_empty() => {
                metadata.title = value.to_string()
            }
            Some(StandardTag::Artist(value)) if !value.trim().is_empty() => {
                metadata.artist = Some(value.to_string())
            }
            Some(StandardTag::Album(value)) if !value.trim().is_empty() => {
                metadata.album = Some(value.to_string())
            }
            _ => {}
        }
    }
}

/// Streaming linear interpolation with an exact rational phase accumulator.
///
/// This is intentionally a modest initial converter: downsampling has no low-pass
/// filter and can alias. A higher-quality band-limited converter can replace it
/// without changing decoder or output adapters. Only the current chunk and at most
/// one tail frame are retained. Output duration is rounded up to one output frame.
pub struct Resampler {
    input_rate: u32,
    output_rate: u32,
    frames: VecDeque<Frame>,
    base: u128,
    received: u128,
    next_position: u128,
}

impl Resampler {
    pub fn new(input_rate: u32, output_rate: u32) -> Result<Self> {
        if input_rate == 0 || output_rate == 0 {
            return Err("Sample rates must be non-zero".into());
        }
        Ok(Self {
            input_rate,
            output_rate,
            frames: VecDeque::new(),
            base: 0,
            received: 0,
            next_position: 0,
        })
    }

    pub fn reset(&mut self) {
        self.frames.clear();
        self.base = 0;
        self.received = 0;
        self.next_position = 0;
    }

    /// Call with `end = true` to flush the final fractional interval at EOF.
    /// After EOF call `reset` before processing a new stream or seek position.
    pub fn process(&mut self, input: &[Frame], end: bool) -> Vec<Frame> {
        self.frames.extend(input.iter().copied());
        self.received += input.len() as u128;
        let denominator = u128::from(self.output_rate);
        let mut output = Vec::new();
        loop {
            let index = self.next_position / denominator;
            let fraction = self.next_position % denominator;
            if index >= self.received || (!end && fraction != 0 && index + 1 >= self.received) {
                break;
            }
            let offset = (index - self.base) as usize;
            let first = self.frames[offset];
            let second = self.frames.get(offset + 1).copied().unwrap_or(first);
            let mix = fraction as f32 / self.output_rate as f32;
            output.push([
                first[0] + (second[0] - first[0]) * mix,
                first[1] + (second[1] - first[1]) * mix,
            ]);
            self.next_position += u128::from(self.input_rate);
        }
        let discard_until = (self.next_position / denominator).min(self.received);
        let discard = (discard_until - self.base) as usize;
        self.frames.drain(..discard);
        self.base = discard_until;
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read, Seek, SeekFrom};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

    struct TestFile(PathBuf);
    impl Drop for TestFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn wav(channels: u16, frames: usize) -> TestFile {
        let path = std::env::temp_dir().join(format!(
            "cadence-decoder-{}-{}.wav",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let spec = hound::WavSpec {
            channels,
            sample_rate: 8000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..frames {
            for channel in 0..channels {
                let sample = (i % 1000) as i16 * if channel == 0 { 1 } else { -1 };
                writer.write_sample(sample).unwrap();
            }
        }
        writer.finalize().unwrap();
        TestFile(path)
    }

    struct CountedStream {
        cursor: Cursor<Arc<[u8]>>,
        read: Arc<AtomicUsize>,
    }

    impl Read for CountedStream {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let count = self.cursor.read(buffer)?;
            self.read.fetch_add(count, Ordering::Relaxed);
            Ok(count)
        }
    }
    impl Seek for CountedStream {
        fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
            self.cursor.seek(position)
        }
    }
    impl MediaSource for CountedStream {
        fn is_seekable(&self) -> bool {
            true
        }
        fn byte_len(&self) -> Option<u64> {
            Some(self.cursor.get_ref().len() as u64)
        }
    }

    #[test]
    fn media_input_decodes_before_full_read_and_reopens_after_logical_eof() {
        let fixture = wav(2, 800_000);
        let bytes: Arc<[u8]> = std::fs::read(&fixture.0).unwrap().into();
        let length = bytes.len();
        let read = Arc::new(AtomicUsize::new(0));
        let opened = Arc::new(AtomicUsize::new(0));
        let counter = read.clone();
        let open_counter = opened.clone();
        let source = MediaInput::new("stream:stable-id", Some("wav".into()), move |_| {
            open_counter.fetch_add(1, Ordering::Relaxed);
            Ok(Box::new(CountedStream {
                cursor: Cursor::new(bytes.clone()),
                read: counter.clone(),
            }))
        });
        let mut track = Track::from_path("remote-item".into());
        track.title = "Server title".into();
        track.media_input = Some(source);
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut decoder = DecoderRegistry::default()
            .open_track(&track, cancelled.clone())
            .unwrap();
        assert_eq!(decoder.metadata().title, "Server title");
        assert_eq!(
            decoder.read_frames(2).unwrap()[1],
            [1.0 / 32768.0, -1.0 / 32768.0]
        );
        assert!(
            read.load(Ordering::Relaxed) < length / 2,
            "opening fetched the entire body"
        );
        let duration = decoder.metadata().duration.unwrap();
        decoder.seek(duration).unwrap();
        assert!(decoder.read_frames(1).unwrap().is_empty());
        decoder.seek(Duration::ZERO).unwrap();
        assert_eq!(opened.load(Ordering::Relaxed), 2);
        assert_eq!(
            decoder.read_frames(2).unwrap()[1],
            [1.0 / 32768.0, -1.0 / 32768.0]
        );
        cancelled.store(true, Ordering::Release);
        assert!(decoder.read_frames(1).unwrap_err().contains("cancelled"));
        assert!(
            decoder
                .seek(Duration::ZERO)
                .unwrap_err()
                .contains("cancelled")
        );
        assert_eq!(opened.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn media_input_identity_is_stable_and_debug_hides_private_transport_details() {
        let source = || {
            MediaInput::new("https://user:password@example.test/audio", None, |_| {
                Err("not used".into())
            })
        };
        assert_eq!(source(), source());
        assert_ne!(
            source(),
            MediaInput::new("different", None, |_| Err("not used".into()))
        );
        assert!(!format!("{:?}", source()).contains("password"));
        let cancelled = Arc::new(AtomicBool::new(true));
        assert!(source().open(cancelled).is_err());
    }

    #[test]
    fn mono_is_duplicated_and_incremental_reads_are_bounded() {
        let file = wav(1, 24_001);
        let mut decoder = DecoderRegistry::default().open(&file.0).unwrap();
        assert_eq!(decoder.metadata().sample_rate, 8000);
        assert_eq!(
            decoder.metadata().duration,
            Some(Duration::from_secs_f64(24_001.0 / 8000.0))
        );
        assert!(decoder.read_frames(0).unwrap().is_empty());
        let mut total = 0;
        loop {
            let block = decoder.read_frames(127).unwrap();
            assert!(block.len() <= 127);
            if block.is_empty() {
                break;
            }
            for frame in &block {
                assert_eq!(frame[0], frame[1]);
                assert!((frame[0] - (total % 1000) as f32 / 32768.0).abs() < 1e-6);
                total += 1;
            }
        }
        assert_eq!(total, 24_001);
        assert!(decoder.read_frames(1).unwrap().is_empty());
    }

    #[test]
    fn stereo_and_seek_report_the_actual_frame() {
        let file = wav(2, 24_000);
        let mut decoder = DecoderRegistry::default().open(&file.0).unwrap();
        let block = decoder.read_frames(10).unwrap();
        assert_eq!(block[1], [1.0 / 32768.0, -1.0 / 32768.0]);
        let landed = decoder.seek(Duration::from_millis(1507)).unwrap();
        assert_eq!(landed, Duration::from_millis(1507));
        let frame_index = (landed.as_secs_f64() * 8000.0).round() as usize;
        let expected = (frame_index % 1000) as f32 / 32768.0;
        assert_eq!(decoder.read_frames(1).unwrap()[0], [expected, -expected]);
        assert_eq!(
            decoder.seek(Duration::from_secs(100)).unwrap(),
            Duration::from_secs(3)
        );
        assert!(decoder.read_frames(1).unwrap().is_empty());
        decoder.seek(Duration::ZERO).unwrap();
        assert_eq!(decoder.read_frames(1).unwrap()[0], [0.0, 0.0]);
        assert_eq!(
            decoder.read_frames(usize::MAX).unwrap().len(),
            MAX_READ_FRAMES
        );
    }

    #[test]
    fn invalid_and_multichannel_files_fail_clearly() {
        let file = wav(3, 100);
        let error = match DecoderRegistry::default().open(&file.0) {
            Ok(_) => panic!("accepted multichannel"),
            Err(e) => e,
        };
        assert!(error.contains("channel"), "{error}");
        std::fs::write(&file.0, b"this is not audio").unwrap();
        assert!(DecoderRegistry::default().open(&file.0).is_err());
    }

    #[test]
    fn id3_metadata_is_used_and_filename_is_the_fallback() {
        let file = wav(1, 10);
        let plain = DecoderRegistry::default().open(&file.0).unwrap();
        assert_eq!(
            plain.metadata().title,
            file.0.file_stem().unwrap().to_string_lossy()
        );
        assert_eq!(plain.metadata().artist, None);
        assert_eq!(plain.metadata().album, None);
        drop(plain);
        let mut tags = Vec::new();
        for (key, value) in [
            (b"TIT2", "Test title"),
            (b"TPE1", "Test artist"),
            (b"TALB", "Test album"),
        ] {
            tags.extend(key);
            tags.extend(((value.len() + 1) as u32).to_be_bytes());
            tags.extend([0, 0, 0]); // frame flags followed by Latin-1 text encoding
            tags.extend(value.as_bytes());
        }
        let mut bytes = b"ID3\x03\x00\x00".to_vec();
        let size = tags.len() as u32;
        bytes.extend([
            (size >> 21) as u8 & 0x7f,
            (size >> 14) as u8 & 0x7f,
            (size >> 7) as u8 & 0x7f,
            size as u8 & 0x7f,
        ]);
        bytes.extend(tags);
        bytes.extend(std::fs::read(&file.0).unwrap());
        std::fs::write(&file.0, bytes).unwrap();
        let decoder = DecoderRegistry::default().open(&file.0).unwrap();
        assert_eq!(decoder.metadata().title, "Test title");
        assert_eq!(decoder.metadata().artist.as_deref(), Some("Test artist"));
        assert_eq!(decoder.metadata().album.as_deref(), Some("Test album"));
    }

    #[test]
    fn registered_decoders_are_tried_before_builtin() {
        struct RejectingFactory;
        impl DecoderFactory for RejectingFactory {
            fn name(&self) -> &str {
                "Test extension"
            }
            fn open(&self, _: &Path) -> Result<Box<dyn AudioDecoder>> {
                Err("deliberate rejection".into())
            }
        }
        let file = wav(1, 10);
        let mut registry = DecoderRegistry::default();
        registry.register(Box::new(RejectingFactory));
        assert_eq!(registry.factories[0].name(), "Test extension");
        assert!(registry.open(&file.0).is_ok());
        std::fs::write(&file.0, b"invalid").unwrap();
        let error = match registry.open(&file.0) {
            Ok(_) => panic!("accepted invalid file"),
            Err(e) => e,
        };
        assert!(error.contains("Test extension: deliberate rejection"));
        assert!(error.contains("Symphonia:"));
    }

    #[test]
    fn resampling_is_chunk_independent_and_preserves_duration() {
        for (input_rate, output_rate) in [
            (44100, 48000),
            (48000, 44100),
            (8000, 48000),
            (48000, 8000),
            (48000, 48000),
        ] {
            let input: Vec<Frame> = (0..input_rate + 7)
                .map(|i| [(i % 97) as f32 / 97.0, -((i % 101) as f32) / 101.0])
                .collect();
            let expected = Resampler::new(input_rate, output_rate)
                .unwrap()
                .process(&input, true);
            assert_eq!(
                expected.len(),
                (input.len() as u64 * u64::from(output_rate)).div_ceil(u64::from(input_rate))
                    as usize
            );
            let mut resampler = Resampler::new(input_rate, output_rate).unwrap();
            let mut actual = Vec::new();
            for chunk in input.chunks(17) {
                actual.extend(resampler.process(chunk, false));
                assert!(resampler.frames.len() <= 1);
            }
            actual.extend(resampler.process(&[], true));
            assert_eq!(actual, expected);
            assert!(resampler.process(&[], true).is_empty());
            resampler.reset();
            assert_eq!(resampler.process(&input, true), expected);
        }
    }

    #[test]
    fn resampler_interpolates_and_rejects_zero_rates() {
        assert!(Resampler::new(0, 48000).is_err());
        assert!(Resampler::new(48000, 0).is_err());
        let mut resampler = Resampler::new(1, 2).unwrap();
        assert_eq!(resampler.process(&[[0.0, 1.0]], false), vec![[0.0, 1.0]]);
        assert_eq!(
            resampler.process(&[[1.0, 0.0]], true),
            vec![[0.5, 0.5], [1.0, 0.0], [1.0, 0.0]]
        );
    }

    fn decode_to_end(decoder: &mut dyn AudioDecoder) -> Vec<Frame> {
        let mut frames = Vec::new();
        loop {
            let block = decoder.read_frames(197).unwrap();
            assert!(block.len() <= 197);
            if block.is_empty() {
                return frames;
            }
            frames.extend(block);
            assert!(
                frames.len() < 500_000,
                "fixture unexpectedly exceeded its frame bound"
            );
        }
    }

    fn check_fixture(name: &str, metadata: bool, mono: bool, lossless: bool) {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        let mut decoder = DecoderRegistry::default().open(&path).unwrap();
        assert_eq!(decoder.metadata().sample_rate, 22_050);
        if metadata {
            assert_eq!(decoder.metadata().title, "Cadence synthetic tone", "{name}");
            assert_eq!(
                decoder.metadata().artist.as_deref(),
                Some("Cadence tests"),
                "{name}"
            );
            assert_eq!(
                decoder.metadata().album.as_deref(),
                Some("Decoder fixtures"),
                "{name}"
            );
        }
        let baseline = decode_to_end(decoder.as_mut());
        assert!(
            (12_000..16_000).contains(&baseline.len()),
            "{name}: {} frames",
            baseline.len()
        );
        assert!(
            baseline
                .iter()
                .flatten()
                .all(|s| s.is_finite() && s.abs() < 1.0)
        );
        assert!(baseline.iter().map(|f| f[0].abs()).sum::<f32>() > 100.0);
        if mono {
            assert!(baseline.iter().all(|f| f[0] == f[1]));
        } else {
            assert!(baseline.iter().any(|f| (f[0] - f[1]).abs() > 0.1));
        }
        assert!(decoder.read_frames(100).unwrap().is_empty());
        for millis in [231, 351] {
            check_seek(
                decoder.as_mut(),
                &baseline,
                name,
                Duration::from_millis(millis),
                lossless,
            );
        }
        assert_eq!(decoder.seek(Duration::ZERO).unwrap(), Duration::ZERO);
        let restarted = decode_to_end(decoder.as_mut());
        assert_eq!(restarted.len(), baseline.len(), "{name}: restart length");
        let max_error = restarted
            .iter()
            .flatten()
            .zip(baseline.iter().flatten())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0_f32, f32::max);
        assert!(max_error < 1e-6, "{name}: restart sample error {max_error}");
        if let Some(duration) = decoder.metadata().duration {
            assert_eq!(decoder.seek(duration).unwrap(), duration);
            assert!(decoder.read_frames(100).unwrap().is_empty());
            assert_eq!(decoder.seek(Duration::ZERO).unwrap(), Duration::ZERO);
            assert!(!decoder.read_frames(100).unwrap().is_empty());
        }
    }

    fn check_seek(
        decoder: &mut dyn AudioDecoder,
        baseline: &[Frame],
        name: &str,
        requested: Duration,
        lossless: bool,
    ) {
        let rate = f64::from(decoder.metadata().sample_rate);
        let actual = decoder.seek(requested).unwrap();
        assert!(
            actual.abs_diff(requested) <= Duration::from_secs_f64(1.0 / rate),
            "{name}: seek reached {actual:?}"
        );
        let tail = decode_to_end(decoder);
        let start = (actual.as_secs_f64() * rate).round() as usize;
        assert!(
            tail.len().abs_diff(baseline.len() - start) <= 1,
            "{name}: {} tail frames vs {} expected",
            tail.len(),
            baseline.len() - start
        );
        // Transform codecs may need overlap history after seeking. Check alignment
        // after 100 ms; lossless codecs must match immediately.
        let warmup = if lossless { 0 } else { (rate / 10.0) as usize };
        let count = tail
            .len()
            .saturating_sub(warmup)
            .min(baseline.len().saturating_sub(start + warmup));
        assert!(count > 2000);
        let mean_error = tail[warmup..warmup + count]
            .iter()
            .zip(&baseline[start + warmup..start + warmup + count])
            .map(|(a, b)| (a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2))
            .sum::<f32>()
            / count as f32;
        assert!(
            mean_error < if lossless { 1e-10 } else { 1e-4 },
            "{name}: seek sample error {mean_error}"
        );
    }

    #[test]
    fn vorbis_long_seeks_preserve_sample_alignment() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tone-long.ogg");
        let mut decoder = DecoderRegistry::default().open(&path).unwrap();
        let baseline = decode_to_end(decoder.as_mut());
        assert_eq!(baseline.len(), 441_000);
        for millis in [1507, 5500, 9500] {
            check_seek(
                decoder.as_mut(),
                &baseline,
                "long Vorbis",
                Duration::from_millis(millis),
                false,
            );
        }
    }

    #[test]
    fn truncated_flac_is_an_error_not_a_clean_eof() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tone.flac");
        let bytes = std::fs::read(fixture).unwrap();
        let file = wav(1, 1);
        std::fs::write(&file.0, &bytes[..bytes.len() - 100]).unwrap();
        let mut decoder = DecoderRegistry::default().open(&file.0).unwrap();
        while let Ok(frames) = decoder.read_frames(197) {
            assert!(!frames.is_empty(), "truncated FLAC was accepted as EOF");
        }
    }

    #[test]
    fn mp3_fixture_playback_seek_metadata_and_restart() {
        check_fixture("tone-mono.mp3", true, true, false);
    }
    #[test]
    fn flac_fixture_playback_seek_metadata_and_restart() {
        check_fixture("tone.flac", true, false, true);
    }
    #[test]
    fn vorbis_fixture_playback_seek_metadata_and_restart() {
        check_fixture("tone.ogg", true, false, false);
    }
    #[test]
    fn adts_aac_fixture_playback_seek_and_restart() {
        check_fixture("tone.aac", false, false, false);
    }
    #[test]
    fn m4a_aac_fixture_playback_seek_metadata_and_restart() {
        check_fixture("tone-aac.m4a", true, false, false);
    }
    #[test]
    fn alac_fixture_playback_seek_metadata_and_restart() {
        check_fixture("tone-alac.m4a", true, false, true);
    }
    #[test]
    fn aiff_fixture_playback_seek_metadata_and_restart() {
        check_fixture("tone.aiff", true, false, true);
    }
    #[test]
    fn caf_fixture_playback_seek_and_restart() {
        check_fixture("tone.caf", false, false, true);
    }
}

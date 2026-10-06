use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::gapless::{Prefetcher, Prepared};

use crate::{
    AudioDecoder, AudioOutput, DecoderRegistry, Frame, MediaInput, OutputFactory, Resampler, Result,
};

const MAX_QUEUE_TRACKS: usize = 16_384;
const COMMAND_CAPACITY: usize = 64;
const OUTPUT_BLOCK: usize = 2048;
const STALL_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlaybackState {
    #[default]
    Stopped,
    Loading,
    Playing,
    Paused,
    Error,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RepeatMode {
    #[default]
    Off,
    All,
    One,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    pub path: PathBuf,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration: Option<Duration>,
    /// Source-managed stream; `None` retains local-path decoder registration.
    pub media_input: Option<MediaInput>,
}

impl Track {
    pub fn from_path(path: PathBuf) -> Self {
        let title = path
            .file_stem()
            .unwrap_or(path.as_os_str())
            .to_string_lossy()
            .into_owned();
        Self {
            path,
            title,
            artist: None,
            album: None,
            duration: None,
            media_input: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub queue: Vec<Track>,
    pub current: Option<usize>,
    pub state: PlaybackState,
    pub position: Duration,
    pub duration: Option<Duration>,
    pub seekable: bool,
    pub volume: f32,
    pub repeat: RepeatMode,
    pub error: Option<String>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            queue: Vec::new(),
            current: None,
            state: PlaybackState::Stopped,
            position: Duration::ZERO,
            duration: None,
            seekable: false,
            volume: 0.75,
            repeat: RepeatMode::Off,
            error: None,
        }
    }
}

#[derive(Clone, Debug)]
pub enum Command {
    /// Append paths without starting playback. Metadata is read when opened.
    Add(Vec<PathBuf>),
    /// Append local or source-managed tracks with metadata supplied by a library.
    AddTracks(Vec<Track>),
    /// Atomically append tracks and start the first newly inserted entry.
    AddTracksAndPlay(Vec<Track>),
    /// Atomically replace the queue and start its first entry. Empty batches are rejected.
    ReplaceTracksAndPlay(Vec<Track>),
    /// Atomically replace the queue and start a specified entry.
    ReplaceTracksAndPlayAt {
        tracks: Vec<Track>,
        index: usize,
    },
    Play,
    Pause,
    Toggle,
    /// Select a queue entry and start playing it from the beginning.
    Select(usize),
    Next,
    Previous,
    Seek(Duration),
    SetVolume(f32),
    SetRepeat(RepeatMode),
    Remove(usize),
    Clear,
}

/// A worker-owned processing extension, after resampling and before master volume.
/// Implementations must process bounded blocks promptly and must not retain them.
pub trait AudioProcessor {
    /// Called before processing; preserve DSP history when the format is unchanged.
    fn prepare(&mut self, _sample_rate: u32, _max_frames: usize) -> Result<()> {
        Ok(())
    }
    /// Worker control work, including while transport is stopped or paused.
    fn service(&mut self) -> Result<()> {
        Ok(())
    }
    fn process(&mut self, frames: &mut [Frame], sample_rate: u32) -> Result<()>;
    fn reset(&mut self) {}
}

/// Construct thread-affine processors on the playback worker. Only the factory
/// crosses threads; its result and every lifecycle callback stay on that worker.
pub type ProcessorFactory = Box<dyn FnOnce() -> Result<Box<dyn AudioProcessor>> + Send>;

pub struct Player {
    commands: SyncSender<QueuedCommand>,
    snapshot: Arc<Mutex<Snapshot>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    input_control: Arc<Mutex<InputControl>>,
}

#[derive(Default)]
struct InputControl {
    generation: u64,
    active: Option<Arc<AtomicBool>>,
    prefetch: Option<Arc<AtomicBool>>,
    queued_tracks: usize,
    queued_current: Option<usize>,
    queued_repeat: RepeatMode,
    sequence: u64,
}

struct QueuedCommand {
    command: Command,
    generation: u64,
    sequence: u64,
}

impl InputControl {
    fn cancel(&self) {
        for active in [&self.active, &self.prefetch].into_iter().flatten() {
            active.store(true, Ordering::Release);
        }
    }
}

fn replaces_input(command: &Command) -> bool {
    matches!(
        command,
        Command::Clear
            | Command::Select(_)
            | Command::Next
            | Command::Previous
            | Command::ReplaceTracksAndPlay(_)
            | Command::ReplaceTracksAndPlayAt { .. }
    )
}

impl Player {
    pub fn new(output: OutputFactory) -> Result<Self> {
        Self::with_registry(output, DecoderRegistry::default())
    }

    pub fn with_registry(output: OutputFactory, registry: DecoderRegistry) -> Result<Self> {
        Self::with_components(output, registry, Vec::new())
    }

    pub fn with_components(
        output: OutputFactory,
        registry: DecoderRegistry,
        processors: Vec<Box<dyn AudioProcessor + Send>>,
    ) -> Result<Self> {
        let factories = processors
            .into_iter()
            .map(|processor| {
                Box::new(move || Ok(processor as Box<dyn AudioProcessor>)) as ProcessorFactory
            })
            .collect();
        Self::with_processor_factories(output, registry, factories)
    }

    pub fn with_processor_factories(
        output: OutputFactory,
        registry: DecoderRegistry,
        factories: Vec<ProcessorFactory>,
    ) -> Result<Self> {
        let (commands, receiver) = mpsc::sync_channel(COMMAND_CAPACITY);
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_snapshot = Arc::clone(&snapshot);
        let worker_stop = Arc::clone(&stop);
        let input_control = Arc::new(Mutex::new(InputControl::default()));
        let worker_control = input_control.clone();
        let worker = thread::Builder::new()
            .name("cadence-playback".into())
            .spawn(move || {
                let mut processors = Vec::with_capacity(factories.len());
                for factory in factories {
                    match factory() {
                        Ok(processor) => processors.push(processor),
                        Err(error) => {
                            let mut snapshot = worker_snapshot
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner());
                            snapshot.state = PlaybackState::Error;
                            snapshot.error =
                                Some(format!("Cannot create audio processor: {error}"));
                            return;
                        }
                    }
                }
                Worker::new(
                    output,
                    registry,
                    processors,
                    worker_snapshot,
                    worker_control,
                )
                .run(receiver, worker_stop)
            })
            .map_err(|error| format!("Cannot start playback worker: {error}"))?;
        Ok(Self {
            commands,
            snapshot,
            stop,
            worker: Some(worker),
            input_control,
        })
    }

    /// Never waits for decoder, device, or worker capacity.
    pub fn send(&self, command: Command) -> Result<()> {
        match &command {
            Command::Add(paths) if paths.len() > MAX_QUEUE_TRACKS => {
                return Err(format!(
                    "The queue supports at most {MAX_QUEUE_TRACKS} tracks"
                ));
            }
            Command::AddTracks(tracks) | Command::AddTracksAndPlay(tracks)
                if tracks.len() > MAX_QUEUE_TRACKS =>
            {
                return Err(format!(
                    "The queue supports at most {MAX_QUEUE_TRACKS} tracks"
                ));
            }
            Command::ReplaceTracksAndPlay(tracks) => validate_replacement(tracks)?,
            Command::ReplaceTracksAndPlayAt { tracks, index } => {
                validate_replacement_at(tracks, *index)?
            }
            Command::SetVolume(volume) if !volume.is_finite() => {
                return Err("Volume must be a finite number".into());
            }
            _ => {}
        }
        // The mutex protects only command/token bookkeeping, never I/O. A token
        // cannot be registered between accepting an invalidation and cancelling it.
        let mut control = self
            .input_control
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // Queue length can be accounted for without I/O: every mutation enters
        // through this channel. Rejected/empty append-and-play must not cancel a
        // healthy current input, including a forward-only stream.
        let queued_tracks = match &command {
            Command::Add(paths) if control.queued_tracks + paths.len() <= MAX_QUEUE_TRACKS => {
                control.queued_tracks + paths.len()
            }
            Command::AddTracks(tracks) | Command::AddTracksAndPlay(tracks)
                if control.queued_tracks + tracks.len() <= MAX_QUEUE_TRACKS =>
            {
                control.queued_tracks + tracks.len()
            }
            Command::ReplaceTracksAndPlay(tracks)
            | Command::ReplaceTracksAndPlayAt { tracks, .. } => tracks.len(),
            Command::Remove(index) if *index < control.queued_tracks => control.queued_tracks - 1,
            Command::Clear => 0,
            _ => control.queued_tracks,
        };
        let queued_current = match &command {
            Command::Add(_) | Command::AddTracks(_)
                if control.queued_current.is_none() && queued_tracks > 0 =>
            {
                Some(0)
            }
            Command::AddTracksAndPlay(_) if queued_tracks > control.queued_tracks => {
                Some(control.queued_tracks)
            }
            Command::ReplaceTracksAndPlay(_) => Some(0),
            Command::ReplaceTracksAndPlayAt { index, .. } => Some(*index),
            Command::Select(index) if *index < queued_tracks => Some(*index),
            Command::Next => control.queued_current.map(|index| {
                if index + 1 < queued_tracks {
                    index + 1
                } else if control.queued_repeat == RepeatMode::All {
                    0
                } else {
                    index
                }
            }),
            Command::Previous => control.queued_current.map(|index| {
                if index == 0 && control.queued_repeat == RepeatMode::All {
                    queued_tracks - 1
                } else {
                    index.saturating_sub(1)
                }
            }),
            Command::Remove(index) if *index < control.queued_tracks => {
                control.queued_current.and_then(|current| {
                    if queued_tracks == 0 {
                        None
                    } else if current == *index {
                        Some(current.min(queued_tracks - 1))
                    } else if *index < current {
                        Some(current - 1)
                    } else {
                        Some(current)
                    }
                })
            }
            Command::Clear => None,
            _ => control.queued_current,
        };
        let queued_repeat = match &command {
            Command::SetRepeat(repeat) => *repeat,
            _ => control.queued_repeat,
        };
        let replaces_input = (replaces_input(&command)
            && !matches!(&command, Command::Select(index) if *index >= control.queued_tracks))
            || matches!(&command, Command::AddTracksAndPlay(tracks) if !tracks.is_empty() && queued_tracks > control.queued_tracks)
            || matches!(&command, Command::Remove(index) if control.queued_current == Some(*index));
        let generation = control.generation.wrapping_add(u64::from(replaces_input));
        let sequence = control.sequence.wrapping_add(1);
        self.commands
            .try_send(QueuedCommand {
                command,
                generation,
                sequence,
            })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => {
                    String::from("Playback is busy; try the command again")
                }
                mpsc::TrySendError::Disconnected(_) => String::from("Playback worker has stopped"),
            })?;
        control.queued_tracks = queued_tracks;
        control.queued_current = queued_current;
        control.queued_repeat = queued_repeat;
        control.sequence = sequence;
        if replaces_input {
            control.generation = generation;
            control.cancel();
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    /// Deterministically release the worker and device. May wait for current I/O.
    /// Normal UI teardown can simply drop Player, which signals stop without waiting.
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::Release);
        self.input_control
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn validate_replacement_at(tracks: &[Track], index: usize) -> Result<()> {
    validate_replacement(tracks)?;
    if index >= tracks.len() {
        return Err("Track index is outside the replacement queue".into());
    }
    Ok(())
}

fn validate_replacement(tracks: &[Track]) -> Result<()> {
    if tracks.is_empty() {
        return Err("Cannot replace the queue with an empty track list".into());
    }
    if tracks.len() > MAX_QUEUE_TRACKS {
        return Err(format!(
            "The queue supports at most {MAX_QUEUE_TRACKS} tracks"
        ));
    }
    Ok(())
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.input_control
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .cancel();
    }
}

struct Boundary {
    at: u64,
    index: usize,
    seekable: bool,
}

struct Session {
    decoder: Box<dyn AudioDecoder>,
    output: Option<Box<dyn AudioOutput>>,
    resampler: Resampler,
    output_rate: u32,
    base: Duration,
    accepted: u64,
    played: u64,
    audible_start: u64,
    decode_start: u64,
    decode_base: Duration,
    decoding_index: usize,
    boundaries: VecDeque<Boundary>,
    pending: Vec<Frame>,
    submitted: VecDeque<Frame>,
    offset: usize,
    eof: bool,
    draining: bool,
    last_progress: Instant,
    last_device_pending: usize,
}

impl Session {
    fn cursor(&self) -> u64 {
        let pending = self
            .output
            .as_ref()
            .map_or(0, |output| output.pending_frames()) as u64;
        self.played + self.accepted.saturating_sub(pending)
    }

    fn position(&self) -> Duration {
        self.base
            + Duration::from_secs_f64(
                self.cursor().saturating_sub(self.audible_start) as f64 / self.output_rate as f64,
            )
    }

    fn reposition(&mut self, position: Duration) -> Result<()> {
        // Dropping discards samples already submitted to the old device.
        self.output.take();
        self.base = self.decoder.seek(position)?;
        self.accepted = 0;
        self.played = 0;
        self.audible_start = 0;
        self.decode_start = 0;
        self.decode_base = self.base;
        self.boundaries.clear();
        self.pending.clear();
        self.submitted.clear();
        self.offset = 0;
        self.eof = false;
        self.draining = false;
        self.resampler.reset();
        self.last_progress = Instant::now();
        self.last_device_pending = 0;
        Ok(())
    }

    fn attach(&mut self, factory: &OutputFactory) -> Result<()> {
        let output = factory()?;
        let rate = output.sample_rate();
        if !(8000..=384_000).contains(&rate) {
            return Err(format!("Unsupported output sample rate: {rate} Hz"));
        }
        let input_rate = self.decoder.metadata().sample_rate;
        if !(1000..=384_000).contains(&input_rate) {
            return Err(format!("Unsupported input sample rate: {input_rate} Hz"));
        }
        if rate != self.output_rate {
            if !self.pending.is_empty() || !self.base.is_zero() || self.played != 0 {
                return Err("Output sample rate changed while playback was paused".into());
            }
            self.resampler = Resampler::new(input_rate, rate)?;
        }
        self.output_rate = rate;
        self.output = Some(output);
        self.last_progress = Instant::now();
        Ok(())
    }

    fn pause(&mut self) {
        let waiting = self
            .output
            .as_ref()
            .map_or(0, |output| output.pending_frames())
            .min(self.submitted.len());
        self.played += self.accepted.saturating_sub(waiting as u64);
        self.accepted = 0;
        self.output.take();
        // Only the device's bounded unconsumed tail is retained. This preserves
        // exact playback continuity even for servers without byte-range support.
        let mut pending = Vec::with_capacity(waiting + self.pending.len() - self.offset);
        pending.extend(self.submitted.drain(self.submitted.len() - waiting..));
        pending.extend_from_slice(&self.pending[self.offset..]);
        self.submitted.clear();
        self.pending = pending;
        self.offset = 0;
        self.draining = false;
        self.last_device_pending = 0;
    }
}

struct Worker {
    output: OutputFactory,
    registry: Arc<DecoderRegistry>,
    prefetcher: Prefetcher,
    processors: Vec<Box<dyn AudioProcessor>>,
    data: Snapshot,
    shared: Arc<Mutex<Snapshot>>,
    session: Option<Session>,
    revision: u64,
    published_revision: u64,
    last_publish: Instant,
    input_control: Arc<Mutex<InputControl>>,
    generation: u64,
    sequence: u64,
    input_cancelled: Option<Arc<AtomicBool>>,
    load_playing: bool,
}

impl Worker {
    fn new(
        output: OutputFactory,
        registry: DecoderRegistry,
        processors: Vec<Box<dyn AudioProcessor>>,
        shared: Arc<Mutex<Snapshot>>,
        input_control: Arc<Mutex<InputControl>>,
    ) -> Self {
        let registry = Arc::new(registry);
        Self {
            output,
            prefetcher: Prefetcher::new(registry.clone()),
            registry,
            processors,
            data: Snapshot::default(),
            shared,
            session: None,
            revision: 0,
            published_revision: 0,
            last_publish: Instant::now(),
            input_control,
            generation: 0,
            sequence: 0,
            input_cancelled: None,
            load_playing: true,
        }
    }

    fn run(mut self, receiver: Receiver<QueuedCommand>, stop: Arc<AtomicBool>) {
        let mut immediate = false;
        while !stop.load(Ordering::Acquire) {
            let delay = if immediate {
                Duration::ZERO
            } else {
                Duration::from_millis(8)
            };
            match receiver.recv_timeout(delay) {
                Ok(command) => {
                    self.queued_command(command);
                    for _ in 0..15 {
                        if stop.load(Ordering::Acquire) {
                            break;
                        }
                        match receiver.try_recv() {
                            Ok(command) => self.queued_command(command),
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => return,
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if stop.load(Ordering::Acquire) {
                break;
            }
            immediate = match self.tick() {
                Ok(progress) => progress,
                Err(error) => {
                    if self.obsolete() {
                        self.discard_cancelled_session();
                    } else {
                        self.fail(error);
                    }
                    false
                }
            };
            self.publish(false);
        }
    }

    fn publish(&mut self, force: bool) {
        self.update_audible();
        // Keep the generation check and snapshot commit indivisible with send's
        // cancellation. No decoder or device I/O occurs while this lock is held.
        let control = self
            .input_control
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if control.generation != self.generation {
            return;
        }
        if !force && self.last_publish.elapsed() < Duration::from_millis(33) {
            return;
        }
        if let Some(session) = &self.session {
            self.data.position = session.position();
            if let Some(duration) = self.data.duration {
                self.data.position = self.data.position.min(duration);
            }
        }
        let mut shared = self
            .shared
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if self.revision != self.published_revision {
            shared.queue.clone_from(&self.data.queue);
            self.published_revision = self.revision;
        }
        shared.current = self.data.current;
        shared.state = self.data.state;
        shared.position = self.data.position;
        shared.duration = self.data.duration;
        shared.seekable = self.data.seekable;
        shared.volume = self.data.volume;
        shared.repeat = self.data.repeat;
        shared.error.clone_from(&self.data.error);
        self.last_publish = Instant::now();
    }

    fn fail(&mut self, error: String) {
        self.prefetcher.cancel();
        if let Some(session) = &self.session {
            self.data.position = session.position();
        }
        self.session.take();
        self.data.state = PlaybackState::Error;
        self.data.error = Some(error);
        self.publish(true);
    }

    fn reset_processors(&mut self) {
        for processor in &mut self.processors {
            processor.reset();
        }
    }

    fn load(&mut self, index: usize, play: bool) -> Result<()> {
        self.prefetcher.cancel();
        let Some(track) = self.data.queue.get(index) else {
            return Err("Track is no longer in the queue".into());
        };
        let track = track.clone();
        self.session.take();
        self.load_playing = play;
        self.data.current = Some(index);
        self.data.position = Duration::ZERO;
        self.data.duration = track.duration;
        self.data.seekable = false;
        self.data.state = PlaybackState::Loading;
        self.data.error = None;
        self.publish(true);
        let cancelled = {
            let mut control = self
                .input_control
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if control.generation != self.generation {
                return Err("Playback request was superseded".into());
            }
            control.cancel();
            let cancelled = Arc::new(AtomicBool::new(false));
            control.active = Some(cancelled.clone());
            cancelled
        };
        self.input_cancelled = Some(cancelled.clone());
        let decoder = self.registry.open_track(&track, cancelled)?;
        self.check_current_input()?;
        self.data.seekable = decoder.is_seekable();
        let metadata = decoder.metadata();
        let track = &mut self.data.queue[index];
        // A remote cache filename is not better metadata than the source title.
        let fallback_title = Track::from_path(track.path.clone()).title;
        if metadata.title != fallback_title || track.title.is_empty() {
            track.title.clone_from(&metadata.title);
        }
        if metadata.artist.is_some() {
            track.artist.clone_from(&metadata.artist);
        }
        if metadata.album.is_some() {
            track.album.clone_from(&metadata.album);
        }
        track.duration = metadata.duration.or(track.duration);
        self.data.duration = track.duration;
        self.revision += 1;
        let rate = metadata.sample_rate;
        let mut session = Session {
            decoder,
            output: None,
            resampler: Resampler::new(rate, rate)?,
            output_rate: rate,
            base: Duration::ZERO,
            accepted: 0,
            played: 0,
            audible_start: 0,
            decode_start: 0,
            decode_base: Duration::ZERO,
            decoding_index: index,
            boundaries: VecDeque::new(),
            pending: Vec::new(),
            submitted: VecDeque::new(),
            offset: 0,
            eof: false,
            draining: false,
            last_progress: Instant::now(),
            last_device_pending: 0,
        };
        if play {
            session.attach(&self.output)?;
        }
        self.check_current_input()?;
        self.reset_processors();
        self.session = Some(session);
        self.data.state = if play {
            PlaybackState::Playing
        } else {
            PlaybackState::Paused
        };
        Ok(())
    }

    fn play(&mut self) -> Result<()> {
        if self.data.state == PlaybackState::Playing {
            return Ok(());
        }
        if self.data.state == PlaybackState::Paused
            && let Some(session) = self.session.as_mut()
        {
            session.attach(&self.output)?;
            self.data.state = PlaybackState::Playing;
            self.data.error = None;
            return Ok(());
        }
        if let Some(index) = self.data.current {
            self.load(index, true)?;
        }
        Ok(())
    }

    fn pause(&mut self) -> Result<()> {
        if self.data.state == PlaybackState::Playing {
            let session = self.session.as_mut().unwrap();
            session.pause();
            self.data.state = PlaybackState::Paused;
        }
        Ok(())
    }

    fn seek(&mut self, position: Duration) -> Result<()> {
        self.prefetcher.cancel();
        if self.session.is_some() && !self.data.seekable {
            self.data.error = Some("This source does not support seeking".into());
            return Ok(());
        }
        let play = self.data.state == PlaybackState::Playing;
        if self
            .session
            .as_ref()
            .is_some_and(|session| Some(session.decoding_index) != self.data.current)
        {
            self.load(self.data.current.unwrap(), false)?;
        }
        if self.session.is_none() {
            let Some(index) = self.data.current else {
                return Ok(());
            };
            self.load(index, false)?;
        }
        if !self.data.seekable {
            self.data.error = Some("This source does not support seeking".into());
            return Ok(());
        }
        let position = self
            .data
            .duration
            .map_or(position, |duration| position.min(duration));
        let session = self.session.as_mut().unwrap();
        session.reposition(position)?;
        if play {
            session.attach(&self.output)?;
        }
        self.reset_processors();
        self.data.state = if play {
            PlaybackState::Playing
        } else {
            PlaybackState::Paused
        };
        self.data.error = None;
        Ok(())
    }

    fn stop(&mut self, retain_position: bool) {
        self.prefetcher.cancel();
        if retain_position {
            if let Some(session) = &self.session {
                self.data.position = session.position();
            }
        } else {
            self.data.position = Duration::ZERO;
        }
        self.session.take();
        self.data.state = PlaybackState::Stopped;
    }

    fn advance(&mut self, natural: bool) -> Result<()> {
        let mut control = self
            .input_control
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if natural && control.generation != self.generation {
            return Err("Playback request was superseded".into());
        }
        if natural && control.sequence != self.sequence {
            // Apply accepted queue/index changes before choosing the automatic
            // successor. Their predicted index must not be overwritten by EOF.
            return Ok(());
        }
        let Some(index) = self.data.current else {
            return Ok(());
        };
        let next = if natural && self.data.repeat == RepeatMode::One {
            Some(index)
        } else if index + 1 < self.data.queue.len() {
            Some(index + 1)
        } else if self.data.repeat == RepeatMode::All {
            Some(0)
        } else {
            None
        };
        // Commit an automatic advance while its generation is current. A queued
        // manual Next then advances from the logically selected entry exactly once.
        if let Some(next) = next {
            self.data.current = Some(next);
            if natural {
                control.queued_current = Some(next);
            }
        }
        drop(control);
        if let Some(next) = next {
            self.load(next, true)
        } else {
            self.stop(natural);
            Ok(())
        }
    }

    fn command(&mut self, command: Command) {
        self.discard_cancelled_session();
        if let Err(error) = self.handle(command) {
            if self.obsolete() {
                return;
            }
            self.fail(error);
        }
        // A cancelled read can be followed by a no-op (for example an invalid
        // Select). Restore the original transport instead of leaving Loading.
        if !self.obsolete()
            && self.data.state == PlaybackState::Loading
            && self.session.is_none()
            && let Some(index) = self.data.current
        {
            let position = self.data.position;
            if let Err(error) = self.load(index, self.load_playing).and_then(|()| {
                if position.is_zero() {
                    Ok(())
                } else {
                    self.seek(position)
                }
            }) && !self.obsolete()
            {
                self.fail(error);
            }
        }
        self.publish(true);
    }

    fn queued_command(&mut self, queued: QueuedCommand) {
        self.update_audible();
        self.generation = queued.generation;
        self.sequence = queued.sequence;
        self.command(queued.command);
    }

    fn obsolete(&self) -> bool {
        self.input_control
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .generation
            != self.generation
    }

    fn check_current_input(&self) -> Result<()> {
        if self.obsolete()
            || self
                .input_cancelled
                .as_ref()
                .is_some_and(|cancelled| cancelled.load(Ordering::Acquire))
        {
            Err("Playback request was superseded".into())
        } else {
            Ok(())
        }
    }

    fn discard_cancelled_session(&mut self) {
        if self
            .input_cancelled
            .as_ref()
            .is_some_and(|cancelled| cancelled.load(Ordering::Acquire))
            && let Some(session) = self.session.take()
        {
            self.data.position = session.position();
            self.load_playing = self.data.state == PlaybackState::Playing;
            self.data.state = PlaybackState::Loading;
        }
    }

    fn append_tracks(&mut self, tracks: Vec<Track>, play: bool) -> Result<()> {
        if self.data.queue.len() + tracks.len() > MAX_QUEUE_TRACKS {
            self.data.error = Some(format!(
                "The queue supports at most {MAX_QUEUE_TRACKS} tracks"
            ));
            return Ok(());
        }
        if tracks.is_empty() {
            return Ok(());
        }
        let first_inserted = self.data.queue.len();
        self.data.queue.extend(tracks);
        self.revision += 1;
        if self.data.current.is_none() {
            self.data.current = Some(0);
        }
        if play {
            self.load(first_inserted, true)?;
        }
        Ok(())
    }

    fn replace_tracks_and_play(&mut self, tracks: Vec<Track>, index: usize) -> Result<()> {
        // Validate before dropping the active session or modifying any queue entry.
        // Match append rejection behavior: report the error while preserving playback.
        if let Err(error) = validate_replacement_at(&tracks, index) {
            self.data.error = Some(error);
            return Ok(());
        }
        self.data.queue = tracks;
        self.revision += 1;
        self.load(index, true)
    }

    fn handle(&mut self, command: Command) -> Result<()> {
        let buffered_wrap = self.data.repeat == RepeatMode::All
            && self.session.as_ref().is_some_and(|session| {
                session
                    .boundaries
                    .iter()
                    .any(|boundary| boundary.index == 0)
            });
        let affects_successor = match &command {
            Command::Add(paths) => {
                buffered_wrap
                    && !paths.is_empty()
                    && self.data.queue.len() + paths.len() <= MAX_QUEUE_TRACKS
            }
            Command::AddTracks(tracks) => {
                buffered_wrap
                    && !tracks.is_empty()
                    && self.data.queue.len() + tracks.len() <= MAX_QUEUE_TRACKS
            }
            Command::Remove(index) => *index < self.data.queue.len(),
            Command::SetRepeat(mode) => *mode != self.data.repeat,
            _ => false,
        };
        if affects_successor {
            self.prefetcher.cancel();
            // Already submitted successors must be discarded when their queue
            // order changes. Ordinary pause preserves them sample for sample.
            if self
                .session
                .as_ref()
                .is_some_and(|session| !session.boundaries.is_empty())
            {
                let position = self.session.as_ref().unwrap().position();
                let playing = self.data.state == PlaybackState::Playing;
                self.load(self.data.current.unwrap(), false)?;
                if self.data.seekable {
                    self.seek(position)?;
                }
                if playing {
                    self.play()?;
                }
            }
        }
        match command {
            Command::Add(paths) => {
                return self.handle(Command::AddTracks(
                    paths.into_iter().map(Track::from_path).collect(),
                ));
            }
            Command::AddTracks(tracks) => self.append_tracks(tracks, false)?,
            Command::AddTracksAndPlay(tracks) => self.append_tracks(tracks, true)?,
            Command::ReplaceTracksAndPlay(tracks) => self.replace_tracks_and_play(tracks, 0)?,
            Command::ReplaceTracksAndPlayAt { tracks, index } => {
                self.replace_tracks_and_play(tracks, index)?
            }
            Command::Play => self.play()?,
            Command::Pause => self.pause()?,
            Command::Toggle => {
                if self.data.state == PlaybackState::Playing {
                    self.pause()?;
                } else {
                    self.play()?;
                }
            }
            Command::Select(index) => {
                if index < self.data.queue.len() {
                    self.load(index, true)?;
                }
            }
            Command::Next => self.advance(false)?,
            Command::Previous => {
                if let Some(index) = self.data.current {
                    let previous = if index == 0 && self.data.repeat == RepeatMode::All {
                        self.data.queue.len() - 1
                    } else {
                        index.saturating_sub(1)
                    };
                    self.load(previous, true)?;
                }
            }
            Command::Seek(position) => self.seek(position)?,
            Command::SetVolume(volume) => self.data.volume = volume.clamp(0.0, 1.0),
            Command::SetRepeat(repeat) => self.data.repeat = repeat,
            Command::Remove(index) => {
                if index >= self.data.queue.len() {
                    return Ok(());
                }
                let was_playing = self.data.state == PlaybackState::Playing
                    || (self.data.state == PlaybackState::Loading && self.load_playing);
                let current = self.data.current;
                self.data.queue.remove(index);
                self.revision += 1;
                if current == Some(index) {
                    self.stop(false);
                    self.data.error = None;
                    self.data.current = if self.data.queue.is_empty() {
                        None
                    } else {
                        Some(index.min(self.data.queue.len() - 1))
                    };
                    self.data.duration = self
                        .data
                        .current
                        .and_then(|next| self.data.queue[next].duration);
                    if was_playing {
                        self.play()?;
                    }
                } else if let Some(current) = current
                    && index < current
                {
                    self.data.current = Some(current - 1);
                    if let Some(session) = &mut self.session {
                        session.decoding_index -= 1;
                    }
                }
            }
            Command::Clear => {
                self.stop(false);
                self.data.queue.clear();
                self.data.current = None;
                self.data.duration = None;
                self.data.seekable = false;
                self.data.error = None;
                self.revision += 1;
            }
        }
        Ok(())
    }

    fn successor(&self) -> Option<usize> {
        let index = self.session.as_ref()?.decoding_index;
        if self.data.repeat == RepeatMode::One {
            Some(index)
        } else if index + 1 < self.data.queue.len() {
            Some(index + 1)
        } else if self.data.repeat == RepeatMode::All && !self.data.queue.is_empty() {
            Some(0)
        } else {
            None
        }
    }

    fn ensure_prefetch(&mut self) -> Result<()> {
        let Some(index) = self.successor() else {
            self.prefetcher.cancel();
            return Ok(());
        };
        // Bound even a playlist containing sub-millisecond tracks.
        if self.session.as_ref().unwrap().boundaries.len() >= 64 {
            return Ok(());
        }
        if let Some(cancelled) = self.prefetcher.request(
            index,
            &self.data.queue[index],
            self.session.as_ref().unwrap().output_rate,
        )? {
            let mut control = self.input_control.lock().unwrap_or_else(|p| p.into_inner());
            if control.generation != self.generation {
                cancelled.store(true, Ordering::Release);
                return Err("Playback request was superseded".into());
            }
            control.prefetch = Some(cancelled);
        }
        Ok(())
    }

    fn update_audible(&mut self) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        let cursor = session.cursor();
        let mut control = self.input_control.lock().unwrap_or_else(|p| p.into_inner());
        if control.generation != self.generation || control.sequence != self.sequence {
            return;
        }
        while session
            .boundaries
            .front()
            .is_some_and(|boundary| cursor >= boundary.at)
        {
            let boundary = session.boundaries.pop_front().unwrap();
            session.audible_start = boundary.at;
            session.base = Duration::ZERO;
            self.data.current = Some(boundary.index);
            self.data.duration = self.data.queue[boundary.index].duration;
            self.data.seekable = boundary.seekable;
            control.queued_current = Some(boundary.index);
        }
    }

    fn continue_gapless(&mut self, index: usize, prepared: Prepared) -> Result<bool> {
        let mut control = self.input_control.lock().unwrap_or_else(|p| p.into_inner());
        if control.generation != self.generation {
            prepared.cancelled.store(true, Ordering::Release);
            return Err("Playback request was superseded".into());
        }
        if control.sequence != self.sequence {
            drop(control);
            self.prefetcher.restore(prepared);
            return Ok(false);
        }
        if let Some(previous) = control.active.replace(prepared.cancelled.clone()) {
            previous.store(true, Ordering::Release);
        }
        control.prefetch = None;
        self.input_cancelled = Some(prepared.cancelled);
        drop(control);
        self.prefetcher.finish();
        let metadata = prepared.decoder.metadata();
        let input_rate = metadata.sample_rate;
        if !(1000..=384_000).contains(&input_rate) {
            return Err(format!("Unsupported input sample rate: {input_rate} Hz"));
        }
        let track = &mut self.data.queue[index];
        let fallback = Track::from_path(track.path.clone()).title;
        if metadata.title != fallback || track.title.is_empty() {
            track.title.clone_from(&metadata.title);
        }
        if metadata.artist.is_some() {
            track.artist.clone_from(&metadata.artist);
        }
        if metadata.album.is_some() {
            track.album.clone_from(&metadata.album);
        }
        track.duration = metadata.duration.or(track.duration);
        self.revision += 1;
        let session = self.session.as_mut().unwrap();
        let at = session.played + session.accepted;
        session.boundaries.push_back(Boundary {
            at,
            index,
            seekable: prepared.decoder.is_seekable(),
        });
        session.decoder = prepared.decoder;
        session.decoding_index = index;
        session.decode_start = at;
        session.decode_base = Duration::ZERO;
        session.resampler = Resampler::new(input_rate, session.output_rate)?;
        session.pending = session.resampler.process(&prepared.first, false);
        session.offset = 0;
        session.eof = false;
        session.draining = false;
        // Preserve CLAP state across the boundary; only the input resampler has
        // a new source clock. No output drop, drain, silence or processor reset.
        for processor in &mut self.processors {
            processor.prepare(session.output_rate, OUTPUT_BLOCK)?;
            for block in session.pending.chunks_mut(OUTPUT_BLOCK) {
                processor.process(block, session.output_rate)?;
            }
        }
        self.update_audible();
        Ok(true)
    }

    fn tick(&mut self) -> Result<bool> {
        if self.obsolete() {
            return Ok(false);
        }
        for processor in &mut self.processors {
            processor.service()?;
        }
        if self.data.state != PlaybackState::Playing {
            return Ok(false);
        }
        self.update_audible();
        self.ensure_prefetch()?;
        let session = self.session.as_mut().unwrap();
        let output = session.output.as_mut().unwrap();
        output.check_error()?;
        let device_pending = output.pending_frames();
        if device_pending < session.last_device_pending {
            session.last_progress = Instant::now();
        }
        session.last_device_pending = device_pending;
        if session.last_progress.elapsed() > STALL_TIMEOUT
            && (device_pending != 0
                || session.offset < session.pending.len()
                || !session.eof
                || session.draining)
        {
            return Err("Audio output stopped consuming samples".into());
        }

        if session.offset < session.pending.len() {
            let end = (session.offset + OUTPUT_BLOCK).min(session.pending.len());
            let frames = &session.pending[session.offset..end];
            let mut mixed = [[0.0; 2]; OUTPUT_BLOCK];
            mixed[..frames.len()].copy_from_slice(frames);
            for frame in &mut mixed[..frames.len()] {
                for sample in frame {
                    *sample = (*sample * self.data.volume).clamp(-1.0, 1.0);
                }
            }
            let accepted = output.write(&mixed[..frames.len()])?;
            if accepted > frames.len() {
                return Err("Audio output reported an invalid accepted frame count".into());
            }
            session.offset += accepted;
            session.accepted += accepted as u64;
            session.submitted.extend(frames[..accepted].iter().copied());
            let waiting = output.pending_frames().min(session.submitted.len());
            session.submitted.drain(..session.submitted.len() - waiting);
            if accepted > 0 {
                session.last_progress = Instant::now();
                session.last_device_pending = output.pending_frames();
            }
            return Ok(accepted > 0);
        }
        if session.eof {
            if session.played + session.accepted == session.decode_start
                && session.decode_base.is_zero()
            {
                return Err("Track contains no decodable audio".into());
            }
            if let Some(next) = self.successor() {
                // Do not choose a successor ahead of already accepted commands.
                let control = self.input_control.lock().unwrap_or_else(|p| p.into_inner());
                if control.sequence != self.sequence {
                    return Ok(false);
                }
                drop(control);
                match self.prefetcher.take_ready(next) {
                    Some(Ok(prepared)) => {
                        if self.session.as_ref().unwrap().draining {
                            // A track appended after the playlist tail started
                            // draining cannot be written to that SAS stream.
                            if self
                                .session
                                .as_ref()
                                .unwrap()
                                .output
                                .as_ref()
                                .unwrap()
                                .is_drained()
                            {
                                self.prefetcher.finish();
                                drop(prepared);
                                self.advance(true)?;
                                return Ok(true);
                            }
                            self.prefetcher.restore(prepared);
                            return Ok(false);
                        }
                        return self.continue_gapless(next, prepared);
                    }
                    Some(Err(error)) => {
                        let session = self.session.as_mut().unwrap();
                        let output = session.output.as_mut().unwrap();
                        if !session.draining {
                            output.begin_drain()?;
                            session.draining = true;
                        }
                        if output.is_drained() {
                            return Err(error);
                        }
                        return Ok(false);
                    }
                    None => return Ok(false),
                }
            }
            let session = self.session.as_mut().unwrap();
            let output = session.output.as_mut().unwrap();
            if !session.draining {
                output.begin_drain()?;
                session.draining = true;
            }
            if output.is_drained() {
                self.stop(true);
                return Ok(true);
            }
            return Ok(false);
        }

        let input_rate = session.decoder.metadata().sample_rate;
        let input_count = ((OUTPUT_BLOCK as u64 * input_rate as u64 / session.output_rate as u64)
            .max(1) as usize)
            .min(OUTPUT_BLOCK);
        let frames = session.decoder.read_frames(input_count)?;
        if self
            .input_cancelled
            .as_ref()
            .is_some_and(|cancelled| cancelled.load(Ordering::Acquire))
        {
            return Err("Playback request was superseded".into());
        }
        session.eof = frames.is_empty();
        session.pending = session.resampler.process(&frames, session.eof);
        session.offset = 0;
        for processor in &mut self.processors {
            processor.prepare(session.output_rate, OUTPUT_BLOCK)?;
            for block in session.pending.chunks_mut(OUTPUT_BLOCK) {
                processor.process(block, session.output_rate)?;
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MediaSource;
    use std::sync::atomic::{AtomicU64, AtomicUsize};
    use std::{
        fs::File,
        io::{Read, Seek, SeekFrom},
    };

    static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    fn wav(frames: usize) -> Fixture {
        let path = std::env::temp_dir().join(format!(
            "cadence-player-{}-{}.wav",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut writer = hound::WavWriter::create(
            &path,
            hound::WavSpec {
                channels: 1,
                sample_rate: 8000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for _ in 0..frames {
            writer.write_sample(16384_i16).unwrap();
        }
        writer.finalize().unwrap();
        Fixture(path)
    }

    #[derive(Default)]
    struct DeviceState {
        pending: AtomicUsize,
        created: AtomicUsize,
        dropped: AtomicUsize,
        drains: AtomicUsize,
        fail: AtomicBool,
        hold_tail: AtomicBool,
        written: Mutex<Vec<Frame>>,
    }
    struct FakeOutput(Arc<DeviceState>);
    impl AudioOutput for FakeOutput {
        fn sample_rate(&self) -> u32 {
            8000
        }
        fn write(&mut self, frames: &[Frame]) -> Result<usize> {
            self.check_error()?;
            let capacity = 512usize.saturating_sub(self.0.pending.load(Ordering::Acquire));
            let count = frames.len().min(capacity);
            self.0
                .written
                .lock()
                .unwrap()
                .extend_from_slice(&frames[..count]);
            self.0.pending.fetch_add(count, Ordering::Release);
            Ok(count)
        }
        fn pending_frames(&self) -> usize {
            self.0.pending.load(Ordering::Acquire)
        }
        fn check_error(&self) -> Result<()> {
            if self.0.fail.load(Ordering::Acquire) {
                Err("Injected output failure".into())
            } else {
                Ok(())
            }
        }
        fn begin_drain(&mut self) -> Result<()> {
            self.0.drains.fetch_add(1, Ordering::Release);
            Ok(())
        }
        fn is_drained(&self) -> bool {
            self.pending_frames() == 0 && !self.0.hold_tail.load(Ordering::Acquire)
        }
    }
    impl Drop for FakeOutput {
        fn drop(&mut self) {
            self.0.pending.store(0, Ordering::Release);
            self.0.dropped.fetch_add(1, Ordering::Release);
        }
    }
    fn factory(device: &Arc<DeviceState>) -> OutputFactory {
        let device = Arc::clone(device);
        Box::new(move || {
            device.created.fetch_add(1, Ordering::Release);
            Ok(Box::new(FakeOutput(Arc::clone(&device))))
        })
    }
    fn wait(player: &Player, condition: impl Fn(&Snapshot) -> bool) -> Snapshot {
        let start = Instant::now();
        loop {
            let snapshot = player.snapshot();
            if condition(&snapshot) {
                return snapshot;
            }
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "Timed out: {snapshot:?}"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn blocked_stream(path: PathBuf, entered: Arc<AtomicBool>) -> Track {
        let mut track = Track::from_path("remote-fixture".into());
        track.title = "Remote title".into();
        track.media_input = Some(MediaInput::new(
            "blocked-stream",
            Some("wav".into()),
            move |cancelled| {
                entered.store(true, Ordering::Release);
                while !cancelled.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(1));
                }
                // Returning an otherwise valid stream after cancellation also checks
                // that stale providers cannot publish a newly opened decoder.
                File::open(&path)
                    .map(|file| Box::new(file) as Box<dyn MediaSource>)
                    .map_err(|error| error.to_string())
            },
        ));
        track
    }

    #[test]
    fn clear_cancels_a_blocked_stream_open_without_publishing_stale_audio() {
        let fixture = wav(16_000);
        let entered = Arc::new(AtomicBool::new(false));
        let device = Arc::new(DeviceState::default());
        let player = Player::new(factory(&device)).unwrap();
        player
            .send(Command::AddTracksAndPlay(vec![blocked_stream(
                fixture.0.clone(),
                entered.clone(),
            )]))
            .unwrap();
        wait(&player, |_| entered.load(Ordering::Acquire));
        let start = Instant::now();
        player.send(Command::Clear).unwrap();
        let cleared = wait(&player, |snapshot| {
            snapshot.state == PlaybackState::Stopped && snapshot.queue.is_empty()
        });
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(cleared.current, None);
        assert!(cleared.error.is_none());
        assert_eq!(device.created.load(Ordering::Acquire), 0);
        player.shutdown();
    }

    #[test]
    fn successive_navigation_keeps_queue_intent_when_an_old_stream_open_is_cancelled() {
        let fixture = wav(16_000);
        let entered = Arc::new(AtomicBool::new(false));
        let device = Arc::new(DeviceState::default());
        let player = Player::new(factory(&device)).unwrap();
        let mut local = Track::from_path(fixture.0.clone());
        local.title = "Local final selection".into();
        player
            .send(Command::AddTracksAndPlay(vec![
                blocked_stream(fixture.0.clone(), entered.clone()),
                local.clone(),
                local,
            ]))
            .unwrap();
        wait(&player, |_| entered.load(Ordering::Acquire));
        player.send(Command::Select(1)).unwrap();
        player.send(Command::Next).unwrap();
        let playing = wait(&player, |snapshot| {
            snapshot.current == Some(2) && snapshot.state == PlaybackState::Playing
        });
        assert_eq!(playing.queue[0].title, "Remote title");
        assert!(playing.error.is_none());
        assert!(device.created.load(Ordering::Acquire) <= 2);
        player.shutdown();
    }

    #[test]
    fn append_and_play_and_removing_the_loading_entry_cancel_blocked_source_open() {
        let fixture = wav(16_000);
        for remove in [false, true] {
            let entered = Arc::new(AtomicBool::new(false));
            let device = Arc::new(DeviceState::default());
            let player = Player::new(factory(&device)).unwrap();
            player
                .send(Command::AddTracksAndPlay(vec![
                    blocked_stream(fixture.0.clone(), entered.clone()),
                    Track::from_path(fixture.0.clone()),
                ]))
                .unwrap();
            wait(&player, |_| entered.load(Ordering::Acquire));
            if remove {
                player.send(Command::Remove(0)).unwrap();
            } else {
                player
                    .send(Command::AddTracksAndPlay(vec![Track::from_path(
                        fixture.0.clone(),
                    )]))
                    .unwrap();
            }
            let playing = wait(&player, |snapshot| snapshot.state == PlaybackState::Playing);
            assert_eq!(playing.current, Some(if remove { 0 } else { 2 }));
            assert_eq!(playing.queue.len(), if remove { 1 } else { 3 });
            assert!(playing.error.is_none());
            assert_eq!(device.created.load(Ordering::Acquire), 1);
            player.shutdown();
        }
    }

    #[test]
    fn pending_selection_and_index_shifts_cannot_open_a_track_already_removed_by_queued_commands() {
        let fixture = wav(16_000);
        for shifted in [false, true] {
            let entered = Arc::new(AtomicBool::new(false));
            let released = Arc::new(AtomicBool::new(false));
            let obsolete_opens = Arc::new(AtomicUsize::new(0));
            let mut first = Track::from_path("initial-blocked.wav".into());
            let gate = released.clone();
            let signal = entered.clone();
            first.media_input = Some(MediaInput::new(
                "gated",
                Some("wav".into()),
                move |cancelled| {
                    signal.store(true, Ordering::Release);
                    let deadline = Instant::now() + Duration::from_secs(2);
                    while !gate.load(Ordering::Acquire) || !cancelled.load(Ordering::Acquire) {
                        if Instant::now() >= deadline {
                            return Err("fixture gate timed out".into());
                        }
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err("cancelled".into())
                },
            ));
            let mut obsolete = Track::from_path("must-not-open.wav".into());
            let open_count = obsolete_opens.clone();
            obsolete.media_input =
                Some(MediaInput::new("obsolete", Some("wav".into()), move |_| {
                    open_count.fetch_add(1, Ordering::Relaxed);
                    Err("obsolete track was opened".into())
                }));
            let local = Track::from_path(fixture.0.clone());
            let tracks = if shifted {
                vec![obsolete, local, first]
            } else {
                vec![first, obsolete, local]
            };
            let device = Arc::new(DeviceState::default());
            let player = Player::new(factory(&device)).unwrap();
            player.send(Command::AddTracks(tracks)).unwrap();
            player
                .send(Command::Select(if shifted { 2 } else { 0 }))
                .unwrap();
            wait(&player, |_| entered.load(Ordering::Acquire));
            if shifted {
                player.send(Command::Remove(0)).unwrap();
            } else {
                player.send(Command::Select(1)).unwrap();
            }
            player.send(Command::Remove(1)).unwrap();
            released.store(true, Ordering::Release);
            let playing = wait(&player, |snapshot| snapshot.state == PlaybackState::Playing);
            assert_eq!(playing.queue.len(), if shifted { 1 } else { 2 });
            assert_eq!(playing.current, Some(if shifted { 0 } else { 1 }));
            assert!(
                playing.queue[playing.current.unwrap()]
                    .media_input
                    .is_none()
            );
            assert!(playing.error.is_none());
            assert_eq!(obsolete_opens.load(Ordering::Relaxed), 0);
            player.shutdown();
        }
    }

    #[test]
    fn replacement_cancels_pending_stream_and_shutdown_cancels_its_current_opener() {
        let fixture = wav(16_000);
        let entered = Arc::new(AtomicBool::new(false));
        let device = Arc::new(DeviceState::default());
        let player = Player::new(factory(&device)).unwrap();
        player
            .send(Command::AddTracksAndPlay(vec![blocked_stream(
                fixture.0.clone(),
                entered.clone(),
            )]))
            .unwrap();
        wait(&player, |_| entered.load(Ordering::Acquire));
        player
            .send(Command::ReplaceTracksAndPlay(vec![Track::from_path(
                fixture.0.clone(),
            )]))
            .unwrap();
        let playing = wait(&player, |snapshot| snapshot.state == PlaybackState::Playing);
        assert_eq!(playing.queue.len(), 1);
        assert_eq!(playing.queue[0].path, fixture.0);
        assert!(playing.queue[0].media_input.is_none());
        assert!(playing.error.is_none());
        player.shutdown();

        let entered = Arc::new(AtomicBool::new(false));
        let player = Player::new(factory(&device)).unwrap();
        player
            .send(Command::AddTracksAndPlay(vec![blocked_stream(
                fixture.0.clone(),
                entered.clone(),
            )]))
            .unwrap();
        wait(&player, |_| entered.load(Ordering::Acquire));
        let start = Instant::now();
        player.shutdown();
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    struct BlockingRead {
        file: File,
        block: Arc<AtomicBool>,
        entered: Arc<AtomicBool>,
        cancelled: Arc<AtomicBool>,
    }
    impl Read for BlockingRead {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.block.load(Ordering::Acquire) {
                self.entered.store(true, Ordering::Release);
                while !self.cancelled.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(1));
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionAborted,
                    "cancelled",
                ));
            }
            self.file.read(buffer)
        }
    }
    impl Seek for BlockingRead {
        fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
            self.file.seek(position)
        }
    }
    impl MediaSource for BlockingRead {
        fn is_seekable(&self) -> bool {
            true
        }
        fn byte_len(&self) -> Option<u64> {
            self.file.metadata().ok().map(|metadata| metadata.len())
        }
    }

    #[test]
    fn selecting_another_track_cancels_a_blocked_read_and_drops_its_output() {
        let fixture = wav(800_000);
        let block = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(AtomicBool::new(false));
        let stream_block = block.clone();
        let stream_entered = entered.clone();
        let path = fixture.0.clone();
        let mut remote = Track::from_path("remote-read".into());
        remote.media_input = Some(MediaInput::new(
            "blocked-read",
            Some("wav".into()),
            move |cancelled| {
                Ok(Box::new(BlockingRead {
                    file: File::open(&path).map_err(|error| error.to_string())?,
                    block: stream_block.clone(),
                    entered: stream_entered.clone(),
                    cancelled,
                }))
            },
        ));
        let device = Arc::new(DeviceState::default());
        let player = Player::new(factory(&device)).unwrap();
        player
            .send(Command::AddTracksAndPlay(vec![
                remote,
                Track::from_path(fixture.0.clone()),
            ]))
            .unwrap();
        wait(&player, |snapshot| {
            snapshot.state == PlaybackState::Playing && device.pending.load(Ordering::Acquire) > 0
        });
        block.store(true, Ordering::Release);
        wait(&player, |_| {
            device.pending.store(0, Ordering::Release);
            entered.load(Ordering::Acquire)
        });
        let start = Instant::now();
        player.send(Command::Select(1)).unwrap();
        let playing = wait(&player, |snapshot| {
            snapshot.current == Some(1) && snapshot.state == PlaybackState::Playing
        });
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(playing.error.is_none());
        assert_eq!(device.created.load(Ordering::Acquire), 2);
        assert_eq!(device.dropped.load(Ordering::Acquire), 1);
        player.shutdown();
    }

    struct ForwardOnly(File);
    impl Read for ForwardOnly {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.0.read(buffer)
        }
    }
    impl Seek for ForwardOnly {
        fn seek(&mut self, _: SeekFrom) -> std::io::Result<u64> {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "forward only",
            ))
        }
    }
    impl MediaSource for ForwardOnly {
        fn is_seekable(&self) -> bool {
            false
        }
        fn byte_len(&self) -> Option<u64> {
            self.0.metadata().ok().map(|metadata| metadata.len())
        }
    }

    #[test]
    fn forward_only_stream_pause_retains_exact_unconsumed_audio_without_reopening() {
        let fixture = wav(16_000);
        let mut writer = hound::WavWriter::create(
            &fixture.0,
            hound::WavSpec {
                channels: 1,
                sample_rate: 8000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for frame in 0..16_000 {
            writer.write_sample((frame % 1000) as i16).unwrap();
        }
        writer.finalize().unwrap();
        let opened = Arc::new(AtomicUsize::new(0));
        let open_count = opened.clone();
        let path = fixture.0.clone();
        let mut track = Track::from_path("forward-only.wav".into());
        track.media_input = Some(MediaInput::new(
            "forward-only",
            Some("wav".into()),
            move |_| {
                open_count.fetch_add(1, Ordering::Relaxed);
                Ok(Box::new(ForwardOnly(
                    File::open(&path).map_err(|error| error.to_string())?,
                )))
            },
        ));
        let device = Arc::new(DeviceState::default());
        let player = Player::new(factory(&device)).unwrap();
        player.send(Command::AddTracksAndPlay(vec![track])).unwrap();
        let playing = wait(&player, |snapshot| {
            snapshot.state == PlaybackState::Playing
                && device.pending.load(Ordering::Acquire) == 512
        });
        assert!(!playing.seekable);
        player.send(Command::Select(99)).unwrap();
        player.send(Command::AddTracksAndPlay(Vec::new())).unwrap();
        player.send(Command::SetVolume(0.75)).unwrap();
        device.pending.fetch_sub(200, Ordering::AcqRel);
        player.send(Command::Pause).unwrap();
        let paused = wait(&player, |snapshot| snapshot.state == PlaybackState::Paused);
        assert_eq!(paused.position, Duration::from_millis(25));
        assert!(paused.error.is_none());
        let written_before_resume = device.written.lock().unwrap().len();
        player.send(Command::Seek(Duration::from_secs(1))).unwrap();
        let rejected = wait(&player, |snapshot| snapshot.error.is_some());
        assert_eq!(rejected.state, PlaybackState::Paused);
        assert_eq!(rejected.position, paused.position);
        assert_eq!(device.created.load(Ordering::Acquire), 1);
        player.send(Command::SetVolume(0.25)).unwrap();
        player.send(Command::Play).unwrap();
        let resumed = wait(&player, |snapshot| {
            snapshot.state == PlaybackState::Playing
                && device.pending.load(Ordering::Acquire) == 512
        });
        assert!(resumed.error.is_none());
        let written = device.written.lock().unwrap();
        for (index, frame) in written[written_before_resume..].iter().enumerate() {
            let sample = ((index + 200) % 1000) as f32 / 32768.0 * 0.25;
            assert_eq!(
                *frame,
                [sample, sample],
                "discontinuity at resumed frame {index}"
            );
        }
        drop(written);
        assert_eq!(
            opened.load(Ordering::Relaxed),
            1,
            "pause reopened the forward stream"
        );
        player.shutdown();
    }

    #[test]
    fn queue_navigation_pause_seek_and_clear_recreate_devices() {
        let tracks = [wav(16000), wav(16000), wav(16000)];
        let device = Arc::new(DeviceState::default());
        let player = Player::new(factory(&device)).unwrap();
        player
            .send(Command::Add(
                tracks.iter().map(|track| track.0.clone()).collect(),
            ))
            .unwrap();
        let initial = wait(&player, |snapshot| snapshot.queue.len() == 3);
        assert_eq!(initial.current, Some(0));
        assert_eq!(initial.state, PlaybackState::Stopped);
        player.send(Command::SetVolume(0.25)).unwrap();
        player.send(Command::Select(1)).unwrap();
        wait(&player, |snapshot| {
            snapshot.state == PlaybackState::Playing
                && device.pending.load(Ordering::Acquire) == 512
        });
        assert_eq!(device.written.lock().unwrap()[0], [0.125, 0.125]);
        player.send(Command::Remove(0)).unwrap();
        let removed = wait(&player, |snapshot| snapshot.queue.len() == 2);
        assert_eq!(removed.current, Some(0));
        assert_eq!(removed.queue[0].path, tracks[1].0);
        assert_eq!(device.created.load(Ordering::Acquire), 1);

        // Consume half the first accepted device block. Queued frames must not
        // advance the audible cursor when pausing and discarding the device.
        device.pending.store(256, Ordering::Release);
        player.send(Command::Pause).unwrap();
        let paused = wait(&player, |snapshot| snapshot.state == PlaybackState::Paused);
        assert_eq!(paused.position, Duration::from_millis(32));
        assert_eq!(device.dropped.load(Ordering::Acquire), 1);
        assert_eq!(device.pending.load(Ordering::Acquire), 0);
        thread::sleep(Duration::from_millis(40));
        assert_eq!(player.snapshot().position, paused.position);
        player
            .send(Command::Seek(Duration::from_millis(500)))
            .unwrap();
        wait(&player, |snapshot| {
            snapshot.position >= Duration::from_millis(499)
        });
        assert_eq!(device.created.load(Ordering::Acquire), 1);
        player.send(Command::Play).unwrap();
        wait(&player, |snapshot| snapshot.state == PlaybackState::Playing);
        assert_eq!(device.created.load(Ordering::Acquire), 2);
        player.send(Command::Next).unwrap();
        wait(&player, |snapshot| {
            snapshot.current == Some(1) && snapshot.state == PlaybackState::Playing
        });
        player.send(Command::Previous).unwrap();
        wait(&player, |snapshot| {
            snapshot.current == Some(0) && snapshot.state == PlaybackState::Playing
        });
        player.send(Command::SetRepeat(RepeatMode::All)).unwrap();
        player.send(Command::Previous).unwrap();
        wait(&player, |snapshot| {
            snapshot.current == Some(1) && snapshot.repeat == RepeatMode::All
        });
        player.send(Command::Clear).unwrap();
        let cleared = wait(&player, |snapshot| snapshot.queue.is_empty());
        assert_eq!(cleared.current, None);
        assert_eq!(cleared.state, PlaybackState::Stopped);
        assert_eq!(cleared.position, Duration::ZERO);
        assert_eq!(
            device.created.load(Ordering::Acquire),
            device.dropped.load(Ordering::Acquire)
        );
        player.shutdown();
    }

    #[test]
    fn eof_flushes_once_and_waits_for_the_device_tail() {
        let track = wav(800);
        let device = Arc::new(DeviceState::default());
        device.hold_tail.store(true, Ordering::Release);
        let player = Player::new(factory(&device)).unwrap();
        player.send(Command::Add(vec![track.0.clone()])).unwrap();
        player.send(Command::Play).unwrap();
        wait(&player, |_| device.pending.load(Ordering::Acquire) == 512);
        device.pending.store(0, Ordering::Release);
        wait(&player, |_| device.drains.load(Ordering::Acquire) == 1);
        assert_eq!(device.pending.load(Ordering::Acquire), 288);
        assert_eq!(player.snapshot().state, PlaybackState::Playing);
        device.pending.store(0, Ordering::Release);
        thread::sleep(Duration::from_millis(40));
        assert_eq!(player.snapshot().state, PlaybackState::Playing);
        assert_eq!(device.drains.load(Ordering::Acquire), 1);
        device.hold_tail.store(false, Ordering::Release);
        let stopped = wait(&player, |snapshot| snapshot.state == PlaybackState::Stopped);
        assert_eq!(stopped.current, Some(0));
        assert_eq!(stopped.position, Duration::from_millis(100));
        assert_eq!(device.written.lock().unwrap().len(), 800);
        assert_eq!(device.dropped.load(Ordering::Acquire), 1);
        player.shutdown();
    }

    #[test]
    fn repeat_one_restarts_a_valid_track_and_output_failure_stays_failed() {
        let track = wav(80);
        let device = Arc::new(DeviceState::default());
        let player = Player::new(factory(&device)).unwrap();
        player.send(Command::Add(vec![track.0.clone()])).unwrap();
        player.send(Command::SetRepeat(RepeatMode::One)).unwrap();
        player.send(Command::Play).unwrap();
        wait(&player, |_| device.pending.load(Ordering::Acquire) == 512);
        assert_eq!(device.drains.load(Ordering::Acquire), 0);
        assert_eq!(device.created.load(Ordering::Acquire), 1);
        assert_eq!(device.written.lock().unwrap().len(), 512);
        device.fail.store(true, Ordering::Release);
        let failed = wait(&player, |snapshot| snapshot.state == PlaybackState::Error);
        assert!(failed.error.unwrap().contains("Injected"));
        thread::sleep(Duration::from_millis(50));
        assert_eq!(device.created.load(Ordering::Acquire), 1);
        assert_eq!(device.dropped.load(Ordering::Acquire), 1);
        player.shutdown();
    }

    fn patterned_wav(rate: u32, count: usize, seed: i16) -> Fixture {
        let fixture = wav(0);
        let mut writer = hound::WavWriter::create(
            &fixture.0,
            hound::WavSpec {
                channels: 2,
                sample_rate: rate,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for index in 0..count {
            let sample = seed + (index % 997) as i16;
            writer.write_sample(sample).unwrap();
            writer.write_sample(-sample).unwrap();
        }
        writer.finalize().unwrap();
        fixture
    }

    fn expected_pcm(track: &Fixture) -> Vec<Frame> {
        let mut decoder = DecoderRegistry::default().open(&track.0).unwrap();
        let mut resampler = Resampler::new(decoder.metadata().sample_rate, 8000).unwrap();
        let mut result = Vec::new();
        loop {
            let block = decoder.read_frames(2048).unwrap();
            result.extend(resampler.process(&block, block.is_empty()));
            if block.is_empty() {
                return result;
            }
        }
    }

    fn fill_device(worker: &mut Worker, device: &DeviceState) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while device.pending.load(Ordering::Acquire) < 512
            && worker.data.state == PlaybackState::Playing
        {
            worker.tick().unwrap();
            if worker
                .session
                .as_ref()
                .is_some_and(|session| session.draining)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "prefetch did not fill the output"
            );
            thread::sleep(Duration::from_millis(1));
        }
        worker.publish(true);
    }

    #[test]
    fn gapless_pcm_is_exact_across_rates_and_pause_at_a_buffered_boundary() {
        struct Processor(Arc<AtomicUsize>);
        impl AudioProcessor for Processor {
            fn process(&mut self, _: &mut [Frame], _: u32) -> Result<()> {
                Ok(())
            }
            fn reset(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let files = [
            patterned_wav(8000, 251, 1000),
            patterned_wav(16000, 806, 5000),
            patterned_wav(11025, 777, 9000),
        ];
        let expected_tracks: Vec<_> = files.iter().map(expected_pcm).collect();
        let expected: Vec<_> = expected_tracks.iter().flatten().copied().collect();
        for (pause, append) in [(false, false), (true, false), (false, true)] {
            let device = Arc::new(DeviceState::default());
            let resets = Arc::new(AtomicUsize::new(0));
            let mut worker = Worker::new(
                factory(&device),
                DecoderRegistry::default(),
                vec![Box::new(Processor(resets.clone()))],
                Arc::new(Mutex::new(Snapshot::default())),
                Arc::new(Mutex::new(InputControl::default())),
            );
            worker.data.volume = 1.;
            worker
                .handle(Command::Add(
                    files
                        .iter()
                        .take(if append { 2 } else { 3 })
                        .map(|f| f.0.clone())
                        .collect(),
                ))
                .unwrap();
            worker.handle(Command::Play).unwrap();
            fill_device(&mut worker, &device);
            assert_eq!(
                worker.data.current,
                Some(0),
                "prefetch must not select the next track early"
            );
            assert!(!worker.session.as_ref().unwrap().boundaries.is_empty());
            if append {
                worker
                    .handle(Command::Add(vec![files[2].0.clone()]))
                    .unwrap();
                assert_eq!(
                    device.created.load(Ordering::Acquire),
                    1,
                    "append interrupted buffered audio"
                );
            }
            let mut consumed = Vec::new();
            let mut read = 0;
            let mut paused = false;
            while worker.data.state == PlaybackState::Playing {
                let count = device.pending.load(Ordering::Acquire).min(128);
                assert!(count > 0, "output underflow before the playlist ended");
                consumed.extend_from_slice(&device.written.lock().unwrap()[read..read + count]);
                read += count;
                device.pending.fetch_sub(count, Ordering::Release);
                worker.publish(true);
                let mut start = 0;
                let index = expected_tracks
                    .iter()
                    .position(|frames| {
                        start += frames.len();
                        consumed.len() < start
                    })
                    .unwrap_or(expected_tracks.len() - 1);
                assert_eq!(
                    worker.data.current,
                    Some(index),
                    "highlight is not at the consumed boundary"
                );
                if pause && !paused {
                    let position = worker.data.position;
                    worker.handle(Command::Pause).unwrap();
                    worker.publish(true);
                    assert_eq!(worker.data.position, position);
                    read = device.written.lock().unwrap().len(); // the old unconsumed device tail was discarded
                    worker.handle(Command::Play).unwrap();
                    paused = true;
                }
                fill_device(&mut worker, &device);
            }
            assert_eq!(
                consumed, expected,
                "inserted, dropped or duplicated PCM at a boundary"
            );
            assert_eq!(
                device.created.load(Ordering::Acquire),
                if pause { 2 } else { 1 }
            );
            assert_eq!(
                device.dropped.load(Ordering::Acquire),
                if pause { 2 } else { 1 }
            );
            assert_eq!(
                device.drains.load(Ordering::Acquire),
                1,
                "only the playlist tail may drain"
            );
            assert_eq!(
                resets.load(Ordering::Acquire),
                1,
                "CLAP state was reset between tracks"
            );
            assert_eq!(worker.data.current, Some(2));
        }
    }

    #[test]
    fn gapless_repeat_all_wraps_without_reopening_output() {
        let files = [
            patterned_wav(8000, 251, 1000),
            patterned_wav(16000, 806, 5000),
        ];
        let cycle: Vec<_> = files.iter().flat_map(expected_pcm).collect();
        let device = Arc::new(DeviceState::default());
        let mut worker = Worker::new(
            factory(&device),
            DecoderRegistry::default(),
            vec![],
            Arc::new(Mutex::new(Snapshot::default())),
            Arc::new(Mutex::new(InputControl::default())),
        );
        worker.data.volume = 1.;
        worker.data.repeat = RepeatMode::All;
        worker
            .handle(Command::Add(files.iter().map(|f| f.0.clone()).collect()))
            .unwrap();
        worker.handle(Command::Play).unwrap();
        let mut consumed = Vec::new();
        let mut read = 0;
        for _ in 0..12 {
            fill_device(&mut worker, &device);
            consumed.extend_from_slice(&device.written.lock().unwrap()[read..read + 128]);
            read += 128;
            device.pending.fetch_sub(128, Ordering::Release);
        }
        assert_eq!(
            consumed,
            cycle
                .iter()
                .cycle()
                .take(consumed.len())
                .copied()
                .collect::<Vec<_>>()
        );
        assert_eq!(device.created.load(Ordering::Acquire), 1);
        assert_eq!(device.drains.load(Ordering::Acquire), 0);
        worker.handle(Command::Clear).unwrap();
        assert_eq!(device.dropped.load(Ordering::Acquire), 1);
    }

    #[test]
    fn seek_and_queue_edits_discard_prefetched_successors_without_stale_audio() {
        let files = [
            patterned_wav(8000, 251, 1000),
            patterned_wav(8000, 800, 5000),
        ];
        let first_pcm = expected_pcm(&files[0]);
        for remove in [false, true] {
            let device = Arc::new(DeviceState::default());
            let mut worker = Worker::new(
                factory(&device),
                DecoderRegistry::default(),
                vec![],
                Arc::new(Mutex::new(Snapshot::default())),
                Arc::new(Mutex::new(InputControl::default())),
            );
            worker.data.volume = 1.;
            worker
                .handle(Command::Add(files.iter().map(|f| f.0.clone()).collect()))
                .unwrap();
            worker.handle(Command::Play).unwrap();
            fill_device(&mut worker, &device);
            assert_eq!(worker.data.current, Some(0));
            assert_eq!(worker.session.as_ref().unwrap().decoding_index, 1);
            worker.handle(Command::Remove(999)).unwrap();
            worker.handle(Command::AddTracks(Vec::new())).unwrap();
            assert_eq!(
                device.created.load(Ordering::Acquire),
                1,
                "no-op edit restarted playback"
            );
            let written = device.written.lock().unwrap().len();
            if remove {
                worker.handle(Command::Remove(1)).unwrap();
            } else {
                worker
                    .handle(Command::Seek(Duration::from_micros(12500)))
                    .unwrap();
            }
            fill_device(&mut worker, &device);
            assert_eq!(worker.data.current, Some(0));
            let after = device.written.lock().unwrap();
            assert_eq!(
                after[written],
                first_pcm[if remove { 0 } else { 100 }],
                "seek used the decoder of the buffered next track"
            );
            if remove {
                assert_eq!(after.len() - written, first_pcm.len());
            }
            assert_eq!(device.created.load(Ordering::Acquire), 2);
            assert_eq!(device.drains.load(Ordering::Acquire), usize::from(remove));
        }
    }

    #[test]
    fn failed_prefetch_preserves_the_current_tail_and_reports_once() {
        let track = wav(800);
        let device = Arc::new(DeviceState::default());
        device.hold_tail.store(true, Ordering::Release);
        let player = Player::new(factory(&device)).unwrap();
        player
            .send(Command::ReplaceTracksAndPlay(vec![
                Track::from_path(track.0.clone()),
                Track::from_path(track.0.with_extension("missing")),
            ]))
            .unwrap();
        wait(&player, |_| device.pending.load(Ordering::Acquire) == 512);
        device.pending.store(0, Ordering::Release);
        wait(&player, |_| device.drains.load(Ordering::Acquire) == 1);
        assert_eq!(device.pending.load(Ordering::Acquire), 288);
        device.pending.store(0, Ordering::Release);
        thread::sleep(Duration::from_millis(40));
        assert_eq!(player.snapshot().state, PlaybackState::Playing);
        device.hold_tail.store(false, Ordering::Release);
        let failed = wait(&player, |snapshot| snapshot.state == PlaybackState::Error);
        assert!(failed.error.unwrap().contains("Unable to open"));
        assert_eq!(device.written.lock().unwrap().len(), 800);
        assert_eq!(device.created.load(Ordering::Acquire), 1);
        assert_eq!(device.dropped.load(Ordering::Acquire), 1);
        assert_eq!(device.drains.load(Ordering::Acquire), 1);
        player.shutdown();
    }

    #[test]
    fn a_command_accepted_during_gapless_handoff_defers_without_losing_pcm() {
        let files = [
            patterned_wav(8000, 251, 1000),
            patterned_wav(8000, 800, 5000),
        ];
        let device = Arc::new(DeviceState::default());
        let control = Arc::new(Mutex::new(InputControl::default()));
        let mut worker = Worker::new(
            factory(&device),
            DecoderRegistry::default(),
            vec![],
            Arc::new(Mutex::new(Snapshot::default())),
            control.clone(),
        );
        worker
            .handle(Command::Add(files.iter().map(|f| f.0.clone()).collect()))
            .unwrap();
        worker.handle(Command::Play).unwrap();
        // Decode, submit, then recognize current EOF, before the handoff tick.
        for _ in 0..3 {
            worker.tick().unwrap();
        }
        assert!(worker.session.as_ref().unwrap().eof);
        let deadline = Instant::now() + Duration::from_secs(3);
        let prepared = loop {
            if let Some(result) = worker.prefetcher.take_ready(1) {
                break result.unwrap();
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        };
        control.lock().unwrap().sequence = 1;
        assert!(!worker.continue_gapless(1, prepared).unwrap());
        assert_eq!(worker.session.as_ref().unwrap().decoding_index, 0);
        worker.sequence = 1; // the queued command has now been handled
        assert!(worker.tick().unwrap());
        assert_eq!(worker.session.as_ref().unwrap().decoding_index, 1);
        assert_eq!(worker.data.current, Some(0));
        assert_eq!(device.pending.load(Ordering::Acquire), 251);
        assert_eq!(device.created.load(Ordering::Acquire), 1);
        assert_eq!(device.drains.load(Ordering::Acquire), 0);
    }

    #[test]
    fn waiting_for_a_slow_source_is_not_reported_as_a_stalled_device() {
        let current = wav(251);
        let entered = Arc::new(AtomicBool::new(false));
        let next = blocked_stream(PathBuf::from("slow-next.wav"), entered.clone());
        let device = Arc::new(DeviceState::default());
        let mut worker = Worker::new(
            factory(&device),
            DecoderRegistry::default(),
            vec![],
            Arc::new(Mutex::new(Snapshot::default())),
            Arc::new(Mutex::new(InputControl::default())),
        );
        worker
            .handle(Command::AddTracksAndPlay(vec![
                Track::from_path(current.0.clone()),
                next,
            ]))
            .unwrap();
        for _ in 0..3 {
            worker.tick().unwrap();
        }
        assert!(worker.session.as_ref().unwrap().eof);
        device.pending.store(0, Ordering::Release);
        let session = worker.session.as_mut().unwrap();
        session.last_device_pending = 0;
        session.last_progress = Instant::now() - STALL_TIMEOUT - Duration::from_secs(1);
        assert!(!worker.tick().unwrap());
        assert_eq!(worker.data.state, PlaybackState::Playing);
        worker.handle(Command::Clear).unwrap();
    }

    #[test]
    fn append_after_terminal_drain_waits_for_the_tail_before_reopening() {
        let first = wav(251);
        let second = wav(16000);
        let device = Arc::new(DeviceState::default());
        device.hold_tail.store(true, Ordering::Release);
        let player = Player::new(factory(&device)).unwrap();
        player
            .send(Command::AddTracksAndPlay(vec![Track::from_path(
                first.0.clone(),
            )]))
            .unwrap();
        wait(&player, |_| device.drains.load(Ordering::Acquire) == 1);
        player.send(Command::Add(vec![second.0.clone()])).unwrap();
        wait(&player, |snapshot| snapshot.queue.len() == 2);
        assert_eq!(device.created.load(Ordering::Acquire), 1);
        device.pending.store(0, Ordering::Release);
        thread::sleep(Duration::from_millis(40));
        assert_eq!(device.created.load(Ordering::Acquire), 1);
        device.hold_tail.store(false, Ordering::Release);
        wait(&player, |snapshot| {
            snapshot.current == Some(1) && device.pending.load(Ordering::Acquire) == 512
        });
        assert_eq!(device.created.load(Ordering::Acquire), 2);
        assert_eq!(device.drains.load(Ordering::Acquire), 1);
        player.send(Command::Clear).unwrap();
        wait(&player, |snapshot| snapshot.queue.is_empty());
        player.shutdown();
    }

    #[test]
    fn gapless_prefetch_open_is_cancelled_without_blocking_clear() {
        let current = wav(16000);
        let entered = Arc::new(AtomicBool::new(false));
        let next = blocked_stream(PathBuf::from("next.wav"), entered.clone());
        let device = Arc::new(DeviceState::default());
        let player = Player::new(factory(&device)).unwrap();
        player
            .send(Command::ReplaceTracksAndPlay(vec![
                Track::from_path(current.0.clone()),
                next,
            ]))
            .unwrap();
        wait(&player, |_| {
            entered.load(Ordering::Acquire) && device.pending.load(Ordering::Acquire) == 512
        });
        player.send(Command::Clear).unwrap();
        let snapshot = wait(&player, |snapshot| snapshot.queue.is_empty());
        assert_eq!(snapshot.state, PlaybackState::Stopped);
        assert_eq!(device.created.load(Ordering::Acquire), 1);
        assert_eq!(device.dropped.load(Ordering::Acquire), 1);
        player.shutdown();
    }

    #[test]
    fn invalid_and_empty_tracks_do_not_retry_automatically() {
        let empty = wav(0);
        let device = Arc::new(DeviceState::default());
        let player = Player::new(factory(&device)).unwrap();
        player
            .send(Command::Add(vec![
                empty.0.with_extension("missing"),
                empty.0.clone(),
            ]))
            .unwrap();
        player.send(Command::SetRepeat(RepeatMode::All)).unwrap();
        player.send(Command::Play).unwrap();
        let failed = wait(&player, |snapshot| snapshot.state == PlaybackState::Error);
        assert_eq!(failed.current, Some(0));
        assert_eq!(device.created.load(Ordering::Acquire), 0);
        player.send(Command::Next).unwrap();
        wait(&player, |snapshot| {
            snapshot.current == Some(1) && snapshot.state == PlaybackState::Error
        });
        let count = device.created.load(Ordering::Acquire);
        thread::sleep(Duration::from_millis(50));
        assert_eq!(device.created.load(Ordering::Acquire), count);
        assert_eq!(player.snapshot().current, Some(1));
        player.shutdown();
    }

    #[test]
    fn full_command_queue_rejects_without_waiting_for_blocked_io() {
        use crate::{DecoderFactory, TrackMetadata};
        use std::path::Path;
        struct BlockingFactory {
            entered: Arc<AtomicBool>,
            release: Arc<AtomicBool>,
        }
        impl DecoderFactory for BlockingFactory {
            fn name(&self) -> &str {
                "blocked test decoder"
            }
            fn open(&self, _: &Path) -> Result<Box<dyn AudioDecoder>> {
                self.entered.store(true, Ordering::Release);
                while !self.release.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(1));
                }
                Ok(Box::new(EmptyDecoder(TrackMetadata {
                    title: "test".into(),
                    artist: None,
                    album: None,
                    duration: Some(Duration::ZERO),
                    sample_rate: 8000,
                })))
            }
        }
        struct EmptyDecoder(TrackMetadata);
        impl AudioDecoder for EmptyDecoder {
            fn metadata(&self) -> &TrackMetadata {
                &self.0
            }
            fn read_frames(&mut self, _: usize) -> Result<Vec<Frame>> {
                Ok(Vec::new())
            }
            fn seek(&mut self, _: Duration) -> Result<Duration> {
                Ok(Duration::ZERO)
            }
        }
        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let mut registry = DecoderRegistry::default();
        registry.register(Box::new(BlockingFactory {
            entered: entered.clone(),
            release: release.clone(),
        }));
        let device = Arc::new(DeviceState::default());
        let player = Player::with_registry(factory(&device), registry).unwrap();
        player.send(Command::Add(vec!["ignored".into()])).unwrap();
        player.send(Command::Play).unwrap();
        wait(&player, |_| entered.load(Ordering::Acquire));
        for _ in 0..COMMAND_CAPACITY {
            player.send(Command::SetVolume(0.5)).unwrap();
        }
        let started = Instant::now();
        assert!(player.send(Command::Pause).unwrap_err().contains("busy"));
        assert!(started.elapsed() < Duration::from_millis(100));
        // Set release before joining, because the test deliberately blocks file I/O.
        release.store(true, Ordering::Release);
        player.shutdown();
    }

    #[test]
    fn processor_factory_owns_non_send_lifecycle_on_the_playback_worker() {
        use std::{rc::Rc, thread::ThreadId};

        type Events = Arc<Mutex<Vec<(&'static str, ThreadId)>>>;
        struct ThreadAffineProcessor {
            // Keeping an Rc makes this processor deliberately !Send and !Sync.
            _local: Rc<()>,
            events: Events,
        }
        impl ThreadAffineProcessor {
            fn record(&self, event: &'static str) {
                self.events
                    .lock()
                    .unwrap()
                    .push((event, thread::current().id()));
            }
        }
        impl AudioProcessor for ThreadAffineProcessor {
            fn prepare(&mut self, rate: u32, max_frames: usize) -> Result<()> {
                self.record("prepare");
                assert_eq!(rate, 8000);
                assert_eq!(max_frames, OUTPUT_BLOCK);
                Ok(())
            }
            fn service(&mut self) -> Result<()> {
                self.record("service");
                Ok(())
            }
            fn process(&mut self, frames: &mut [Frame], rate: u32) -> Result<()> {
                self.record("process");
                assert_eq!(rate, 8000);
                for frame in frames {
                    frame[0] *= 0.5;
                    frame[1] *= 0.5;
                }
                Ok(())
            }
            fn reset(&mut self) {
                self.record("reset");
            }
        }
        impl Drop for ThreadAffineProcessor {
            fn drop(&mut self) {
                self.record("drop");
            }
        }

        let track = wav(16_000);
        let device = Arc::new(DeviceState::default());
        let events: Events = Arc::new(Mutex::new(Vec::new()));
        let factory_events = events.clone();
        let processor_factory: ProcessorFactory = Box::new(move || {
            factory_events
                .lock()
                .unwrap()
                .push(("factory", thread::current().id()));
            Ok(Box::new(ThreadAffineProcessor {
                _local: Rc::new(()),
                events: factory_events,
            }))
        });
        let player = Player::with_processor_factories(
            factory(&device),
            DecoderRegistry::default(),
            vec![processor_factory],
        )
        .unwrap();
        // Control work must also run before transport starts.
        wait(&player, |_| {
            events
                .lock()
                .unwrap()
                .iter()
                .any(|(event, _)| *event == "service")
        });
        player.send(Command::Add(vec![track.0.clone()])).unwrap();
        player.send(Command::Play).unwrap();
        wait(&player, |_| device.pending.load(Ordering::Acquire) == 512);
        assert_eq!(device.written.lock().unwrap()[0], [0.1875, 0.1875]);
        player.send(Command::Pause).unwrap();
        wait(&player, |snapshot| snapshot.state == PlaybackState::Paused);
        let service_count = events
            .lock()
            .unwrap()
            .iter()
            .filter(|(event, _)| *event == "service")
            .count();
        wait(&player, |_| {
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|(event, _)| *event == "service")
                .count()
                > service_count
        });
        player.shutdown();

        let events = events.lock().unwrap();
        let worker = events[0].1;
        assert_ne!(worker, thread::current().id());
        assert!(events.iter().all(|(_, owner)| *owner == worker));
        for expected in ["factory", "prepare", "process", "service", "reset", "drop"] {
            assert!(
                events.iter().any(|(event, _)| *event == expected),
                "missing {expected}: {events:?}"
            );
        }
        assert_eq!(events.last().unwrap().0, "drop");
    }

    #[test]
    fn processor_factory_and_lifecycle_failures_reach_the_player_snapshot() {
        struct FailingProcessor(&'static str);
        impl AudioProcessor for FailingProcessor {
            fn prepare(&mut self, _: u32, _: usize) -> Result<()> {
                if self.0 == "prepare" {
                    return Err("Injected prepare failure".into());
                }
                Ok(())
            }
            fn service(&mut self) -> Result<()> {
                if self.0 == "service" {
                    return Err("Injected service failure".into());
                }
                Ok(())
            }
            fn process(&mut self, _: &mut [Frame], _: u32) -> Result<()> {
                if self.0 == "process" {
                    return Err("Injected process failure".into());
                }
                Ok(())
            }
        }

        let track = wav(16_000);
        for stage in ["factory", "prepare", "process", "service"] {
            let device = Arc::new(DeviceState::default());
            let processor_factory: ProcessorFactory = Box::new(move || {
                if stage == "factory" {
                    return Err("Injected factory failure".into());
                }
                Ok(Box::new(FailingProcessor(stage)))
            });
            let player = Player::with_processor_factories(
                factory(&device),
                DecoderRegistry::default(),
                vec![processor_factory],
            )
            .unwrap();
            if stage != "factory" {
                player.send(Command::Add(vec![track.0.clone()])).unwrap();
                player.send(Command::Play).unwrap();
            }
            let failed = wait(&player, |snapshot| snapshot.state == PlaybackState::Error);
            assert!(
                failed
                    .error
                    .unwrap()
                    .contains(&format!("Injected {stage} failure"))
            );
            assert!(device.written.lock().unwrap().is_empty());
            player.shutdown();
            assert_eq!(
                device.created.load(Ordering::Acquire),
                device.dropped.load(Ordering::Acquire)
            );
        }
    }

    #[test]
    fn supplied_library_metadata_survives_plain_wav_decoder_fallback() {
        let file = wav(16_000);
        let supplied = Track {
            path: file.0.clone(),
            title: "Source title, not the cache filename".into(),
            artist: Some("Source artist".into()),
            album: Some("Source album".into()),
            duration: Some(Duration::from_secs(999)),
            media_input: None,
        };
        let device = Arc::new(DeviceState::default());
        let player = Player::new(factory(&device)).unwrap();
        player
            .send(Command::AddTracks(vec![supplied.clone()]))
            .unwrap();
        let queued = wait(&player, |snapshot| snapshot.queue.len() == 1);
        assert_eq!(queued.queue[0], supplied);
        player.send(Command::Play).unwrap();
        let playing = wait(&player, |snapshot| snapshot.state == PlaybackState::Playing);
        assert_eq!(playing.queue[0].path, supplied.path);
        assert_eq!(playing.queue[0].title, supplied.title);
        assert_eq!(playing.queue[0].artist, supplied.artist);
        assert_eq!(playing.queue[0].album, supplied.album);
        // Decoder duration is authoritative even when its textual metadata is absent.
        assert_eq!(playing.queue[0].duration, Some(Duration::from_secs(2)));
        assert_eq!(playing.duration, Some(Duration::from_secs(2)));
        player.shutdown();
    }

    #[test]
    fn atomic_append_and_play_uses_the_worker_queue_after_other_queued_commands() {
        let file = wav(16_000);
        let supplied = |title: &str| Track {
            path: file.0.clone(),
            title: title.into(),
            artist: Some("Library artist".into()),
            album: Some("Library album".into()),
            duration: None,
            media_input: None,
        };
        let device = Arc::new(DeviceState::default());
        let player = Player::new(factory(&device)).unwrap();
        player.send(Command::AddTracksAndPlay(Vec::new())).unwrap();
        player.send(Command::SetVolume(0.5)).unwrap();
        let empty = wait(&player, |snapshot| snapshot.volume == 0.5);
        assert!(empty.queue.is_empty());
        assert_eq!(empty.current, None);
        assert_eq!(empty.state, PlaybackState::Stopped);
        assert_eq!(device.created.load(Ordering::Acquire), 0);

        player.send(Command::Add(vec![file.0.clone()])).unwrap();
        let stale = wait(&player, |snapshot| snapshot.queue.len() == 1);
        // Queue several changes without refreshing the UI's previous snapshot.
        player
            .send(Command::Add(vec![file.0.clone(), file.0.clone()]))
            .unwrap();
        player
            .send(Command::AddTracksAndPlay(vec![
                supplied("First A"),
                supplied("First B"),
            ]))
            .unwrap();
        player.send(Command::Add(vec![file.0.clone()])).unwrap();
        player
            .send(Command::AddTracksAndPlay(vec![
                supplied("Second A"),
                supplied("Second B"),
            ]))
            .unwrap();
        let playing = wait(&player, |snapshot| {
            snapshot.queue.len() == 8
                && snapshot.current == Some(6)
                && snapshot.state == PlaybackState::Playing
        });
        assert_eq!(stale.queue.len(), 1);
        assert_eq!(playing.queue[3].title, "First A");
        assert_eq!(playing.queue[4].title, "First B");
        assert_eq!(playing.queue[6].title, "Second A");
        assert_eq!(playing.queue[7].title, "Second B");
        assert_eq!(playing.queue[6].artist.as_deref(), Some("Library artist"));
        assert_eq!(playing.queue[6].album.as_deref(), Some("Library album"));

        // A pending clear also changes the insertion index before the atomic command.
        player.send(Command::Clear).unwrap();
        player.send(Command::Add(vec![file.0.clone()])).unwrap();
        player
            .send(Command::AddTracksAndPlay(vec![
                supplied("After clear A"),
                supplied("After clear B"),
            ]))
            .unwrap();
        let after_clear = wait(&player, |snapshot| {
            snapshot.queue.len() == 3
                && snapshot.current == Some(1)
                && snapshot.state == PlaybackState::Playing
        });
        assert_eq!(after_clear.queue[1].title, "After clear A");
        assert_eq!(after_clear.queue[2].title, "After clear B");
        assert!(after_clear.error.is_none());
        player.shutdown();
    }

    #[test]
    fn atomic_append_capacity_failure_and_empty_batch_preserve_playback() {
        let file = wav(16_000);
        let supplied = |title: &str| Track {
            path: file.0.clone(),
            title: title.into(),
            artist: None,
            album: None,
            duration: None,
            media_input: None,
        };
        let device = Arc::new(DeviceState::default());
        let player = Player::new(factory(&device)).unwrap();
        player
            .send(Command::AddTracksAndPlay(vec![supplied("Keep playing")]))
            .unwrap();
        wait(&player, |snapshot| {
            snapshot.state == PlaybackState::Playing
                && device.pending.load(Ordering::Acquire) == 512
        });
        player
            .send(Command::AddTracks(vec![
                supplied("Existing filler");
                MAX_QUEUE_TRACKS - 1
            ]))
            .unwrap();
        player
            .send(Command::AddTracksAndPlay(vec![supplied(
                "Must be rejected",
            )]))
            .unwrap();
        let rejected = wait(&player, |snapshot| {
            snapshot.queue.len() == MAX_QUEUE_TRACKS && snapshot.error.is_some()
        });
        assert_eq!(rejected.current, Some(0));
        assert_eq!(rejected.state, PlaybackState::Playing);
        assert_eq!(rejected.queue[0].title, "Keep playing");
        assert!(
            rejected.queue[1..]
                .iter()
                .all(|track| track.title == "Existing filler")
        );
        assert!(
            rejected
                .error
                .as_ref()
                .unwrap()
                .contains(&MAX_QUEUE_TRACKS.to_string())
        );
        assert_eq!(device.created.load(Ordering::Acquire), 1);
        assert_eq!(device.dropped.load(Ordering::Acquire), 0);

        assert!(
            player
                .send(Command::AddTracksAndPlay(vec![
                    supplied("Oversized");
                    MAX_QUEUE_TRACKS + 1
                ]))
                .unwrap_err()
                .contains(&MAX_QUEUE_TRACKS.to_string())
        );
        player.send(Command::AddTracksAndPlay(Vec::new())).unwrap();
        // A following command provides an acknowledgement without a timing sleep.
        player.send(Command::SetVolume(0.33)).unwrap();
        let after_empty = wait(&player, |snapshot| snapshot.volume == 0.33);
        assert_eq!(after_empty.queue, rejected.queue);
        assert_eq!(after_empty.current, rejected.current);
        assert_eq!(after_empty.state, rejected.state);
        assert_eq!(after_empty.error, rejected.error);
        assert_eq!(device.created.load(Ordering::Acquire), 1);
        assert_eq!(device.dropped.load(Ordering::Acquire), 0);
        player.shutdown();
    }

    #[test]
    fn atomic_replace_starts_the_first_track_even_when_the_old_queue_is_full() {
        let file = wav(16_000);
        let supplied = |title: &str| Track {
            path: file.0.clone(),
            title: title.into(),
            artist: Some("Library artist".into()),
            album: Some("Library album".into()),
            duration: None,
            media_input: None,
        };
        let device = Arc::new(DeviceState::default());
        let player = Player::new(factory(&device)).unwrap();
        player
            .send(Command::AddTracks(vec![supplied("Old"); MAX_QUEUE_TRACKS]))
            .unwrap();
        player.send(Command::Select(MAX_QUEUE_TRACKS - 1)).unwrap();
        wait(&player, |snapshot| {
            snapshot.current == Some(MAX_QUEUE_TRACKS - 1)
                && snapshot.state == PlaybackState::Playing
        });
        player.send(Command::SetVolume(0.4)).unwrap();
        player.send(Command::SetRepeat(RepeatMode::All)).unwrap();
        player
            .send(Command::ReplaceTracksAndPlay(vec![
                supplied("Album first"),
                supplied("Album second"),
            ]))
            .unwrap();
        let replaced = wait(&player, |snapshot| {
            snapshot.queue.len() == 2 && snapshot.state == PlaybackState::Playing
        });
        assert_eq!(replaced.current, Some(0));
        assert_eq!(replaced.queue[0].title, "Album first");
        assert_eq!(replaced.queue[1].title, "Album second");
        assert_eq!(replaced.queue[0].artist.as_deref(), Some("Library artist"));
        assert_eq!(replaced.queue[0].album.as_deref(), Some("Library album"));
        assert_eq!(replaced.duration, Some(Duration::from_secs(2)));
        assert_eq!(replaced.position, Duration::ZERO);
        assert_eq!(replaced.volume, 0.4);
        assert_eq!(replaced.repeat, RepeatMode::All);
        assert!(replaced.error.is_none());
        assert_eq!(device.created.load(Ordering::Acquire), 2);
        assert_eq!(device.dropped.load(Ordering::Acquire), 1);
        player.shutdown();
    }

    #[test]
    fn indexed_replacement_opens_only_selected_track_and_rejects_invalid_index() {
        let file = wav(16_000);
        let device = Arc::new(DeviceState::default());
        let player = Player::new(factory(&device)).unwrap();
        let track = Track::from_path(file.0.clone());
        let mut unavailable = track.clone();
        unavailable.path = PathBuf::from("/missing-first.wav");
        player
            .send(Command::ReplaceTracksAndPlayAt {
                tracks: vec![unavailable, track.clone()],
                index: 1,
            })
            .unwrap();
        let before = wait(&player, |snapshot| {
            snapshot.current == Some(1) && snapshot.state == PlaybackState::Playing
        });
        assert_eq!(device.created.load(Ordering::Acquire), 1);
        assert!(
            player
                .send(Command::ReplaceTracksAndPlayAt {
                    tracks: vec![track],
                    index: 1
                })
                .unwrap_err()
                .contains("index")
        );
        player.send(Command::SetVolume(0.42)).unwrap();
        let after = wait(&player, |snapshot| snapshot.volume == 0.42);
        assert_eq!(before.queue, after.queue);
        assert_eq!(after.current, Some(1));
        assert_eq!(after.state, PlaybackState::Playing);
        assert_eq!(device.created.load(Ordering::Acquire), 1);
        player.shutdown();
    }

    #[test]
    fn rejected_replacement_batches_preserve_the_existing_queue_and_session() {
        let file = wav(16_000);
        let track = Track::from_path(file.0.clone());
        let device = Arc::new(DeviceState::default());
        let player = Player::new(factory(&device)).unwrap();
        player
            .send(Command::AddTracksAndPlay(vec![track.clone()]))
            .unwrap();
        let before = wait(&player, |snapshot| snapshot.state == PlaybackState::Playing);
        assert!(
            player
                .send(Command::ReplaceTracksAndPlay(Vec::new()))
                .unwrap_err()
                .contains("empty")
        );
        assert!(
            player
                .send(Command::ReplaceTracksAndPlay(vec![
                    track;
                    MAX_QUEUE_TRACKS + 1
                ]))
                .unwrap_err()
                .contains(&MAX_QUEUE_TRACKS.to_string())
        );
        // A following command acknowledges worker progress without a timing sleep.
        player.send(Command::SetVolume(0.42)).unwrap();
        let after = wait(&player, |snapshot| snapshot.volume == 0.42);
        assert_eq!(after.queue, before.queue);
        assert_eq!(after.current, before.current);
        assert_eq!(after.state, before.state);
        assert_eq!(after.duration, before.duration);
        assert_eq!(after.error, before.error);
        assert_eq!(device.created.load(Ordering::Acquire), 1);
        assert_eq!(device.dropped.load(Ordering::Acquire), 0);
        player.shutdown();
    }

    #[test]
    fn worker_revalidates_replacement_before_changing_the_queue() {
        let file = wav(16_000);
        let track = Track::from_path(file.0.clone());
        let device = Arc::new(DeviceState::default());
        let mut worker = Worker::new(
            factory(&device),
            DecoderRegistry::default(),
            Vec::new(),
            Arc::new(Mutex::new(Snapshot::default())),
            Arc::new(Mutex::new(InputControl::default())),
        );
        worker.command(Command::AddTracksAndPlay(vec![track.clone()]));
        let before = worker.data.clone();
        for (batch, expected_error) in [
            (Vec::new(), "empty"),
            (vec![track; MAX_QUEUE_TRACKS + 1], "16384"),
        ] {
            worker.command(Command::ReplaceTracksAndPlay(batch));
            assert_eq!(worker.data.queue, before.queue);
            assert_eq!(worker.data.current, before.current);
            assert_eq!(worker.data.state, PlaybackState::Playing);
            assert_eq!(worker.data.duration, before.duration);
            assert!(worker.data.error.as_ref().unwrap().contains(expected_error));
            assert!(worker.session.is_some());
            assert_eq!(device.created.load(Ordering::Acquire), 1);
            assert_eq!(device.dropped.load(Ordering::Acquire), 0);
        }
    }
}

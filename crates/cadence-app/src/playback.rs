//! Playback presentation state. Position ticks belong to the transport, not the app shell.
use crate::{state, ui::clock};
use cadence_core::{Command, PlaybackState, Player, Snapshot};
use scarlet_ui::prelude::*;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct QueueState {
    pub tracks: State<Vec<cadence_core::Track>>,
    pub current: State<Option<usize>>,
    pub selected: State<Option<usize>>,
}
#[derive(Clone)]
pub struct TransportState {
    pub revision: State<u64>,
    pub position: State<f32>,
    pub time: State<String>,
    pub volume: State<f32>,
    pub volume_text: State<String>,
    pub scrubbing: State<bool>,
    pub seek_pending: Rc<Cell<Option<f32>>>,
    pub seek_hold_until: Rc<Cell<Instant>>,
}
#[derive(Clone)]
pub struct Playback {
    pub player: Rc<Player>,
    pub snapshot: Rc<RefCell<Snapshot>>,
    pub status: State<String>,
    pub queue: QueueState,
    pub transport: TransportState,
    pub last_poll: Rc<Cell<Instant>>,
}
impl Playback {
    pub fn new(player: Player) -> Self {
        let snapshot = player.snapshot();
        Self {
            player: Rc::new(player),
            snapshot: Rc::new(RefCell::new(snapshot)),
            status: state("Ready".into()),
            queue: QueueState {
                tracks: state(vec![]),
                current: state(None),
                selected: state(None),
            },
            transport: TransportState {
                revision: state(0),
                position: state(0.),
                time: state("0:00 / —:——".into()),
                volume: state(0.8),
                volume_text: state("80%".into()),
                scrubbing: state(false),
                seek_pending: Rc::new(Cell::new(None)),
                seek_hold_until: Rc::new(Cell::new(Instant::now())),
            },
            last_poll: Rc::new(Cell::new(Instant::now())),
        }
    }
    pub fn send(&self, command: Command) {
        if let Err(error) = self.player.send(command) {
            self.status.set(error);
        }
    }
    pub fn select_queue_row(&self, index: usize) {
        self.queue.selected.set(Some(index));
        self.send(Command::Select(index));
    }
    pub fn poll(&self) {
        if !self.transport.scrubbing.get()
            && let Some(seconds) = self.transport.seek_pending.take()
        {
            self.send(Command::Seek(Duration::from_secs_f32(seconds.max(0.))));
            self.transport
                .seek_hold_until
                .set(Instant::now() + Duration::from_millis(250));
        }
        if self.last_poll.get().elapsed() < Duration::from_millis(100) {
            return;
        }
        self.last_poll.set(Instant::now());
        let next = self.player.snapshot();
        let previous = self.snapshot.borrow();
        let structural = next.queue != previous.queue
            || next.current != previous.current
            || next.state != previous.state
            || next.repeat != previous.repeat
            || next.seekable != previous.seekable
            || next.duration != previous.duration;
        if next.queue != previous.queue {
            self.queue.tracks.set(next.queue.clone());
            self.queue.selected.set(None);
        }
        if next.current != previous.current {
            self.queue.current.set(next.current);
            self.queue.selected.set(None);
        }
        if next.error != previous.error {
            self.status
                .set(next.error.clone().unwrap_or_else(|| "Ready".into()));
        } else if next.state != previous.state && next.error.is_none() {
            self.status.set(
                match next.state {
                    PlaybackState::Playing => "Playing",
                    PlaybackState::Paused => "Paused",
                    PlaybackState::Loading => "Opening audio…",
                    PlaybackState::Stopped => "Ready",
                    PlaybackState::Error => "Playback error",
                }
                .into(),
            );
        }
        let next_position = next.position.as_secs_f32();
        // ScarletUI State::set notifies even when the value is unchanged.
        // An idle transport must not invalidate the retained window every poll.
        if !self.transport.scrubbing.get()
            && Instant::now() >= self.transport.seek_hold_until.get()
            && self.transport.position.get() != next_position
        {
            self.transport.position.set(next_position);
        }
        let display_position = if self.transport.scrubbing.get() {
            Duration::from_secs_f32(self.transport.position.get().max(0.))
        } else {
            next.position
        };
        let next_time = format!(
            "{} / {}",
            clock(display_position),
            next.duration.map(clock).unwrap_or_else(|| "—:——".into())
        );
        if self.transport.time.get() != next_time {
            self.transport.time.set(next_time);
        }
        drop(previous);
        *self.snapshot.borrow_mut() = next;
        if structural {
            self.transport
                .revision
                .set(self.transport.revision.get().wrapping_add(1));
        }
    }
}

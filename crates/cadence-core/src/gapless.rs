//! One bounded background decoder opener. Output and processors stay on playback.
use crate::{AudioDecoder, DecoderRegistry, Frame, Result, Track};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, SyncSender, TryRecvError},
};

pub(super) struct Prepared {
    pub decoder: Box<dyn AudioDecoder>,
    pub first: Vec<Frame>,
    pub cancelled: Arc<AtomicBool>,
}
struct Request {
    track: Track,
    output_rate: u32,
    cancelled: Arc<AtomicBool>,
    result: SyncSender<Result<Prepared>>,
}
struct Pending {
    index: usize,
    track: Track,
    cancelled: Arc<AtomicBool>,
    receiver: Receiver<Result<Prepared>>,
    ready: Option<Result<Prepared>>,
}
pub(super) struct Prefetcher {
    registry: Arc<DecoderRegistry>,
    sender: Option<SyncSender<Request>>,
    pending: Option<Pending>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Prefetcher {
    pub fn new(registry: Arc<DecoderRegistry>) -> Self {
        Self {
            registry,
            sender: None,
            pending: None,
            worker: None,
        }
    }
    pub fn cancel(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.cancelled.store(true, Ordering::Release);
        }
    }
    pub fn request(
        &mut self,
        index: usize,
        track: &Track,
        output_rate: u32,
    ) -> Result<Option<Arc<AtomicBool>>> {
        if self
            .pending
            .as_ref()
            .is_some_and(|job| job.index == index && job.track == *track)
        {
            return Ok(None);
        }
        self.cancel();
        if self.sender.is_none() {
            let (sender, receiver) = mpsc::sync_channel::<Request>(1);
            let registry = self.registry.clone();
            self.worker = Some(
                std::thread::Builder::new()
                    .name("cadence-prefetch".into())
                    .spawn(move || {
                        while let Ok(request) = receiver.recv() {
                            let result = (|| {
                                if request.cancelled.load(Ordering::Acquire) {
                                    return Err("Prefetch was cancelled".into());
                                }
                                let mut decoder = registry
                                    .open_track(&request.track, request.cancelled.clone())?;
                                let input_rate = decoder.metadata().sample_rate;
                                if !(1000..=384_000).contains(&input_rate) {
                                    return Err(format!(
                                        "Unsupported input sample rate: {input_rate} Hz"
                                    ));
                                }
                                let count = (2048u64 * input_rate as u64
                                    / request.output_rate as u64)
                                    .clamp(1, 2048)
                                    as usize;
                                let first = decoder.read_frames(count)?;
                                if request.cancelled.load(Ordering::Acquire) {
                                    return Err("Prefetch was cancelled".into());
                                }
                                if first.is_empty() {
                                    return Err("Track contains no decodable audio".into());
                                }
                                Ok(Prepared {
                                    decoder,
                                    first,
                                    cancelled: request.cancelled.clone(),
                                })
                            })();
                            let _ = request.result.send(result);
                        }
                    })
                    .map_err(|error| format!("Cannot start decoder prefetch: {error}"))?,
            );
            self.sender = Some(sender);
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let (result, receiver) = mpsc::sync_channel(1);
        let request = Request {
            track: track.clone(),
            output_rate,
            cancelled: cancelled.clone(),
            result,
        };
        match self.sender.as_ref().unwrap().try_send(request) {
            Ok(()) => {
                self.pending = Some(Pending {
                    index,
                    track: track.clone(),
                    cancelled: cancelled.clone(),
                    receiver,
                    ready: None,
                });
                Ok(Some(cancelled))
            }
            // A cancelled network open can still be returning. Never spawn an
            // additional worker or block current playback waiting for it.
            Err(mpsc::TrySendError::Full(_)) => Ok(None),
            Err(mpsc::TrySendError::Disconnected(_)) => Err("Decoder prefetch stopped".into()),
        }
    }
    pub fn take_ready(&mut self, index: usize) -> Option<Result<Prepared>> {
        let pending = self.pending.as_mut()?;
        if pending.index != index {
            return None;
        }
        if pending.ready.is_none() {
            match pending.receiver.try_recv() {
                Ok(result) => pending.ready = Some(result),
                Err(TryRecvError::Empty) => return None,
                Err(TryRecvError::Disconnected) => {
                    pending.ready = Some(Err("Decoder prefetch stopped".into()))
                }
            }
        }
        // Preserve an error until the current device tail has drained.
        if let Some(Err(error)) = &pending.ready {
            return Some(Err(error.clone()));
        }
        pending.ready.take()
    }
    pub fn restore(&mut self, prepared: Prepared) {
        if let Some(pending) = &mut self.pending {
            pending.ready = Some(Ok(prepared));
        }
    }
    pub fn finish(&mut self) {
        self.pending.take();
    }
}
impl Drop for Prefetcher {
    fn drop(&mut self) {
        self.cancel();
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

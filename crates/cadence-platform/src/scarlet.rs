//! Scarlet Audio Server connection, exclusively owned by the playback worker.
use crate::{drain::TailHold, pcm::s16le};
use cadence_core::output::{AudioOutput, Frame, Result};
use sas_client::{SasClient, SasStream, StreamConfig};
use std::time::{Duration, Instant};

const SAMPLE_RATE: u32 = 48_000;
const PERIOD_FRAMES: usize = 256;
const FRAME_BYTES: usize = 4;
const CONFIGURE_TIMEOUT: Duration = Duration::from_secs(3);
// Pinned Scarlet b3d2a557, user/bin/src/sas/main.rs: OUTPUT_RATE=48000,
// OUTPUT_PERIOD_FRAMES=960 and OUTPUT_BUFFER_FRAMES=9600. Both pinned output
// backends (virtio-snd / Chromebook LPASS) accept those parameters unchanged.
// SAS publishes client read_frames before hardware commit, and disconnecting
// its last client stops/clears the device. With no hardware drain ACK in this
// protocol, hold the connection for one device buffer plus two periods after
// first observing the client ring empty. This conservative 240ms bound must be
// revisited if SAS configuration/caps change; it is not a gapless-playback API.
const SAS_OUTPUT_PERIOD_FRAMES: u64 = 960;
const SAS_OUTPUT_BUFFER_FRAMES: u64 = 9_600;
const HARDWARE_TAIL: Duration = Duration::from_nanos(
    (SAS_OUTPUT_BUFFER_FRAMES + 2 * SAS_OUTPUT_PERIOD_FRAMES) * 1_000_000_000 / SAMPLE_RATE as u64,
);

pub struct Output {
    // Disconnect the producer before unmapping its view of the shared ring.
    client: Option<SasClient>,
    stream: SasStream,
    bytes: [u8; PERIOD_FRAMES * FRAME_BYTES],
    draining: bool,
    tail: TailHold,
}

impl Output {
    pub fn open() -> Result<Self> {
        let mut client = SasClient::connect().map_err(|error| {
            format!(
                "Scarlet audio: {}. Check that SAS is running.",
                error.as_str()
            )
        })?;
        let started = Instant::now();
        let stream = client
            .configure_cancellable(
                &StreamConfig {
                    format: 1, // Scarlet native PCM ABI: S16LE.
                    rate: SAMPLE_RATE,
                    channels: 2,
                    period_frames: PERIOD_FRAMES as u32,
                    buffer_frames: (PERIOD_FRAMES * 4) as u32,
                },
                || started.elapsed() >= CONFIGURE_TIMEOUT,
            )
            .map_err(|error| format!("Scarlet audio configuration: {}", error.as_str()))?
            .ok_or("Scarlet audio configuration timed out after 3 seconds.")?;
        Ok(Self {
            client: Some(client),
            stream,
            bytes: [0; PERIOD_FRAMES * FRAME_BYTES],
            draining: false,
            tail: TailHold::new(HARDWARE_TAIL),
        })
    }
}

impl AudioOutput for Output {
    fn sample_rate(&self) -> u32 {
        SAMPLE_RATE
    }

    fn write(&mut self, frames: &[Frame]) -> Result<usize> {
        self.check_error()?;
        if self.draining {
            return Err("Cannot write audio after starting the Scarlet stream drain.".into());
        }
        let count = frames
            .len()
            .min(PERIOD_FRAMES)
            .min(self.stream.writable_frames());
        for (destination, frame) in self.bytes[..count * FRAME_BYTES]
            .chunks_exact_mut(FRAME_BYTES)
            .zip(&frames[..count])
        {
            destination[..2].copy_from_slice(&s16le(frame[0]));
            destination[2..].copy_from_slice(&s16le(frame[1]));
        }
        // SAS returns frames, not bytes. The caller retains any unaccepted tail.
        Ok(self.stream.write(&self.bytes[..count * FRAME_BYTES]))
    }

    fn pending_frames(&self) -> usize {
        self.stream
            .write_frames()
            .saturating_sub(self.stream.read_frames()) as usize
    }

    fn begin_drain(&mut self) -> Result<()> {
        self.check_error()?;
        if !self.draining {
            // This flag allows SAS to consume a final partial period. Never use
            // the blocking control RPC: the worker must keep handling commands.
            self.stream.begin_drain();
            self.draining = true;
        }
        Ok(())
    }

    fn is_drained(&self) -> bool {
        self.draining
            && self
                .tail
                .finished_at(Instant::now(), self.pending_frames() == 0)
    }

    fn check_error(&self) -> Result<()> {
        if self.stream.is_closed() {
            Err("Scarlet Audio Server closed the stream. Check SAS and try playback again.".into())
        } else {
            Ok(())
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        // Closing the socket removes the stream from SAS. Avoid a blocking
        // close/drain RPC, including during pause, seek, errors, and shutdown.
        drop(self.client.take());
    }
}

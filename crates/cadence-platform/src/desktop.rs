//! CPAL owns the native device; its callback only consumes bounded PCM.
use crate::{pcm::normalized, ring};
use cadence_core::output::{AudioOutput, Frame, Result};
use cpal::{
    FromSample, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

struct CallbackState {
    failed: AtomicBool,
    active: AtomicBool,
    audible_until_nanos: AtomicU64,
    epoch: Instant,
}

impl CallbackState {
    fn new() -> Self {
        Self {
            failed: AtomicBool::new(false),
            active: AtomicBool::new(false),
            audible_until_nanos: AtomicU64::new(0),
            epoch: Instant::now(),
        }
    }

    fn tail_finished_at(&self, elapsed: Duration) -> bool {
        !self.active.load(Ordering::Acquire)
            && elapsed.as_nanos() >= self.audible_until_nanos.load(Ordering::Acquire) as u128
    }

    fn record_tail(&self, elapsed: Duration, latency: Duration, frames: usize, rate: u32) {
        if frames != 0 {
            let duration = Duration::from_secs_f64(frames as f64 / rate as f64);
            let deadline = elapsed.saturating_add(latency).saturating_add(duration);
            self.audible_until_nanos.fetch_max(
                deadline.as_nanos().min(u64::MAX as u128) as u64,
                Ordering::Release,
            );
        }
    }
}

pub struct Output {
    stream: Option<cpal::Stream>,
    producer: ring::Producer,
    callback: Arc<CallbackState>,
    sample_rate: u32,
}

impl Output {
    pub fn open() -> Result<Self> {
        let device = cpal::default_host()
            .default_output_device()
            .ok_or("No audio output device is available.")?;
        let supported = device
            .default_output_config()
            .map_err(|error| format!("Cannot read audio output configuration: {error}"))?;
        let config: cpal::StreamConfig = supported.clone().into();
        if config.channels == 0 || config.sample_rate.0 == 0 {
            return Err("The output device reported an invalid audio configuration.".into());
        }
        // Around 85–185 ms at common rates, with a fixed upper memory bound.
        let capacity =
            ((config.sample_rate.0 as usize / 10).clamp(512, 16_384)).next_power_of_two();
        let (producer, consumer) = ring::new(capacity);
        let callback = Arc::new(CallbackState::new());
        let stream = match supported.sample_format() {
            cpal::SampleFormat::I8 => build::<i8>(&device, &config, consumer, callback.clone()),
            cpal::SampleFormat::I16 => build::<i16>(&device, &config, consumer, callback.clone()),
            cpal::SampleFormat::I32 => build::<i32>(&device, &config, consumer, callback.clone()),
            cpal::SampleFormat::I64 => build::<i64>(&device, &config, consumer, callback.clone()),
            cpal::SampleFormat::U8 => build::<u8>(&device, &config, consumer, callback.clone()),
            cpal::SampleFormat::U16 => build::<u16>(&device, &config, consumer, callback.clone()),
            cpal::SampleFormat::U32 => build::<u32>(&device, &config, consumer, callback.clone()),
            cpal::SampleFormat::U64 => build::<u64>(&device, &config, consumer, callback.clone()),
            cpal::SampleFormat::F32 => build::<f32>(&device, &config, consumer, callback.clone()),
            cpal::SampleFormat::F64 => build::<f64>(&device, &config, consumer, callback.clone()),
            format => return Err(format!("Unsupported output sample format: {format}")),
        }?;
        stream
            .play()
            .map_err(|error| format!("Cannot start audio output: {error}"))?;
        Ok(Self {
            stream: Some(stream),
            producer,
            callback,
            sample_rate: config.sample_rate.0,
        })
    }
}

impl AudioOutput for Output {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn write(&mut self, frames: &[Frame]) -> Result<usize> {
        self.check_error()?;
        Ok(self.producer.push(frames))
    }

    fn pending_frames(&self) -> usize {
        self.producer.pending()
    }

    fn is_drained(&self) -> bool {
        // Read ring progress before active: observing the callback's last pop
        // also observes its earlier active=true publication. Thus an EOF check
        // cannot race the callback between removing PCM and recording its tail.
        self.producer.pending() == 0
            && self
                .callback
                .tail_finished_at(self.callback.epoch.elapsed())
    }

    fn check_error(&self) -> Result<()> {
        if self.callback.failed.load(Ordering::Acquire) {
            Err("Audio output failed or the device disconnected. Try playback again.".into())
        } else {
            Ok(())
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        // Stop callback execution before releasing the producer's ring handle.
        drop(self.stream.take());
    }
}

fn build<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut consumer: ring::Consumer,
    callback: Arc<CallbackState>,
) -> Result<cpal::Stream> {
    let channels = config.channels as usize;
    let rate = config.sample_rate.0;
    let errors = callback.clone();
    device
        .build_output_stream(
            config,
            move |data: &mut [T], info| {
                callback.active.store(true, Ordering::Release);
                let elapsed = callback.epoch.elapsed();
                let last_audio_frame = render(data, channels, &mut consumer);
                let timestamp = info.timestamp();
                let latency = timestamp
                    .playback
                    .duration_since(&timestamp.callback)
                    .unwrap_or_default();
                callback.record_tail(elapsed, latency, last_audio_frame, rate);
                callback.active.store(false, Ordering::Release);
            },
            move |_| errors.failed.store(true, Ordering::Release),
            None,
        )
        .map_err(|error| format!("Cannot create audio output stream: {error}"))
}

fn render<T: SizedSample + FromSample<f32>>(
    output: &mut [T],
    channels: usize,
    consumer: &mut ring::Consumer,
) -> usize {
    output.fill(T::EQUILIBRIUM);
    if channels == 0 {
        return 0;
    }
    let mut last_audio_frame = 0;
    for (index, destination) in output.chunks_exact_mut(channels).enumerate() {
        let Some(frame) = consumer.pop() else {
            continue;
        };
        last_audio_frame = index + 1;
        let [left, right] = frame.map(normalized);
        if channels == 1 {
            destination[0] = T::from_sample((left + right) * 0.5);
        } else {
            destination[0] = T::from_sample(left);
            destination[1] = T::from_sample(right);
            // Surround channels stay silent; no accidental LFE duplication.
        }
    }
    last_audio_frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_channels_and_underflow_silence() {
        let (mut producer, mut consumer) = ring::new(2);
        producer.push(&[[0.5, -0.25]]);
        let mut out = [99.0f32; 4];
        render(&mut out, 2, &mut consumer);
        assert_eq!(out, [0.5, -0.25, 0.0, 0.0]);
        assert_eq!(producer.pending(), 0);
    }

    #[test]
    fn mono_downmix_and_surround_padding() {
        let (mut producer, mut consumer) = ring::new(2);
        producer.push(&[[1.0, -0.5], [0.5, -0.25]]);
        let mut mono = [0.0f32];
        render(&mut mono, 1, &mut consumer);
        assert_eq!(mono, [0.25]);
        let mut surround = [99.0f64; 6];
        render(&mut surround, 6, &mut consumer);
        assert_eq!(surround, [0.5, -0.25, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn integer_full_scale_and_unsigned_silence() {
        let (mut producer, mut consumer) = ring::new(2);
        producer.push(&[[-2.0, 2.0]]);
        let mut signed = [0i16; 2];
        render(&mut signed, 2, &mut consumer);
        assert_eq!(signed, [i16::MIN, i16::MAX]);
        let mut unsigned = [0u16; 2];
        render(&mut unsigned, 2, &mut consumer);
        assert_eq!(unsigned, [32768, 32768]);
    }

    #[test]
    fn malformed_audio_and_partial_device_frames_stay_silent() {
        let (mut producer, mut consumer) = ring::new(2);
        producer.push(&[[f32::NAN, f32::INFINITY], [0.5, 0.5]]);
        let mut out = [99.0f32; 3];
        render(&mut out, 2, &mut consumer);
        assert_eq!(out, [0.0; 3]);
        assert_eq!(producer.pending(), 1);
    }

    #[test]
    fn draining_waits_for_device_latency_and_last_audio_frame() {
        let callback = CallbackState::new();
        callback.record_tail(
            Duration::from_millis(100),
            Duration::from_millis(20),
            480,
            48_000,
        );
        assert!(!callback.tail_finished_at(Duration::from_millis(129)));
        assert!(callback.tail_finished_at(Duration::from_millis(130)));
        // Silent callbacks must not perpetually move the completion deadline.
        callback.record_tail(
            Duration::from_millis(140),
            Duration::from_millis(20),
            0,
            48_000,
        );
        assert!(callback.tail_finished_at(Duration::from_millis(140)));
        // A callback that just popped the final frame has not yet published
        // that frame's deadline. Never signal completion inside that window.
        callback.active.store(true, Ordering::Release);
        assert!(!callback.tail_finished_at(Duration::from_millis(200)));
    }
}

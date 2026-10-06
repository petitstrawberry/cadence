//! Bounded single-producer/single-consumer stereo ring, without unsafe code.
//! Atomic sample bits avoid interior-mutable storage or unchecked Send impls.
//! The two unique handles enforce the SPSC ownership contract.

use cadence_core::output::Frame;
use std::sync::{
    Arc,
    atomic::{AtomicU32, AtomicUsize, Ordering},
};

struct Ring {
    frames: Box<[[AtomicU32; 2]]>,
    read: AtomicUsize,
    write: AtomicUsize,
}

pub struct Producer(Arc<Ring>);
pub struct Consumer(Arc<Ring>);

pub fn new(capacity: usize) -> (Producer, Consumer) {
    // A power of two means index wrap at usize::MAX preserves slot order.
    assert!(capacity.is_power_of_two());
    let ring = Arc::new(Ring {
        frames: (0..capacity)
            .map(|_| [AtomicU32::new(0), AtomicU32::new(0)])
            .collect(),
        read: AtomicUsize::new(0),
        write: AtomicUsize::new(0),
    });
    (Producer(ring.clone()), Consumer(ring))
}

impl Producer {
    pub fn push(&mut self, frames: &[Frame]) -> usize {
        let ring = &self.0;
        let write = ring.write.load(Ordering::Relaxed);
        // Acquire pairs with the consumer's release after its sample reads.
        let read = ring.read.load(Ordering::Acquire);
        let capacity = ring.frames.len();
        let count = frames.len().min(capacity - write.wrapping_sub(read));
        for (offset, frame) in frames[..count].iter().enumerate() {
            let slot = &ring.frames[write.wrapping_add(offset) & (capacity - 1)];
            slot[0].store(frame[0].to_bits(), Ordering::Relaxed);
            slot[1].store(frame[1].to_bits(), Ordering::Relaxed);
        }
        // Publish complete stereo frames only, never one channel at a time.
        ring.write
            .store(write.wrapping_add(count), Ordering::Release);
        count
    }

    pub fn pending(&self) -> usize {
        let write = self.0.write.load(Ordering::Relaxed);
        let read = self.0.read.load(Ordering::Acquire);
        write.wrapping_sub(read)
    }
}

impl Consumer {
    pub fn pop(&mut self) -> Option<Frame> {
        let ring = &self.0;
        let read = ring.read.load(Ordering::Relaxed);
        // Acquire makes the producer's sample writes visible before reading.
        if read == ring.write.load(Ordering::Acquire) {
            return None;
        }
        let slot = &ring.frames[read & (ring.frames.len() - 1)];
        let frame = [
            f32::from_bits(slot[0].load(Ordering::Relaxed)),
            f32::from_bits(slot[1].load(Ordering::Relaxed)),
        ];
        ring.read.store(read.wrapping_add(1), Ordering::Release);
        Some(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_writes_and_wrap_preserve_the_exact_suffix() {
        let (mut producer, mut consumer) = new(4);
        let frames = [[1.0, -1.0], [2.0, -2.0], [3.0, -3.0]];
        assert_eq!(producer.push(&frames), 3);
        assert_eq!(producer.push(&frames), 1);
        assert_eq!(producer.pending(), 4);
        assert_eq!(producer.push(&frames), 0);
        assert_eq!(consumer.pop(), Some(frames[0]));
        assert_eq!(consumer.pop(), Some(frames[1]));
        assert_eq!(producer.push(&frames[1..]), 2);
        for expected in [frames[2], frames[0], frames[1], frames[2]] {
            assert_eq!(consumer.pop(), Some(expected));
        }
        assert_eq!(consumer.pop(), None);
        assert_eq!(producer.pending(), 0);
    }

    #[test]
    fn monotonic_counters_wrap_without_losing_frames() {
        let (mut producer, mut consumer) = new(4);
        producer.0.read.store(usize::MAX - 1, Ordering::Relaxed);
        producer.0.write.store(usize::MAX - 1, Ordering::Relaxed);
        let frames = [[0.0; 2], [1.0; 2], [2.0; 2], [3.0; 2]];
        assert_eq!(producer.push(&frames), 4);
        assert_eq!(producer.pending(), 4);
        for frame in frames {
            assert_eq!(consumer.pop(), Some(frame));
        }
        assert_eq!(producer.pending(), 0);
    }

    #[test]
    fn concurrent_producer_and_consumer_do_not_tear_stereo_frames() {
        let (mut producer, mut consumer) = new(16);
        let writer = std::thread::spawn(move || {
            for index in 0..100_000 {
                let frame = [index as f32, -(index as f32)];
                while producer.push(&[frame]) == 0 {
                    std::thread::yield_now();
                }
            }
        });
        for index in 0..100_000 {
            let frame = loop {
                if let Some(frame) = consumer.pop() {
                    break frame;
                }
                std::thread::yield_now();
            };
            assert_eq!(frame, [index as f32, -(index as f32)]);
        }
        writer.join().unwrap();
        assert_eq!(consumer.pop(), None);
    }
}

//! Output adapters are created and owned on the playback worker.
//! A drop discards pending audio; only the playlist end drains and drops the adapter.
pub type Result<T> = std::result::Result<T, String>;
pub type Frame = [f32; 2];

pub trait AudioOutput {
    fn sample_rate(&self) -> u32;
    /// Accept a prefix, never block awaiting device capacity. Return zero when full.
    fn write(&mut self, frames: &[Frame]) -> Result<usize>;
    /// Submit any final partial period once EOF has been accepted.
    fn begin_drain(&mut self) -> Result<()> {
        Ok(())
    }
    /// Frames still awaiting consumption (position excludes hardware latency).
    fn pending_frames(&self) -> usize;
    /// Whether the EOF tail has finished playing, including any known hardware delay.
    fn is_drained(&self) -> bool {
        self.pending_frames() == 0
    }
    fn check_error(&self) -> Result<()>;
}

pub type OutputFactory = Box<dyn Fn() -> Result<Box<dyn AudioOutput>> + Send>;

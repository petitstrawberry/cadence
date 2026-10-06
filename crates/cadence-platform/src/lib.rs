//! Native platform adapters. Audio outputs belong to the playback worker;
//! folder selection belongs to the UI thread.

pub mod folder_picker;
mod pcm;

#[cfg(any(target_os = "scarlet", test))]
mod drain;

#[cfg(not(target_os = "scarlet"))]
mod desktop;
#[cfg(not(target_os = "scarlet"))]
mod ring;
#[cfg(target_os = "scarlet")]
mod scarlet;

use cadence_core::output::OutputFactory;

pub fn output_factory() -> OutputFactory {
    #[cfg(target_os = "scarlet")]
    return Box::new(|| scarlet::Output::open().map(|output| Box::new(output) as _));
    #[cfg(not(target_os = "scarlet"))]
    return Box::new(|| desktop::Output::open().map(|output| Box::new(output) as _));
}

pub const fn backend_name() -> &'static str {
    #[cfg(target_os = "scarlet")]
    return "Scarlet Audio Server";
    #[cfg(not(target_os = "scarlet"))]
    return "CPAL";
}

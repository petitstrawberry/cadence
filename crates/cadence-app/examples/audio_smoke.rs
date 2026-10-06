//! Exercise the real output device. Use a short silent WAV for unattended checks.
use cadence_core::{Command, PlaybackState, Player};
use std::{
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

fn main() -> Result<(), String> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("Pass a short audio file (less than 10 seconds)")?;
    let player = Player::new(cadence_platform::output_factory())?;
    player.send(Command::Add(vec![path]))?;
    player.send(Command::Play)?;
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut advanced = false;
    loop {
        let snapshot = player.snapshot();
        if let Some(error) = snapshot.error {
            return Err(error);
        }
        advanced |= snapshot.position > Duration::from_millis(50);
        if advanced && snapshot.state == PlaybackState::Stopped {
            println!(
                "{}: playback advanced and drained at {:.3} seconds",
                cadence_platform::backend_name(),
                snapshot.position.as_secs_f64()
            );
            player.shutdown();
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("Output did not finish: {snapshot:?}"));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

//! CLAP effects using Resonara's native host. All plug-in code, including loading,
//! control and destruction of instances runs on the owning playback worker.
//! Installed-library descriptor discovery uses a separate catalog worker. The UI exchanges
//! bounded commands and owned snapshots only. Native effects are trusted code.
//!
//! The initial host supports stereo float32 effects with zero reported latency,
//! opaque state and generic parameters. Native editors, instruments, automation,
//! EOF tail extension and out-of-process isolation are not provided.
mod catalog;

use cadence_core::{AudioProcessor, Frame, ProcessorFactory, Result};
use resonara_clap::{HostPlugin, PluginOwner, RealtimePlugin};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
};

pub const MAX_EFFECTS: usize = 32;
pub const MAX_EFFECT_STATE_BYTES: usize = 1024 * 1024;
pub const MAX_RACK_STATE_BYTES: usize = 8 * 1024 * 1024;
const COMMAND_CAPACITY: usize = 64;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RackConfig {
    pub effects: Vec<EffectConfig>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectConfig {
    pub path: PathBuf,
    pub plugin_id: String,
    pub bypass: bool,
    pub state: Vec<u8>,
}

impl RackConfig {
    /// Validate configuration bounds before opening any native binary.
    pub fn validate(&self) -> Result<()> {
        if self.effects.len() > MAX_EFFECTS {
            return Err(format!("At most {MAX_EFFECTS} effects can be loaded"));
        }
        let mut bytes = 0usize;
        for effect in &self.effects {
            if effect.plugin_id.is_empty() || effect.path.as_os_str().is_empty() {
                return Err("Effect path and plug-in ID must be present".into());
            }
            if effect.state.len() > MAX_EFFECT_STATE_BYTES {
                return Err("An effect state exceeds 1 MiB".into());
            }
            bytes = bytes.saturating_add(effect.state.len());
        }
        if bytes > MAX_RACK_STATE_BYTES {
            return Err("Combined effect state exceeds 8 MiB".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParameterSnapshot {
    pub id: u32,
    pub name: String,
    pub module: String,
    pub value: f64,
    pub min: f64,
    pub max: f64,
    pub stepped: bool,
    pub read_only: bool,
    pub hidden: bool,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EffectSnapshot {
    pub name: String,
    pub vendor: String,
    pub bypass: bool,
    pub parameters: Vec<ParameterSnapshot>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DiscoveredPlugin {
    pub path: PathBuf,
    pub id: String,
    pub name: String,
    pub vendor: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RackSnapshot {
    pub effects: Vec<EffectSnapshot>,
    pub discovered: Vec<DiscoveredPlugin>,
    pub config: RackConfig,
    /// False after a failed restore until an explicit rack edit succeeds.
    /// Descriptor discovery must not turn an empty fallback into saved state.
    pub config_persistable: bool,
    pub error: Option<String>,
    pub revision: u64,
    /// A command is waiting for or being handled by the playback worker.
    pub pending: bool,
}

#[derive(Clone)]
pub struct EffectController {
    sender: SyncSender<Command>,
    shared: Arc<Mutex<RackSnapshot>>,
    pending: Arc<AtomicUsize>,
}

impl EffectController {
    fn send(&self, command: Command) -> Result<()> {
        self.pending.fetch_add(1, Ordering::Release);
        if let Err(error) = self.sender.try_send(command) {
            self.pending.fetch_sub(1, Ordering::Release);
            return Err(match error {
                mpsc::TrySendError::Full(_) => "Effect rack is busy; try again".into(),
                mpsc::TrySendError::Disconnected(_) => "Effect worker has stopped".into(),
            });
        }
        Ok(())
    }
    /// Inspect an explicitly selected native library or macOS .clap bundle.
    pub fn discover(&self, path: PathBuf) -> Result<()> {
        self.send(Command::Discover(path))
    }
    pub fn scan_installed(&self) -> Result<()> {
        self.send(Command::ScanInstalled(catalog::roots()))
    }
    pub fn add(&self, path: PathBuf, plugin_id: Option<String>) -> Result<()> {
        self.send(Command::Add(path, plugin_id))
    }
    pub fn remove(&self, index: usize) -> Result<()> {
        self.send(Command::Remove(index))
    }
    /// Move one existing effect to the destination index without reloading it.
    pub fn move_effect(&self, from: usize, to: usize) -> Result<()> {
        self.send(Command::Move(from, to))
    }
    pub fn set_bypass(&self, index: usize, bypass: bool) -> Result<()> {
        self.send(Command::Bypass(index, bypass))
    }
    pub fn set_parameter(&self, index: usize, id: u32, value: f64) -> Result<()> {
        if !value.is_finite() {
            return Err("Parameter value must be finite".into());
        }
        self.send(Command::Parameter(index, id, value))
    }
    /// Replace the rack transactionally after loading the whole configuration.
    pub fn restore(&self, config: RackConfig) -> Result<()> {
        config.validate()?;
        self.send(Command::Restore(config))
    }
    /// Cheap control gating without cloning opaque plug-in state.
    pub fn is_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire) != 0
    }
    /// Poll without copying opaque state while the visible rack is unchanged.
    pub fn snapshot_if_changed(&self, revision: u64, pending: bool) -> Option<RackSnapshot> {
        let shared = self.shared.lock().unwrap_or_else(|p| p.into_inner());
        let current_pending = self.pending.load(Ordering::Acquire) != 0;
        if shared.revision == revision && current_pending == pending {
            return None;
        }
        let mut snapshot = shared.clone();
        snapshot.pending = current_pending;
        Some(snapshot)
    }
    pub fn snapshot(&self) -> RackSnapshot {
        let shared = self.shared.lock().unwrap_or_else(|p| p.into_inner());
        let mut snapshot = shared.clone();
        // Publication precedes the worker's pending decrement. Keep the guard
        // until this read so an older config can never claim that edits settled.
        snapshot.pending = self.pending.load(Ordering::Acquire) != 0;
        drop(shared);
        snapshot
    }
}

enum Command {
    Discover(PathBuf),
    ScanInstalled(Vec<PathBuf>),
    Add(PathBuf, Option<String>),
    Remove(usize),
    Move(usize, usize),
    Bypass(usize, bool),
    Parameter(usize, u32, f64),
    Restore(RackConfig),
}

/// Constructs a rack controller and a factory that must be passed to Player's
/// `with_processor_factories`. The factory creates all native objects on worker.
pub struct EffectRack;
impl EffectRack {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(config: RackConfig) -> (EffectController, ProcessorFactory) {
        let (sender, receiver) = mpsc::sync_channel(COMMAND_CAPACITY);
        let shared = Arc::new(Mutex::new(RackSnapshot::default()));
        let pending = Arc::new(AtomicUsize::new(1));
        let controller = EffectController {
            sender,
            shared: shared.clone(),
            pending: pending.clone(),
        };
        let factory = Box::new(move || {
            let mut processor = RackProcessor {
                receiver,
                shared,
                pending,
                slots: Vec::new(),
                format: None,
                snapshot: RackSnapshot::default(),
                discovery: None,
            };
            let result = processor.restore(config);
            processor.snapshot.config_persistable = result.is_ok();
            processor.finish_command(result);
            Ok(Box::new(processor) as Box<dyn AudioProcessor>)
        });
        (controller, factory)
    }
}

struct Active {
    // Proxy must be destroyed before owner, on this same worker.
    realtime: Option<RealtimePlugin>,
    owner: Option<PluginOwner>,
}
impl Active {
    fn deactivate(mut self) -> Result<HostPlugin> {
        drop(self.realtime.take());
        self.owner
            .take()
            .unwrap()
            .deactivate()
            .map_err(|e| e.to_string())
    }
}
impl Drop for Active {
    fn drop(&mut self) {
        drop(self.realtime.take());
        drop(self.owner.take());
    }
}

struct Slot {
    config: EffectConfig,
    snapshot: EffectSnapshot,
    inactive: Option<HostPlugin>,
    active: Option<Active>,
}
impl Slot {
    fn load(path: PathBuf, id: Option<&str>, state: Option<&[u8]>, bypass: bool) -> Result<Self> {
        // Persist an absolute path even when the UI was given a relative one.
        let path = path
            .canonicalize()
            .map_err(|e| format!("Plugin path: {e}"))?;
        let mut host = HostPlugin::load(&path, id).map_err(|e| e.to_string())?;
        if let Some(state) = state {
            host.load_state(state).map_err(|e| e.to_string())?;
        }
        let descriptor = host.descriptor();
        let mut slot = Self {
            config: EffectConfig {
                path,
                plugin_id: descriptor.id.clone(),
                bypass,
                state: Vec::new(),
            },
            snapshot: EffectSnapshot {
                name: descriptor.name.clone(),
                vendor: descriptor.vendor.clone(),
                bypass,
                parameters: Vec::new(),
            },
            inactive: None,
            active: None,
        };
        slot.read_inactive(&mut host, MAX_EFFECT_STATE_BYTES)?;
        slot.inactive = Some(host);
        Ok(slot)
    }
    fn read_inactive(&mut self, host: &mut HostPlugin, state_limit: usize) -> Result<()> {
        let parameters = host
            .parameters()
            .iter()
            .map(|p| {
                let value = host.parameter_value(p.id).map_err(|e| e.to_string())?;
                Ok(ParameterSnapshot {
                    id: p.id,
                    name: p.name.clone(),
                    module: p.module.clone(),
                    value,
                    min: p.min_value,
                    max: p.max_value,
                    stepped: p.stepped,
                    read_only: p.read_only,
                    hidden: p.hidden,
                    text: host
                        .parameter_text(p.id, value)
                        .map_err(|e| e.to_string())?,
                })
            })
            .collect::<Result<_>>()?;
        let state = host.save_state().map_err(|e| e.to_string())?;
        if state.len() > state_limit.min(MAX_EFFECT_STATE_BYTES) {
            return Err(
                "Effect state exceeds the rack persistence limit (1 MiB each / 8 MiB total)".into(),
            );
        }
        self.snapshot.parameters = parameters;
        self.config.state = state;
        Ok(())
    }
    fn deactivate(&mut self) -> Result<()> {
        if let Some(active) = self.active.take() {
            self.inactive = Some(active.deactivate()?);
        }
        Ok(())
    }
    fn activate(&mut self, format: Option<(u32, usize)>) -> Result<()> {
        if self.config.bypass || self.active.is_some() {
            return Ok(());
        }
        if let Some((rate, max_frames)) = format {
            let host = self
                .inactive
                .take()
                .ok_or("Effect is unavailable; remove and reload it")?;
            let (owner, realtime) = host
                .activate(rate as f64, max_frames)
                .map_err(|e| e.to_string())?;
            self.active = Some(Active {
                realtime: Some(realtime),
                owner: Some(owner),
            });
        }
        Ok(())
    }
    fn set_parameter(
        &mut self,
        id: u32,
        value: f64,
        format: Option<(u32, usize)>,
        state_limit: usize,
    ) -> Result<()> {
        let parameter = self
            .snapshot
            .parameters
            .iter()
            .find(|p| p.id == id)
            .ok_or("Unknown effect parameter")?;
        if parameter.read_only
            || !value.is_finite()
            || value < parameter.min
            || value > parameter.max
            || (parameter.stepped && value.fract() != 0.)
        {
            return Err("Invalid or read-only effect parameter".into());
        }
        self.deactivate()?;
        let mut host = self
            .inactive
            .take()
            .ok_or("Effect is unavailable; remove and reload it")?;
        let result = host
            .set_parameter(id, value)
            .map_err(|e| e.to_string())
            .and_then(|()| self.read_inactive(&mut host, state_limit));
        if result.is_err() {
            // Keep the last persistable state after a rejected edit.
            if let Err(error) = host.load_state(&self.config.state) {
                self.inactive = Some(host);
                return Err(format!(
                    "Cannot restore effect after rejected edit: {error}"
                ));
            }
        }
        self.inactive = Some(host);
        let activation = self.activate(format);
        result.and(activation)
    }
    fn service(&mut self, state_limit: usize) -> Result<bool> {
        if let Some(active) = self.active.as_mut() {
            let owner = active.owner.as_ref().unwrap();
            owner.service_main_thread().map_err(|e| e.to_string())?;
            active
                .realtime
                .as_mut()
                .unwrap()
                .flush_pending()
                .map_err(|e| e.to_string())?;
            if let Some((values, state)) =
                owner.editor_snapshot(false).map_err(|e| e.to_string())?
            {
                if state.len() > state_limit.min(MAX_EFFECT_STATE_BYTES) {
                    return Err("Effect state exceeds the rack persistence limit (1 MiB each / 8 MiB total)".into());
                }
                self.config.state = state;
                for (id, value) in values {
                    if let Some(parameter) =
                        self.snapshot.parameters.iter_mut().find(|p| p.id == id)
                    {
                        parameter.value = value;
                        parameter.text = value.to_string();
                    }
                }
                return Ok(true);
            }
        } else if let Some(host) = self.inactive.as_mut() {
            host.service_main_thread().map_err(|e| e.to_string())?;
        }
        Ok(false)
    }
}

struct RackProcessor {
    receiver: Receiver<Command>,
    shared: Arc<Mutex<RackSnapshot>>,
    pending: Arc<AtomicUsize>,
    slots: Vec<Slot>,
    format: Option<(u32, usize)>,
    snapshot: RackSnapshot,
    discovery: Option<Receiver<(Vec<DiscoveredPlugin>, Vec<String>)>>,
}
impl RackProcessor {
    fn start_discovery(&mut self, roots: Vec<PathBuf>) -> Result<()> {
        if self.discovery.is_some() {
            return Err("CLAP discovery is already running".into());
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("cadence-clap-catalog".into())
            .spawn(move || {
                let (paths, mut warnings) = catalog::index(roots);
                let mut plugins = Vec::new();
                for path in paths {
                    match resonara_clap::discover(&path) {
                        Ok(descriptors) => plugins.extend(
                            descriptors
                                .into_iter()
                                .filter(|d| {
                                    d.features.iter().any(|f| f == "audio-effect")
                                        && !d
                                            .features
                                            .iter()
                                            .any(|f| f == "instrument" || f == "note-effect")
                                })
                                .map(|d| DiscoveredPlugin {
                                    path: path.clone(),
                                    id: d.id,
                                    name: d.name,
                                    vendor: d.vendor,
                                }),
                        ),
                        Err(error) => warnings.push(format!("{}: {error}", path.display())),
                    }
                }
                plugins.sort_by(|a, b| (&a.name, &a.path, &a.id).cmp(&(&b.name, &b.path, &b.id)));
                plugins.dedup();
                let _ = sender.send((plugins, warnings));
            })
            .map_err(|error| format!("Unable to start CLAP discovery: {error}"))?;
        self.discovery = Some(receiver);
        Ok(())
    }
    fn poll_discovery(&mut self) {
        let result = self.discovery.as_ref().map(|receiver| receiver.try_recv());
        match result {
            Some(Ok((plugins, warnings))) => {
                self.discovery = None;
                self.snapshot.discovered = plugins;
                self.finish_command(if warnings.is_empty() {
                    Ok(())
                } else {
                    Err(warnings.into_iter().take(8).collect::<Vec<_>>().join("\n"))
                });
            }
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                self.discovery = None;
                self.finish_command(Err("CLAP discovery stopped unexpectedly".into()));
            }
            _ => {}
        }
    }
    fn publish(&mut self) {
        self.snapshot.effects = self.slots.iter().map(|s| s.snapshot.clone()).collect();
        self.snapshot.config.effects = self.slots.iter().map(|s| s.config.clone()).collect();
        self.snapshot.revision = self.snapshot.revision.wrapping_add(1);
        *self.shared.lock().unwrap_or_else(|p| p.into_inner()) = self.snapshot.clone();
    }
    fn finish_command(&mut self, result: Result<()>) {
        self.snapshot.error = result.err();
        self.publish();
        self.pending.fetch_sub(1, Ordering::Release);
    }
    fn error(&mut self, error: String) -> String {
        if self.snapshot.error.as_ref() != Some(&error) {
            self.snapshot.error = Some(error.clone());
            self.publish();
        }
        error
    }
    fn restore(&mut self, config: RackConfig) -> Result<()> {
        config.validate()?;
        let mut slots = Vec::with_capacity(config.effects.len());
        for effect in config.effects {
            let mut slot = Slot::load(
                effect.path,
                Some(&effect.plugin_id),
                Some(&effect.state),
                effect.bypass,
            )?;
            slot.activate(self.format)?;
            slots.push(slot);
        }
        if slots.iter().map(|s| s.config.state.len()).sum::<usize>() > MAX_RACK_STATE_BYTES {
            return Err("Combined effect state exceeds 8 MiB".into());
        }
        self.slots = slots;
        Ok(())
    }
    fn command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::ScanInstalled(_) => unreachable!("discovery is scheduled by service"),
            Command::Discover(path) => {
                let discovered = resonara_clap::discover(&path).map_err(|e| e.to_string())?;
                self.snapshot.discovered = discovered
                    .into_iter()
                    .filter(|d| {
                        d.features.iter().any(|f| f == "audio-effect")
                            && !d
                                .features
                                .iter()
                                .any(|f| f == "instrument" || f == "note-effect")
                    })
                    .map(|d| DiscoveredPlugin {
                        path: path.clone(),
                        id: d.id,
                        name: d.name,
                        vendor: d.vendor,
                    })
                    .collect();
                if self.snapshot.discovered.is_empty() {
                    return Err("This library contains no supported audio effects".into());
                }
            }
            Command::Add(path, id) => {
                if self.slots.len() >= MAX_EFFECTS {
                    return Err(format!("At most {MAX_EFFECTS} effects can be loaded"));
                }
                let mut slot = Slot::load(path, id.as_deref(), None, false)?;
                slot.activate(self.format)?;
                if self
                    .slots
                    .iter()
                    .map(|s| s.config.state.len())
                    .sum::<usize>()
                    + slot.config.state.len()
                    > MAX_RACK_STATE_BYTES
                {
                    return Err("Combined effect state exceeds 8 MiB".into());
                }
                self.slots.push(slot);
            }
            Command::Remove(index) => {
                if index >= self.slots.len() {
                    return Err("Effect is no longer in the rack".into());
                }
                self.slots.remove(index);
            }
            Command::Move(from, to) => {
                if from >= self.slots.len() || to >= self.slots.len() {
                    return Err("Effect is no longer in the rack".into());
                }
                let slot = self.slots.remove(from);
                self.slots.insert(to, slot);
            }
            Command::Bypass(index, bypass) => {
                let slot = self
                    .slots
                    .get_mut(index)
                    .ok_or("Effect is no longer in the rack")?;
                if slot.config.bypass != bypass {
                    if bypass {
                        slot.deactivate()?;
                    }
                    slot.config.bypass = bypass;
                    slot.snapshot.bypass = bypass;
                    slot.activate(self.format)?;
                }
            }
            Command::Parameter(index, id, value) => {
                let others = self
                    .slots
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != index)
                    .map(|(_, s)| s.config.state.len())
                    .sum::<usize>();
                let slot = self
                    .slots
                    .get_mut(index)
                    .ok_or("Effect is no longer in the rack")?;
                slot.set_parameter(
                    id,
                    value,
                    self.format,
                    MAX_RACK_STATE_BYTES.saturating_sub(others),
                )?;
            }
            Command::Restore(config) => self.restore(config)?,
        }
        Ok(())
    }
}
impl AudioProcessor for RackProcessor {
    fn prepare(&mut self, sample_rate: u32, max_frames: usize) -> Result<()> {
        if !(8000..=384000).contains(&sample_rate) || !(1..=65536).contains(&max_frames) {
            return Err(self.error("Invalid effect audio format".into()));
        }
        let format = Some((sample_rate, max_frames));
        if self.format == format {
            return Ok(());
        }
        // A failed activation must never leave the rack claiming a format that
        // some later insert has not adopted. Tear every old activation down first.
        self.format = None;
        for slot in &mut self.slots {
            if let Err(error) = slot.deactivate() {
                return Err(self.error(error));
            }
        }
        for slot in &mut self.slots {
            if let Err(error) = slot.activate(format) {
                return Err(self.error(error));
            }
        }
        self.format = format;
        Ok(())
    }
    fn service(&mut self) -> Result<()> {
        // Directory and descriptor discovery must not stop audio processing.
        self.poll_discovery();
        // Limit control work per turn. No commands or native calls run on the UI.
        for _ in 0..16 {
            let Ok(command) = self.receiver.try_recv() else {
                break;
            };
            if let Command::ScanInstalled(roots) = command {
                if let Err(error) = self.start_discovery(roots) {
                    self.finish_command(Err(error));
                }
                continue;
            }
            let changes_rack = !matches!(&command, Command::Discover(_));
            if matches!(&command, Command::Restore(_)) {
                self.snapshot.config_persistable = false;
            }
            let result = self.command(command);
            if changes_rack && result.is_ok() {
                self.snapshot.config_persistable = true;
            }
            self.finish_command(result);
        }
        let mut changed = false;
        let mut total_bytes = self
            .slots
            .iter()
            .map(|s| s.config.state.len())
            .sum::<usize>();
        for slot in &mut self.slots {
            let old_bytes = slot.config.state.len();
            let limit = MAX_RACK_STATE_BYTES.saturating_sub(total_bytes - old_bytes);
            match slot.service(limit) {
                Ok(dirty) => {
                    changed |= dirty;
                    total_bytes = total_bytes - old_bytes + slot.config.state.len();
                }
                Err(error) => return Err(self.error(error)),
            }
        }
        if changed {
            self.publish();
        }
        Ok(())
    }
    fn process(&mut self, frames: &mut [Frame], sample_rate: u32) -> Result<()> {
        let Some((rate, max)) = self.format else {
            return Err(self.error("Effect rack must be prepared before processing".into()));
        };
        if rate != sample_rate || frames.len() > max {
            return Err(self.error("Effect block does not match its prepared format".into()));
        }
        for slot in &mut self.slots {
            if slot.config.bypass {
                continue;
            }
            let result = slot
                .active
                .as_mut()
                .ok_or_else(|| "Effect is unavailable; remove and reload it".to_owned())
                .and_then(|active| {
                    active
                        .realtime
                        .as_mut()
                        .unwrap()
                        .process(frames)
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                });
            if let Err(error) = result {
                return Err(self.error(error));
            }
        }
        Ok(())
    }
    fn reset(&mut self) {
        for slot in &mut self.slots {
            if let Some(active) = slot.active.as_mut() {
                active.realtime.as_mut().unwrap().reset_transport();
            }
        }
    }
}

#[cfg(test)]
mod catalog_worker_tests {
    use super::*;
    #[test]
    fn pending_catalog_does_not_block_audio_and_native_descriptors_are_published() {
        let Some(fixture) = std::env::var_os("CADENCE_TEST_CLAP").map(PathBuf::from) else {
            return;
        };
        let root =
            std::env::temp_dir().join(format!("cadence-catalog-native-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::copy(fixture, root.join("gain.clap")).unwrap();
        let (controller, factory) = EffectRack::new(Default::default());
        let mut processor = factory().unwrap();
        processor.prepare(48_000, 64).unwrap();
        controller
            .restore(RackConfig {
                effects: vec![EffectConfig {
                    path: root.join("missing.clap"),
                    plugin_id: "missing.effect".into(),
                    bypass: false,
                    state: vec![],
                }],
            })
            .unwrap();
        processor.service().unwrap();
        assert!(!controller.snapshot().config_persistable);
        controller
            .send(Command::ScanInstalled(vec![root.clone()]))
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while controller.is_pending() {
            processor.service().unwrap();
            let mut frames = vec![[0.25, -0.25]; 64];
            processor.process(&mut frames, 48_000).unwrap();
            assert!(frames.iter().all(|frame| *frame == [0.25, -0.25]));
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let snapshot = controller.snapshot();
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        assert_eq!(snapshot.discovered.len(), 1);
        assert_eq!(snapshot.discovered[0].id, "org.resonara.gain");
        assert_eq!(
            snapshot.discovered[0].path,
            root.join("gain.clap").canonicalize().unwrap()
        );
        assert!(snapshot.effects.is_empty());
        assert!(!snapshot.config_persistable);
        controller
            .add(root.join("gain.clap"), Some("org.resonara.gain".into()))
            .unwrap();
        processor.service().unwrap();
        assert!(controller.snapshot().config_persistable);
        drop(processor);
        std::fs::remove_dir_all(root).unwrap();
    }
}

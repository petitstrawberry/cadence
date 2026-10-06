//! Real CLAP shared-library integration. Build the bundled Resonara fixture, then run:
//! ```sh
//! cargo build --release --manifest-path ../resonara/plugins/resonara-gain/Cargo.toml
//! cargo test -p cadence-effects --test native_gain -- --ignored
//! ```
//! Set `CADENCE_TEST_CLAP` to use another build of the same Resonara Gain fixture.

use cadence_core::AudioProcessor;
use cadence_effects::{EffectController, EffectRack, RackConfig};
use std::path::PathBuf;

fn gain_path() -> PathBuf {
    std::env::var_os("CADENCE_TEST_CLAP")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let filename = if cfg!(target_os = "macos") {
                "libresonara_gain.dylib"
            } else if cfg!(target_os = "windows") {
                "resonara_gain.dll"
            } else {
                "libresonara_gain.so"
            };
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../../resonara/plugins/resonara-gain/target/release")
                .join(filename)
        })
}

fn config_json(controller: &EffectController) -> String {
    serde_json::to_string(&controller.snapshot().config).unwrap()
}

fn assert_audio(processor: &mut dyn AudioProcessor, sample_rate: u32, count: usize, gain: f32) {
    let mut frames = vec![[0.5, -0.25]; count];
    processor.process(&mut frames, sample_rate).unwrap();
    assert!(
        frames
            .iter()
            .all(|frame| *frame == [0.5 * gain, -0.25 * gain]),
        "unexpected processed audio: {frames:?}"
    );
}

#[test]
fn missing_plugin_leaves_empty_rack_usable_and_reports_error() {
    let (controller, factory) = EffectRack::new(RackConfig::default());
    std::thread::spawn(move || {
        let mut processor = factory().unwrap();
        processor.prepare(48_000, 64).unwrap();
        controller
            .add(PathBuf::from("/definitely/not/a/cadence-plugin.clap"), None)
            .unwrap();
        processor.service().unwrap();
        let snapshot = controller.snapshot();
        assert!(snapshot.error.is_some());
        assert!(snapshot.effects.is_empty());
        assert_audio(processor.as_mut(), 48_000, 64, 1.0);

        controller.remove(0).unwrap();
        processor.service().unwrap();
        assert!(controller.snapshot().error.is_some());
        assert!(controller.snapshot().effects.is_empty());
        assert_audio(processor.as_mut(), 48_000, 64, 1.0);
    })
    .join()
    .unwrap();
}

#[test]
#[ignore = "requires the built Resonara Gain native fixture; see module run command"]
fn real_gain_rack_discovery_edits_state_audio_and_worker_teardown() {
    let path = gain_path();
    assert!(
        path.is_file(),
        "Build ../resonara/plugins/resonara-gain first or set CADENCE_TEST_CLAP: {}",
        path.display()
    );
    let quarantined_before = resonara_clap::quarantined_instance_count();
    let (controller, factory) = EffectRack::new(RackConfig::default());
    // The factory crosses the thread boundary. Every plugin instance is created,
    // serviced, processed, deactivated and destroyed on this same worker.
    std::thread::spawn(move || {
        let mut processor = factory().unwrap();
        processor.prepare(48_000, 128).unwrap();
        controller.discover(path.clone()).unwrap();
        processor.service().unwrap();
        let discovered = controller.snapshot().discovered;
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].id, "org.resonara.gain");
        assert_eq!(discovered[0].name, "Resonara Gain");
        assert_eq!(discovered[0].vendor, "Resonara");
        assert_eq!(discovered[0].path, path);

        for _ in 0..2 {
            controller
                .add(path.clone(), Some("org.resonara.gain".into()))
                .unwrap();
            processor.service().unwrap();
        }
        let initial = controller.snapshot();
        assert!(initial.error.is_none(), "{:?}", initial.error);
        assert_eq!(initial.effects.len(), 2);
        for effect in &initial.effects {
            assert_eq!(effect.name, "Resonara Gain");
            assert_eq!(effect.vendor, "Resonara");
            assert!(!effect.bypass);
            assert_eq!(effect.parameters.len(), 1);
            let gain = &effect.parameters[0];
            assert_eq!(gain.id, 0);
            assert_eq!(gain.name, "Gain");
            assert_eq!(gain.value, 1.0);
            assert_eq!(gain.min, 0.0);
            assert_eq!(gain.max, 2.0);
            assert!(!gain.stepped);
            assert!(!gain.read_only);
            assert!(!gain.hidden);
        }

        controller.set_parameter(0, 0, 0.25).unwrap();
        controller.set_parameter(1, 0, 1.5).unwrap();
        processor.service().unwrap();
        assert_eq!(controller.snapshot().effects[0].parameters[0].value, 0.25);
        assert_eq!(controller.snapshot().effects[1].parameters[0].value, 1.5);
        assert!(
            controller.snapshot().effects[0].parameters[0]
                .text
                .contains("0.25")
        );
        for count in [1, 3, 64, 128, 128, 3] {
            assert_audio(processor.as_mut(), 48_000, count, 0.375);
        }

        let saved_json = config_json(&controller);
        let saved: RackConfig = serde_json::from_str(&saved_json).unwrap();
        assert_eq!(serde_json::to_string(&saved).unwrap(), saved_json);
        controller.move_effect(0, 1).unwrap();
        processor.service().unwrap();
        assert_eq!(controller.snapshot().effects[0].parameters[0].value, 1.5);
        assert_eq!(controller.snapshot().effects[1].parameters[0].value, 0.25);

        controller.set_bypass(0, true).unwrap();
        processor.service().unwrap();
        assert!(controller.snapshot().effects[0].bypass);
        assert_audio(processor.as_mut(), 48_000, 128, 0.25);
        controller.set_parameter(0, 0, 0.5).unwrap();
        processor.service().unwrap();
        assert_audio(processor.as_mut(), 48_000, 128, 0.25);
        controller.set_bypass(0, false).unwrap();
        processor.service().unwrap();
        assert_audio(processor.as_mut(), 48_000, 128, 0.125);

        controller.remove(1).unwrap();
        processor.service().unwrap();
        assert_eq!(controller.snapshot().effects.len(), 1);
        assert_audio(processor.as_mut(), 48_000, 128, 0.5);
        controller.restore(saved).unwrap();
        processor.service().unwrap();
        assert_eq!(config_json(&controller), saved_json);
        assert_eq!(controller.snapshot().effects.len(), 2);
        assert_audio(processor.as_mut(), 48_000, 128, 0.375);

        // An invalid UI edit must report the failure without replacing a live chain.
        let expected = config_json(&controller);
        macro_rules! assert_rejected {
            ($command:expr) => {{
                $command.unwrap();
                processor.service().unwrap();
                assert!(controller.snapshot().error.is_some());
                assert_eq!(config_json(&controller), expected);
                assert_audio(processor.as_mut(), 48_000, 128, 0.375);
            }};
        }
        assert_rejected!(controller.remove(99));
        assert_rejected!(controller.move_effect(0, 99));
        assert_rejected!(controller.set_bypass(99, true));
        assert_rejected!(controller.set_parameter(99, 0, 0.5));
        assert_rejected!(controller.set_parameter(0, 99, 0.5));
        assert!(controller.set_parameter(0, 0, f64::NAN).is_err());
        assert_eq!(config_json(&controller), expected);
        assert_rejected!(controller.set_parameter(0, 0, 2.01));
        assert_rejected!(controller.add(PathBuf::from("/definitely/not/a/plugin.clap"), None));

        processor.prepare(44_100, 16).unwrap();
        for count in [1, 3, 16, 16] {
            assert_audio(processor.as_mut(), 44_100, count, 0.375);
        }
        processor.reset();
        assert_audio(processor.as_mut(), 44_100, 16, 0.375);

        // Keep two instances alive while cycling more than the fixture's 64 slots.
        // A leaked teardown cannot be hidden by unloading the shared library.
        for _ in 0..100 {
            controller.add(path.clone(), None).unwrap();
            processor.service().unwrap();
            assert!(controller.snapshot().error.is_none());
            assert_eq!(controller.snapshot().effects.len(), 3);
            assert_audio(processor.as_mut(), 44_100, 16, 0.375);
            controller.remove(2).unwrap();
            processor.service().unwrap();
            assert_eq!(controller.snapshot().effects.len(), 2);
        }
        // Processing failures reach the same visible rack status as UI failures.
        let mut invalid = [[f32::NAN, 0.0]];
        assert!(processor.process(&mut invalid, 44_100).is_err());
        assert!(
            controller
                .snapshot()
                .error
                .as_deref()
                .unwrap()
                .contains("InvalidBlock")
        );
        controller.restore(RackConfig::default()).unwrap();
        processor.service().unwrap();
        assert!(controller.snapshot().effects.is_empty());
        assert_audio(processor.as_mut(), 44_100, 16, 1.0);
        drop(processor);
    })
    .join()
    .unwrap();
    assert_eq!(
        resonara_clap::quarantined_instance_count(),
        quarantined_before,
        "worker teardown must not quarantine CLAP owners"
    );
}

#[test]
fn persisted_rack_rejects_oversized_state_before_loading_native_code() {
    use cadence_effects::{
        EffectConfig, MAX_EFFECT_STATE_BYTES, MAX_EFFECTS, MAX_RACK_STATE_BYTES,
    };
    let small = EffectConfig {
        path: PathBuf::from("/not/a/native/plugin.clap"),
        plugin_id: "example.effect".into(),
        bypass: false,
        state: vec![0],
    };
    let too_many = RackConfig {
        effects: vec![small.clone(); MAX_EFFECTS + 1],
    };
    assert!(too_many.validate().unwrap_err().contains("effects"));
    let mut oversized = small.clone();
    oversized.state = vec![0; MAX_EFFECT_STATE_BYTES + 1];
    assert!(
        RackConfig {
            effects: vec![oversized]
        }
        .validate()
        .unwrap_err()
        .contains("1 MiB")
    );
    let mut full = small;
    full.state = vec![0; MAX_EFFECT_STATE_BYTES];
    let combined = RackConfig {
        effects: vec![full; MAX_RACK_STATE_BYTES / MAX_EFFECT_STATE_BYTES + 1],
    };
    assert!(combined.validate().unwrap_err().contains("8 MiB"));
    let (controller, factory) = EffectRack::new(RackConfig::default());
    let _processor = factory().unwrap();
    assert!(controller.restore(combined).is_err());
    assert!(!controller.snapshot().pending);
}

#[test]
fn snapshot_poll_tracks_pending_and_completed_commands() {
    let (controller, factory) = EffectRack::new(RackConfig::default());
    assert!(controller.is_pending());
    assert!(controller.snapshot_if_changed(0, true).is_none());
    let mut processor = factory().unwrap();
    let initialized = controller.snapshot_if_changed(0, true).unwrap();
    assert!(!initialized.pending);
    assert!(!controller.is_pending());
    assert!(
        controller
            .snapshot_if_changed(initialized.revision, false)
            .is_none()
    );
    controller.remove(99).unwrap();
    let queued = controller
        .snapshot_if_changed(initialized.revision, false)
        .unwrap();
    assert!(queued.pending);
    assert_eq!(queued.revision, initialized.revision);
    processor.service().unwrap();
    let completed = controller
        .snapshot_if_changed(queued.revision, true)
        .unwrap();
    assert!(!completed.pending);
    assert!(completed.error.is_some());
    assert_ne!(completed.revision, queued.revision);
    assert!(
        controller
            .snapshot_if_changed(completed.revision, false)
            .is_none()
    );
}

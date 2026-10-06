//! Durable application settings. Call file operations on the application I/O worker.

use cadence_effects::RackConfig;
use cadence_sources::SourceConfig;
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
};

const MAX_CONFIG_BYTES: usize = 40 * 1024 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppConfig {
    pub sources: Vec<SourceConfig>,
    pub effects: RackConfig,
}

/// Failed reads use defaults for this session, but must never be saved over the
/// original file. Keep `writable` alongside the loaded configuration in the UI.
pub struct LoadedConfig {
    pub config: AppConfig,
    pub writable: bool,
    pub error: Option<String>,
}

/// Serial settings writer. UI calls never perform filesystem operations. The
/// final queued settings are flushed when this writer is dropped at shutdown.
pub struct ConfigWriter {
    sender: Option<SyncSender<AppConfig>>,
    results: Receiver<Result<(), String>>,
    worker: Option<JoinHandle<()>>,
}

impl ConfigWriter {
    pub fn new(directory: PathBuf, writable: bool) -> Self {
        let (result_sender, results) = mpsc::channel();
        if !writable {
            return Self {
                sender: None,
                results,
                worker: None,
            };
        }
        let (sender, receiver) = mpsc::sync_channel(8);
        let errors = result_sender.clone();
        match thread::Builder::new()
            .name("cadence-settings".into())
            .spawn(move || {
                while let Ok(mut config) = receiver.recv() {
                    // Skip obsolete snapshots without reordering configuration writes.
                    while let Ok(newer) = receiver.try_recv() {
                        config = newer;
                    }
                    if result_sender.send(save(&directory, &config)).is_err() {
                        break;
                    }
                }
            }) {
            Ok(worker) => Self {
                sender: Some(sender),
                results,
                worker: Some(worker),
            },
            Err(error) => {
                let _ = errors.send(Err(format!("Cannot start settings writer: {error}")));
                Self {
                    sender: None,
                    results,
                    worker: None,
                }
            }
        }
    }

    pub fn save(&self, config: AppConfig) -> Result<(), String> {
        let sender = self
            .sender
            .as_ref()
            .ok_or("Settings saving is disabled for this session")?;
        sender.try_send(config).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => "Settings writer is busy; try again".into(),
            mpsc::TrySendError::Disconnected(_) => "Settings writer has stopped".into(),
        })
    }

    /// Queue the final snapshot during shutdown, waiting for queue space when
    /// necessary. File operations remain on the writer thread; dropping this
    /// writer afterwards waits for the accepted snapshot to finish saving.
    pub fn shutdown_save(&self, config: AppConfig) -> Result<(), String> {
        let sender = self
            .sender
            .as_ref()
            .ok_or("Settings saving is disabled for this session")?;
        sender
            .send(config)
            .map_err(|_| "Settings writer has stopped".into())
    }

    pub fn try_recv(&self) -> Option<Result<(), String>> {
        self.results.try_recv().ok()
    }
}

impl Drop for ConfigWriter {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn data_directory() -> PathBuf {
    directory_from_environment(|name| std::env::var_os(name))
}

fn directory_from_environment(mut lookup: impl FnMut(&str) -> Option<OsString>) -> PathBuf {
    let nonempty = |value: OsString| (!value.is_empty()).then_some(value);
    if let Some(path) = lookup("CADENCE_DATA_DIR").and_then(nonempty) {
        return PathBuf::from(path);
    }
    if let Some(path) = lookup("XDG_DATA_HOME").and_then(nonempty) {
        return PathBuf::from(path).join("cadence");
    }
    if let Some(path) = lookup("HOME").and_then(nonempty) {
        return PathBuf::from(path).join(".local/share/cadence");
    }
    PathBuf::from("/home/cadence")
}

pub fn load(directory: &Path) -> LoadedConfig {
    match read(&directory.join("app.json")) {
        Ok(config) => LoadedConfig {
            config,
            writable: true,
            error: None,
        },
        Err(error) => LoadedConfig {
            config: AppConfig::default(),
            writable: false,
            error: Some(format!(
                "Cannot load application settings: {error}. The original app.json is preserved; settings changes will not be saved this session"
            )),
        },
    }
}

fn read(path: &Path) -> Result<AppConfig, String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AppConfig::default());
        }
        Err(error) => return Err(error.to_string()),
    };
    // Bound the actual read instead of relying on metadata, which may change
    // between the stat and read operations.
    let mut bytes = Vec::new();
    file.take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err("app.json exceeds 40 MiB".into());
    }
    let config: AppConfig = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    config.effects.validate()?;
    Ok(config)
}

/// Atomically replace app.json after synchronizing a private temporary file.
/// Callers must honor `LoadedConfig::writable` after loading settings.
pub fn save(directory: &Path, config: &AppConfig) -> Result<(), String> {
    config.effects.validate()?;
    let mut buffer = ConfigBuffer::default();
    serde_json::to_writer(&mut buffer, config).map_err(|error| error.to_string())?;
    let mut directories = fs::DirBuilder::new();
    directories.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directories.mode(0o700);
    }
    directories
        .create(directory)
        .map_err(|error| error.to_string())?;
    let (temporary, mut file) = create_temporary(directory)?;
    let write_result = file.write_all(&buffer.0).and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(error.to_string());
    }
    if let Err(error) = fs::rename(&temporary, directory.join("app.json")) {
        let _ = fs::remove_file(&temporary);
        return Err(error.to_string());
    }
    // Some platforms/filesystems cannot open or synchronize directories. The
    // replacement is already committed, so this durability enhancement is best effort.
    if let Ok(parent) = File::open(directory) {
        let _ = parent.sync_all();
    }
    Ok(())
}

#[derive(Default)]
struct ConfigBuffer(Vec<u8>);

impl Write for ConfigBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_CONFIG_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other(
                "Application settings exceed 40 MiB; app.json was not changed",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn create_temporary(directory: &Path) -> Result<(PathBuf, File), String> {
    for _ in 0..64 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(".app.json.{}.{}.tmp", std::process::id(), sequence));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("Could not create a unique settings temporary file".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            loop {
                let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "cadence-settings-test-{}-{sequence}",
                    std::process::id()
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("create test directory: {error}"),
                }
            }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sample_config() -> AppConfig {
        AppConfig {
            sources: vec![SourceConfig {
                id: "kanade-home".into(),
                name: "Home music".into(),
                plugin_id: "kanade".into(),
                settings: serde_json::json!({"url": "http://music.local", "token": "example"}),
            }],
            effects: RackConfig::default(),
        }
    }

    #[test]
    fn data_directory_priority_without_mutating_process_environment() {
        let path = |values: &[(&str, &str)]| {
            directory_from_environment(|name| {
                values
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            })
        };
        assert_eq!(
            path(&[("CADENCE_DATA_DIR", "/custom"), ("HOME", "/user")]),
            PathBuf::from("/custom")
        );
        assert_eq!(
            path(&[
                ("CADENCE_DATA_DIR", ""),
                ("XDG_DATA_HOME", "/data"),
                ("HOME", "/user")
            ]),
            PathBuf::from("/data/cadence")
        );
        assert_eq!(
            path(&[("HOME", "/user")]),
            PathBuf::from("/user/.local/share/cadence")
        );
        assert_eq!(path(&[]), PathBuf::from("/home/cadence"));
    }

    #[test]
    fn missing_settings_are_writable_defaults() {
        let directory = TestDirectory::new();
        let loaded = load(&directory.0);
        assert_eq!(loaded.config, AppConfig::default());
        assert!(loaded.writable);
        assert!(loaded.error.is_none());
        assert!(!directory.0.join("app.json").exists());
    }

    #[test]
    fn corrupt_settings_are_preserved_and_disable_saving() {
        let directory = TestDirectory::new();
        let path = directory.0.join("app.json");
        fs::write(&path, b"{broken").unwrap();
        let loaded = load(&directory.0);
        assert!(!loaded.writable);
        assert!(loaded.error.is_some());
        assert_eq!(loaded.config, AppConfig::default());
        assert_eq!(fs::read(path).unwrap(), b"{broken");
    }

    #[test]
    fn unknown_configuration_fields_are_preserved() {
        let directory = TestDirectory::new();
        fs::write(
            directory.0.join("app.json"),
            br#"{"newer_version_setting":true}"#,
        )
        .unwrap();
        assert!(!load(&directory.0).writable);
    }

    #[test]
    fn atomic_save_replaces_settings_without_leaving_temporary_files() {
        let directory = TestDirectory::new();
        let nested = directory.0.join("new/data");
        save(&nested, &AppConfig::default()).unwrap();
        let config = sample_config();
        save(&nested, &config).unwrap();
        let loaded = load(&nested);
        assert!(loaded.writable);
        assert_eq!(loaded.config, config);
        assert_eq!(fs::read_dir(&nested).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(nested.join("app.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn oversized_configuration_cannot_be_read_or_replace_good_settings() {
        let directory = TestDirectory::new();
        let mut config = sample_config();
        save(&directory.0, &config).unwrap();
        let original = fs::read(directory.0.join("app.json")).unwrap();
        config.sources[0].settings = serde_json::Value::String("x".repeat(MAX_CONFIG_BYTES));
        assert!(save(&directory.0, &config).is_err());
        assert_eq!(fs::read(directory.0.join("app.json")).unwrap(), original);
        fs::write(
            directory.0.join("app.json"),
            vec![b' '; MAX_CONFIG_BYTES + 1],
        )
        .unwrap();
        let loaded = load(&directory.0);
        assert!(!loaded.writable);
        assert!(loaded.error.unwrap().contains("40 MiB"));
    }

    #[test]
    fn writer_flushes_latest_accepted_configuration_on_shutdown() {
        let directory = TestDirectory::new();
        let writer = ConfigWriter::new(directory.0.clone(), true);
        writer.save(AppConfig::default()).unwrap();
        let config = sample_config();
        writer.shutdown_save(config.clone()).unwrap();
        drop(writer);
        assert_eq!(load(&directory.0).config, config);
    }

    #[test]
    fn shutdown_save_keeps_final_snapshot_when_regular_queue_is_full() {
        // Keep the consumer paused until all eight slots are occupied.
        let (sender, receiver) = mpsc::sync_channel(8);
        let (_, results) = mpsc::channel();
        let writer = ConfigWriter {
            sender: Some(sender),
            results,
            worker: None,
        };
        for _ in 0..8 {
            writer.save(AppConfig::default()).unwrap();
        }
        assert!(writer.save(sample_config()).is_err());
        let final_config = sample_config();
        let expected = final_config.clone();
        let shutdown = thread::spawn(move || writer.shutdown_save(final_config));
        let snapshots: Vec<_> = receiver.into_iter().collect();
        shutdown.join().unwrap().unwrap();
        assert_eq!(snapshots.len(), 9);
        assert_eq!(snapshots.last(), Some(&expected));
    }

    #[test]
    fn disabled_writer_never_overwrites_corrupt_settings() {
        let directory = TestDirectory::new();
        let path = directory.0.join("app.json");
        fs::write(&path, b"{broken").unwrap();
        let loaded = load(&directory.0);
        let writer = ConfigWriter::new(directory.0.clone(), loaded.writable);
        assert!(writer.save(sample_config()).is_err());
        assert!(writer.shutdown_save(sample_config()).is_err());
        assert!(writer.try_recv().is_none());
        drop(writer);
        assert_eq!(fs::read(path).unwrap(), b"{broken");
    }
}

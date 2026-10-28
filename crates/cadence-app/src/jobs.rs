//! Serialized library I/O. The UI only sends requests and consumes snapshots.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cadence_library::{Library, LibraryTrack, ScanOptions, ScanProgress, ScanReport};

/// Mutations are persisted before a successful snapshot is published.
#[derive(Debug)]
pub enum LibraryCommand {
    Load,
    Scan(PathBuf),
    Rescan,
    RemoveRoot(PathBuf),
    RemoveSource(String),
    AuthorizeSource {
        config: cadence_sources::SourceConfig,
        cancel: Arc<AtomicBool>,
    },
    Upsert(Vec<LibraryTrack>),
    SyncSource {
        config: cadence_sources::SourceConfig,
        control: Arc<SourceSyncControl>,
    },
    Remove(String),
}

/// Cancellation wins before saving starts; once saving starts the UI waits for
/// the durable result instead of claiming a cancelled operation was discarded.
#[derive(Debug, Default)]
pub struct SourceSyncControl {
    cancelled: AtomicBool,
    phase: AtomicU8,
}
impl SourceSyncControl {
    pub fn cancel(&self) -> bool {
        if self
            .phase
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.cancelled.store(true, Ordering::Release);
            true
        } else {
            self.phase.load(Ordering::Acquire) == 1
        }
    }
    pub fn is_saving(&self) -> bool {
        self.phase.load(Ordering::Acquire) == 2
    }
    fn begin_save(&self) -> bool {
        self.phase
            .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

impl LibraryCommand {
    fn is_scan(&self) -> bool {
        matches!(self, Self::Scan(_) | Self::Rescan)
    }
}

#[derive(Clone, Debug)]
pub struct LibraryEvent {
    pub tracks: Vec<LibraryTrack>,
    pub roots: Vec<PathBuf>,
    pub status: String,
    pub error: Option<String>,
    /// A failed load protects the original file from subsequent mutations.
    pub read_only: bool,
    pub removed_source: Option<String>,
    pub synced_source: Option<String>,
    pub source_removal: Option<String>,
    pub authorized_source: Option<String>,
}

pub struct LibraryWorker {
    requests: Option<SyncSender<LibraryCommand>>,
    events: Option<Receiver<LibraryEvent>>,
    scan_cancel: Arc<AtomicBool>,
    scan_pending: Arc<AtomicBool>,
    scan_status: Arc<Mutex<ScanStatus>>,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl LibraryWorker {
    /// Starts loading `library.json` immediately, without doing filesystem I/O
    /// on the calling thread. Both channels are bounded to limit queued work.
    pub fn new(data_directory: PathBuf) -> Self {
        let (requests, receiver) = mpsc::sync_channel::<LibraryCommand>(8);
        let (sender, events) = mpsc::sync_channel::<LibraryEvent>(2);
        let scan_cancel = Arc::new(AtomicBool::new(false));
        let scan_pending = Arc::new(AtomicBool::new(false));
        let scan_status = Arc::new(Mutex::new(ScanStatus::default()));
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&scan_cancel);
        let worker_pending = Arc::clone(&scan_pending);
        let worker_status = Arc::clone(&scan_status);
        let worker_stopped = Arc::clone(&stopped);
        // Retain an event sender to report resource exhaustion without panicking
        // during UI startup. It is dropped after the thread is started.
        let startup_sender = sender.clone();
        let worker = match thread::Builder::new()
            .name("cadence-library".into())
            .spawn(move || {
                let mut state = LibraryState::new(data_directory.join("library.json"));
                state.scan_status = worker_status;
                let event = state.load();
                if sender.send(event).is_err() && !worker_stopped.load(Ordering::Acquire) {
                    return;
                }
                if !state.read_only
                    && state.library.needs_ordering_metadata_refresh()
                    && !worker_stopped.load(Ordering::Acquire)
                    && worker_pending
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                {
                    let event = state.refresh_ordering_metadata(&worker_cancel);
                    worker_pending.store(false, Ordering::Release);
                    if sender.send(event).is_err() && !worker_stopped.load(Ordering::Acquire) {
                        return;
                    }
                }
                while let Ok(command) = receiver.recv() {
                    let is_scan = command.is_scan();
                    if is_scan && worker_stopped.load(Ordering::Acquire) {
                        worker_pending.store(false, Ordering::Release);
                        continue;
                    }
                    if worker_stopped.load(Ordering::Acquire)
                        && matches!(
                            &command,
                            LibraryCommand::AuthorizeSource { .. }
                                | LibraryCommand::SyncSource { .. }
                        )
                    {
                        continue;
                    }
                    let event = state.run(command, &worker_cancel);
                    if is_scan {
                        worker_pending.store(false, Ordering::Release);
                    }
                    if sender.send(event).is_err() && !worker_stopped.load(Ordering::Acquire) {
                        break;
                    }
                }
            }) {
            Ok(worker) => Some(worker),
            Err(error) => {
                let _ = startup_sender.try_send(LibraryEvent {
                    tracks: Vec::new(),
                    roots: Vec::new(),
                    status: "Library unavailable".into(),
                    error: Some(format!("Unable to start library worker: {error}")),
                    read_only: true,
                    removed_source: None,
                    synced_source: None,
                    source_removal: None,
                    authorized_source: None,
                });
                None
            }
        };
        Self {
            requests: Some(requests),
            events: Some(events),
            scan_cancel,
            scan_pending,
            scan_status,
            stopped,
            worker,
        }
    }

    /// Never waits for the worker. Busy/disconnected errors should be displayed
    /// by the UI instead of silently dropping an action.
    pub fn send(&self, command: LibraryCommand) -> Result<(), String> {
        let requests = self.requests.as_ref().ok_or("Library worker has stopped")?;
        let is_scan = command.is_scan();
        let mut previous_scan = None;
        if is_scan {
            self.scan_pending
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .map_err(|_| "A library scan is already queued or running".to_owned())?;
            self.scan_cancel.store(false, Ordering::Relaxed);
            previous_scan = Some(std::mem::take(
                &mut *self.scan_status.lock().unwrap_or_else(|e| e.into_inner()),
            ));
        }
        match requests.try_send(command) {
            Ok(()) => Ok(()),
            Err(error) => {
                if is_scan {
                    *self.scan_status.lock().unwrap_or_else(|e| e.into_inner()) =
                        previous_scan.expect("scan status was saved");
                    self.scan_pending.store(false, Ordering::Release);
                }
                Err(match error {
                    TrySendError::Full(_) => "Library is busy; please try again".into(),
                    TrySendError::Disconnected(_) => "Library worker is unavailable".into(),
                })
            }
        }
    }

    pub fn try_recv(&self) -> Option<LibraryEvent> {
        self.events.as_ref()?.try_recv().ok()
    }

    pub fn cancel_scan(&self) {
        self.scan_cancel.store(true, Ordering::Relaxed);
    }

    /// Includes a queued scan, so another scan cannot be submitted while the
    /// current one is waiting behind an ordered catalog mutation.
    pub fn is_scanning(&self) -> bool {
        self.scan_pending.load(Ordering::Acquire)
    }

    pub fn scan_progress(&self) -> Option<ScanProgress> {
        self.scan_status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .progress
            .clone()
    }

    /// Read once when a scan finishes. Error details are bounded independently
    /// of the live progress value, which stays inexpensive to poll.
    pub fn scan_report(&self) -> Option<ScanReport> {
        self.scan_status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .report
            .clone()
    }

    /// The latest scan could not be accepted or saved. Discovery counts in its
    /// report must not be presented as committed library changes in this case.
    /// This outcome is finalized before `is_scanning()` becomes false.
    pub fn scan_failure(&self) -> Option<String> {
        self.scan_status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failure
            .clone()
    }
}

impl Drop for LibraryWorker {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        self.scan_cancel.store(true, Ordering::Relaxed);
        // Close both channels before joining: publishing a snapshot may be
        // blocked on the bounded event queue. The worker still drains accepted
        // catalog mutations, while scans are cancelled or skipped on shutdown.
        self.requests.take();
        self.events.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct LibraryState {
    path: PathBuf,
    library: Library,
    read_only: bool,
    scan_status: Arc<Mutex<ScanStatus>>,
}

#[derive(Default)]
struct ScanStatus {
    progress: Option<ScanProgress>,
    report: Option<ScanReport>,
    failure: Option<String>,
}

impl LibraryState {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            library: Library::new(),
            read_only: false,
            scan_status: Arc::new(Mutex::new(ScanStatus::default())),
        }
    }

    fn event(&self, status: String, error: Option<String>) -> LibraryEvent {
        LibraryEvent {
            tracks: self.library.tracks().to_vec(),
            roots: self.library.roots().to_vec(),
            status,
            error,
            read_only: self.read_only,
            removed_source: None,
            synced_source: None,
            source_removal: None,
            authorized_source: None,
        }
    }

    fn load(&mut self) -> LibraryEvent {
        match Library::load(&self.path) {
            Ok(library) => {
                self.library = library;
                self.read_only = false;
                self.event(
                    format!(
                        "Loaded {} library tracks{}",
                        self.library.tracks().len(),
                        if self.library.needs_ordering_metadata_refresh() {
                            " · reading album tags"
                        } else {
                            ""
                        }
                    ),
                    None,
                )
            }
            Err(error) => {
                self.library = Library::new();
                self.read_only = true;
                self.event(
                    "Library is read-only after a load error".into(),
                    Some(error),
                )
            }
        }
    }

    fn run(&mut self, command: LibraryCommand, cancel: &AtomicBool) -> LibraryEvent {
        let source = match &command {
            LibraryCommand::SyncSource { config, .. } => Some(config.id.clone()),
            _ => None,
        };
        let removal = match &command {
            LibraryCommand::RemoveSource(source) => Some(source.clone()),
            _ => None,
        };
        let authorized = match &command {
            LibraryCommand::AuthorizeSource { config, .. } => Some(config.id.clone()),
            _ => None,
        };
        let mut event = self.run_inner(command, cancel);
        event.synced_source = source;
        event.source_removal = removal;
        event.authorized_source = authorized;
        event
    }

    fn run_inner(&mut self, command: LibraryCommand, cancel: &AtomicBool) -> LibraryEvent {
        if matches!(command, LibraryCommand::Load) {
            return self.load();
        }
        if command.is_scan() {
            *self.scan_status.lock().unwrap_or_else(|e| e.into_inner()) = ScanStatus::default();
        }
        if self.read_only {
            let error = "Repair the library file and reload it before saving changes".to_owned();
            if command.is_scan() {
                *self.scan_status.lock().unwrap_or_else(|e| e.into_inner()) = ScanStatus {
                    progress: Some(ScanProgress {
                        finished: true,
                        ..ScanProgress::default()
                    }),
                    report: None,
                    failure: Some(error.clone()),
                };
            }
            return self.event("Library is read-only".into(), Some(error));
        }
        // Keep both the in-memory and on-disk catalogs unchanged if validation,
        // scanning cancellation, or persistence fails.
        let mut candidate = self.library.clone();
        let (status, warning) = match command {
            LibraryCommand::Scan(root) => {
                return self.scan(candidate, vec![root], cancel);
            }
            LibraryCommand::Rescan => {
                let roots = candidate.roots().to_vec();
                return self.scan(candidate, roots, cancel);
            }
            LibraryCommand::RemoveRoot(root) => match candidate.remove_root(&root) {
                Ok(count) => (
                    format!("Source removed · {count} songs removed from library · Files retained"),
                    None,
                ),
                Err(error) => return self.event("Unable to remove source".into(), Some(error)),
            },
            LibraryCommand::RemoveSource(source) => {
                let count = candidate.remove_source(&source);
                let mut event = self.commit(
                    candidate,
                    format!("Source removed · {count} songs removed from library"),
                    None,
                );
                if event.error.is_none() {
                    cadence_sources::SourceRegistry::builtin().forget_source_identity(&source);
                    event.removed_source = Some(source);
                }
                return event;
            }
            LibraryCommand::AuthorizeSource { config, cancel } => {
                if cancel.load(Ordering::Acquire) {
                    return self.event(
                        "Certificate authorization cancelled".into(),
                        Some("Certificate authorization cancelled".into()),
                    );
                }
                let registry = cadence_sources::SourceRegistry::builtin();
                let result = registry.authorize_source_identity(&config);
                if cancel.load(Ordering::Acquire) {
                    registry.forget_source_identity(&config.id);
                    return self.event(
                        "Certificate authorization cancelled".into(),
                        Some("Certificate authorization cancelled".into()),
                    );
                }
                return match result {
                    Ok(()) => self.event("Client certificate authorized".into(), None),
                    Err(error) => self.event(
                        "Client certificate authorization failed".into(),
                        Some(error),
                    ),
                };
            }
            LibraryCommand::SyncSource { config, control } => {
                if config.plugin_id != "kanade" {
                    return self.event(
                        "Source sync failed".into(),
                        Some("This source does not support library sync".into()),
                    );
                }
                let tracks = match cadence_sources::SourceRegistry::builtin()
                    .catalog_cancellable(&config, &control.cancelled)
                {
                    Ok(tracks) => tracks,
                    Err(_) if control.cancelled.load(Ordering::Acquire) => {
                        return self.event("Sync cancelled · Library unchanged".into(), None);
                    }
                    Err(error) => {
                        return self
                            .event("Source sync failed · Library unchanged".into(), Some(error));
                    }
                };
                return self.replace_source(&config.id, tracks, &control);
            }
            LibraryCommand::Upsert(tracks) => {
                let count = tracks.len();
                match candidate.upsert_many(tracks) {
                    Ok(added) => (format!("Imported {count} tracks ({added} new)"), None),
                    Err(error) => return self.event("Import failed".into(), Some(error)),
                }
            }
            LibraryCommand::Remove(id) => (
                if candidate.remove(&id) {
                    "Removed track from library".into()
                } else {
                    "Track was already absent from library".into()
                },
                None,
            ),
            LibraryCommand::Load => unreachable!("load was handled above"),
        };
        self.commit(candidate, status, warning)
    }

    fn replace_source(
        &mut self,
        source: &str,
        tracks: Vec<LibraryTrack>,
        control: &SourceSyncControl,
    ) -> LibraryEvent {
        let mut identities = std::collections::HashSet::new();
        if tracks
            .iter()
            .any(|track| track.source_id != source || !identities.insert(track.id.clone()))
        {
            return self.event(
                "Source sync failed · Library unchanged".into(),
                Some("Source returned duplicate tracks or a different source identity".into()),
            );
        }
        let count = tracks.len();
        let mut candidate = self.library.clone();
        candidate.remove_source(source);
        if let Err(error) = candidate.upsert_many(tracks) {
            return self.event("Source sync failed · Library unchanged".into(), Some(error));
        }
        if !control.begin_save() {
            return self.event("Sync cancelled · Library unchanged".into(), None);
        }
        self.commit(candidate, format!("Library synced · {count} songs"), None)
    }

    fn refresh_ordering_metadata(&mut self, cancel: &AtomicBool) -> LibraryEvent {
        let mut candidate = self.library.clone();
        let mut last_publish = None;
        let report = candidate.refresh_ordering_metadata(cancel, |progress| {
            let now = Instant::now();
            if progress.finished
                || last_publish
                    .is_none_or(|last| now.duration_since(last) >= Duration::from_millis(100))
            {
                self.scan_status
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .progress = Some(progress.clone());
                last_publish = Some(now);
            }
        });
        self.scan_status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .report = Some(report.clone());
        if report.cancelled {
            return self.event("Tag refresh cancelled; library unchanged".into(), None);
        }
        let status = format!(
            "Album tags refreshed: {} tracks, {} errors",
            report.updated, report.error_count
        );
        let warning = report
            .errors
            .first()
            .map(|error| format!("{}: {}", error.path.display(), error.message));
        if report.updated == 0 {
            return self.event(status, warning);
        }
        self.commit(candidate, status, warning)
    }

    fn scan(
        &mut self,
        mut candidate: Library,
        roots: Vec<PathBuf>,
        cancel: &AtomicBool,
    ) -> LibraryEvent {
        if roots.is_empty() {
            *self.scan_status.lock().unwrap_or_else(|e| e.into_inner()) = ScanStatus {
                progress: Some(ScanProgress {
                    finished: true,
                    ..ScanProgress::default()
                }),
                report: Some(ScanReport::default()),
                failure: None,
            };
            return self.event("No library folders to rescan".into(), None);
        }
        let mut last_publish = None;
        let mut published_path = false;
        let mut report =
            candidate.scan_with_progress(&roots, &ScanOptions::default(), cancel, |progress| {
                let now = Instant::now();
                if progress.finished
                    || (!published_path && progress.current_path.is_some())
                    || last_publish
                        .is_none_or(|last| now.duration_since(last) >= Duration::from_millis(100))
                {
                    self.scan_status
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .progress = Some(progress.clone());
                    last_publish = Some(now);
                    published_path |= progress.current_path.is_some();
                }
            });
        report.cancelled |= cancel.load(Ordering::Relaxed);
        {
            let mut status = self.scan_status.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(progress) = status.progress.as_mut() {
                progress.cancelled = report.cancelled;
            }
            status.report = Some(report.clone());
        }
        if report.cancelled {
            return self.event("Scan cancelled; library unchanged".into(), None);
        }
        let mut status = format!(
            "Scan complete: {} added, {} updated, {} errors",
            report.added, report.updated, report.error_count,
        );
        if report.limit_reached {
            status.push_str(" (scan limit reached)");
        }
        let warning = report
            .errors
            .first()
            .map(|error| format!("{}: {}", error.path.display(), error.message));
        match candidate.save(&self.path) {
            Ok(()) => {
                self.library = candidate;
                self.event(status, warning)
            }
            Err(error) => {
                self.scan_status
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .failure = Some(error.clone());
                self.event("Library changes could not be saved".into(), Some(error))
            }
        }
    }

    fn commit(
        &mut self,
        candidate: Library,
        status: String,
        warning: Option<String>,
    ) -> LibraryEvent {
        match candidate.save(&self.path) {
            Ok(()) => {
                self.library = candidate;
                self.event(status, warning)
            }
            Err(error) => self.event("Library changes could not be saved".into(), Some(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cadence_library::{MediaLocation, track_id};
    use std::sync::atomic::AtomicU64;
    use std::time::Duration;

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct Directory(PathBuf);

    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "cadence-library-worker-{}-{}",
                std::process::id(),
                NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed),
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn track(item_id: &str) -> LibraryTrack {
        LibraryTrack {
            id: track_id("test", item_id),
            source_id: "test".into(),
            item_id: item_id.into(),
            title: item_id.into(),
            artist: None,
            album: None,
            album_id: None,
            album_artist: None,
            track_number: None,
            disc_number: None,
            duration_seconds: None,
            location: MediaLocation::Provider,
            artwork: None,
        }
    }

    fn next(worker: &LibraryWorker) -> LibraryEvent {
        worker
            .events
            .as_ref()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
    }

    #[test]
    fn source_refresh_replaces_only_that_source_and_empty_refresh_removes_stale_tracks() {
        let directory = Directory::new();
        let path = directory.0.join("library.json");
        let mut state = LibraryState::new(path.clone());
        let mut other = track("other");
        other.source_id = "other-source".into();
        other.id = track_id(&other.source_id, &other.item_id);
        state
            .library
            .upsert_many([track("removed"), track("updated"), other.clone()])
            .unwrap();
        state.library.save(&path).unwrap();
        let mut updated = track("updated");
        updated.title = "Updated title".into();
        let event = state.replace_source(
            "test",
            vec![updated.clone(), track("new")],
            &SourceSyncControl::default(),
        );
        assert!(event.error.is_none());
        assert_eq!(event.tracks.len(), 3);
        assert!(event.tracks.contains(&updated));
        assert!(event.tracks.contains(&other));
        assert!(!event.tracks.iter().any(|entry| entry.item_id == "removed"));
        assert_eq!(Library::load(&path).unwrap().tracks(), event.tracks);
        let empty = state.replace_source("test", vec![], &SourceSyncControl::default());
        assert!(empty.error.is_none());
        assert!(empty.status.contains("0 songs"));
        assert_eq!(empty.tracks, vec![other]);
        assert_eq!(Library::load(&path).unwrap().tracks(), empty.tracks);
    }

    #[test]
    fn source_refresh_validation_cancellation_and_save_errors_preserve_previous_catalog() {
        let directory = Directory::new();
        let path = directory.0.join("library.json");
        let mut state = LibraryState::new(path.clone());
        state.library.upsert(track("existing")).unwrap();
        state.library.save(&path).unwrap();
        let original = std::fs::read(&path).unwrap();
        let cancelled = SourceSyncControl::default();
        assert!(cancelled.cancel());
        let event = state.replace_source("test", vec![], &cancelled);
        assert!(event.status.contains("cancelled"));
        assert_eq!(event.tracks, vec![track("existing")]);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let mut wrong_source = track("new");
        wrong_source.source_id = "different".into();
        let mut invalid = track("invalid");
        invalid.id = "invalid identity".into();
        for tracks in [
            vec![wrong_source],
            vec![invalid],
            vec![track("duplicate"), track("duplicate")],
        ] {
            let event = state.replace_source("test", tracks, &SourceSyncControl::default());
            assert!(event.error.is_some());
            assert_eq!(event.tracks, vec![track("existing")]);
            assert_eq!(std::fs::read(&path).unwrap(), original);
        }
        state.path = directory.0.clone(); // a directory cannot become a catalog file
        let event = state.replace_source("test", vec![], &SourceSyncControl::default());
        assert!(event.error.is_some());
        assert_eq!(event.tracks, vec![track("existing")]);
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn source_sync_read_only_rejection_reports_source_without_connecting() {
        let directory = Directory::new();
        let mut state = LibraryState::new(directory.0.join("library.json"));
        state.read_only = true;
        let event = state.run(
            LibraryCommand::SyncSource {
                config: cadence_sources::SourceConfig {
                    id: "protected".into(),
                    name: "Protected".into(),
                    plugin_id: "kanade".into(),
                    settings: serde_json::json!({"url":"invalid"}),
                },
                control: Arc::new(SourceSyncControl::default()),
            },
            &AtomicBool::new(false),
        );
        assert_eq!(event.synced_source.as_deref(), Some("protected"));
        assert!(event.error.unwrap().contains("Repair the library file"));
    }

    #[test]
    fn cancelled_or_invalid_certificate_authorization_never_mutates_the_catalog() {
        let directory = Directory::new();
        let path = directory.0.join("library.json");
        let mut state = LibraryState::new(path.clone());
        state.library.upsert(track("existing")).unwrap();
        state.library.save(&path).unwrap();
        let original = std::fs::read(&path).unwrap();
        // Cancellation is checked before the identity provider. Invalid URL
        // validation also finishes before any Keychain query or native prompt.
        for (url, cancelled) in [("https://example.invalid", true), ("invalid-url", false)] {
            let event = state.run(
                LibraryCommand::AuthorizeSource {
                    config: cadence_sources::SourceConfig {
                        id: "test".into(),
                        name: "Test".into(),
                        plugin_id: "kanade".into(),
                        settings: serde_json::json!({"url":url,"kanade_app_identity":true}),
                    },
                    cancel: Arc::new(AtomicBool::new(cancelled)),
                },
                &AtomicBool::new(false),
            );
            assert_eq!(event.authorized_source.as_deref(), Some("test"));
            assert!(event.error.is_some());
            assert_eq!(event.tracks, vec![track("existing")]);
            assert_eq!(std::fs::read(&path).unwrap(), original);
        }
    }

    #[test]
    fn source_sync_cannot_be_cancelled_after_saving_starts() {
        let control = SourceSyncControl::default();
        assert!(control.begin_save());
        assert!(control.is_saving());
        assert!(!control.cancel());
        assert!(!control.cancelled.load(Ordering::Acquire));
    }

    #[test]
    fn startup_refreshes_legacy_album_tags_without_requesting_a_rescan() {
        let directory = Directory::new();
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../cadence-core/tests/fixtures/tone-order.flac");
        let mut track = cadence_library::inspect_track(&fixture, &ScanOptions::default()).unwrap();
        track.track_number = None;
        track.disc_number = None;
        track.album_artist = None;
        let mut library = Library::new();
        library.upsert(track.clone()).unwrap();
        let mut old = serde_json::to_value(library).unwrap();
        old.as_object_mut()
            .unwrap()
            .remove("ordering_metadata_version");
        let path = directory.0.join("library.json");
        std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
        let worker = LibraryWorker::new(directory.0.clone());
        assert_eq!(next(&worker).tracks[0], track);
        let refreshed = next(&worker);
        assert_eq!(refreshed.tracks[0].track_number, Some(7));
        assert_eq!(refreshed.tracks[0].disc_number, Some(2));
        assert_eq!(refreshed.tracks[0].id, track.id);
        assert_eq!(Library::load(&path).unwrap().tracks(), refreshed.tracks);
        assert!(
            !Library::load(&path)
                .unwrap()
                .needs_ordering_metadata_refresh()
        );
        drop(worker);
        let reloaded = LibraryWorker::new(directory.0.clone());
        assert_eq!(next(&reloaded).tracks, refreshed.tracks);
        assert!(
            reloaded
                .events
                .as_ref()
                .unwrap()
                .recv_timeout(Duration::from_millis(100))
                .is_err()
        );
    }

    #[test]
    fn shutdown_flushes_queued_mutations_without_consuming_events() {
        let directory = Directory::new();
        let worker = LibraryWorker::new(directory.0.clone());
        // More requests than the event queue holds exercise shutdown while the
        // worker is blocked publishing a snapshot, including its initial load.
        for id in ["one", "two", "three", "four"] {
            worker
                .send(LibraryCommand::Upsert(vec![track(id)]))
                .unwrap();
        }
        worker
            .send(LibraryCommand::Remove(track_id("test", "one")))
            .unwrap();
        drop(worker);
        let saved = Library::load(&directory.0.join("library.json")).unwrap();
        assert_eq!(saved.tracks().len(), 3);
        assert!(saved.tracks().iter().all(|track| track.item_id != "one"));
        for id in ["two", "three", "four"] {
            assert!(saved.tracks().contains(&track(id)));
        }
    }

    #[test]
    fn worker_persists_ordered_mutations_and_reloads() {
        let directory = Directory::new();
        let worker = LibraryWorker::new(directory.0.clone());
        assert!(next(&worker).tracks.is_empty());
        worker
            .send(LibraryCommand::Upsert(vec![track("one"), track("two")]))
            .unwrap();
        worker
            .send(LibraryCommand::Remove(track_id("test", "one")))
            .unwrap();
        assert_eq!(next(&worker).tracks.len(), 2);
        let removed = next(&worker);
        assert_eq!(removed.tracks, vec![track("two")]);
        assert!(removed.error.is_none());
        let saved = Library::load(&directory.0.join("library.json")).unwrap();
        assert_eq!(saved.tracks(), &[track("two")]);
        worker.send(LibraryCommand::Load).unwrap();
        assert_eq!(next(&worker).tracks, saved.tracks());
        worker
            .send(LibraryCommand::Remove(track_id("test", "two")))
            .unwrap();
        assert!(next(&worker).tracks.is_empty());
    }

    #[test]
    fn corrupt_library_is_preserved_and_can_recover_after_repair() {
        let directory = Directory::new();
        let path = directory.0.join("library.json");
        std::fs::write(&path, b"corrupt catalog").unwrap();
        let worker = LibraryWorker::new(directory.0.clone());
        let loaded = next(&worker);
        assert!(loaded.read_only);
        assert!(loaded.error.is_some());
        worker
            .send(LibraryCommand::Scan(directory.0.clone()))
            .unwrap();
        let scan_refused = next(&worker);
        assert!(scan_refused.read_only);
        assert!(!worker.is_scanning());
        assert!(worker.scan_progress().unwrap().finished);
        assert!(worker.scan_report().is_none());
        assert_eq!(worker.scan_failure(), scan_refused.error);
        assert!(worker.scan_failure().unwrap().contains("Repair"));
        worker
            .send(LibraryCommand::Upsert(vec![track("one")]))
            .unwrap();
        let refused = next(&worker);
        assert!(refused.read_only);
        assert!(refused.tracks.is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), b"corrupt catalog");
        Library::new().save(&path).unwrap();
        worker.send(LibraryCommand::Load).unwrap();
        assert!(!next(&worker).read_only);
        worker
            .send(LibraryCommand::Upsert(vec![track("one")]))
            .unwrap();
        assert_eq!(next(&worker).tracks.len(), 1);
    }

    #[test]
    fn failed_save_retains_previous_memory_and_disk_catalog() {
        let directory = Directory::new();
        let path = directory.0.join("library.json");
        let mut state = LibraryState::new(path.clone());
        let cancel = AtomicBool::new(false);
        assert!(
            state
                .run(LibraryCommand::Upsert(vec![track("one")]), &cancel)
                .error
                .is_none()
        );
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let failed = state.run(LibraryCommand::Upsert(vec![track("two")]), &cancel);
        assert!(failed.error.is_some());
        assert_eq!(failed.tracks, vec![track("one")]);
        assert!(path.is_dir());
    }

    #[test]
    fn failed_scan_save_reports_failure_without_committing_discovered_tracks() {
        let directory = Directory::new();
        let data = directory.0.join("data");
        let music = directory.0.join("music");
        std::fs::create_dir(&music).unwrap();
        // A short real PCM wave file ensures the scan discovers a valid new
        // track before the deliberately blocked catalog save fails.
        let mut wav = b"RIFF".to_vec();
        wav.extend(196_u32.to_le_bytes());
        wav.extend(b"WAVEfmt ");
        wav.extend(16_u32.to_le_bytes());
        wav.extend(1_u16.to_le_bytes());
        wav.extend(1_u16.to_le_bytes());
        wav.extend(8000_u32.to_le_bytes());
        wav.extend(16000_u32.to_le_bytes());
        wav.extend(2_u16.to_le_bytes());
        wav.extend(16_u16.to_le_bytes());
        wav.extend(b"data");
        wav.extend(160_u32.to_le_bytes());
        wav.extend([0_u8; 160]);
        std::fs::write(music.join("new.wav"), wav).unwrap();

        let mut library = Library::new();
        library.upsert(track("previous")).unwrap();
        library.save(&data.join("library.json")).unwrap();
        let previous = std::fs::read(data.join("library.json")).unwrap();
        let worker = LibraryWorker::new(data.clone());
        assert_eq!(next(&worker).tracks, vec![track("previous")]);
        let preserved = directory.0.join("preserved-data");
        std::fs::rename(&data, &preserved).unwrap();
        std::fs::write(&data, b"blocks catalog directory creation").unwrap();

        worker.send(LibraryCommand::Scan(music)).unwrap();
        let failed = next(&worker);
        assert!(!worker.is_scanning());
        assert_eq!(worker.scan_failure(), failed.error);
        assert!(
            worker
                .scan_failure()
                .unwrap()
                .contains("create library directory")
        );
        assert_eq!(failed.tracks, vec![track("previous")]);
        assert!(failed.roots.is_empty());
        assert_eq!(
            std::fs::read(preserved.join("library.json")).unwrap(),
            previous
        );
        assert_eq!(
            std::fs::read(&data).unwrap(),
            b"blocks catalog directory creation"
        );
        let progress = worker.scan_progress().unwrap();
        let report = worker.scan_report().unwrap();
        assert!(progress.finished);
        assert!(!progress.cancelled && !report.cancelled);
        assert_eq!(progress.added, 1);
        assert_eq!(report.added, 1);
        assert_eq!(report.error_count, 0);

        // A subsequent accepted scan clears the failure and can commit once
        // storage is repaired; the prior failure was not a cancellation.
        std::fs::remove_file(&data).unwrap();
        std::fs::rename(&preserved, &data).unwrap();
        worker
            .send(LibraryCommand::Scan(directory.0.join("music")))
            .unwrap();
        assert_eq!(next(&worker).tracks.len(), 2);
        assert!(worker.scan_failure().is_none());
        assert_eq!(
            Library::load(&data.join("library.json"))
                .unwrap()
                .tracks()
                .len(),
            2
        );
    }

    #[test]
    fn cancelled_scan_does_not_persist_partial_changes() {
        let directory = Directory::new();
        let path = directory.0.join("library.json");
        let mut state = LibraryState::new(path.clone());
        state.run(
            LibraryCommand::Upsert(vec![track("previous")]),
            &AtomicBool::new(false),
        );
        let previous = std::fs::read(&path).unwrap();
        let event = state.run(
            LibraryCommand::Scan(directory.0.clone()),
            &AtomicBool::new(true),
        );
        assert!(event.status.contains("cancelled"));
        assert_eq!(event.tracks, vec![track("previous")]);
        assert!(event.roots.is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), previous);
        let scan = state.scan_status.lock().unwrap();
        let progress = scan.progress.as_ref().unwrap();
        assert!(progress.cancelled && progress.finished);
        assert_eq!(progress.entries_seen, 0);
        assert!(scan.report.as_ref().unwrap().cancelled);
    }

    #[test]
    fn worker_keeps_final_progress_and_detailed_report_available() {
        let directory = Directory::new();
        let worker = LibraryWorker::new(directory.0.clone());
        next(&worker);
        assert!(!worker.is_scanning());
        assert!(worker.scan_progress().is_none());
        let missing = directory.0.join("missing-folder");
        worker.send(LibraryCommand::Scan(missing.clone())).unwrap();
        let event = next(&worker);
        assert!(event.error.is_some());
        assert!(!worker.is_scanning());
        assert!(worker.scan_failure().is_none());
        let progress = worker.scan_progress().unwrap();
        assert!(progress.finished);
        assert_eq!(progress.entries_seen, 1);
        assert_eq!(progress.audio_files_processed, 0);
        assert_eq!(progress.error_count, 1);
        assert_eq!(progress.current_path.as_ref(), Some(&missing));
        let report = worker.scan_report().unwrap();
        assert_eq!(report.error_count, 1);
        assert_eq!(report.errors[0].path, missing);
        assert!(!report.errors[0].message.is_empty());
        worker.send(LibraryCommand::Load).unwrap();
        next(&worker);
        assert_eq!(worker.scan_progress(), Some(progress));
        assert_eq!(worker.scan_report(), Some(report));
        worker.send(LibraryCommand::Rescan).unwrap();
        assert!(next(&worker).status.contains("No library folders"));
        assert!(worker.scan_progress().unwrap().finished);
        assert_eq!(worker.scan_report(), Some(ScanReport::default()));
    }

    #[test]
    fn worker_removes_root_tracks_but_preserves_other_sources_and_audio_files() {
        let directory = Directory::new();
        let root = directory.0.join("music");
        std::fs::create_dir(&root).unwrap();
        let audio = root.join("keep.wav");
        std::fs::write(&audio, b"keep existing file").unwrap();
        let mut library = Library::new();
        library.set_roots(vec![root.clone()]).unwrap();
        let mut local = track("removed");
        local.source_id = "local".into();
        local.item_id = audio.to_string_lossy().into_owned();
        local.id = track_id("local", &local.item_id);
        local.location = MediaLocation::Local(audio.clone());
        library.upsert(local).unwrap();
        library.upsert(track("previous")).unwrap();
        library.save(&directory.0.join("library.json")).unwrap();
        let worker = LibraryWorker::new(directory.0.clone());
        assert_eq!(next(&worker).roots, vec![root.clone()]);
        worker.send(LibraryCommand::RemoveRoot(root)).unwrap();
        let event = next(&worker);
        assert!(event.error.is_none());
        assert!(event.roots.is_empty());
        assert_eq!(event.tracks, vec![track("previous")]);
        assert_eq!(std::fs::read(&audio).unwrap(), b"keep existing file");
        let saved = Library::load(&directory.0.join("library.json")).unwrap();
        assert!(saved.roots().is_empty());
        assert_eq!(saved.tracks(), event.tracks);
    }

    #[test]
    fn bounded_requests_report_busy_and_preserve_scan_cancellation() {
        let (requests, receiver) = mpsc::sync_channel(1);
        let (_sender, events) = mpsc::sync_channel(1);
        let worker = LibraryWorker {
            requests: Some(requests),
            events: Some(events),
            scan_cancel: Arc::new(AtomicBool::new(false)),
            scan_pending: Arc::new(AtomicBool::new(false)),
            scan_status: Arc::new(Mutex::new(ScanStatus {
                progress: Some(ScanProgress {
                    finished: true,
                    ..ScanProgress::default()
                }),
                report: Some(ScanReport::default()),
                failure: Some("previous scan failure".into()),
            })),
            stopped: Arc::new(AtomicBool::new(false)),
            worker: None,
        };
        worker.send(LibraryCommand::Load).unwrap();
        assert!(worker.send(LibraryCommand::Rescan).is_err());
        assert!(!worker.scan_pending.load(Ordering::Acquire));
        assert!(worker.scan_progress().unwrap().finished);
        assert!(worker.scan_report().is_some());
        assert_eq!(
            worker.scan_failure().as_deref(),
            Some("previous scan failure")
        );
        receiver.try_recv().unwrap();
        worker.send(LibraryCommand::Rescan).unwrap();
        assert!(worker.is_scanning());
        assert!(worker.scan_progress().is_none());
        assert!(worker.scan_report().is_none());
        assert!(worker.scan_failure().is_none());
        worker.cancel_scan();
        assert!(worker.send(LibraryCommand::Rescan).is_err());
        assert!(worker.scan_cancel.load(Ordering::Relaxed));
        receiver.try_recv().unwrap();
        drop(receiver);
        assert!(worker.send(LibraryCommand::Load).is_err());
    }
}

#[cfg(test)]
mod source_removal_tests {
    use super::*;
    use cadence_library::{MediaLocation, track_id};
    fn entry(source: &str) -> LibraryTrack {
        LibraryTrack {
            id: track_id(source, "song"),
            source_id: source.into(),
            item_id: "song".into(),
            title: "Song".into(),
            artist: None,
            album: None,
            album_id: None,
            album_artist: None,
            track_number: None,
            disc_number: None,
            duration_seconds: None,
            location: MediaLocation::Provider,
            artwork: None,
        }
    }
    #[test]
    fn source_removal_is_published_only_after_persistence_succeeds() {
        let directory =
            std::env::temp_dir().join(format!("cadence-source-removal-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("library.json");
        let mut state = LibraryState::new(path.clone());
        let cancel = AtomicBool::new(false);
        state.run(
            LibraryCommand::Upsert(vec![entry("remove"), entry("keep")]),
            &cancel,
        );
        let before = std::fs::read(&path).unwrap();
        state.path = directory.clone(); // A directory cannot be replaced by a catalog.
        let rejected = state.run(LibraryCommand::RemoveSource("remove".into()), &cancel);
        assert!(rejected.error.is_some());
        assert!(rejected.removed_source.is_none());
        assert_eq!(rejected.tracks.len(), 2);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        state.path = path.clone();
        let committed = state.run(LibraryCommand::RemoveSource("remove".into()), &cancel);
        assert!(committed.error.is_none());
        assert_eq!(committed.removed_source.as_deref(), Some("remove"));
        assert_eq!(committed.tracks, vec![entry("keep")]);
        assert_eq!(Library::load(&path).unwrap().tracks(), committed.tracks);
        std::fs::remove_dir_all(directory).unwrap();
    }
}

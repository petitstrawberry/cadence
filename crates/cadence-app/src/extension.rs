//! Application-owned catalog/source state. Blocking I/O stays on cancellable workers.
use crate::{
    Cadence, button, caption,
    jobs::{IdentityRequest, LibraryCommand, LibraryWorker, SourceSyncControl},
    persistence::{AppConfig, ConfigWriter},
    state, text, truncate,
    ui::*,
};
use cadence_core::{Command, Track};
use cadence_library::{ArtworkBitmap, ArtworkRef, LibraryTrack, MediaLocation};
use cadence_sources::{SourceConfig, SourceRegistry};
use scarlet_ui::{hstack, prelude::*, views::BitmapImage, vstack};
use std::{
    any::Any,
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Albums,
    Artists,
    Playing,
    Library,
    Sources,
    Effects,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SourcePage {
    Overview,
    Connect,
    Browse,
    Certificate,
}

/// Stable source identity; folder roots and remote providers share navigation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderId {
    Local(PathBuf),
    Remote(String),
}

#[derive(Clone)]
pub enum SourceItem {
    Folder { id: String, title: String },
    Track(Arc<LibraryTrack>),
}
enum NetworkResult {
    Browse {
        source: String,
        parent: Option<String>,
        result: std::result::Result<Vec<SourceItem>, String>,
    },
}
enum NetworkRequest {
    Browse(SourceConfig, Option<String>),
}
struct NetworkJob {
    handle: std::thread::JoinHandle<()>,
    cancel: Arc<AtomicBool>,
    receiver: mpsc::Receiver<NetworkResult>,
}
struct ArtResult {
    path: PathBuf,
    metadata: Option<LibraryTrack>,
    result: std::result::Result<Option<ArtworkBitmap>, String>,
}
struct ArtJob {
    handle: std::thread::JoinHandle<()>,
    cancel: Arc<AtomicBool>,
    receiver: mpsc::Receiver<ArtResult>,
}
#[derive(Clone)]
pub struct Workspace {
    pub tab: State<Tab>,
    pub source_page: State<SourcePage>,
    pub library: State<Vec<LibraryTrack>>,
    pub filtered: State<Vec<LibraryTrack>>,
    pub library_selection: State<Option<usize>>,
    pub query: State<String>,
    last_query: Rc<RefCell<String>>,
    pub library_status: State<String>,
    pub(crate) roots: State<Vec<PathBuf>>,
    pub(crate) read_only: State<bool>,
    pub folders: crate::library_folders::FolderState,
    pub sources: State<Vec<SourceConfig>>,
    pub source_selection: State<Option<ProviderId>>,
    pub source_entries: State<Vec<SourceItem>>,
    pub item_selection: State<Option<usize>>,
    pub source_name: State<String>,
    pub source_url: State<String>,
    pub source_kind: State<usize>,
    source_kanade_identity: State<bool>,
    pub(crate) certificate_path: State<String>,
    certificate_secret: crate::secret_field::SecretInput,
    certificate_remember: State<bool>,
    certificate_removing: Rc<Cell<bool>>,
    pub source_status: State<String>,
    source_sync: State<Option<String>>,
    source_sync_saving: State<bool>,
    source_authorizing: State<Option<String>>,
    authorization_config: Rc<RefCell<Option<SourceConfig>>>,
    authorization_cancel: Rc<RefCell<Option<Arc<AtomicBool>>>>,
    sync_statuses: State<HashMap<String, String>>,
    sync_control: Rc<RefCell<Option<Arc<SourceSyncControl>>>>,
    pending_sync: Rc<RefCell<VecDeque<String>>>,
    startup_sync: Rc<Cell<bool>>,
    pending_removals: Rc<RefCell<HashSet<String>>>,
    pub warning: State<String>,
    history: Rc<RefCell<Vec<Option<String>>>>,
    folder_titles: Rc<RefCell<HashMap<String, String>>>,
    active_source: Rc<RefCell<Option<String>>>,
    network: Rc<RefCell<Option<NetworkJob>>>,
    pending_network: Rc<RefCell<Option<NetworkRequest>>>,
    art_job: Rc<RefCell<Option<ArtJob>>>,
    pending_art: Rc<RefCell<Option<PathBuf>>>,
    artwork_path: Rc<RefCell<Option<PathBuf>>>,
    pub artwork: Rc<RefCell<Option<BitmapImage>>>,
    pub artwork_revision: State<u64>,
    metadata: Rc<RefCell<HashMap<PathBuf, LibraryTrack>>>,
    directory: PathBuf,
    worker: Option<Rc<LibraryWorker>>,
    config: Rc<RefCell<AppConfig>>,
    writer: Option<Rc<ConfigWriter>>,
    dirty: Rc<Cell<bool>>,
    config_writable: bool,
    save_after: Rc<Cell<Instant>>,
}
impl Workspace {
    pub(crate) fn configure_certificate_clipboard(
        &self,
        clipboard: Option<scarlet_ui::clipboard::TextClipboard>,
    ) {
        self.certificate_secret.set_clipboard(clipboard);
    }
    pub fn memory() -> Self {
        Self {
            tab: state(Tab::Albums),
            source_page: state(SourcePage::Overview),
            library: state(vec![]),
            filtered: state(vec![]),
            library_selection: state(None),
            query: state(String::new()),
            last_query: Rc::new(RefCell::new(String::new())),
            library_status: state("Import a folder to build your library".into()),
            roots: state(vec![]),
            read_only: state(false),
            folders: crate::library_folders::FolderState::new(),
            sources: state(vec![]),
            source_selection: state(None),
            source_entries: state(vec![]),
            item_selection: state(None),
            source_name: state(String::new()),
            source_url: state(String::new()),
            source_kind: state(0),
            source_kanade_identity: state(false),
            certificate_path: state(String::new()),
            certificate_secret: crate::secret_field::SecretInput::new(),
            certificate_remember: state(false),
            certificate_removing: Rc::new(Cell::new(false)),
            source_status: state(String::new()),
            source_sync: state(None),
            source_sync_saving: state(false),
            source_authorizing: state(None),
            authorization_config: Rc::new(RefCell::new(None)),
            authorization_cancel: Rc::new(RefCell::new(None)),
            sync_statuses: state(HashMap::new()),
            sync_control: Rc::new(RefCell::new(None)),
            pending_sync: Rc::new(RefCell::new(VecDeque::new())),
            startup_sync: Rc::new(Cell::new(false)),
            pending_removals: Rc::new(RefCell::new(HashSet::new())),
            warning: state(String::new()),
            history: Rc::new(RefCell::new(vec![])),
            folder_titles: Rc::new(RefCell::new(HashMap::new())),
            active_source: Rc::new(RefCell::new(None)),
            network: Rc::new(RefCell::new(None)),
            pending_network: Rc::new(RefCell::new(None)),
            art_job: Rc::new(RefCell::new(None)),
            pending_art: Rc::new(RefCell::new(None)),
            artwork_path: Rc::new(RefCell::new(None)),
            artwork: Rc::new(RefCell::new(None)),
            artwork_revision: state(0),
            metadata: Rc::new(RefCell::new(HashMap::new())),
            directory: PathBuf::new(),
            worker: None,
            config: Rc::new(RefCell::new(AppConfig::default())),
            writer: None,
            dirty: Rc::new(Cell::new(false)),
            config_writable: false,
            save_after: Rc::new(Cell::new(Instant::now())),
        }
    }
    pub fn persistent(directory: PathBuf, loaded: crate::persistence::LoadedConfig) -> Self {
        let mut this = Self::memory();
        this.config_writable = loaded.writable;
        this.sources.set(loaded.config.sources.clone());
        this.warning.set(loaded.error.unwrap_or_default());
        this.config = Rc::new(RefCell::new(loaded.config));
        this.worker = Some(Rc::new(LibraryWorker::new(directory.clone())));
        this.writer = Some(Rc::new(ConfigWriter::new(
            directory.clone(),
            loaded.writable,
        )));
        this.directory = directory;
        this.startup_sync.set(true);
        this
    }
    pub fn media_cache_directory(&self) -> PathBuf {
        self.directory.join("cache")
    }
    pub fn save_effects(&self, effects: cadence_effects::RackConfig) {
        self.config.borrow_mut().effects = effects;
        self.dirty.set(true);
    }
    fn save_sources(&self) {
        self.config.borrow_mut().sources = self.sources.get();
        self.dirty.set(true);
    }
    pub fn library_command(&self, command: LibraryCommand) {
        self.send_library_command(command);
    }
    fn send_library_command(&self, command: LibraryCommand) -> bool {
        if let Some(worker) = &self.worker {
            let is_scan = matches!(&command, LibraryCommand::Scan(_) | LibraryCommand::Rescan);
            let scan_root = match &command {
                LibraryCommand::Scan(path) => Some(path.clone()),
                _ => None,
            };
            match worker.send(command) {
                Ok(()) => {
                    if is_scan {
                        self.folders.active_root.set(scan_root);
                        self.folders.scan_started();
                    }
                    self.library_status.set("Updating library…".into());
                    return true;
                }
                Err(error) => self.library_status.set(error),
            }
        } else {
            self.library_status
                .set("Library storage is unavailable in this session".into());
        }
        false
    }
    pub fn cancel(&self) {
        self.certificate_secret.clear();
        if let Some(worker) = &self.worker {
            worker.cancel_scan();
        }
        self.cancel_network();
        self.cancel_source_sync(None);
        if let Some(cancel) = self.authorization_cancel.borrow().as_ref() {
            cancel.store(true, Ordering::Release);
        }
        self.source_status.set("Cancelled".into());
    }
    pub fn shutdown(&self) {
        if self.dirty.get()
            && self.config_writable
            && let Some(writer) = &self.writer
            && let Err(error) = writer.shutdown_save(self.config.borrow().clone())
        {
            self.warning.set(error);
        }
        self.cancel();
        self.pending_art.borrow_mut().take();
        if let Some(job) = self.art_job.borrow().as_ref() {
            job.cancel.store(true, Ordering::Relaxed);
        }
    }
    pub fn add_source(&self, app: &Cadence) {
        if self.source_kind.get() == 0 {
            app.import_folder();
            return;
        }
        let endpoint = self.source_url.get().trim().to_owned();
        let name = self.source_name.get().trim().to_owned();
        if endpoint.is_empty() {
            self.source_status
                .set("Enter the source address first".into());
            return;
        }
        let endpoint = if self.source_kind.get() == 1 {
            match cadence_sources::normalize_kanade_url(&endpoint) {
                Ok(url) => url,
                Err(error) => {
                    self.source_status.set(error);
                    return;
                }
            }
        } else {
            endpoint
        };
        let (plugin_id, settings) = match self.source_kind.get() {
            2 => ("dlna", serde_json::json!({"url":endpoint})),
            3 => (
                "process",
                serde_json::json!({"executable":endpoint,"args":[]}),
            ),
            _ => (
                "kanade",
                serde_json::json!({"url":endpoint, "client_certificate": self.source_kanade_identity.get()}),
            ),
        };
        if plugin_id == "process" && !PathBuf::from(&endpoint).is_absolute() {
            self.source_status
                .set("Source plugin executable must be an absolute path".into());
            return;
        }
        let mut sources = self.sources.get();
        let ordinal = sources.len() + 1;
        // A removed connection must never give a different server the identity
        // still referenced by saved tracks.
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let mut serial = 0u64;
        let id = loop {
            let candidate = format!("{plugin_id}-{epoch:x}-{serial:x}");
            if !sources.iter().any(|source| source.id == candidate)
                && !self
                    .library
                    .get()
                    .iter()
                    .any(|track| track.source_id == candidate)
            {
                break candidate;
            }
            serial += 1;
        };
        let config = SourceConfig {
            id,
            name: if name.is_empty() {
                format!("{plugin_id} {ordinal}")
            } else {
                name
            },
            plugin_id: plugin_id.into(),
            settings,
        };
        if let Err(error) = SourceRegistry::builtin().validate_config(&config) {
            self.source_status.set(error);
            return;
        }
        if !self.config_writable {
            self.source_status
                .set("Repair the app settings and restart before adding a source".into());
            return;
        }
        sources.push(config.clone());
        self.folder_titles.borrow_mut().clear();
        self.source_selection
            .set(Some(ProviderId::Remote(config.id.clone())));
        self.sources.set(sources);
        self.save_sources();
        self.source_url.set(String::new());
        self.source_name.set(String::new());
        self.source_kanade_identity.set(false);
        self.history.borrow_mut().clear();
        self.source_page.set(SourcePage::Browse);
        if config
            .settings
            .get("client_certificate")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            self.open_certificate_setup();
        } else {
            self.request_source_sync(&config);
            self.browse(app, config, None);
        }
    }
    fn remove_selected_source(&self) {
        let Some(provider) = self.source_selection.get() else {
            return;
        };
        if self.read_only.get() || self.folders.busy.get() {
            return;
        }
        let removing_source = match &provider {
            ProviderId::Remote(id) => Some(id.clone()),
            _ => None,
        };
        if removing_source.as_ref().is_some_and(|id| {
            self.pending_removals.borrow().contains(id)
                || self.source_authorizing.get().as_ref() == Some(id)
        }) {
            return;
        }
        let command = match provider {
            ProviderId::Local(path) => LibraryCommand::RemoveRoot(path),
            ProviderId::Remote(id) => {
                if !self.config_writable {
                    return;
                }
                self.cancel_source_sync(Some(&id));
                let credential_config = self.selected_source().filter(|source| {
                    source.plugin_id == "kanade"
                        && (source.settings.get("credential_store").is_some()
                            || source.settings.get("certificate_path").is_some())
                });
                LibraryCommand::RemoveSource {
                    source: id,
                    credential_config,
                }
            }
        };
        if !self.send_library_command(command) {
            self.source_status.set(self.library_status.get());
            return;
        }
        if let Some(id) = removing_source {
            self.pending_removals.borrow_mut().insert(id);
        }
        self.cancel_network();
        self.active_source.borrow_mut().take();
        self.source_selection.set(None);
        self.source_entries.set(vec![]);
        self.item_selection.set(None);
        self.history.borrow_mut().clear();
        self.source_status
            .set("Removing source and its library songs…".into());
        self.source_page.set(SourcePage::Overview);
    }
    fn toggle_kanade_identity(&self, app: &Cadence) {
        let Some(mut config) = self
            .selected_source()
            .filter(|source| source.plugin_id == "kanade")
        else {
            return;
        };
        if self.pending_removals.borrow().contains(&config.id)
            || self.source_authorizing.get().as_ref() == Some(&config.id)
            || !self.config_writable
            || self.read_only.get()
        {
            return;
        }
        if self.source_sync.get().as_ref() == Some(&config.id)
            && self
                .sync_control
                .borrow()
                .as_ref()
                .is_some_and(|control| control.is_saving())
        {
            self.set_sync_status(
                &config.id,
                "Wait for the library save before changing the connection".into(),
            );
            return;
        }
        let enabled = config
            .settings
            .get("client_certificate")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let Some(settings) = config.settings.as_object_mut() else {
            self.source_status
                .set("Repair this source's settings before reconnecting".into());
            return;
        };
        settings.remove("kanade_app_identity");
        settings.insert("client_certificate".into(), (!enabled).into());
        if let Err(error) = SourceRegistry::builtin().validate_config(&config) {
            self.set_sync_status(&config.id, format!("Connection unchanged · {error}"));
            return;
        }
        self.cancel_source_sync(Some(&config.id));
        if enabled {
            SourceRegistry::builtin().forget_source_identity(&config.id);
        }
        let mut sources = self.sources.get();
        if let Some(source) = sources.iter_mut().find(|source| source.id == config.id) {
            *source = config.clone();
        }
        self.sources.set(sources);
        self.save_sources();
        // The previous request may still be unwinding. Keep one retry behind
        // it; polling resolves the current config rather than a stale copy.
        self.pending_sync.borrow_mut().push_back(config.id.clone());
        self.set_sync_status(&config.id, "Reconnecting and syncing library…".into());
        let parent = self.history.borrow().last().cloned().flatten();
        self.browse(app, config, parent);
    }
    fn open_certificate_setup(&self) {
        let Some(config) = self
            .selected_source()
            .filter(|source| source.plugin_id == "kanade")
        else {
            return;
        };
        self.certificate_secret.clear();
        self.certificate_remember.set(false);
        self.certificate_path.set(
            config
                .settings
                .get("certificate_path")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .into(),
        );
        self.source_status.set(String::new());
        self.source_page.set(SourcePage::Certificate);
    }
    /// Explicit user action; background sync never loads a file or Keychain identity.
    fn authorize_kanade_identity(&self, saved: bool) {
        let Some(mut config) = self
            .selected_source()
            .filter(|source| source.plugin_id == "kanade")
        else {
            return;
        };
        if self.pending_removals.borrow().contains(&config.id)
            || self.source_authorizing.get().is_some()
            || self.read_only.get()
            || !self.config_writable
        {
            return;
        }
        if config
            .settings
            .get("client_certificate")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        {
            self.set_sync_status(&config.id, "Enable the client certificate first".into());
            return;
        }
        let request = if saved {
            IdentityRequest::Unlock {
                password: self.certificate_secret.take(),
            }
        } else {
            let path = PathBuf::from(self.certificate_path.get().trim());
            if !path.is_absolute() {
                self.source_status
                    .set("Choose a PKCS#12 file or enter its absolute path".into());
                return;
            }
            config
                .settings
                .as_object_mut()
                .unwrap()
                .remove("certificate_path");
            config.settings["credential_store"] = self
                .directory
                .join("credentials")
                .to_string_lossy()
                .into_owned()
                .into();
            IdentityRequest::Import {
                path,
                password: self.certificate_secret.take(),
                remember: true,
            }
        };
        self.certificate_removing.set(false);
        self.begin_identity_request(config, request);
    }
    fn remove_saved_certificate(&self) {
        let Some(config) = self
            .selected_source()
            .filter(|source| source.plugin_id == "kanade")
        else {
            return;
        };
        if self.source_authorizing.get().is_some()
            || self.pending_removals.borrow().contains(&config.id)
            || self.read_only.get()
            || !self.config_writable
        {
            return;
        }
        self.certificate_secret.clear();
        self.certificate_removing.set(true);
        self.begin_identity_request(config, IdentityRequest::RemoveSaved);
    }
    fn begin_identity_request(&self, config: SourceConfig, request: IdentityRequest) {
        self.cancel_source_sync(Some(&config.id));
        let cancel = Arc::new(AtomicBool::new(false));
        let result = self
            .worker
            .as_ref()
            .ok_or_else(|| "Library storage is unavailable".to_owned())
            .and_then(|worker| {
                worker.send(LibraryCommand::AuthorizeSource {
                    config: config.clone(),
                    request,
                    cancel: cancel.clone(),
                })
            });
        match result {
            Ok(()) => {
                let mut sources = self.sources.get();
                if let Some(source) = sources.iter_mut().find(|source| source.id == config.id) {
                    *source = config.clone();
                }
                self.sources.set(sources);
                self.save_sources();
                self.source_authorizing.set(Some(config.id.clone()));
                *self.authorization_config.borrow_mut() = Some(config.clone());
                *self.authorization_cancel.borrow_mut() = Some(cancel);
                let status = if self.certificate_removing.get() {
                    "Removing saved certificate…"
                } else {
                    "Unlocking client certificate…"
                };
                self.source_status.set(status.into());
                self.set_sync_status(&config.id, status.into());
            }
            Err(error) => self.source_status.set(error),
        }
    }
    fn set_sync_status(&self, source: &str, status: String) {
        let mut statuses = self.sync_statuses.get();
        if statuses.get(source) != Some(&status) {
            statuses.insert(source.to_owned(), status);
            self.sync_statuses.set(statuses);
        }
    }
    fn request_source_sync(&self, config: &SourceConfig) {
        if config.plugin_id != "kanade"
            || self.pending_removals.borrow().contains(&config.id)
            || self.source_authorizing.get().as_ref() == Some(&config.id)
        {
            return;
        }
        if self.read_only.get() || !self.config_writable {
            self.set_sync_status(&config.id, "Library is read-only · Sync unavailable".into());
            return;
        }
        if self.source_sync.get().as_ref() == Some(&config.id)
            || self.pending_sync.borrow().contains(&config.id)
        {
            return;
        }
        self.pending_sync.borrow_mut().push_back(config.id.clone());
        self.set_sync_status(&config.id, "Waiting to sync library…".into());
    }
    fn cancel_source_sync(&self, source: Option<&str>) {
        self.pending_sync
            .borrow_mut()
            .retain(|id| source.is_some_and(|source| source != id));
        if let Some(active) = self.source_sync.get()
            && source.is_none_or(|source| source == active)
            && let Some(control) = self.sync_control.borrow().as_ref()
        {
            if control.cancel() {
                self.set_sync_status(&active, "Cancelling sync…".into());
            } else {
                self.set_sync_status(&active, "Saving library…".into());
            }
        }
    }
    fn poll_source_sync(&self) {
        if let Some(active) = self.source_sync.get() {
            let saving = self
                .sync_control
                .borrow()
                .as_ref()
                .is_some_and(|control| control.is_saving());
            if saving != self.source_sync_saving.get() {
                self.source_sync_saving.set(saving);
                if saving {
                    self.set_sync_status(&active, "Saving library…".into());
                    self.library_status.set("Saving synced library…".into());
                }
            }
            return;
        }
        let Some(id) = self.pending_sync.borrow_mut().pop_front() else {
            return;
        };
        if self.pending_removals.borrow().contains(&id) {
            return;
        }
        let Some(config) = self
            .sources
            .get()
            .into_iter()
            .find(|config| config.id == id)
        else {
            return;
        };
        if self.read_only.get() || !self.config_writable {
            self.set_sync_status(&id, "Library is read-only · Sync unavailable".into());
            return;
        }
        let control = Arc::new(SourceSyncControl::default());
        let name = config.name.clone();
        let result = self
            .worker
            .as_ref()
            .ok_or_else(|| "Library storage is unavailable".to_owned())
            .and_then(|worker| {
                worker.send(LibraryCommand::SyncSource {
                    config,
                    control: control.clone(),
                })
            });
        match result {
            Ok(()) => {
                self.source_sync.set(Some(id.clone()));
                self.source_sync_saving.set(false);
                *self.sync_control.borrow_mut() = Some(control);
                self.set_sync_status(&id, "Syncing library…".into());
                self.library_status.set(format!("Syncing {name} library…"));
            }
            Err(error) => self.set_sync_status(&id, error),
        }
    }
    fn selected_source(&self) -> Option<SourceConfig> {
        let ProviderId::Remote(id) = self.source_selection.get()? else {
            return None;
        };
        self.sources
            .get()
            .into_iter()
            .find(|source| source.id == id)
    }
    pub fn open_provider(&self, app: &Cadence, provider: ProviderId) {
        self.certificate_secret.clear();
        self.cancel_network();
        self.active_source.borrow_mut().take();
        self.history.borrow_mut().clear();
        self.folder_titles.borrow_mut().clear();
        self.source_selection.set(Some(provider));
        self.source_page.set(SourcePage::Browse);
        self.browse_provider(app, None);
    }
    pub fn add_folder_source(&self, app: &Cadence, path: PathBuf) {
        self.open_provider(app, ProviderId::Local(path.clone()));
        self.library_command(LibraryCommand::Scan(path));
    }
    fn browse_provider(&self, app: &Cadence, parent: Option<String>) {
        match self.source_selection.get() {
            Some(ProviderId::Remote(_)) => {
                if let Some(config) = self.selected_source() {
                    self.browse(app, config, parent);
                }
            }
            Some(ProviderId::Local(root)) => {
                self.populate_local_source(&root, parent.as_deref());
                if self.history.borrow().last() != Some(&parent) {
                    self.history.borrow_mut().push(parent);
                }
            }
            None => {}
        }
    }
    fn back_in_source(&self, app: &Cadence) {
        if self.history.borrow().len() > 1 {
            let parent = {
                let mut history = self.history.borrow_mut();
                history.pop();
                history.last().cloned().flatten()
            };
            self.browse_provider(app, parent);
        } else {
            self.source_page.set(SourcePage::Overview);
        }
    }
    fn current_source_title(&self, root_title: String) -> String {
        self.history
            .borrow()
            .last()
            .and_then(|id| id.as_ref())
            .and_then(|id| self.folder_titles.borrow().get(id).cloned())
            .unwrap_or(root_title)
    }
    fn remember_folders(&self, entries: &[SourceItem]) {
        for entry in entries {
            if let SourceItem::Folder { id, title } = entry {
                self.folder_titles
                    .borrow_mut()
                    .insert(id.clone(), title.clone());
            }
        }
    }
    fn populate_local_source(&self, root: &std::path::Path, parent: Option<&str>) {
        // Browse the indexed catalog, never the filesystem on the UI thread.
        let directory = parent
            .map(PathBuf::from)
            .filter(|path| path.starts_with(root))
            .unwrap_or_else(|| root.to_owned());
        let mut folders = std::collections::BTreeMap::new();
        let mut tracks = Vec::new();
        for track in self.library.get() {
            let MediaLocation::Local(path) = &track.location else {
                continue;
            };
            let Ok(relative) = path.strip_prefix(&directory) else {
                continue;
            };
            let mut parts = relative.components();
            let Some(first) = parts.next() else {
                continue;
            };
            if parts.next().is_some() {
                let path = directory.join(first);
                folders
                    .entry(path.to_string_lossy().into_owned())
                    .or_insert_with(|| first.as_os_str().to_string_lossy().into_owned());
            } else {
                tracks.push(track);
            }
        }
        tracks.sort_by(|a, b| {
            a.disc_number
                .cmp(&b.disc_number)
                .then(a.track_number.cmp(&b.track_number))
                .then(a.title.cmp(&b.title))
        });
        let entries = folders
            .into_iter()
            .map(|(id, title)| SourceItem::Folder { id, title })
            .chain(
                tracks
                    .into_iter()
                    .map(|track| SourceItem::Track(Arc::new(track))),
            )
            .collect::<Vec<_>>();
        self.remember_folders(&entries);
        self.source_status
            .set(format!("{} items · {}", entries.len(), directory.display()));
        self.source_entries.set(entries);
        self.item_selection.set(None);
    }
    pub fn browse(&self, _app: &Cadence, config: SourceConfig, parent: Option<String>) {
        self.source_entries.set(vec![]);
        self.item_selection.set(None);
        *self.active_source.borrow_mut() = Some(config.id.clone());
        if self.defer_network(NetworkRequest::Browse(config.clone(), parent.clone())) {
            return;
        }
        self.source_status.set(format!("Reading {}…", config.name));
        let (sender, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let token = cancel.clone();
        match std::thread::Builder::new()
            .name("cadence-source".into())
            .spawn(move || {
                let result = SourceRegistry::builtin()
                    .browse_cancellable(&config, parent.as_deref(), &token)
                    .map(|page| {
                        page.containers
                            .into_iter()
                            .map(|entry| SourceItem::Folder {
                                id: entry.id,
                                title: entry.title,
                            })
                            .chain(
                                page.tracks
                                    .into_iter()
                                    .map(|track| SourceItem::Track(Arc::new(track))),
                            )
                            .collect()
                    });
                if !token.load(Ordering::Relaxed) {
                    let _ = sender.send(NetworkResult::Browse {
                        source: config.id,
                        parent,
                        result,
                    });
                }
            }) {
            Ok(handle) => {
                *self.network.borrow_mut() = Some(NetworkJob {
                    handle,
                    cancel,
                    receiver,
                })
            }
            Err(error) => self.source_status.set(error.to_string()),
        }
    }
    fn cancel_network(&self) {
        self.pending_network.borrow_mut().take();
        if let Some(job) = self.network.borrow().as_ref() {
            job.cancel.store(true, Ordering::Relaxed);
        }
    }
    /// Keep a single source task alive while cancellation reaches a socket. Only
    /// the most recent user request waits behind it.
    fn defer_network(&self, request: NetworkRequest) -> bool {
        let busy = self
            .network
            .borrow()
            .as_ref()
            .is_some_and(|job| !job.handle.is_finished());
        if busy {
            self.cancel_network();
            *self.pending_network.borrow_mut() = Some(request);
            self.source_status
                .set("Cancelling the previous request…".into());
            return true;
        }
        self.network.borrow_mut().take();
        self.pending_network.borrow_mut().take();
        false
    }
    /// Resolve lightweight stream handles now; all network/decoder I/O remains on the audio worker.
    pub fn queue_collection(&self, app: &Cadence, entries: &[LibraryTrack], start: Option<usize>) {
        if entries.is_empty() || start.is_some_and(|index| index >= entries.len()) {
            return;
        }
        let registry = SourceRegistry::builtin();
        let sources = self.sources.get();
        let mut tracks = Vec::with_capacity(entries.len());
        for entry in entries {
            let (path, media_input) = match &entry.location {
                MediaLocation::Local(path) => (path.clone(), None),
                _ => {
                    let Some(config) = sources.iter().find(|source| source.id == entry.source_id)
                    else {
                        app.playback
                            .status
                            .set("Reconnect this track's source in Sources before playing".into());
                        return;
                    };
                    let input = match registry.stream_input(config, entry) {
                        Ok(input) => input,
                        Err(error) => {
                            app.playback.status.set(error);
                            return;
                        }
                    };
                    (self.directory.join("streams").join(input.id()), Some(input))
                }
            };
            tracks.push(Track {
                path,
                media_input,
                title: entry.title.clone(),
                artist: entry.artist.clone(),
                album: entry.album.clone(),
                duration: entry
                    .duration_seconds
                    .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok()),
            });
        }
        if let Some(index) = start {
            let current = app.playback.queue.tracks.get();
            let same = current.len() == tracks.len()
                && current
                    .iter()
                    .zip(&tracks)
                    .all(|(left, right)| left.path == right.path);
            if same {
                app.send(Command::Select(index));
                return;
            }
            self.cancel_network();
        }
        for (track, entry) in tracks.iter().zip(entries) {
            self.metadata
                .borrow_mut()
                .insert(track.path.clone(), entry.clone());
        }
        app.send(match start {
            Some(index) => Command::ReplaceTracksAndPlayAt { tracks, index },
            None => Command::AddTracks(tracks),
        });
    }

    pub fn queue_track(&self, app: &Cadence, track: LibraryTrack, play: bool) {
        if let MediaLocation::Local(path) = &track.location {
            if play {
                self.cancel_network();
            }
            self.enqueue(app, track.clone(), path.clone(), play);
            return;
        }
        let Some(config) = self
            .sources
            .get()
            .into_iter()
            .find(|source| source.id == track.source_id)
        else {
            app.playback
                .status
                .set("Reconnect this track's source in Sources before playing".into());
            return;
        };
        let input = match SourceRegistry::builtin().stream_input(&config, &track) {
            Ok(input) => input,
            Err(error) => {
                self.source_status.set(error.clone());
                app.playback.status.set(error);
                return;
            }
        };
        if play {
            self.cancel_network();
        }
        // This identity is only a key for queue metadata/artwork. The decoder
        // opens MediaInput on its worker; no audio file is created or cached.
        let path = self.directory.join("streams").join(input.id());
        let queue_track = Track {
            path: path.clone(),
            media_input: Some(input),
            title: track.title.clone(),
            artist: track.artist.clone(),
            album: track.album.clone(),
            duration: track
                .duration_seconds
                .and_then(|value| Duration::try_from_secs_f64(value).ok()),
        };
        self.metadata.borrow_mut().insert(path, track);
        app.send(if play {
            Command::AddTracksAndPlay(vec![queue_track])
        } else {
            Command::AddTracks(vec![queue_track])
        });
        self.source_status.set("Track queued for streaming".into());
    }
    fn enqueue(&self, app: &Cadence, entry: LibraryTrack, path: PathBuf, play: bool) {
        let track = Track {
            path: path.clone(),
            media_input: None,
            title: entry.title.clone(),
            artist: entry.artist.clone(),
            album: entry.album.clone(),
            duration: entry
                .duration_seconds
                .and_then(|value| Duration::try_from_secs_f64(value).ok()),
        };
        self.metadata.borrow_mut().insert(path, entry);
        app.send(if play {
            Command::AddTracksAndPlay(vec![track])
        } else {
            Command::AddTracks(vec![track])
        });
    }
    pub fn poll(&self, app: &Cadence) {
        if self.tab.get() != Tab::Sources || self.source_page.get() != SourcePage::Certificate {
            self.certificate_secret.clear();
        }
        self.certificate_secret.poll();
        if self.dirty.get()
            && self.config_writable
            && Instant::now() >= self.save_after.get()
            && let Some(writer) = &self.writer
        {
            match writer.save(self.config.borrow().clone()) {
                Ok(()) => self.dirty.set(false),
                Err(error) => {
                    self.warning.set(error);
                    self.save_after.set(Instant::now() + Duration::from_secs(2));
                }
            }
        }
        if let Some(writer) = &self.writer {
            while let Some(result) = writer.try_recv() {
                match result {
                    Ok(()) => self.warning.set(String::new()),
                    Err(error) => {
                        self.warning.set(error);
                        self.dirty.set(true);
                        self.save_after.set(Instant::now() + Duration::from_secs(2));
                    }
                }
            }
        }
        if let Some(worker) = &self.worker {
            self.folders.poll(worker);
            while let Some(event) = worker.try_recv() {
                if let Some(source) = &event.removed_source {
                    let mut sources = self.sources.get();
                    sources.retain(|entry| entry.id != *source);
                    self.sources.set(sources);
                    self.save_sources();
                }
                if let Some(source) = &event.source_removal {
                    self.pending_removals.borrow_mut().remove(source);
                }
                if let Some(source) = &event.authorized_source {
                    let requested = self.authorization_config.borrow_mut().take();
                    let removing = self.certificate_removing.replace(false);
                    self.source_authorizing.set(None);
                    self.authorization_cancel.borrow_mut().take();
                    if let Some(error) = &event.error {
                        self.source_status.set(error.clone());
                        self.set_sync_status(
                            source,
                            format!("Certificate operation failed · {error}"),
                        );
                    } else if removing {
                        self.source_status
                            .set("Saved certificate removed; new connections locked".into());
                        self.set_sync_status(
                            source,
                            "Saved certificate removed; new connections locked".into(),
                        );
                    } else if let Some(config) = requested
                        && !self.pending_removals.borrow().contains(source)
                        && self.sources.get().iter().any(|current| {
                            current.id == config.id && current.settings == config.settings
                        })
                    {
                        self.certificate_secret.clear();
                        self.request_source_sync(&config);
                        if self.source_selection.get() == Some(ProviderId::Remote(source.clone())) {
                            self.source_page.set(SourcePage::Browse);
                            let parent = self.history.borrow().last().cloned().flatten();
                            self.browse(app, config, parent);
                        }
                    }
                }
                if let Some(source) = &event.synced_source {
                    self.set_sync_status(
                        source,
                        event
                            .error
                            .as_ref()
                            .map(|error| format!("Sync failed · Library unchanged · {error}"))
                            .unwrap_or_else(|| event.status.clone()),
                    );
                    if self.source_sync.get().as_ref() == Some(source) {
                        self.source_sync.set(None);
                        self.source_sync_saving.set(false);
                        self.sync_control.borrow_mut().take();
                    }
                }
                if self.read_only.get() != event.read_only {
                    self.read_only.set(event.read_only);
                }
                if self.roots.get() != event.roots {
                    self.roots.set(event.roots);
                }
                self.library_status.set(event.error.unwrap_or(event.status));
                for entry in &event.tracks {
                    if let MediaLocation::Local(path) = &entry.location {
                        self.metadata
                            .borrow_mut()
                            .insert(path.clone(), entry.clone());
                    }
                }
                if self.library.get() != event.tracks {
                    self.library.set(event.tracks);
                    self.filter();
                    if let Some(ProviderId::Local(root)) = self.source_selection.get() {
                        let parent = self.history.borrow().last().cloned().flatten();
                        self.populate_local_source(&root, parent.as_deref());
                    }
                }
                if self.startup_sync.replace(false) && !event.read_only {
                    for source in self.sources.get() {
                        if !self
                            .library
                            .get()
                            .iter()
                            .any(|track| track.source_id == source.id)
                        {
                            self.request_source_sync(&source);
                        }
                    }
                }
            }
        }
        self.poll_source_sync();
        if *self.last_query.borrow() != self.query.get() {
            self.filter();
        }
        let finished = self
            .network
            .borrow()
            .as_ref()
            .is_some_and(|job| job.handle.is_finished());
        let result = if finished {
            self.network.borrow_mut().take().and_then(|job| {
                if job.cancel.load(Ordering::Relaxed) {
                    None
                } else {
                    job.receiver.try_recv().ok()
                }
            })
        } else {
            None
        };
        if let Some(result) = result {
            match result {
                NetworkResult::Browse {
                    source,
                    parent,
                    result,
                } => {
                    if self.active_source.borrow().as_ref() == Some(&source) {
                        match result {
                            Ok(entries) => {
                                self.source_status.set(format!(
                                    "{} items · open folders or select a track",
                                    entries.len()
                                ));
                                self.remember_folders(&entries);
                                self.source_entries.set(entries);
                                if self.history.borrow().last() != Some(&parent) {
                                    self.history.borrow_mut().push(parent);
                                }
                            }
                            Err(error) => self.source_status.set(error),
                        }
                    }
                }
            }
        }
        if self.network.borrow().is_none() {
            let request = self.pending_network.borrow_mut().take();
            if let Some(request) = request {
                match request {
                    NetworkRequest::Browse(config, parent) => self.browse(app, config, parent),
                }
            }
        }
        let path = {
            let snapshot = app.playback.snapshot.borrow();
            snapshot
                .current
                .and_then(|index| snapshot.queue.get(index).map(|track| track.path.clone()))
        };
        if *self.artwork_path.borrow() != path {
            *self.artwork_path.borrow_mut() = path.clone();
            if self.artwork.borrow_mut().take().is_some() {
                self.artwork_revision
                    .set(self.artwork_revision.get().wrapping_add(1));
            }
            self.start_artwork(path);
        }
        let finished = self
            .art_job
            .borrow()
            .as_ref()
            .is_some_and(|job| job.handle.is_finished());
        let art = if finished {
            self.art_job.borrow_mut().take().and_then(|job| {
                if job.cancel.load(Ordering::Relaxed) {
                    None
                } else {
                    job.receiver.try_recv().ok()
                }
            })
        } else {
            None
        };
        if let Some(art) = art
            && self.artwork_path.borrow().as_ref() == Some(&art.path)
        {
            if let Some(entry) = art.metadata {
                self.metadata.borrow_mut().insert(art.path, entry);
            }
            match art.result {
                Ok(Some(bitmap)) => {
                    *self.artwork.borrow_mut() = Some(BitmapImage::from_bgra(
                        bitmap.pixels,
                        bitmap.width,
                        bitmap.height,
                    ));
                    self.artwork_revision
                        .set(self.artwork_revision.get().wrapping_add(1));
                }
                Ok(None) => {}
                Err(error) => app
                    .playback
                    .status
                    .set(format!("Artwork unavailable: {error}")),
            }
        }
        if self.art_job.borrow().is_none() {
            let pending = self.pending_art.borrow_mut().take();
            if let Some(path) = pending {
                self.start_artwork(Some(path));
            }
        }
    }
    fn filter(&self) {
        let query = self.query.get().trim().to_lowercase();
        let tracks = self
            .library
            .get()
            .into_iter()
            .filter(|track| {
                query.is_empty()
                    || format!(
                        "{} {} {} {}",
                        track.title,
                        track.artist.as_deref().unwrap_or(""),
                        track.album.as_deref().unwrap_or(""),
                        track.album_artist.as_deref().unwrap_or("")
                    )
                    .to_lowercase()
                    .contains(&query)
            })
            .collect();
        self.filtered.set(tracks);
        self.library_selection.set(None);
        *self.last_query.borrow_mut() = self.query.get();
    }
    fn start_artwork(&self, path: Option<PathBuf>) {
        let busy = self
            .art_job
            .borrow()
            .as_ref()
            .is_some_and(|job| !job.handle.is_finished());
        if let Some(job) = self.art_job.borrow().as_ref() {
            job.cancel.store(true, Ordering::Relaxed);
        }
        if busy {
            *self.pending_art.borrow_mut() = path;
            return;
        }
        self.art_job.borrow_mut().take();
        self.pending_art.borrow_mut().take();
        let Some(path) = path else {
            return;
        };
        let metadata = self.metadata.borrow().get(&path).cloned();
        let sources = self.sources.get();
        let cache = self.directory.join("cache");
        let (sender, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let token = cancel.clone();
        if let Ok(handle) = std::thread::Builder::new()
            .name("cadence-artwork".into())
            .spawn(move || {
                let metadata = metadata
                    .or_else(|| cadence_library::inspect_track(&path, &Default::default()).ok());
                let result = (|| {
                    let Some(reference) =
                        metadata.as_ref().and_then(|entry| entry.artwork.as_ref())
                    else {
                        return Ok(None);
                    };
                    if token.load(Ordering::Relaxed) {
                        return Ok(None);
                    }
                    let data = match reference {
                        ArtworkRef::File(_) | ArtworkRef::Embedded { .. } => {
                            cadence_library::read_artwork(
                                reference,
                                cadence_library::MAX_ARTWORK_BYTES,
                            )?
                        }
                        _ => {
                            let config = sources
                                .iter()
                                .find(|config| {
                                    Some(&config.id)
                                        == metadata.as_ref().map(|entry| &entry.source_id)
                                })
                                .ok_or("Artwork source is not connected")?;
                            SourceRegistry::builtin()
                                .fetch_artwork(config, reference, &cache, &token)?
                        }
                    };
                    cadence_library::decode_artwork(&data, 512).map(Some)
                })();
                if !token.load(Ordering::Relaxed) {
                    let _ = sender.send(ArtResult {
                        path,
                        metadata,
                        result,
                    });
                }
            })
        {
            *self.art_job.borrow_mut() = Some(ArtJob {
                handle,
                cancel,
                receiver,
            })
        }
    }
    pub fn cancel_library_scan(&self) {
        if let Some(worker) = &self.worker {
            worker.cancel_scan();
        }
        self.library_status.set("Cancelling scan…".into());
    }
    pub fn library_view(&self, app: &Cadence, width: f32, height: f32) -> AnyView {
        AnyView::new(LibraryPane {
            app: app.clone(),
            width,
            height,
        })
    }

    pub fn sources_view(&self, app: &Cadence, width: f32, height: f32) -> AnyView {
        AnyView::new(SourcesPane {
            app: app.clone(),
            width,
            height,
        })
    }
}
// Catalog state rebuilds its own pane. Transport ticks and text editing never
// recreate these views; ListView owns the row and scroll subscriptions.
#[derive(Clone)]
struct LibraryPane {
    app: Cadence,
    width: f32,
    height: f32,
}
impl LibraryPane {
    fn body(&self) -> AnyView {
        let workspace = &self.app.workspace;
        let width = self.width;
        let list_height = (self.height - LIBRARY_HEADER_HEIGHT - 28.).max(80.);
        let select = workspace.clone();
        let play_app = self.app.clone();
        let columns = library_columns(width - 44.);
        let content = if workspace.filtered.get().is_empty() {
            let (title, detail) = if workspace.library.get().is_empty() {
                (
                    "Your music belongs here",
                    "Add a source to start building your music library.",
                )
            } else {
                (
                    "No matching tracks",
                    "Try a different title, artist, or album.",
                )
            };
            catalog_empty(Icon::Music, title, detail, list_height)
        } else {
            AnyView::new(ListView::new(
                workspace.filtered.clone(),
                workspace.library_selection.clone(),
                40.,
                move |index, entry, selected| {
                    let select = select.clone();
                    let play_app = play_app.clone();
                    let add = play_app.clone();
                    let added = entry.clone();
                    let duration = entry.duration_seconds
                        .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
                        .map(clock).unwrap_or_else(|| "—".into());
                    hstack! {
                      hstack! {
                        catalog_cell(&entry.title, columns[0], false),
                        catalog_cell(entry.artist.as_deref().unwrap_or("Unknown artist"), columns[1], false),
                        catalog_cell(entry.album.as_deref().unwrap_or("—"), columns[2], false),
                        catalog_cell(&duration, columns[3], true),
                    }
                    .spacing(12.)
                    .frame(width - 84., 40.)
                    .on_click(move || {
                        select.library_selection.set(Some(index));
                        select.queue_collection(&play_app, &select.filtered.get(), Some(index));
                    }),
                    Button::icon_only(Icon::Plus).header_style().on_click(move || add.workspace.queue_track(&add, added.clone(), false)).frame(28.,28.),
                    }.spacing(12.).padding_insets(EdgeInsets::horizontal(20.)).frame(width,40.)
                     .background(catalog_row_color(selected == Some(index), index))
                },
            ).item_key(|track| track.id.clone().into()).content_bottom_inset(PLAYER_CLEARANCE).frame(width, list_height))
        };
        AnyView::new(
            Surface::new(
                vstack! {
                    library_header(&self.app, "All songs", format!("{} songs", workspace.filtered.get().len()), workspace.query.clone(), "Search title, artist, or album", width),
                    HeaderBar::new(hstack! {
                        catalog_heading("Title", columns[0], false),
                        catalog_heading("Artist", columns[1], false),
                        catalog_heading("Album", columns[2], false),
                        catalog_heading("Time", columns[3], true),
                    }.spacing(12.).padding_insets(EdgeInsets::horizontal(20.))).height(28.),
                    content,
                }
                .spacing(0.),
                SurfaceRole::Canvas,
            )
            .frame(width, self.height),
        )
    }
}
impl View for LibraryPane {
    fn create_element(&self) -> Box<dyn Element> {
        Box::new(scarlet_ui::ComponentElement::new_with_builder(
            self.clone(),
            |pane| Box::new(pane.body()),
        ))
    }
    fn listenables(&self) -> Vec<&dyn Listenable> {
        vec![
            &self.app.workspace.library,
            &self.app.workspace.filtered,
            &self.app.workspace.read_only,
        ]
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Clone)]
struct SourcesPane {
    app: Cadence,
    width: f32,
    height: f32,
}
#[derive(Clone)]
struct ProviderRow {
    id: ProviderId,
    title: String,
    detail: String,
    icon: Icon,
}
impl SourcesPane {
    fn body(&self) -> AnyView {
        match self.app.workspace.source_page.get() {
            SourcePage::Overview => self.overview(),
            SourcePage::Connect => self.connect(),
            SourcePage::Browse => self.browse(),
            SourcePage::Certificate => self.certificate(),
        }
    }
    fn heading(&self, title: &str, detail: String, actions: AnyView) -> AnyView {
        page_header(title, detail, actions, self.width)
    }
    fn overview(&self) -> AnyView {
        let workspace = &self.app.workspace;
        let width = self.width;
        let connect = workspace.clone();
        let open = self.app.clone();
        let tracks = workspace.library.get();
        let mut rows = workspace.roots.get().into_iter().map(|path| {
            let count = tracks.iter().filter(|track| matches!(&track.location, MediaLocation::Local(file) if file.starts_with(&path))).count();
            ProviderRow { title: path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string()),
                detail: format!("Local folder · {count} songs · {}", path.display()), icon: Icon::Folder, id: ProviderId::Local(path) }
        }).chain(workspace.sources.get().into_iter().map(|source| {
            let count = tracks.iter().filter(|track| track.source_id == source.id).count();
            let kind = match source.plugin_id.as_str() { "kanade" => "Kanade", "dlna" => "DLNA", _ => "Source plugin" };
            let detail = workspace.sync_statuses.get().get(&source.id).map(|status| format!("{kind} · {count} songs · {status}")).unwrap_or_else(|| format!("{kind} · {count} songs"));
            ProviderRow { id: ProviderId::Remote(source.id), title: source.name, detail, icon: Icon::Network }
        })).collect::<Vec<_>>();
        rows.sort_by_key(|row| row.title.to_lowercase());
        let count = rows.len();
        let content = if rows.is_empty() {
            catalog_empty(
                Icon::FolderPlus,
                "Bring your music together",
                "Add a source to browse music from a folder, server, or plugin.",
                self.height - LIBRARY_HEADER_HEIGHT,
            )
        } else {
            AnyView::new(ListView::new(state(rows), state(None), 76., move |_, row, _| {
                let app = open.clone();
                Surface::section(hstack! {
                    IconView::new(row.icon).size(IconSize::Medium).color(ColorPalette::default().primary()).alignment(Alignment::Center).frame(32.,48.),
                    vstack! {
                        text(truncate(&row.title, ((width - 148.) / 8.) as usize)).font_size(15.),
                        caption(truncate(&row.detail, ((width - 148.) / 6.5) as usize)),
                    }.spacing(3.).alignment(Alignment::Leading).frame_width(width - 148.),
                    IconView::new(Icon::ChevronRight).size(IconSize::Small).alignment(Alignment::Center).frame(20.,48.),
                }.spacing(12.).padding_insets(EdgeInsets::horizontal(16.)).frame(width - 40.,64.))
                    .frame(width - 40.,64.).padding_insets(EdgeInsets::new(20.,6.,20.,6.))
                    .on_click(move || app.workspace.open_provider(&app, row.id.clone()))
            }).item_key(|row| format!("{:?}", row.id).into()).content_bottom_inset(PLAYER_CLEARANCE).frame(width, self.height - LIBRARY_HEADER_HEIGHT))
        };
        AnyView::new(Surface::new(vstack! {
            self.heading("Sources", counted(count,"source"), AnyView::new(
                button("Add source").icon(Icon::Plus).header_style().on_click(move || { connect.source_status.set(String::new()); connect.source_page.set(SourcePage::Connect); }).frame(110.,32.))),
            content,
        }.spacing(0.), SurfaceRole::Canvas).frame(width,self.height))
    }
    fn connect(&self) -> AnyView {
        let workspace = &self.app.workspace;
        let width = self.width;
        let back = workspace.clone();
        let connect = self.app.clone();
        let form_width = (width - 40.).min(560.);
        let local = workspace.source_kind.get() == 0;
        let setup = if local {
            AnyView::new(vstack! {
                caption("Choose a music folder. Subfolders, tags, and artwork are indexed automatically.")
                    .alignment(Alignment::Leading).frame(form_width,40.),
                button("Choose folder…").icon(Icon::FolderOpen).on_click(move || connect.workspace.add_source(&connect)).frame(152.,32.)
                    .alignment(Alignment::Leading).frame(form_width,40.),
            }.spacing(12.))
        } else {
            AnyView::new(vstack! {
                caption("Name").alignment(Alignment::Leading).frame(form_width,20.),
                TextField::new(workspace.source_name.clone()).placeholder("Connection name (optional)").font_size(13.).padding(8.).on_key(|_| true).frame(form_width,36.),
                caption(if workspace.source_kind.get() == 3 { "Plugin executable" } else { "Server address" }).alignment(Alignment::Leading).frame(form_width,20.),
                TextField::new(workspace.source_url.clone()).placeholder(match workspace.source_kind.get() {
                    2 => "DLNA device description URL", 3 => "Absolute path to the source plugin executable", _ => "Kanade server URL",
                }).font_size(13.).padding(8.).on_key(|_| true).frame(form_width,36.),
                if workspace.source_kind.get() == 1 {
                    AnyView::new(hstack! {
                        Toggle::new(workspace.source_kanade_identity.clone()).frame(36.,22.),
                        caption("Use a client certificate").alignment(Alignment::Leading).frame(form_width - 48.,32.),
                    }.spacing(12.).frame(form_width,32.))
                } else { AnyView::new(Spacer::new().frame(0.,0.)) },
                button("Connect").icon(Icon::Network).on_click(move || connect.workspace.add_source(&connect)).frame(110.,32.)
                    .alignment(Alignment::Leading).frame(form_width,40.),
            }.spacing(10.))
        };
        AnyView::new(Surface::new(vstack! {
            back_header("Add source",AnyView::new(Spacer::new().frame(0.,32.)),0.,width,move || back.source_page.set(SourcePage::Overview)),
            vstack! {
                caption("Source type").alignment(Alignment::Leading).frame(form_width,20.),
                Select::new(vec!["Local folder".into(), "Kanade".into(), "DLNA".into(), "Source plugin".into()], workspace.source_kind.clone()).width(form_width).row_height(32.),
                setup,
                Text::from_state(workspace.source_status.clone()).font_size(12.).color(ColorPalette::default().text_secondary())
                    .alignment(Alignment::Leading).frame(form_width,36.),
            }.spacing(10.).padding(20.).alignment(Alignment::TopLeading).frame(width,self.height - LIBRARY_HEADER_HEIGHT - PLAYER_CLEARANCE),
        }.spacing(0.), SurfaceRole::Canvas).frame(width,self.height))
    }
    fn certificate(&self) -> AnyView {
        let workspace = &self.app.workspace;
        let width = self.width;
        let form_width = (width - 40.).min(560.);
        let back = workspace.clone();
        let choose = self.app.clone();
        let unlock = workspace.clone();
        let saved = workspace.clone();
        let forget = workspace.clone();
        let busy = workspace.source_authorizing.get().is_some();
        let status_lines = certificate_status_lines(&workspace.source_status.get(), form_width);
        let actions = if busy {
            AnyView::new(caption("Certificate operation in progress…").frame(form_width, 36.))
        } else {
            AnyView::new(vstack! {
                button("Import and connect").on_click(move || unlock.authorize_kanade_identity(false)).frame(180.,32.)
                    .alignment(Alignment::Leading).frame(form_width,36.),
                {
                    AnyView::new(hstack! {
                        button("Unlock imported certificate").header_style().on_click(move || saved.authorize_kanade_identity(true)).frame(200.,32.),
                        button("Remove imported certificate").header_style().on_click(move || forget.remove_saved_certificate()).frame(200.,32.),
                    }.spacing(8.).alignment(Alignment::Leading).frame(form_width,36.))
                },
            }.spacing(8.))
        };
        AnyView::new(Surface::new(vstack! {
            back_header("Client certificate",AnyView::new(Spacer::new().frame(0.,32.)),0.,width,move || {
                back.certificate_secret.clear();
                back.source_page.set(SourcePage::Browse);
            }),
            vstack! {
                caption("Choose your PKCS#12 (.p12 or .pfx) certificate.").alignment(Alignment::Leading).frame(form_width,24.),
                hstack! {
                    TextField::new(workspace.certificate_path.clone()).placeholder("Certificate file path").font_size(13.).padding(8.).on_key(|_| true).frame(form_width-132.,36.),
                    button("Choose file…").on_click(move || choose.open_certificate()).frame(120.,32.),
                }.spacing(12.).frame(form_width,36.),
                caption("Certificate password").alignment(Alignment::Leading).frame(form_width,20.),
                workspace.certificate_secret.view(form_width),
                vstack! {
                    caption(if cfg!(target_os = "macos") {
                        "Password-encrypted in the login Keychain."
                    } else { "A password-encrypted copy is stored in Cadence." }).alignment(Alignment::Leading).frame(form_width,20.),
                    caption("Enter its password to unlock each session.").alignment(Alignment::Leading).frame(form_width,20.),
                }.spacing(0.).frame(form_width,40.),
                actions,
                vstack! {
                    caption(status_lines[0].clone()).alignment(Alignment::Leading).frame(form_width,20.).clip(),
                    caption(status_lines[1].clone()).alignment(Alignment::Leading).frame(form_width,20.).clip(),
                    caption(status_lines[2].clone()).alignment(Alignment::Leading).frame(form_width,20.).clip(),
                }.spacing(0.).frame(form_width,60.),
            }.spacing(10.).padding(20.).alignment(Alignment::TopLeading).frame(width,self.height - LIBRARY_HEADER_HEIGHT - PLAYER_CLEARANCE),
        }.spacing(0.), SurfaceRole::Canvas).frame(width,self.height))
    }
    fn browse(&self) -> AnyView {
        let workspace = &self.app.workspace;
        let width = self.width;
        let back = self.app.clone();
        let root = self.app.clone();
        let remove = workspace.clone();
        let open = self.app.clone();
        let local = matches!(workspace.source_selection.get(), Some(ProviderId::Local(_)));
        let kanade = workspace
            .selected_source()
            .is_some_and(|source| source.plugin_id == "kanade");
        let sync_here = workspace
            .source_sync
            .get()
            .is_some_and(|id| workspace.source_selection.get() == Some(ProviderId::Remote(id)));
        let sync_status = workspace
            .selected_source()
            .and_then(|source| workspace.sync_statuses.get().get(&source.id).cloned())
            .unwrap_or_default();
        let identity_controls = kanade;
        let identity_enabled = workspace.selected_source().is_some_and(|source| {
            source
                .settings
                .get("client_certificate")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        });
        let status_height = if kanade { 52. } else { 28. }
            + if identity_controls {
                if identity_enabled { 72. } else { 36. }
            } else {
                0.
            };
        let title = match workspace.source_selection.get() {
            Some(ProviderId::Local(path)) => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            _ => workspace
                .selected_source()
                .map(|source| source.name)
                .unwrap_or_else(|| "Source".into()),
        };
        let title = workspace.current_source_title(title);
        let activity_here = local
            && workspace.folders.active_root.get().is_none_or(|root| {
                workspace.source_selection.get() == Some(ProviderId::Local(root))
            });
        let activity_height = if activity_here
            && (workspace.folders.busy.get()
                || !workspace.folders.errors.get().is_empty()
                || !workspace.folders.failure.get().is_empty())
        {
            64. + if workspace.folders.details_expanded.get() {
                (workspace.folders.errors.get().len() as f32 * 48.).min(144.)
                    + if workspace.folders.failure.get().is_empty() {
                        0.
                    } else {
                        24.
                    }
            } else {
                0.
            }
        } else {
            0.
        };
        let activity = crate::library_folders::source_activity(workspace, width, activity_height);
        let refresh = if sync_here {
            if workspace.source_sync_saving.get() {
                AnyView::new(caption("Saving…").frame(100., 32.))
            } else {
                let cancel = workspace.clone();
                AnyView::new(
                    button("Cancel sync")
                        .header_style()
                        .on_click(move || {
                            if let Some(ProviderId::Remote(id)) = cancel.source_selection.get() {
                                cancel.cancel_source_sync(Some(&id));
                            }
                        })
                        .frame(100., 32.),
                )
            }
        } else if activity_here && workspace.folders.can_cancel() {
            let cancel = workspace.clone();
            AnyView::new(
                button("Cancel scan")
                    .header_style()
                    .on_click(move || cancel.cancel_library_scan())
                    .frame(100., 32.),
            )
        } else if local && workspace.folders.busy.get() {
            AnyView::new(
                caption(if activity_here {
                    "Saving…"
                } else {
                    "Updating…"
                })
                .alignment(Alignment::Leading)
                .frame(100., 32.),
            )
        } else if (local || kanade) && workspace.read_only.get() {
            AnyView::new(caption("Read only").frame(100., 32.))
        } else {
            AnyView::new(
                button("Refresh")
                    .icon(Icon::Refresh)
                    .header_style()
                    .on_click(move || match root.workspace.source_selection.get() {
                        Some(ProviderId::Local(path)) => {
                            root.workspace.library_command(LibraryCommand::Scan(path))
                        }
                        _ => {
                            if let Some(config) = root.workspace.selected_source() {
                                root.workspace.request_source_sync(&config);
                            }
                            let parent = root.workspace.history.borrow().last().cloned().flatten();
                            root.workspace.browse_provider(&root, parent);
                        }
                    })
                    .frame(100., 32.),
            )
        };
        let remove_action = if workspace.read_only.get()
            || workspace.folders.busy.get()
            || workspace.source_authorizing.get().is_some()
        {
            AnyView::new(Spacer::new().frame(0., 32.))
        } else {
            AnyView::new(
                button("Remove source")
                    .header_style()
                    .on_click(move || {
                        remove.remove_selected_source();
                    })
                    .frame(112., 32.),
            )
        };
        let identity_control = if identity_controls {
            let reconnect = self.app.clone();
            let enabled = workspace.selected_source().is_some_and(|source| {
                source
                    .settings
                    .get("client_certificate")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
            });
            if workspace.config_writable
                && !workspace.read_only.get()
                && !workspace.source_sync_saving.get()
                && workspace.source_authorizing.get().is_none()
            {
                AnyView::new(
                    button(if enabled {
                        "Client certificate: On"
                    } else {
                        "Client certificate: Off"
                    })
                    .header_style()
                    .on_click(move || reconnect.workspace.toggle_kanade_identity(&reconnect))
                    .frame(240., 28.)
                    .alignment(Alignment::Leading)
                    .padding_insets(EdgeInsets::horizontal(20.))
                    .frame(width, 36.),
                )
            } else {
                AnyView::new(
                    caption(if enabled {
                        "Client certificate: On"
                    } else {
                        "Client certificate: Off"
                    })
                    .alignment(Alignment::Leading)
                    .padding_insets(EdgeInsets::horizontal(20.))
                    .frame(width, 36.),
                )
            }
        } else {
            AnyView::new(Spacer::new().frame(0., 0.))
        };
        let authorization_control = if identity_controls && identity_enabled {
            if workspace.source_authorizing.get().is_some() {
                AnyView::new(
                    caption("Certificate authorization pending…")
                        .alignment(Alignment::Leading)
                        .padding_insets(EdgeInsets::horizontal(20.))
                        .frame(width, 36.),
                )
            } else if workspace.config_writable && !workspace.read_only.get() {
                let authorize = workspace.clone();
                AnyView::new(
                    button("Set up / unlock certificate…")
                        .header_style()
                        .on_click(move || authorize.open_certificate_setup())
                        .frame(240., 28.)
                        .alignment(Alignment::Leading)
                        .padding_insets(EdgeInsets::horizontal(20.))
                        .frame(width, 36.),
                )
            } else {
                AnyView::new(Spacer::new().frame(width, 36.))
            }
        } else {
            AnyView::new(Spacer::new().frame(0., 0.))
        };
        let content_height = self.height - LIBRARY_HEADER_HEIGHT - status_height - activity_height;
        let entries = if workspace.source_entries.get().is_empty() {
            catalog_empty(
                Icon::Music,
                "No music to show yet",
                "Connection status appears above. Try refreshing this source.",
                content_height,
            )
        } else {
            AnyView::new(ListView::new(workspace.source_entries.clone(), workspace.item_selection.clone(), 52., move |index, entry, selection| {
                let app = open.clone();
                let (title, subtitle, icon, duration) = match &entry {
                    SourceItem::Folder { title, .. } => (title.clone(), "Browse collection".into(), Icon::Folder, String::new()),
                    SourceItem::Track(track) => (track.title.clone(), track.artist.clone().unwrap_or_else(|| "Unknown artist".into()), Icon::FileMusic,
                        track.duration_seconds.and_then(|seconds| Duration::try_from_secs_f64(seconds).ok()).map(clock).unwrap_or_default()),
                };
                hstack! {
                    hstack! {
                        IconView::new(icon).size(IconSize::Small).alignment(Alignment::Center).frame(20.,44.),
                        vstack! {
                            text(truncate(&title, ((width - 200.) / 7.) as usize)).font_size(14.),
                            caption(truncate(&subtitle, ((width - 200.) / 6.5) as usize)),
                        }.spacing(3.).alignment(Alignment::Leading).frame_width(width - 200.),
                        caption(duration).alignment(Alignment::Trailing).frame(48.,44.),
                    }.spacing(12.).frame(width - 84.,52.).on_click(move || match &entry {
                        SourceItem::Folder { id, .. } => { app.workspace.browse_provider(&app, Some(id.clone())); }
                        SourceItem::Track(track) => {
                            app.workspace.item_selection.set(Some(index));
                            let entries = app.workspace.source_entries.get();
                            let track_index = entries.iter().take(index).filter(|entry| matches!(entry, SourceItem::Track(_))).count();
                            let tracks = entries.into_iter().filter_map(|entry| match entry { SourceItem::Track(track) => Some((*track).clone()), _ => None }).collect::<Vec<_>>();
                            let _ = track;
                            app.workspace.queue_collection(&app, &tracks, Some(track_index));
                        }
                    }),
                    IconView::new(if icon == Icon::Folder { Icon::ChevronRight } else { Icon::PlayerPlay }).size(IconSize::Small).alignment(Alignment::Center).frame(28.,28.),
                }.spacing(12.).padding_insets(EdgeInsets::horizontal(20.)).frame(width,52.)
                    .background(catalog_row_color(selection == Some(index), index))
            }).content_bottom_inset(PLAYER_CLEARANCE).frame(width,content_height))
        };
        AnyView::new(Surface::new(vstack! {
            back_header(&title,AnyView::new(hstack! { refresh, remove_action }.spacing(8.)),220.,width,move || back.workspace.back_in_source(&back)),
            caption(truncate(&workspace.source_status.get(), ((width - 40.) / 6.5).max(1.) as usize)).font_size(11.).alignment(Alignment::Leading)
                .padding_insets(EdgeInsets::horizontal(20.)).frame(width,28.).clip(),
            if kanade { AnyView::new(caption(truncate(&sync_status, ((width - 40.) / 6.5).max(1.) as usize)).alignment(Alignment::Leading).padding_insets(EdgeInsets::horizontal(20.)).frame(width,24.).clip()) } else { AnyView::new(Spacer::new().frame(0.,0.)) },
            identity_control,
            authorization_control,
            activity,
            entries,
        }.spacing(0.),SurfaceRole::Canvas).frame(width,self.height))
    }
}

fn certificate_status_lines(message: &str, width: f32) -> [String; 3] {
    let limit = ((width / 7.5) as usize).max(12);
    let mut lines = [String::new(), String::new(), String::new()];
    let mut line = 0;
    for word in message.split_whitespace() {
        let length = lines[line].chars().count();
        if length > 0 && length + 1 + word.chars().count() > limit && line < 2 {
            line += 1;
        }
        if !lines[line].is_empty() {
            lines[line].push(' ');
        }
        lines[line].push_str(word);
    }
    for line in &mut lines {
        *line = truncate(line, limit);
    }
    lines
}
impl View for SourcesPane {
    fn create_element(&self) -> Box<dyn Element> {
        Box::new(scarlet_ui::ComponentElement::new_with_builder(
            self.clone(),
            |pane| Box::new(pane.body()),
        ))
    }
    fn listenables(&self) -> Vec<&dyn Listenable> {
        let workspace = &self.app.workspace;
        vec![
            &workspace.sources,
            &workspace.source_entries,
            &workspace.source_selection,
            &workspace.source_kind,
            &workspace.source_page,
            &workspace.source_status,
            &workspace.source_sync,
            &workspace.source_sync_saving,
            &workspace.source_authorizing,
            &workspace.certificate_remember,
            &workspace.sync_statuses,
            &workspace.roots,
            &workspace.library,
            &workspace.read_only,
            &workspace.folders.busy,
            &workspace.folders.saving,
            &workspace.folders.active_root,
            &workspace.folders.details_expanded,
            &workspace.folders.errors,
            &workspace.folders.failure,
        ]
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn library_columns(width: f32) -> [f32; 4] {
    let text_width = (width - 120.).max(120.);
    [text_width * 0.44, text_width * 0.28, text_width * 0.28, 48.]
}
fn catalog_cell(value: &str, width: f32, trailing: bool) -> AnyView {
    AnyView::new(
        text(truncate(value, (width / 7.2).max(1.) as usize))
            .font_size(13.)
            .alignment(if trailing {
                Alignment::Trailing
            } else {
                Alignment::Leading
            })
            .frame(width, 32.),
    )
}
fn catalog_heading(value: &str, width: f32, trailing: bool) -> AnyView {
    AnyView::new(
        caption(value)
            .font_size(11.)
            .alignment(if trailing {
                Alignment::Trailing
            } else {
                Alignment::Leading
            })
            .frame(width, 28.),
    )
}
fn catalog_row_color(selected: bool, index: usize) -> Color {
    let palette = ColorPalette::default();
    if selected {
        palette
            .primary()
            .with_opacity(0.12)
            .blend_over(palette.background())
    } else if index % 2 == 1 {
        palette
            .background_secondary()
            .with_opacity(0.35)
            .blend_over(palette.background())
    } else {
        palette.background()
    }
}
fn catalog_empty(icon: Icon, title: &str, detail: &str, height: f32) -> AnyView {
    AnyView::new(vstack! {
        Spacer::new(),
        IconView::new(icon).size(IconSize::Large).color(ColorPalette::default().text_secondary()),
        text(title).font_size(15.),
        caption(detail),
        Spacer::new(),
    }.spacing(8.).padding(16.).frame(f32::INFINITY, height))
}

impl Cadence {
    pub(super) fn selected_library(&self, play: bool) {
        if let Some(track) = self
            .workspace
            .library_selection
            .get()
            .and_then(|index| self.workspace.filtered.get().get(index).cloned())
        {
            self.workspace.queue_track(self, track, play);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn certificate_status_wraps_keychain_error_without_hiding_status_code() {
        let message = "Keychain import failed (OSStatus -50). Check Keychain access and retry.";
        let lines = certificate_status_lines(message, 220.);
        assert!(lines.join(" ").contains("OSStatus -50"));
        assert!(lines[1..].iter().any(|line| !line.is_empty()));
    }
    fn app() -> Cadence {
        Cadence::new(cadence_core::Player::new(Box::new(|| Err("No test output".into()))).unwrap())
    }
    fn source(id: &str) -> SourceConfig {
        SourceConfig {
            id: id.into(),
            name: id.into(),
            plugin_id: "kanade".into(),
            settings: serde_json::json!({"url":"http://unused.invalid"}),
        }
    }
    fn hold_network(workspace: &Workspace) -> mpsc::Sender<()> {
        let (release, wait) = mpsc::channel();
        let (sender, receiver) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let _ = wait.recv();
            drop(sender);
        });
        *workspace.network.borrow_mut() = Some(NetworkJob {
            handle,
            cancel: Arc::new(AtomicBool::new(false)),
            receiver,
        });
        release
    }
    #[test]
    fn entered_kanade_hostname_is_normalized_before_saving_the_source() {
        let app = app();
        let mut workspace = Workspace::memory();
        workspace.config_writable = true;
        workspace.source_kind.set(1);
        workspace.source_url.set("kanade.ichigo.dev".into());
        let release = hold_network(&workspace);
        workspace.add_source(&app);
        assert_eq!(workspace.sources.get().len(), 1);
        assert_eq!(
            workspace.sources.get()[0].settings["url"],
            "https://kanade.ichigo.dev/"
        );
        assert_eq!(
            workspace.config.borrow().sources[0].settings["url"],
            "https://kanade.ichigo.dev/"
        );
        workspace
            .source_url
            .set("https://user:secret@example.com".into());
        workspace.add_source(&app);
        assert_eq!(workspace.sources.get().len(), 1);
        assert!(workspace.source_status.get().contains("Credentials"));
        workspace.cancel();
        release.send(()).unwrap();
        workspace
            .network
            .borrow_mut()
            .take()
            .unwrap()
            .handle
            .join()
            .unwrap();
    }
    #[test]
    fn certificate_source_opens_standalone_setup_without_reading_credentials() {
        let app = app();
        let mut workspace = Workspace::memory();
        workspace.config_writable = true;
        workspace.source_kind.set(1);
        workspace.source_url.set("kanade.example.com".into());
        workspace.source_kanade_identity.set(true);
        workspace.add_source(&app);
        assert!(workspace.source_page.get() == SourcePage::Certificate);
        assert!(workspace.source_authorizing.get().is_none());
        assert!(workspace.pending_sync.borrow().is_empty());
        assert!(workspace.network.borrow().is_none());
        let config = &workspace.config.borrow().sources[0];
        assert_eq!(config.settings["client_certificate"], true);
        assert!(config.settings.get("kanade_app_identity").is_none());
        assert!(config.settings.get("password").is_none());
        assert!(!workspace.certificate_remember.get());
    }
    #[test]
    fn kanade_sync_is_queued_once_and_read_only_sources_are_not_synced() {
        let mut workspace = Workspace::memory();
        workspace.config_writable = true;
        let config = source("kanade");
        workspace.request_source_sync(&config);
        workspace.request_source_sync(&config);
        assert_eq!(workspace.pending_sync.borrow().len(), 1);
        workspace.cancel_source_sync(Some("kanade"));
        assert!(workspace.pending_sync.borrow().is_empty());
        workspace.read_only.set(true);
        workspace.request_source_sync(&config);
        assert!(workspace.pending_sync.borrow().is_empty());
        assert!(workspace.sync_statuses.get()["kanade"].contains("read-only"));
        workspace.read_only.set(false);
        let mut dlna = config;
        dlna.plugin_id = "dlna".into();
        workspace.request_source_sync(&dlna);
        assert!(workspace.pending_sync.borrow().is_empty());
    }
    #[test]
    fn certificate_authorization_requires_explicit_opt_in_and_shutdown_cancels_pending_requests() {
        let mut workspace = Workspace::memory();
        workspace.config_writable = true;
        workspace.sources.set(vec![source("unapproved")]);
        workspace
            .source_selection
            .set(Some(ProviderId::Remote("unapproved".into())));
        workspace.authorize_kanade_identity(true);
        assert!(workspace.source_authorizing.get().is_none());
        assert!(workspace.authorization_config.borrow().is_none());
        assert!(workspace.sync_statuses.get()["unapproved"].contains("Enable"));
        let cancel = Arc::new(AtomicBool::new(false));
        *workspace.authorization_cancel.borrow_mut() = Some(cancel.clone());
        workspace.shutdown();
        assert!(cancel.load(Ordering::Acquire));
    }
    #[test]
    fn insecure_source_cannot_persist_a_client_certificate_setting() {
        let app = app();
        let mut workspace = Workspace::memory();
        workspace.config_writable = true;
        let config = source("insecure");
        workspace.sources.set(vec![config.clone()]);
        workspace
            .source_selection
            .set(Some(ProviderId::Remote(config.id.clone())));
        workspace.toggle_kanade_identity(&app);
        assert_eq!(workspace.sources.get()[0].settings, config.settings);
        assert!(!workspace.dirty.get());
        assert!(workspace.pending_sync.borrow().is_empty());
        assert!(workspace.network.borrow().is_none());
        assert!(workspace.sync_statuses.get()["insecure"].contains("HTTPS"));
    }
    #[test]
    fn saved_kanade_certificate_setting_reconnects_without_removing_library() {
        let app = app();
        let mut workspace = Workspace::memory();
        workspace.config_writable = true;
        let mut config = source("existing");
        config.settings["url"] = "https://example.invalid".into();
        workspace.sources.set(vec![config]);
        workspace
            .source_selection
            .set(Some(ProviderId::Remote("existing".into())));
        let release = hold_network(&workspace);
        workspace.toggle_kanade_identity(&app);
        assert_eq!(workspace.sources.get()[0].id, "existing");
        assert_eq!(
            workspace.sources.get()[0].settings["client_certificate"],
            true
        );
        assert_eq!(workspace.pending_sync.borrow().len(), 1);
        assert!(workspace.dirty.get());
        workspace.toggle_kanade_identity(&app);
        assert_eq!(
            workspace.sources.get()[0].settings["client_certificate"],
            false
        );
        assert_eq!(workspace.pending_sync.borrow().len(), 1);
        assert_eq!(workspace.sources.get().len(), 1);
        workspace.cancel();
        release.send(()).unwrap();
        workspace
            .network
            .borrow_mut()
            .take()
            .unwrap()
            .handle
            .join()
            .unwrap();
    }
    #[test]
    fn removing_source_cannot_be_requeued_by_startup_or_refresh() {
        let mut workspace = Workspace::memory();
        workspace.config_writable = true;
        let config = source("removing");
        workspace.sources.set(vec![config.clone()]);
        workspace
            .pending_removals
            .borrow_mut()
            .insert(config.id.clone());
        workspace.request_source_sync(&config);
        assert!(workspace.pending_sync.borrow().is_empty());
        // Also reject an already queued request, before contacting the worker.
        workspace
            .pending_sync
            .borrow_mut()
            .push_back(config.id.clone());
        workspace.poll_source_sync();
        assert!(workspace.pending_sync.borrow().is_empty());
        assert!(workspace.source_sync.get().is_none());
        assert!(workspace.sync_statuses.get().is_empty());
        workspace.pending_removals.borrow_mut().remove(&config.id);
        workspace.request_source_sync(&config);
        assert_eq!(workspace.pending_sync.borrow().len(), 1);
    }
    #[test]
    fn browsing_another_source_does_not_cancel_library_sync() {
        let app = app();
        let workspace = &app.workspace;
        let release = hold_network(workspace);
        let control = Arc::new(SourceSyncControl::default());
        *workspace.sync_control.borrow_mut() = Some(control.clone());
        workspace.source_sync.set(Some("indexing".into()));
        workspace.open_provider(&app, ProviderId::Remote("different".into()));
        assert_eq!(workspace.source_sync.get().as_deref(), Some("indexing"));
        assert!(!control.is_saving());
        assert!(!workspace.sync_statuses.get().contains_key("indexing"));
        workspace.cancel();
        release.send(()).unwrap();
        workspace
            .network
            .borrow_mut()
            .take()
            .unwrap()
            .handle
            .join()
            .unwrap();
    }
    #[test]
    fn remote_queue_addition_is_lazy_and_keeps_provider_metadata() {
        let app = app();
        let workspace = &app.workspace;
        workspace.sources.set(vec![source("remote")]);
        let entry = LibraryTrack {
            id: "remote-album-song".into(),
            source_id: "remote".into(),
            item_id: "private-provider-item".into(),
            title: "Streamed song".into(),
            artist: Some("Remote artist".into()),
            album: Some("Remote album".into()),
            album_id: Some("remote-album".into()),
            album_artist: Some("Remote artist".into()),
            track_number: Some(1),
            disc_number: Some(1),
            duration_seconds: Some(123.),
            location: MediaLocation::Provider,
            artwork: None,
        };
        workspace.queue_track(&app, entry.clone(), false);
        let deadline = Instant::now() + Duration::from_secs(1);
        let snapshot = loop {
            let snapshot = app.playback.player.snapshot();
            if !snapshot.queue.is_empty() {
                break snapshot;
            }
            assert!(Instant::now() < deadline, "Remote input was not queued");
            std::thread::yield_now();
        };
        let queued = &snapshot.queue[0];
        assert!(queued.media_input.is_some());
        assert_eq!(queued.title, entry.title);
        assert_eq!(queued.artist, entry.artist);
        assert_eq!(queued.album, entry.album);
        assert_eq!(queued.duration, Some(Duration::from_secs(123)));
        assert_eq!(workspace.metadata.borrow().get(&queued.path), Some(&entry));
        assert!(workspace.network.borrow().is_none());
        assert_eq!(snapshot.state, cadence_core::PlaybackState::Stopped);
        assert!(
            snapshot.error.is_none(),
            "Adding to queue opened the source"
        );
        assert!(!queued.path.to_string_lossy().contains(&entry.item_id));
    }
    #[test]
    fn superseded_source_requests_wait_for_one_live_job_and_clear_old_rows() {
        let app = app();
        let workspace = &app.workspace;
        let release = hold_network(workspace);
        let original = workspace
            .network
            .borrow()
            .as_ref()
            .unwrap()
            .handle
            .thread()
            .id();
        workspace.source_entries.set(vec![SourceItem::Folder {
            id: "old".into(),
            title: "Old server folder".into(),
        }]);
        workspace.item_selection.set(Some(0));
        workspace.browse(&app, source("second"), None);
        workspace.browse(&app, source("latest"), Some("album".into()));
        assert_eq!(
            workspace
                .network
                .borrow()
                .as_ref()
                .unwrap()
                .handle
                .thread()
                .id(),
            original
        );
        assert!(
            workspace
                .network
                .borrow()
                .as_ref()
                .unwrap()
                .cancel
                .load(Ordering::Relaxed)
        );
        assert!(workspace.source_entries.get().is_empty());
        assert_eq!(workspace.item_selection.get(), None);
        assert!(
            matches!(workspace.pending_network.borrow().as_ref(),Some(NetworkRequest::Browse(config,Some(parent))) if config.id=="latest"&&parent=="album")
        );
        workspace.cancel();
        assert!(workspace.pending_network.borrow().is_none());
        release.send(()).unwrap();
        workspace
            .network
            .borrow_mut()
            .take()
            .unwrap()
            .handle
            .join()
            .unwrap();
    }
    #[test]
    fn newest_artwork_waits_for_previous_task_and_cancel_clears_pending() {
        let workspace = Workspace::memory();
        let (release, wait) = mpsc::channel();
        let (sender, receiver) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let _ = wait.recv();
            drop(sender);
        });
        let original = handle.thread().id();
        *workspace.art_job.borrow_mut() = Some(ArtJob {
            handle,
            cancel: Arc::new(AtomicBool::new(false)),
            receiver,
        });
        workspace.start_artwork(Some(PathBuf::from("old.flac")));
        workspace.start_artwork(Some(PathBuf::from("new.flac")));
        assert_eq!(
            workspace
                .art_job
                .borrow()
                .as_ref()
                .unwrap()
                .handle
                .thread()
                .id(),
            original
        );
        assert_eq!(
            *workspace.pending_art.borrow(),
            Some(PathBuf::from("new.flac"))
        );
        workspace.start_artwork(None);
        assert!(workspace.pending_art.borrow().is_none());
        release.send(()).unwrap();
        workspace
            .art_job
            .borrow_mut()
            .take()
            .unwrap()
            .handle
            .join()
            .unwrap();
    }
    #[test]
    fn source_completion_is_consumed_only_after_the_worker_finishes() {
        let app = app();
        let workspace = &app.workspace;
        let (release, wait) = mpsc::channel();
        let (sender, receiver) = mpsc::channel();
        let (sent, ready) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            sender
                .send(NetworkResult::Browse {
                    source: "test".into(),
                    parent: None,
                    result: Ok(vec![SourceItem::Folder {
                        id: "album".into(),
                        title: "Completed album".into(),
                    }]),
                })
                .unwrap();
            sent.send(()).unwrap();
            let _ = wait.recv();
        });
        *workspace.active_source.borrow_mut() = Some("test".into());
        *workspace.network.borrow_mut() = Some(NetworkJob {
            handle,
            cancel: Arc::new(AtomicBool::new(false)),
            receiver,
        });
        ready.recv().unwrap();
        workspace.poll(&app);
        assert!(workspace.source_entries.get().is_empty());
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !workspace
            .network
            .borrow()
            .as_ref()
            .unwrap()
            .handle
            .is_finished()
        {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        workspace.poll(&app);
        assert_eq!(workspace.source_entries.get().len(), 1);
        assert!(workspace.network.borrow().is_none());
    }
    #[test]
    fn immediate_local_play_cancels_an_older_remote_play_intent() {
        let app = app();
        let workspace = &app.workspace;
        let release = hold_network(workspace);
        let path = PathBuf::from("/missing/cadence-test.wav");
        workspace.queue_track(
            &app,
            LibraryTrack {
                id: "local-test".into(),
                source_id: "local".into(),
                item_id: "test".into(),
                title: "Local choice".into(),
                artist: None,
                album: None,
                album_id: None,
                album_artist: None,
                track_number: None,
                disc_number: None,
                duration_seconds: Some(1e30),
                location: MediaLocation::Local(path),
                artwork: None,
            },
            true,
        );
        assert!(
            workspace
                .network
                .borrow()
                .as_ref()
                .unwrap()
                .cancel
                .load(Ordering::Relaxed)
        );
        release.send(()).unwrap();
        workspace
            .network
            .borrow_mut()
            .take()
            .unwrap()
            .handle
            .join()
            .unwrap();
    }
}

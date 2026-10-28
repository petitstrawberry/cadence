//! Blocking source providers. Call these APIs from an application I/O worker.
//!
//! The versioned trait is a Rust extension point, not a stable dynamic-library ABI.
//! Provider identifiers and opaque item identifiers are persisted; temporary signed
//! media URLs are resolved lazily when playback or an explicit download starts.

mod dlna;
mod kanade;
mod kanade_tls;
mod net;
mod process;
#[cfg(target_os = "scarlet")]
mod scarlet_random;
mod stream;

use cadence_core::{MediaInput, MediaSource};
use cadence_library::{ArtworkData, ArtworkRef, LibraryTrack, MediaLocation};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
};

pub type Result<T> = std::result::Result<T, String>;
pub const SOURCE_API_VERSION: u32 = 1;

/// Normalize a user-entered Kanade server address for storage. Bare hostnames
/// default to HTTPS; explicit HTTP/WS endpoints remain available for local use.
/// Credentials, unsupported protocols, queries and fragments are rejected.
pub fn normalize_kanade_url(value: &str) -> Result<String> {
    kanade::normalize_server_url(value)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SourceConfig {
    pub id: String,
    pub name: String,
    pub plugin_id: String,
    #[serde(default)]
    pub settings: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginDescriptor {
    pub id: String,
    pub name: String,
    pub api_version: u32,
    pub capabilities: Vec<SourceCapability>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceCapability {
    Browse,
    Artwork,
    Download,
    Stream,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceContainer {
    pub id: String,
    pub title: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SourcePage {
    pub containers: Vec<SourceContainer>,
    pub tracks: Vec<LibraryTrack>,
}

/// Register another implementation with `SourceRegistry::register`.
/// Network calls belong on a background thread. Providers own expiring credentials
/// and sessions for the lifetime of an open stream or an explicit download.
pub trait SourcePlugin: Send + Sync {
    fn descriptor(&self) -> PluginDescriptor;
    fn validate_config(&self, _config: &SourceConfig) -> Result<()> {
        Ok(())
    }
    fn browse(&self, config: &SourceConfig, container: Option<&str>) -> Result<SourcePage>;
    /// Return a complete catalogue, or fail without publishing a partial result.
    /// Providers without a full-library endpoint keep their existing browsing API.
    fn catalog_cancellable(
        &self,
        _config: &SourceConfig,
        cancel: &AtomicBool,
    ) -> Result<Vec<LibraryTrack>> {
        check_cancel(cancel)?;
        Err("This source does not support library synchronization".into())
    }
    fn browse_cancellable(
        &self,
        config: &SourceConfig,
        container: Option<&str>,
        cancel: &AtomicBool,
    ) -> Result<SourcePage> {
        check_cancel(cancel)?;
        let result = self.browse(config, container)?;
        check_cancel(cancel)?;
        Ok(result)
    }
    /// Open a progressive source on the playback worker. The returned reader owns
    /// any temporary authorization or server lease until playback releases it.
    fn open_stream(
        &self,
        config: &SourceConfig,
        track: &LibraryTrack,
        cancel: Arc<AtomicBool>,
    ) -> Result<Box<dyn MediaSource>> {
        let _ = config;
        check_cancel(&cancel)?;
        match &track.location {
            MediaLocation::Local(path) => std::fs::File::open(path)
                .map(|file| Box::new(file) as Box<dyn MediaSource>)
                .map_err(|_| "Cannot open source media file".into()),
            MediaLocation::Remote { url } => Ok(Box::new(stream::HttpStream::open(url, cancel)?)),
            MediaLocation::Provider => Err("This source does not support streaming".into()),
        }
    }
    fn materialize(
        &self,
        config: &SourceConfig,
        track: &LibraryTrack,
        cache: &Path,
        cancel: &AtomicBool,
    ) -> Result<PathBuf> {
        let _ = config;
        match &track.location {
            MediaLocation::Local(path) => Ok(path.clone()),
            MediaLocation::Remote { url } => {
                net::download(url, &track.id, cache, cancel, &mut || Ok(()))
            }
            MediaLocation::Provider => {
                Err("This source does not implement media resolution".into())
            }
        }
    }
    fn fetch_artwork(
        &self,
        config: &SourceConfig,
        artwork: &ArtworkRef,
        cache: &Path,
        cancel: &AtomicBool,
    ) -> Result<ArtworkData> {
        let _ = (config, cache);
        match artwork {
            ArtworkRef::Remote { url } => net::artwork(url, cancel, &mut || Ok(())),
            ArtworkRef::File(_) | ArtworkRef::Embedded { .. } => {
                cadence_library::read_artwork(artwork, cadence_library::MAX_ARTWORK_BYTES)
            }
            ArtworkRef::Provider { .. } => {
                Err("This source does not implement artwork resolution".into())
            }
        }
    }
}

#[derive(Clone, Default)]
pub struct SourceRegistry {
    plugins: BTreeMap<String, Arc<dyn SourcePlugin>>,
}

impl SourceRegistry {
    pub fn builtin() -> Self {
        let mut registry = Self::default();
        registry
            .register(Arc::new(kanade::KanadePlugin))
            .expect("built-in Kanade provider");
        registry
            .register(Arc::new(dlna::DlnaPlugin))
            .expect("built-in DLNA provider");
        registry
            .register(Arc::new(process::ProcessPlugin))
            .expect("built-in executable provider");
        registry
    }
    pub fn register(&mut self, plugin: Arc<dyn SourcePlugin>) -> Result<()> {
        let descriptor = plugin.descriptor();
        if descriptor.api_version != SOURCE_API_VERSION {
            return Err(format!(
                "Unsupported source API version {}",
                descriptor.api_version
            ));
        }
        if descriptor.id.is_empty() || self.plugins.contains_key(&descriptor.id) {
            return Err("Source plugin ID is empty or already registered".into());
        }
        self.plugins.insert(descriptor.id, plugin);
        Ok(())
    }
    pub fn descriptors(&self) -> Vec<PluginDescriptor> {
        self.plugins
            .values()
            .map(|plugin| plugin.descriptor())
            .collect()
    }
    fn plugin(&self, config: &SourceConfig) -> Result<&Arc<dyn SourcePlugin>> {
        if config.id.is_empty() {
            return Err("Source instance ID must not be empty".into());
        }
        self.plugins
            .get(&config.plugin_id)
            .ok_or_else(|| format!("Unknown source plugin: {}", config.plugin_id))
    }
    pub fn browse(&self, config: &SourceConfig, container: Option<&str>) -> Result<SourcePage> {
        self.plugin(config)?.browse(config, container)
    }
    /// Validate configuration without network access or reading credentials.
    pub fn validate_config(&self, config: &SourceConfig) -> Result<()> {
        self.plugin(config)?.validate_config(config)
    }
    /// Import/unlock a user-selected PKCS#12 on a worker. The password is not
    /// persisted unless `remember` explicitly selects the platform secure store.
    /// The caller must clear its masked input and avoid logging the password.
    pub fn import_client_identity(
        &self,
        config: &SourceConfig,
        path: &Path,
        password: &str,
        remember: bool,
    ) -> Result<()> {
        self.plugin(config)?;
        if config.plugin_id != "kanade" {
            return Err("This source does not support client certificates".into());
        }
        kanade_tls::Transport::import_client_identity(config, path, password, remember)
    }
    /// Explicit user action only. Unlock this source's Cadence-owned secure
    /// store entry; background catalogue/media requests never read credentials.
    pub fn authorize_saved_identity(&self, config: &SourceConfig) -> Result<()> {
        self.plugin(config)?;
        if config.plugin_id != "kanade" {
            return Err("This source does not support client certificates".into());
        }
        kanade_tls::Transport::authorize_saved_identity(config)
    }
    /// Unlock an already imported identity without accessing its original file.
    pub fn unlock_imported_identity(&self, config: &SourceConfig, password: &str) -> Result<()> {
        self.plugin(config)?;
        if config.plugin_id != "kanade" {
            return Err("This source does not support client certificates".into());
        }
        kanade_tls::Transport::unlock_imported_identity(config, password)
    }
    /// Explicit user action only. Delete this source/origin's saved Cadence
    /// credential and forget its session. The selected PKCS#12 file is retained.
    pub fn remove_saved_identity(&self, config: &SourceConfig) -> Result<()> {
        self.plugin(config)?;
        if config.plugin_id != "kanade" {
            return Err("This source does not support client certificates".into());
        }
        kanade_tls::Transport::remove_saved_identity(config)
    }
    /// Forget approved in-memory transport state when its source is removed or
    /// certificate access is disabled. This never changes Keychain contents.
    pub fn forget_source_identity(&self, source_id: &str) {
        kanade_tls::Transport::forget_source(source_id);
    }
    /// Fetch a complete library snapshot on a cancellable background worker.
    pub fn catalog_cancellable(
        &self,
        config: &SourceConfig,
        cancel: &AtomicBool,
    ) -> Result<Vec<LibraryTrack>> {
        check_cancel(cancel)?;
        let tracks = self.plugin(config)?.catalog_cancellable(config, cancel)?;
        check_cancel(cancel)?;
        Ok(tracks)
    }
    pub fn browse_cancellable(
        &self,
        config: &SourceConfig,
        container: Option<&str>,
        cancel: &AtomicBool,
    ) -> Result<SourcePage> {
        check_cancel(cancel)?;
        self.plugin(config)?
            .browse_cancellable(config, container, cancel)
    }
    /// Build a lazy input: this performs no network I/O. Every playback open
    /// resolves a fresh session, so queued tracks never persist signed URLs.
    pub fn stream_input(&self, config: &SourceConfig, track: &LibraryTrack) -> Result<MediaInput> {
        if track.source_id != config.id {
            return Err("Track belongs to a different source".into());
        }
        let plugin = self.plugin(config)?.clone();
        if !plugin
            .descriptor()
            .capabilities
            .contains(&SourceCapability::Stream)
        {
            return Err("This source does not support streaming".into());
        }
        let config = config.clone();
        let track = track.clone();
        // IDs may themselves contain private provider data. Hash the complete
        // source identity rather than exposing it via queue/debug output.
        use sha2::{Digest, Sha256};
        // Re-selecting a song after changing transport/auth settings must open a
        // new input rather than reusing a queued closure with the old settings.
        let identity =
            serde_json::to_vec(&(&config.id, &config.plugin_id, &config.settings, &track.id))
                .map_err(|_| "Cannot encode source identity")?;
        let id = format!("source:{:x}", Sha256::digest(identity));
        let extension_hint = match &track.location {
            MediaLocation::Local(path) => {
                path.extension().and_then(|s| s.to_str()).map(str::to_owned)
            }
            MediaLocation::Remote { url } => url::Url::parse(url).ok().and_then(|url| {
                Path::new(url.path())
                    .extension()
                    .and_then(|s| s.to_str())
                    .map(str::to_owned)
            }),
            MediaLocation::Provider => None,
        }
        // A hint is diagnostic metadata. Never echo an arbitrary private path
        // suffix (or a provider's token) through the decoder's Debug output.
        .map(|extension| extension.to_ascii_lowercase())
        .filter(|extension| cadence_core::EXTENSIONS.contains(&extension.as_str()));
        Ok(MediaInput::new(id, extension_hint, move |cancel| {
            let plugin = plugin.clone();
            let config = config.clone();
            let track = track.clone();
            stream::open_source(cancel, move |cancel| {
                plugin.open_stream(&config, &track, cancel)
            })
        }))
    }
    pub fn materialize(
        &self,
        config: &SourceConfig,
        track: &LibraryTrack,
        cache: &Path,
        cancel: &AtomicBool,
    ) -> Result<PathBuf> {
        if track.source_id != config.id {
            return Err("Track belongs to a different source".into());
        }
        self.plugin(config)?
            .materialize(config, track, cache, cancel)
    }
    pub fn fetch_artwork(
        &self,
        config: &SourceConfig,
        artwork: &ArtworkRef,
        cache: &Path,
        cancel: &AtomicBool,
    ) -> Result<ArtworkData> {
        if let ArtworkRef::Provider { source_id, .. } = artwork
            && source_id != &config.id
        {
            return Err("Artwork belongs to a different source".into());
        }
        self.plugin(config)?
            .fetch_artwork(config, artwork, cache, cancel)
    }
}

pub(crate) fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        Err("Source request cancelled".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod streaming_contract_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture(Arc<AtomicUsize>);
    impl SourcePlugin for Fixture {
        fn descriptor(&self) -> PluginDescriptor {
            PluginDescriptor {
                id: "fixture".into(),
                name: "Fixture".into(),
                api_version: SOURCE_API_VERSION,
                capabilities: vec![SourceCapability::Stream],
            }
        }
        fn browse(&self, _: &SourceConfig, _: Option<&str>) -> Result<SourcePage> {
            Ok(SourcePage::default())
        }
        fn open_stream(
            &self,
            _: &SourceConfig,
            _: &LibraryTrack,
            _: Arc<AtomicBool>,
        ) -> Result<Box<dyn MediaSource>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err("Expected fixture open".into())
        }
    }

    #[test]
    fn stream_factory_is_lazy_reopenable_and_keeps_private_identity_out_of_debug() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = SourceRegistry::default();
        registry.register(Arc::new(Fixture(calls.clone()))).unwrap();
        let config = SourceConfig {
            id: "private-instance".into(),
            name: "Fixture".into(),
            plugin_id: "fixture".into(),
            settings: serde_json::json!({"token":"secret-token"}),
        };
        let track = LibraryTrack {
            id: "private-item".into(),
            source_id: config.id.clone(),
            item_id: "hidden-item".into(),
            title: "Track".into(),
            artist: None,
            album: None,
            album_id: None,
            album_artist: None,
            track_number: None,
            disc_number: None,
            duration_seconds: None,
            location: MediaLocation::Remote {
                url: "http://example.invalid/private.secret-token?sig=secret-token".into(),
            },
            artwork: None,
        };
        let input = registry.stream_input(&config, &track).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(input.id().starts_with("source:"));
        assert!(!input.id().contains("private"));
        assert!(input.extension_hint().is_none());
        assert!(!format!("{input:?}").contains("secret"));
        assert_eq!(input, registry.stream_input(&config, &track).unwrap());
        let mut reconfigured = config.clone();
        reconfigured.settings = serde_json::json!({"token":"another-token"});
        assert_ne!(input, registry.stream_input(&reconfigured, &track).unwrap());
        let mut queued = cadence_core::Track::from_path(PathBuf::new());
        queued.media_input = Some(input);
        for _ in 0..2 {
            assert!(
                cadence_core::DecoderRegistry::default()
                    .open_track(&queued, Arc::new(AtomicBool::new(false)))
                    .is_err()
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let other = SourceConfig {
            id: "other-instance".into(),
            ..config
        };
        assert!(registry.stream_input(&other, &track).is_err());
    }
}

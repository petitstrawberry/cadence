//! Optional reuse of KanadeApp's existing macOS client identity.
//!
//! Only an explicit user authorization action reads the named Keychain items. The
//! PKCS#12 is decoded in memory: importing it through Security.framework or
//! native-tls would create a default or temporary keychain. Credentials never
//! enter SourceConfig, logs, cache files, or a new Keychain item.

use crate::{Result, SourceConfig};
use std::time::Duration;
use url::Url;

#[derive(Clone, Default)]
pub(crate) struct Transport {
    identity_origin: Option<Url>,
    #[cfg(target_os = "macos")]
    tls: Option<std::sync::Arc<rustls::ClientConfig>>,
}

impl Transport {
    pub(crate) fn for_source(config: &SourceConfig) -> Result<Self> {
        let enabled = match config.settings.get("kanade_app_identity") {
            None => false,
            Some(value) => value
                .as_bool()
                .ok_or("Kanade certificate setting must be true or false")?,
        };
        if !enabled {
            return Ok(Self::default());
        }
        let origin = identity_origin(config)?;
        #[cfg(not(target_os = "macos"))]
        {
            let _ = origin;
            Err("KanadeApp Keychain certificates are available only on macOS".into())
        }
        #[cfg(target_os = "macos")]
        {
            let tls = macos::authorized_identity(config, &origin)?;
            Ok(Self {
                identity_origin: Some(origin),
                tls: Some(tls),
            })
        }
    }

    /// Only call in direct response to the user choosing "Authorize certificate".
    /// This can display macOS Keychain authorization dialogs. Background browse,
    /// refresh, startup, artwork and playback must use `for_source` instead.
    pub(crate) fn authorize_source(config: &SourceConfig) -> Result<()> {
        if config
            .settings
            .get("kanade_app_identity")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        {
            return Err("Enable the KanadeApp certificate for this source first".into());
        }
        let origin = identity_origin(config)?;
        #[cfg(target_os = "macos")]
        {
            macos::authorize_identity(config, &origin).map(|_| ())
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = origin;
            Err("KanadeApp Keychain certificates are available only on macOS".into())
        }
    }

    pub(crate) fn forget_source(source_id: &str) {
        #[cfg(target_os = "macos")]
        macos::forget_source(source_id);
        #[cfg(not(target_os = "macos"))]
        let _ = source_id;
    }

    #[cfg(not(target_os = "scarlet"))]
    pub(crate) fn ws_connector(&self) -> Option<tungstenite::Connector> {
        #[cfg(target_os = "macos")]
        if let Some(tls) = &self.tls {
            return Some(tungstenite::Connector::Rustls(tls.clone()));
        }
        None
    }

    pub(crate) fn http_agent(&self) -> ureq::Agent {
        let builder = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(3))
            .timeout_read(Duration::from_secs(2))
            .timeout_write(Duration::from_secs(2))
            // A redirected request must not present the client identity to a
            // different origin. Callers validate any explicit redirect first.
            .redirects(0);
        #[cfg(target_os = "macos")]
        let builder = match &self.tls {
            Some(tls) => builder.tls_config(tls.clone()),
            None => builder,
        };
        builder.build()
    }

    /// Apply before every media request, including an explicit redirect.
    pub(crate) fn validate_media_url(&self, value: &str) -> Result<()> {
        let url = crate::net::http_url(value)?;
        if let Some(origin) = &self.identity_origin
            && url.origin() != origin.origin()
        {
            return Err("Kanade client certificate is restricted to this server's HTTPS origin; check the server's media URL configuration".into());
        }
        Ok(())
    }
}

fn identity_origin(config: &SourceConfig) -> Result<Url> {
    let value = config
        .settings
        .get("url")
        .and_then(serde_json::Value::as_str)
        .ok_or("Kanade source needs a URL")?;
    let mut url = Url::parse(value).map_err(|_| "Invalid Kanade URL")?;
    if url.scheme() == "wss" {
        url.set_scheme("https").map_err(|_| "Invalid Kanade URL")?;
    }
    let url = crate::net::http_url(url.as_str())?;
    if url.scheme() != "https" {
        return Err("Kanade client certificates require an HTTPS or WSS server address".into());
    }
    Ok(url)
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use p12_keystore::{KeyStore, KeyStoreEntry, Pkcs12ImportPolicy};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use security_framework::item::{ItemClass, ItemSearchOptions, SearchResult};
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex, OnceLock},
    };
    use zeroize::Zeroizing;

    const KEYCHAIN_UNAVAILABLE: &str = "KanadeApp certificate is unavailable or needs macOS Keychain authorization. Set up TLS / mTLS in KanadeApp, then choose Authorize certificate in this source";
    const AUTHORIZATION_REQUIRED: &str = "Choose Authorize certificate in this source to use the KanadeApp certificate for this Cadence session";
    const MAX_IDENTITIES: usize = 16;

    type CacheKey = (String, String);
    #[derive(Default)]
    struct IdentityCache {
        entries: VecDeque<(CacheKey, Arc<rustls::ClientConfig>)>,
        generation: u64,
    }
    fn identity_cache() -> &'static Mutex<IdentityCache> {
        static CACHE: OnceLock<Mutex<IdentityCache>> = OnceLock::new();
        CACHE.get_or_init(|| Mutex::new(IdentityCache::default()))
    }

    pub(super) fn forget_source(source_id: &str) {
        let mut cache = identity_cache().lock().unwrap_or_else(|e| e.into_inner());
        cache.entries.retain(|((id, _), _)| id != source_id);
        cache.generation = cache.generation.wrapping_add(1);
    }

    pub(super) fn authorized_identity(
        config: &SourceConfig,
        origin: &Url,
    ) -> Result<Arc<rustls::ClientConfig>> {
        let key = (config.id.clone(), origin.origin().ascii_serialization());
        cached_identity(&key)
    }

    fn cached_identity(key: &CacheKey) -> Result<Arc<rustls::ClientConfig>> {
        // There is deliberately no Keychain fallback here. A saved checkbox is
        // a connection preference, not approval to read another app's identity.
        let cache = identity_cache().lock().unwrap_or_else(|e| e.into_inner());
        cache
            .entries
            .iter()
            .find(|(entry, _)| entry == key)
            .map(|(_, tls)| tls.clone())
            .ok_or_else(|| AUTHORIZATION_REQUIRED.into())
    }

    pub(super) fn authorize_identity(
        config: &SourceConfig,
        origin: &Url,
    ) -> Result<Arc<rustls::ClientConfig>> {
        let key = (config.id.clone(), origin.origin().ascii_serialization());
        store_authorized_identity(key, existing_identity)
    }

    fn store_authorized_identity(
        key: CacheKey,
        load: impl FnOnce() -> Result<rustls::ClientConfig>,
    ) -> Result<Arc<rustls::ClientConfig>> {
        let generation = {
            let cache = identity_cache().lock().unwrap_or_else(|e| e.into_inner());
            cache.generation
        };
        // No lock is held during the OS dialog. A removal while it is open
        // invalidates this result so a late grant cannot resurrect credentials.
        let tls = Arc::new(load()?);
        let mut cache = identity_cache().lock().unwrap_or_else(|e| e.into_inner());
        if cache.generation != generation {
            return Err(
                "Source configuration changed during certificate authorization; retry the source"
                    .into(),
            );
        }
        cache.entries.retain(|(entry, _)| entry != &key);
        while cache.entries.len() >= MAX_IDENTITIES {
            cache.entries.pop_front();
        }
        cache.entries.push_back((key, tls.clone()));
        Ok(tls)
    }

    #[cfg(test)]
    #[test]
    fn certificate_cache_is_scoped_to_source_and_origin_and_forget_revokes_it() {
        let fixture = || {
            Ok(rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth())
        };
        let key = ("tls-cache-test".into(), "https://one.example".into());
        assert!(cached_identity(&key).is_err());
        let initial = store_authorized_identity(key.clone(), fixture).unwrap();
        let cached = cached_identity(&key).unwrap();
        assert!(Arc::ptr_eq(&initial, &cached));
        assert!(cached_identity(&("tls-cache-other".into(), key.1.clone())).is_err());
        let other =
            store_authorized_identity(("tls-cache-other".into(), key.1.clone()), fixture).unwrap();
        assert!(!Arc::ptr_eq(&initial, &other));
        assert!(cached_identity(&(key.0.clone(), "https://two.example".into())).is_err());
        let other_origin =
            store_authorized_identity((key.0.clone(), "https://two.example".into()), fixture)
                .unwrap();
        assert!(!Arc::ptr_eq(&initial, &other_origin));
        let refreshed = store_authorized_identity(key.clone(), fixture).unwrap();
        assert!(!Arc::ptr_eq(&initial, &refreshed));
        forget_source(&key.0);
        assert!(cached_identity(&key).is_err());
        let after_removal = store_authorized_identity(key.clone(), fixture).unwrap();
        assert!(!Arc::ptr_eq(&refreshed, &after_removal));
        forget_source(&key.0);
        forget_source("tls-cache-other");
    }

    fn existing_identity() -> Result<rustls::ClientConfig> {
        let pkcs12 = read_item("kanade.p12")?.ok_or(KEYCHAIN_UNAVAILABLE)?;
        let password = read_item("kanade.p12password")?.unwrap_or_default();
        let password = std::str::from_utf8(&password).map_err(
            |_| "KanadeApp certificate password is not valid UTF-8; reconfigure it in KanadeApp",
        )?;
        decode_identity(&pkcs12, password)
    }

    fn read_item(account: &str) -> Result<Option<Zeroizing<Vec<u8>>>> {
        // This matches KanadeApp's account-only generic-password query. Do not
        // invent a service identifier or search unrelated identities. This is
        // reached only through the explicit user authorization action; macOS
        // owns any access/password prompt. Background paths cannot call it.
        let results = ItemSearchOptions::new()
            .class(ItemClass::generic_password())
            .account(account)
            .load_data(true)
            .limit(2)
            .search();
        let items = match results {
            Ok(items) => items,
            Err(error) if error.code() == -25300 => return Ok(None), // errSecItemNotFound
            Err(_) => return Err(KEYCHAIN_UNAVAILABLE.into()),
        };
        let items = items
            .into_iter()
            .map(|item| match item {
                SearchResult::Data(data) => Ok(Zeroizing::new(data)),
                _ => Err("KanadeApp certificate setting has an unsupported format".to_owned()),
            })
            .collect::<Result<Vec<_>>>()?;
        if items.len() > 1 {
            return Err("More than one KanadeApp certificate setting matches in Keychain; resolve the duplicate in KanadeApp before retrying".into());
        }
        match items.into_iter().next() {
            Some(data) if data.len() <= 4 * 1024 * 1024 => Ok(Some(data)),
            None => Ok(None),
            _ => Err("KanadeApp certificate setting has an unsupported format or size".into()),
        }
    }

    pub(super) fn decode_identity(pkcs12: &[u8], password: &str) -> Result<rustls::ClientConfig> {
        let roots =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        decode_identity_with_roots(pkcs12, password, roots)
    }

    pub(super) fn decode_identity_with_roots(
        pkcs12: &[u8],
        password: &str,
        roots: rustls::RootCertStore,
    ) -> Result<rustls::ClientConfig> {
        let store = KeyStore::from_pkcs12(pkcs12, password, Pkcs12ImportPolicy::Strict)
            .map_err(|_| "Cannot unlock the KanadeApp client certificate; check its PKCS#12 file and password in KanadeApp")?;
        let mut identities = store.entries().filter_map(|(_, entry)| match entry {
            KeyStoreEntry::PrivateKeyChain(chain) => Some(chain),
            _ => None,
        });
        let identity = identities
            .next()
            .ok_or("KanadeApp PKCS#12 contains no matching client certificate and private key")?;
        if identities.next().is_some() {
            return Err("KanadeApp PKCS#12 contains multiple identities; configure a single client certificate in KanadeApp".into());
        }
        let certificates = identity
            .certs()
            .iter()
            .map(|cert| CertificateDer::from(cert.as_der().to_vec()))
            .collect();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.key().as_der().to_vec()));
        rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|_| "Cannot configure Kanade TLS protocols")?
        .with_root_certificates(roots)
        .with_client_auth_cert(certificates, key)
        .map_err(|_| {
            "KanadeApp client certificate and private key are invalid or unsupported".into()
        })
    }
}

#[cfg(all(test, target_os = "macos"))]
#[path = "kanade_tls_tests.rs"]
mod mtls_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config(value: serde_json::Value) -> SourceConfig {
        SourceConfig {
            id: "fixture".into(),
            name: "Fixture".into(),
            plugin_id: "kanade".into(),
            settings: value,
        }
    }

    #[test]
    fn optional_identity_does_not_read_keychain_and_rejects_insecure_endpoints() {
        assert!(Transport::for_source(&config(json!({"url":"http://localhost"}))).is_ok());
        assert!(Transport::for_source(&config(json!({"kanade_app_identity":"true"}))).is_err());
        assert!(identity_origin(&config(json!({"url":"http://localhost"}))).is_err());
        assert!(identity_origin(&config(json!({"url":"https://user:secret@localhost"}))).is_err());
        assert_eq!(
            identity_origin(&config(json!({"url":"wss://localhost:8443/ws"})))
                .unwrap()
                .scheme(),
            "https"
        );
    }

    #[test]
    fn identity_is_never_sent_to_a_different_media_origin() {
        let transport = Transport {
            identity_origin: Some(Url::parse("https://example.com/kanade").unwrap()),
            ..Transport::default()
        };
        assert!(
            transport
                .validate_media_url("https://example.com/media/tracks/one?sig=test")
                .is_ok()
        );
        for rejected in [
            "http://example.com/media",
            "https://other.example/media",
            "https://example.com:8443/media",
        ] {
            assert!(transport.validate_media_url(rejected).is_err());
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn invalid_identity_errors_do_not_disclose_password_or_contents() {
        let error = macos::decode_identity(b"private-certificate-data", "private-password")
            .err()
            .unwrap();
        assert!(!error.contains("private-certificate-data"));
        assert!(!error.contains("private-password"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn background_identity_cache_miss_requires_explicit_user_authorization() {
        let source = SourceConfig {
            id: "never-authorized-tls-fixture".into(),
            name: "Fixture".into(),
            plugin_id: "kanade".into(),
            settings: json!({"url":"https://example.invalid", "kanade_app_identity":true}),
        };
        // for_source has no path to read_item, even with a saved opt-in flag.
        let error = Transport::for_source(&source).err().unwrap();
        assert!(error.contains("Authorize certificate"));
        assert!(error.contains("this Cadence session"));
    }
}

//! Cadence-owned client identities, independent of any other application.
//!
//! Every platform reads a user-selected PKCS#12 file and decodes it in memory.
//! The portable path retains only the encrypted file reference in SourceConfig;
//! the password is never serialized. Optional macOS persistence is isolated in
//! `secure_store`, under Cadence's own service and source/origin-scoped account.
//! Background requests use only an explicitly unlocked process-memory identity.

use crate::{Result, SourceConfig};
use p12_keystore::{KeyStore, KeyStoreEntry, Pkcs12ImportPolicy};
use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    io::Read,
    path::Path,
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use url::Url;
use zeroize::Zeroizing;

const MAX_PKCS12_BYTES: usize = 4 * 1024 * 1024;
const MAX_PASSWORD_BYTES: usize = 16 * 1024;
const MAX_IDENTITIES: usize = 16;
const LOCKED: &str = "Unlock this source's client certificate in Cadence before connecting";

// No Debug implementation: private key bytes must never enter logs or errors.
struct IdentityMaterial {
    certificates: Vec<Vec<u8>>,
    private_key: Zeroizing<Vec<u8>>,
}

#[derive(Clone, Default)]
pub(crate) struct Transport {
    identity_origin: Option<Url>,
    #[cfg(not(target_os = "scarlet"))]
    tls: Option<Arc<rustls::ClientConfig>>,
}

impl Transport {
    pub(crate) fn for_source(config: &SourceConfig) -> Result<Self> {
        if !certificate_enabled(config)? {
            return Ok(Self::default());
        }
        let origin = identity_origin(config)?;
        let identity = cached_identity(&cache_key(config, &origin))?;
        #[cfg(not(target_os = "scarlet"))]
        {
            Ok(Self {
                identity_origin: Some(origin),
                tls: Some(Arc::new(tls_config(&identity, public_roots())?)),
            })
        }
        #[cfg(target_os = "scarlet")]
        {
            let _ = identity;
            Err("This Scarlet build does not yet support HTTPS / WSS transport; the client certificate is unlocked but cannot be used over plaintext".into())
        }
    }

    /// User-initiated import/unlock only. All file I/O and cryptography belongs
    /// on a worker. `remember` is explicit consent to the platform secure store.
    pub(crate) fn import_client_identity(
        config: &SourceConfig,
        path: &Path,
        password: &str,
        remember: bool,
    ) -> Result<()> {
        Self::import_client_identity_cancellable(
            config,
            path,
            password,
            remember,
            &std::sync::atomic::AtomicBool::new(false),
        )
    }
    pub(crate) fn import_client_identity_cancellable(
        config: &SourceConfig,
        path: &Path,
        password: &str,
        remember: bool,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<()> {
        require_certificate(config)?;
        if cancel.load(Ordering::Acquire) {
            return Err("Certificate import cancelled".into());
        }
        let origin = identity_origin(config)?;
        if password.len() > MAX_PASSWORD_BYTES {
            return Err("Certificate password exceeds the size limit".into());
        }
        let key = cache_key(config, &origin);
        store_authorized_identity(key.clone(), remember, || {
            let pkcs12 = read_pkcs12(path)?;
            let identity = decode_identity(&pkcs12, password)?;
            validate_identity(&identity)?;
            if remember {
                secure_store::save(config, &key, &pkcs12, password, cancel)?;
            }
            Ok(identity)
        })
        .map(|_| ())
    }

    /// Explicit user action only; may show a platform secure-store prompt.
    pub(crate) fn authorize_saved_identity(config: &SourceConfig) -> Result<()> {
        require_certificate(config)?;
        let origin = identity_origin(config)?;
        let key = cache_key(config, &origin);
        store_authorized_identity(key.clone(), false, || {
            let record = secure_store::load(config, &key)?;
            let (pkcs12, password) = decode_saved_record(&record)?;
            let identity = decode_identity(pkcs12, password)?;
            validate_identity(&identity)?;
            Ok(identity)
        })
        .map(|_| ())
    }

    /// Explicit user action; deletes only this source/origin's Cadence-owned
    /// secure-store entry. It never removes the user-selected certificate file.
    pub(crate) fn unlock_imported_identity(config: &SourceConfig, password: &str) -> Result<()> {
        require_certificate(config)?;
        let origin = identity_origin(config)?;
        let key = cache_key(config, &origin);
        store_authorized_identity(key.clone(), false, || {
            let record = secure_store::load_imported(config, &key, password)?;
            let identity = decode_identity(&record, password)?;
            validate_identity(&identity)?;
            Ok(identity)
        })
        .map(|_| ())
    }

    pub(crate) fn remove_saved_identity(config: &SourceConfig) -> Result<()> {
        if config.id.is_empty() {
            return Err("Source instance ID must not be empty".into());
        }
        let origin = identity_origin(config)?;
        Self::forget_source(&config.id);
        secure_store::remove(config, &cache_key(config, &origin))
    }

    /// Clears process-memory authorization only. A user's selected file and
    /// explicitly remembered secure-store entry are retained.
    pub(crate) fn forget_source(source_id: &str) {
        let mut cache = identity_cache().lock().unwrap_or_else(|e| e.into_inner());
        cache.entries.retain(|((id, _), _)| id != source_id);
        if let Some(generation) = cache.generations.get(source_id).and_then(Weak::upgrade) {
            generation.fetch_add(1, Ordering::Relaxed);
        }
        cache
            .generations
            .retain(|_, generation| generation.strong_count() > 0);
    }

    #[cfg(not(target_os = "scarlet"))]
    pub(crate) fn ws_connector(&self) -> Option<tungstenite::Connector> {
        self.tls
            .as_ref()
            .map(|tls| tungstenite::Connector::Rustls(tls.clone()))
    }

    pub(crate) fn http_agent(&self) -> ureq::Agent {
        let builder = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(3))
            .timeout_read(Duration::from_secs(2))
            .timeout_write(Duration::from_secs(2))
            .redirects(0);
        #[cfg(not(target_os = "scarlet"))]
        let builder = match &self.tls {
            Some(tls) => builder.tls_config(tls.clone()),
            None => builder,
        };
        builder.build()
    }

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

pub(crate) fn certificate_enabled(config: &SourceConfig) -> Result<bool> {
    // Old configs must not silently change identity ownership or access another
    // application's credentials. The owner must configure a Cadence identity.
    if config
        .settings
        .get("kanade_app_identity")
        .is_some_and(|value| value != &serde_json::Value::Bool(false))
    {
        return Err("This source uses a legacy certificate setting. Configure a client certificate directly in Cadence".into());
    }
    match config.settings.get("client_certificate") {
        None => Ok(false),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| "Client certificate setting must be true or false".into()),
    }
}

fn require_certificate(config: &SourceConfig) -> Result<()> {
    if !certificate_enabled(config)? {
        return Err("Enable Client certificate for this source first".into());
    }
    if config.id.is_empty() {
        return Err("Source instance ID must not be empty".into());
    }
    Ok(())
}

fn identity_origin(config: &SourceConfig) -> Result<Url> {
    let value = config
        .settings
        .get("url")
        .and_then(serde_json::Value::as_str)
        .ok_or("Kanade source needs a URL")?;
    // Credential management is portable even when a platform transport cannot
    // open HTTPS yet; do not route this through transport availability checks.
    let value = value.trim();
    let absolute = if value.contains("://") {
        value.to_owned()
    } else {
        format!("https://{value}")
    };
    let mut url = Url::parse(&absolute).map_err(|_| "Invalid Kanade URL")?;
    if url.scheme() == "wss" {
        url.set_scheme("https").map_err(|_| "Invalid Kanade URL")?;
    }
    if url.scheme() != "https" || url.host_str().is_none() {
        return Err("Client certificates require an HTTPS or WSS server address".into());
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("Certificate server address must not contain credentials, query parameters, or a fragment".into());
    }
    Ok(url)
}

fn read_pkcs12(path: &Path) -> Result<Zeroizing<Vec<u8>>> {
    if !path.is_absolute() {
        return Err("Choose an absolute path to a PKCS#12 certificate file".into());
    }
    let file = File::open(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => {
            "Certificate file not found; choose an existing .p12 or .pfx file"
        }
        std::io::ErrorKind::PermissionDenied => {
            "Cannot access the certificate file; check its permissions"
        }
        _ => "Cannot open the selected certificate file",
    })?;
    let metadata = file
        .metadata()
        .map_err(|_| "Cannot inspect the selected certificate file")?;
    if !metadata.is_file() {
        return Err(
            "The selected certificate is not a regular file; choose a .p12 or .pfx file".into(),
        );
    }
    if metadata.len() == 0 {
        return Err(
            "The selected certificate file is empty (0 bytes); choose a nonempty .p12 or .pfx file"
                .into(),
        );
    }
    if metadata.len() > MAX_PKCS12_BYTES as u64 {
        return Err("Certificate file exceeds the 4 MiB limit".into());
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(MAX_PKCS12_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read the selected certificate file")?;
    if bytes.len() > MAX_PKCS12_BYTES {
        return Err("Certificate file exceeds the 4 MiB limit".into());
    }
    if bytes.is_empty() {
        return Err(
            "The selected certificate file is empty (0 bytes); choose a nonempty .p12 or .pfx file"
                .into(),
        );
    }
    Ok(bytes)
}

type CacheKey = (String, String);
fn cache_key(config: &SourceConfig, origin: &Url) -> CacheKey {
    (config.id.clone(), origin.origin().ascii_serialization())
}
#[derive(Default)]
struct IdentityCache {
    entries: VecDeque<(CacheKey, Arc<IdentityMaterial>)>,
    // Only in-flight imports retain a strong reference. Old source IDs do not
    // accumulate after the operation finishes; unrelated sources stay isolated.
    generations: HashMap<String, Weak<AtomicU64>>,
}
fn identity_cache() -> &'static Mutex<IdentityCache> {
    static CACHE: OnceLock<Mutex<IdentityCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(IdentityCache::default()))
}
fn cached_identity(key: &CacheKey) -> Result<Arc<IdentityMaterial>> {
    let cache = identity_cache().lock().unwrap_or_else(|e| e.into_inner());
    cache
        .entries
        .iter()
        .find(|(entry, _)| entry == key)
        .map(|(_, identity)| identity.clone())
        .ok_or_else(|| LOCKED.into())
}
fn store_authorized_identity(
    key: CacheKey,
    remembers_on_success: bool,
    load: impl FnOnce() -> Result<IdentityMaterial>,
) -> Result<Arc<IdentityMaterial>> {
    let (source_generation, generation) = {
        let mut cache = identity_cache().lock().unwrap_or_else(|e| e.into_inner());
        cache
            .generations
            .retain(|_, value| value.strong_count() > 0);
        let source_generation = cache
            .generations
            .get(&key.0)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| Arc::new(AtomicU64::new(0)));
        let generation = source_generation
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        cache
            .generations
            .insert(key.0.clone(), Arc::downgrade(&source_generation));
        (source_generation, generation)
    };
    // Never hold the cache lock across file reads, cryptography or an OS dialog.
    let identity = Arc::new(load()?);
    let mut cache = identity_cache().lock().unwrap_or_else(|e| e.into_inner());
    if source_generation.load(Ordering::Relaxed) != generation {
        return Err(if remembers_on_success {
            "Certificate was saved in Cadence storage, but this source changed while it was being unlocked. New connections remain locked; unlock the saved certificate again if needed"
        } else {
            "Source configuration changed while unlocking the certificate; retry the source"
        }.into());
    }
    cache.entries.retain(|(entry, _)| entry != &key);
    while cache.entries.len() >= MAX_IDENTITIES {
        cache.entries.pop_front();
    }
    cache.entries.push_back((key, identity.clone()));
    Ok(identity)
}

fn decode_identity(pkcs12: &[u8], password: &str) -> Result<IdentityMaterial> {
    if pkcs12.len() > MAX_PKCS12_BYTES || password.len() > MAX_PASSWORD_BYTES {
        return Err("Certificate or password exceeds the size limit".into());
    }
    let store = KeyStore::from_pkcs12(pkcs12, password, Pkcs12ImportPolicy::Strict)
        .map_err(pkcs12_error_message)?;
    let mut identities = store.entries().filter_map(|(_, entry)| match entry {
        KeyStoreEntry::PrivateKeyChain(chain) => Some(chain),
        _ => None,
    });
    let identity = identities
        .next()
        .ok_or("PKCS#12 contains no matching certificate and private key")?;
    if identities.next().is_some() {
        return Err(
            "PKCS#12 contains multiple identities; choose a file containing one client certificate"
                .into(),
        );
    }
    Ok(IdentityMaterial {
        certificates: identity
            .certs()
            .iter()
            .map(|cert| cert.as_der().to_vec())
            .collect(),
        private_key: Zeroizing::new(identity.key().as_der().to_vec()),
    })
}

fn pkcs12_error_message(error: p12_keystore::error::Error) -> &'static str {
    use p12_keystore::error::Error;
    match error {
        Error::UnsupportedEncryptionScheme | Error::UnsupportedMacAlgorithm => {
            "This PKCS#12 uses an unsupported encryption or integrity algorithm"
        }
        Error::InvalidPrivateKey => {
            "The PKCS#12 private key is invalid or uses an unsupported format"
        }
        Error::MacError(_) | Error::UnpadError => {
            "Cannot verify or decrypt the PKCS#12; the password may be wrong or the file may be damaged"
        }
        _ => "Cannot parse or unlock this PKCS#12; check the file format and password",
    }
}

fn validate_identity(identity: &IdentityMaterial) -> Result<()> {
    #[cfg(not(target_os = "scarlet"))]
    {
        tls_config(identity, public_roots()).map(|_| ())
    }
    #[cfg(target_os = "scarlet")]
    {
        if identity.certificates.is_empty() || identity.private_key.is_empty() {
            Err("PKCS#12 contains an empty identity".into())
        } else {
            Ok(())
        }
    }
}

#[cfg(not(target_os = "scarlet"))]
fn public_roots() -> rustls::RootCertStore {
    rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned())
}
#[cfg(not(target_os = "scarlet"))]
fn tls_config(
    identity: &IdentityMaterial,
    roots: rustls::RootCertStore,
) -> Result<rustls::ClientConfig> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    let certificates = identity
        .certificates
        .iter()
        .cloned()
        .map(CertificateDer::from)
        .collect();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.private_key.to_vec()));
    // rustls uses webpki to parse the *local* client certificate when it
    // checks key consistency; webpki rejects X.509 v1. For a v1 client
    // identity, parse the certificate with x509-parser and compare its full
    // SubjectPublicKeyInfo against the TLS provider's derived public key.
    // This only affects local identity selection: server certificates still
    // use the normal rustls verifier and the server validates the client.
    let leaf = identity
        .certificates
        .first()
        .ok_or("The client certificate is missing")?;
    let (trailing, parsed) = x509_parser::parse_x509_certificate(leaf)
        .map_err(|_| "The client certificate cannot be parsed")?;
    if !trailing.is_empty() {
        return Err("The client certificate contains trailing data".into());
    }
    if parsed.version().0 == 0 {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let signing_key = provider.key_provider.load_private_key(key).map_err(
            |_| "The private key format or algorithm is not supported by Cadence's TLS provider",
        )?;
        let public_key = signing_key
            .public_key()
            .ok_or("Cannot verify that the client certificate matches its private key")?;
        if public_key.as_ref() != parsed.public_key().raw {
            return Err("The client certificate does not match its private key; choose a PKCS#12 with the matching pair".into());
        }
        let certified = rustls::sign::CertifiedKey::new(certificates, signing_key);
        return Ok(rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|_| "Cannot configure TLS protocols")?
            .with_root_certificates(roots)
            .with_client_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(certified))));
    }
    rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|_| "Cannot configure TLS protocols")?
        .with_root_certificates(roots)
        .with_client_auth_cert(certificates, key)
        .map_err(|error| match error {
            rustls::Error::InconsistentKeys(rustls::InconsistentKeys::KeyMismatch) =>
                "The client certificate does not match its private key; choose a PKCS#12 with the matching pair".into(),
            rustls::Error::InvalidCertificate(_) | rustls::Error::NoCertificatesPresented =>
                "The client certificate is missing or cannot be parsed".into(),
            _ => "The private key format or algorithm is not supported by Cadence's TLS provider".into(),
        })
}

fn encrypt_imported_pkcs12(pkcs12: &[u8], password: &str) -> Result<Zeroizing<Vec<u8>>> {
    if password.is_empty() {
        return Err("Use a nonempty certificate password to import securely".into());
    }
    let store = KeyStore::from_pkcs12(pkcs12, password, Pkcs12ImportPolicy::Strict)
        .map_err(pkcs12_error_message)?;
    let encrypted = Zeroizing::new(
        store
            .writer(password)
            .encryption_iterations(100_000)
            .mac_iterations(100_000)
            .write()
            .map_err(|_| "Cannot encrypt the imported certificate")?,
    );
    if encrypted.len() > MAX_PKCS12_BYTES {
        return Err(
            "Encrypted certificate exceeds the 4 MiB limit; previous import was retained".into(),
        );
    }
    Ok(encrypted)
}

const RECORD_HEADER: &[u8] = b"CadenceIdentity1";
#[cfg(any(target_os = "macos", test))]
fn encode_saved_record(pkcs12: &[u8], password: &str) -> Result<Zeroizing<Vec<u8>>> {
    if pkcs12.len() > MAX_PKCS12_BYTES || password.len() > MAX_PASSWORD_BYTES {
        return Err("Certificate or password exceeds the size limit".into());
    }
    let mut record = Zeroizing::new(Vec::with_capacity(
        RECORD_HEADER.len() + 4 + pkcs12.len() + password.len(),
    ));
    record.extend_from_slice(RECORD_HEADER);
    record.extend_from_slice(&(pkcs12.len() as u32).to_be_bytes());
    record.extend_from_slice(pkcs12);
    record.extend_from_slice(password.as_bytes());
    Ok(record)
}
fn decode_saved_record(record: &[u8]) -> Result<(&[u8], &str)> {
    let invalid =
        || "Saved Cadence certificate is invalid; import the certificate again".to_owned();
    let length_at = RECORD_HEADER.len();
    if !record.starts_with(RECORD_HEADER) || record.len() < length_at + 4 {
        return Err(invalid());
    }
    let length = u32::from_be_bytes(
        record[length_at..length_at + 4]
            .try_into()
            .map_err(|_| invalid())?,
    ) as usize;
    let start = length_at + 4;
    let end = start.checked_add(length).ok_or_else(invalid)?;
    if length == 0
        || length > MAX_PKCS12_BYTES
        || end > record.len()
        || record.len() - end > MAX_PASSWORD_BYTES
    {
        return Err(invalid());
    }
    let password = std::str::from_utf8(&record[end..]).map_err(|_| invalid())?;
    Ok((&record[start..end], password))
}

#[cfg(target_os = "macos")]
mod secure_store {
    use super::*;
    use core_foundation::{
        base::{CFType, TCFType},
        boolean::CFBoolean,
        data::CFData,
        dictionary::CFDictionary,
        string::CFString,
    };
    use security_framework::passwords::{self, PasswordOptions};
    use security_framework_sys::{
        base::{errSecDuplicateItem, errSecSuccess},
        item::{
            kSecAttrAccount, kSecAttrLabel, kSecAttrService, kSecAttrSynchronizable, kSecClass,
            kSecClassGenericPassword, kSecValueData,
        },
        keychain_item::{SecItemAdd, SecItemUpdate},
    };
    use sha2::{Digest, Sha256};
    const SERVICE: &str = "org.scarlet-os.cadence.client-identity";
    fn account(key: &CacheKey) -> String {
        // Length-prefix the source ID so delimiters inside either field cannot
        // make different source/origin pairs share a credential entry.
        let mut digest = Sha256::new();
        digest.update((key.0.len() as u64).to_be_bytes());
        digest.update(key.0.as_bytes());
        digest.update(key.1.as_bytes());
        format!("v1:{:x}", digest.finalize())
    }
    struct MacItems;
    impl crate::identity_pair::ItemStore for MacItems {
        fn get(&mut self, account: &str) -> Result<Option<Zeroizing<Vec<u8>>>> {
            let mut options = PasswordOptions::new_generic_password(SERVICE, account);
            options.set_access_synchronized(Some(false));
            match passwords::generic_password(options) {
                Ok(value) => Ok(Some(Zeroizing::new(value))),
                Err(error) if error.code() == -25300 => Ok(None),
                Err(error) => Err(format!(
                    "Keychain read failed (OSStatus {}). Check Keychain access.",
                    error.code()
                )),
            }
        }
        fn create(&mut self, account: &str, data: &[u8]) -> Result<()> {
            write_item(account, data, false)
        }
        fn upsert(&mut self, account: &str, data: &[u8]) -> Result<()> {
            write_item(account, data, true)
        }
        fn delete(&mut self, account: &str) -> Result<()> {
            let mut options = PasswordOptions::new_generic_password(SERVICE, account);
            options.set_access_synchronized(Some(false));
            match passwords::delete_generic_password_options(options) {
                Ok(()) => Ok(()),
                Err(error) if error.code() == -25300 => Ok(()),
                Err(error) => Err(format!(
                    "Keychain removal failed (OSStatus {}). Check Keychain access.",
                    error.code()
                )),
            }
        }
    }
    fn write_item(account: &str, data: &[u8], replace: bool) -> Result<()> {
        let pairs: Vec<(CFString, CFType)> = unsafe {
            vec![
                (
                    CFString::wrap_under_get_rule(kSecClass),
                    CFString::wrap_under_get_rule(kSecClassGenericPassword).into_CFType(),
                ),
                (
                    CFString::wrap_under_get_rule(kSecAttrService),
                    CFString::from(SERVICE).into_CFType(),
                ),
                (
                    CFString::wrap_under_get_rule(kSecAttrAccount),
                    CFString::from(account).into_CFType(),
                ),
                (
                    CFString::wrap_under_get_rule(kSecAttrSynchronizable),
                    CFBoolean::false_value().into_CFType(),
                ),
            ]
        };
        let mut add = pairs.clone();
        let label = if account.ends_with(":password") {
            "Cadence certificate password"
        } else if account.ends_with(":certificate") {
            "Cadence client certificate"
        } else {
            "Cadence certificate selection"
        };
        unsafe {
            add.push((
                CFString::wrap_under_get_rule(kSecAttrLabel),
                CFString::from(label).into_CFType(),
            ));
            add.push((
                CFString::wrap_under_get_rule(kSecValueData),
                CFData::from_buffer(data).into_CFType(),
            ));
        }
        let attributes = CFDictionary::from_CFType_pairs(&add);
        let status = unsafe { SecItemAdd(attributes.as_concrete_TypeRef(), std::ptr::null_mut()) };
        let status = if status == errSecDuplicateItem && replace {
            let query = CFDictionary::from_CFType_pairs(&pairs);
            let update = CFDictionary::from_CFType_pairs(&[unsafe {
                (
                    CFString::wrap_under_get_rule(kSecValueData),
                    CFData::from_buffer(data).into_CFType(),
                )
            }]);
            // Only update value data: never replace or lower an existing ACL.
            unsafe { SecItemUpdate(query.as_concrete_TypeRef(), update.as_concrete_TypeRef()) }
        } else {
            status
        };
        if status == errSecSuccess {
            Ok(())
        } else {
            Err(format!(
                "Keychain import failed (OSStatus {status}). Check Keychain access and retry."
            ))
        }
    }
    pub(super) fn save(
        _: &SourceConfig,
        key: &CacheKey,
        pkcs12: &[u8],
        password: &str,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<()> {
        use crate::identity_pair::{self, ItemStore};
        // Re-encrypt even when the certificate password is empty. Both the
        // certificate and its password are protected as separate items by the
        // login Keychain; the password is never stored in application config.
        let store = KeyStore::from_pkcs12(pkcs12, password, Pkcs12ImportPolicy::Strict)
            .map_err(pkcs12_error_message)?;
        let encrypted = Zeroizing::new(
            store
                .writer(password)
                .encryption_iterations(100_000)
                .mac_iterations(100_000)
                .write()
                .map_err(|_| "Cannot encrypt the imported certificate")?,
        );
        if encrypted.len() > MAX_PKCS12_BYTES {
            return Err(
                "Encrypted certificate exceeds the 4 MiB limit; previous import was retained"
                    .into(),
            );
        }
        let base = account(key);
        let mut items = MacItems;
        identity_pair::save_cancellable(
            &mut items,
            &base,
            &encrypted,
            password.as_bytes(),
            cancel,
        )?;
        if items.delete(&base).is_err() {
            return Err(
                "Import completed, but the previous legacy Keychain item could not be removed"
                    .into(),
            );
        }
        Ok(())
    }
    pub(super) fn load(_: &SourceConfig, key: &CacheKey) -> Result<Zeroizing<Vec<u8>>> {
        use crate::identity_pair::{self, ItemStore};
        let base = account(key);
        let mut items = MacItems;
        if let Some((certificate, password)) = identity_pair::load(&mut items, &base)? {
            let password = std::str::from_utf8(&password).map_err(
                |_| "Saved certificate password is invalid; import the certificate again",
            )?;
            return encode_saved_record(&certificate, password);
        }
        let old = items
            .get(&base)?
            .ok_or("No imported Cadence certificate found in Keychain")?;
        if !old.starts_with(RECORD_HEADER) {
            return Err("This older import needs its password; enter it to unlock".into());
        }
        Ok(old)
    }
    pub(super) fn load_imported(
        config: &SourceConfig,
        key: &CacheKey,
        _: &str,
    ) -> Result<Zeroizing<Vec<u8>>> {
        use crate::identity_pair::{self, ItemStore};
        let base = account(key);
        let mut items = MacItems;
        if let Some((certificate, _)) = identity_pair::load(&mut items, &base)? {
            return Ok(certificate);
        }
        let record = items
            .get(&base)?
            .ok_or("No imported Cadence certificate found in Keychain")?;
        if record.starts_with(RECORD_HEADER) {
            let (pkcs12, _) = decode_saved_record(&record)?;
            Ok(Zeroizing::new(pkcs12.to_vec()))
        } else {
            let _ = config;
            Ok(record)
        }
    }
    pub(super) fn remove(_: &SourceConfig, key: &CacheKey) -> Result<()> {
        use crate::identity_pair::{self, ItemStore};
        let base = account(key);
        let mut items = MacItems;
        identity_pair::remove(&mut items, &base)?;
        items.delete(&base).map_err(|_| {
            "Certificate removal is incomplete; some items may already have been removed".into()
        })
    }
    #[test]
    fn credential_accounts_are_scoped_to_source_and_origin() {
        assert_ne!(
            account(&("one".into(), "https://server".into())),
            account(&("two".into(), "https://server".into()))
        );
        assert_ne!(
            account(&("one".into(), "https://server".into())),
            account(&("one".into(), "https://other".into()))
        );
        assert!(SERVICE.contains("cadence"));
    }
}
#[cfg(any(not(target_os = "macos"), test))]
#[allow(dead_code)]
mod portable_store {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::{
        fs::{self, OpenOptions},
        io::Write,
        path::PathBuf,
        sync::atomic::AtomicU64,
    };
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    fn path(config: &SourceConfig, key: &CacheKey) -> Result<PathBuf> {
        let directory = config
            .settings
            .get("credential_store")
            .and_then(serde_json::Value::as_str)
            .ok_or("Cadence credential storage is unavailable")?;
        let directory = PathBuf::from(directory);
        if !directory.is_absolute() {
            return Err("Cadence credential storage path must be absolute".into());
        }
        let mut digest = Sha256::new();
        digest.update((key.0.len() as u64).to_be_bytes());
        digest.update(key.0.as_bytes());
        digest.update(key.1.as_bytes());
        Ok(directory.join(format!("{:x}.p12", digest.finalize())))
    }
    pub(super) fn save(
        config: &SourceConfig,
        key: &CacheKey,
        pkcs12: &[u8],
        password: &str,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<()> {
        let encrypted = encrypt_imported_pkcs12(pkcs12, password)?;
        let destination = path(config, key)?;
        let dir = destination
            .parent()
            .ok_or("Invalid credential storage path")?;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(dir)
            .map_err(|_| "Cannot create Cadence credential storage")?;
        let temp = dir.join(format!(
            ".import-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp)
            .map_err(|_| "Cannot create a temporary certificate import")?;
        let result = file
            .write_all(&encrypted)
            .and_then(|_| file.sync_all())
            .and_then(|_| {
                if cancel.load(Ordering::Acquire) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "cancelled",
                    ));
                }
                fs::rename(&temp, &destination)
            });
        if result.is_err() {
            let _ = fs::remove_file(&temp);
            return Err(
                "Cannot save the imported certificate; previous import was retained".into(),
            );
        }
        Ok(())
    }
    pub(super) fn load(_: &SourceConfig, _: &CacheKey) -> Result<Zeroizing<Vec<u8>>> {
        Err("Enter the password to unlock the imported certificate".into())
    }
    pub(super) fn load_imported(
        config: &SourceConfig,
        key: &CacheKey,
        _: &str,
    ) -> Result<Zeroizing<Vec<u8>>> {
        read_pkcs12(&path(config, key)?)
    }
    pub(super) fn remove(config: &SourceConfig, key: &CacheKey) -> Result<()> {
        match fs::remove_file(path(config, key)?) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err("Cannot remove the imported certificate".into()),
        }
    }
}

#[cfg(not(target_os = "macos"))]
use portable_store as secure_store;

#[cfg(all(test, not(target_os = "scarlet")))]
#[path = "kanade_tls_tests.rs"]
mod mtls_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn config(id: &str) -> SourceConfig {
        SourceConfig {
            id: id.into(),
            name: "Fixture".into(),
            plugin_id: "kanade".into(),
            settings: json!({"url":"https://example.invalid", "client_certificate":true}),
        }
    }
    #[test]
    fn pkcs12_failure_messages_distinguish_unsupported_encryption_and_invalid_key() {
        use p12_keystore::error::Error;
        assert!(
            pkcs12_error_message(Error::UnsupportedEncryptionScheme)
                .contains("unsupported encryption")
        );
        assert!(pkcs12_error_message(Error::UnsupportedMacAlgorithm).contains("integrity"));
        assert!(pkcs12_error_message(Error::InvalidPrivateKey).contains("private key"));
        assert!(pkcs12_error_message(Error::UnpadError).contains("password may be wrong"));
    }
    #[test]
    fn certificate_file_errors_explain_empty_directory_missing_and_oversize() {
        let dir = std::env::temp_dir().join(format!(
            "cadence-cert-metadata-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let empty = dir.join("empty.p12");
        std::fs::write(&empty, []).unwrap();
        assert!(read_pkcs12(&empty).unwrap_err().contains("empty (0 bytes)"));
        assert!(
            read_pkcs12(&dir)
                .unwrap_err()
                .contains("not a regular file")
        );
        assert!(
            read_pkcs12(&dir.join("missing.p12"))
                .unwrap_err()
                .contains("not found")
        );
        let large = dir.join("large.p12");
        let file = File::create(&large).unwrap();
        file.set_len(MAX_PKCS12_BYTES as u64 + 1).unwrap();
        assert!(read_pkcs12(&large).unwrap_err().contains("4 MiB"));
        #[cfg(unix)]
        {
            let link = dir.join("link.p12");
            std::os::unix::fs::symlink(&empty, &link).unwrap();
            assert!(read_pkcs12(&link).unwrap_err().contains("empty (0 bytes)"));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn encrypted_import_survives_source_removal_and_failed_replacement() {
        let dir = std::env::temp_dir().join(format!(
            "cadence-import-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let original = dir.join("original.p12");
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mtls/client.p12"),
            &original,
        )
        .unwrap();
        let mut source = config("portable-durable");
        source.settings["credential_store"] =
            dir.join("owned").to_string_lossy().to_string().into();
        let key = cache_key(&source, &identity_origin(&source).unwrap());
        let data = read_pkcs12(&original).unwrap();
        portable_store::save(
            &source,
            &key,
            &data,
            "test-fixture-only",
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        std::fs::remove_file(&original).unwrap();
        let imported = portable_store::load_imported(&source, &key, "test-fixture-only").unwrap();
        assert!(decode_identity(&imported, "test-fixture-only").is_ok());
        assert!(
            portable_store::save(
                &source,
                &key,
                b"invalid",
                "test-fixture-only",
                &std::sync::atomic::AtomicBool::new(false)
            )
            .is_err()
        );
        assert!(
            decode_identity(
                &portable_store::load_imported(&source, &key, "test-fixture-only").unwrap(),
                "test-fixture-only"
            )
            .is_ok()
        );
        assert!(decode_identity(&imported, "incorrect").is_err());
        portable_store::save(
            &source,
            &key,
            &imported,
            "test-fixture-only",
            &std::sync::atomic::AtomicBool::new(false),
        )
        .unwrap();
        assert!(
            decode_identity(
                &portable_store::load_imported(&source, &key, "test-fixture-only").unwrap(),
                "test-fixture-only"
            )
            .is_ok()
        );
        portable_store::remove(&source, &key).unwrap();
        assert!(portable_store::load_imported(&source, &key, "test-fixture-only").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[cfg(not(target_os = "scarlet"))]
    #[test]
    fn tls_identity_errors_distinguish_mismatched_certificate_and_bad_key() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mtls");
        let pkcs12 = std::fs::read(fixture.join("client.p12")).unwrap();
        let mut identity = decode_identity(&pkcs12, "test-fixture-only").unwrap();
        identity.certificates = vec![std::fs::read(fixture.join("server.der")).unwrap()];
        assert!(
            validate_identity(&identity)
                .unwrap_err()
                .contains("does not match")
        );
        identity.private_key = Zeroizing::new(vec![1, 2, 3]);
        assert!(
            validate_identity(&identity)
                .unwrap_err()
                .contains("private key format or algorithm")
        );
    }
    #[test]
    fn legacy_v1_certificate_is_accepted_as_a_client_identity() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mtls/client-v1.p12");
        let identity =
            decode_identity(&std::fs::read(path).unwrap(), "synthetic-test-only").unwrap();
        assert!(validate_identity(&identity).is_ok());
    }
    #[test]
    fn encrypted_keychain_record_never_contains_the_password_and_unlocks_v1() {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mtls/client-v1.p12");
        let input = std::fs::read(fixture).unwrap();
        let password = "synthetic-test-only";
        assert!(encrypt_imported_pkcs12(&input, "").is_err());
        let encrypted = encrypt_imported_pkcs12(&input, password).unwrap();
        assert!(
            !encrypted
                .windows(password.len())
                .any(|bytes| bytes == password.as_bytes())
        );
        assert!(decode_identity(&encrypted, "wrong-password").is_err());
        assert!(validate_identity(&decode_identity(&encrypted, password).unwrap()).is_ok());
    }
    #[test]
    fn mac_keychain_import_can_retain_an_empty_certificate_password() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mtls/client.p12");
        let bytes = std::fs::read(fixture).unwrap();
        let source =
            KeyStore::from_pkcs12(&bytes, "test-fixture-only", Pkcs12ImportPolicy::Strict).unwrap();
        let empty_password_p12 = Zeroizing::new(source.writer("").write().unwrap());
        let decoded =
            KeyStore::from_pkcs12(&empty_password_p12, "", Pkcs12ImportPolicy::Strict).unwrap();
        let reencoded = Zeroizing::new(decoded.writer("").write().unwrap());
        assert!(decode_identity(&reencoded, "").is_ok());
    }
    #[cfg(not(target_os = "scarlet"))]
    #[test]
    fn pkcs12_certificate_bags_yield_leaf_first_der_and_not_pem() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mtls");
        // This OpenSSL fixture puts certificate bags before the shrouded key,
        // with the leaf and its CA in separate bags.
        let pkcs12 = std::fs::read(fixture.join("client.p12")).unwrap();
        let identity = decode_identity(&pkcs12, "test-fixture-only").unwrap();
        assert_eq!(identity.certificates.len(), 2);
        assert_eq!(
            identity.certificates[1],
            std::fs::read(fixture.join("root.der")).unwrap()
        );
        for cert in &identity.certificates {
            let (remaining, _) = x509_parser::parse_x509_certificate(cert).unwrap();
            assert!(remaining.is_empty());
            assert!(!cert.starts_with(b"-----BEGIN"));
        }
        assert!(validate_identity(&identity).is_ok());
        assert!(
            decode_identity(
                b"-----BEGIN CERTIFICATE-----\ninvalid\n-----END CERTIFICATE-----",
                ""
            )
            .is_err()
        );
    }
    #[test]
    fn nested_safe_contents_are_currently_not_supported() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mtls/client-nested.p12");
        let error = decode_identity(&std::fs::read(path).unwrap(), "synthetic-test-only")
            .err()
            .unwrap();
        assert!(error.contains("no matching certificate and private key"));
    }
    #[cfg(not(target_os = "scarlet"))]
    #[test]
    fn legacy_v1_certificate_still_rejects_a_different_private_key() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mtls");
        let mut identity = decode_identity(
            &std::fs::read(fixture.join("client-v1.p12")).unwrap(),
            "synthetic-test-only",
        )
        .unwrap();
        let other = decode_identity(
            &std::fs::read(fixture.join("client.p12")).unwrap(),
            "test-fixture-only",
        )
        .unwrap();
        identity.private_key = other.private_key;
        assert!(
            validate_identity(&identity)
                .unwrap_err()
                .contains("does not match")
        );
    }
    #[test]
    fn credentials_are_opt_in_and_legacy_imports_require_migration() {
        let mut source = config("none");
        source.settings = json!({"url":"http://localhost"});
        assert!(Transport::for_source(&source).is_ok());
        source.settings = json!({"url":"https://localhost", "kanade_app_identity":true});
        assert!(
            Transport::for_source(&source)
                .err()
                .unwrap()
                .contains("legacy")
        );
        source.settings = json!({"url":"http://localhost", "client_certificate":true});
        assert!(identity_origin(&source).is_err());
        source.settings = json!({"url":"https://user:secret@localhost", "client_certificate":true});
        assert!(identity_origin(&source).is_err());
    }
    #[test]
    fn background_cache_miss_never_reads_file_or_secure_store() {
        let mut source = config("never-authorized-tls-fixture");
        source.settings["certificate_path"] = "/path/that/must/not/be/read.p12".into();
        assert_eq!(Transport::for_source(&source).err().unwrap(), LOCKED);
    }
    #[test]
    fn unlocking_one_source_is_not_invalidated_by_forgetting_another() {
        let key = ("import-isolated-a".into(), "https://example.invalid".into());
        let identity = store_authorized_identity(key.clone(), false, || {
            Transport::forget_source("import-isolated-b");
            Ok(IdentityMaterial {
                certificates: vec![],
                private_key: Zeroizing::new(vec![1]),
            })
        })
        .unwrap();
        assert!(Arc::ptr_eq(&identity, &cached_identity(&key).unwrap()));
        Transport::forget_source(&key.0);
    }
    #[test]
    fn forgetting_same_source_rejects_late_unlock_and_reports_completed_persistence() {
        let key = ("import-revoked".into(), "https://example.invalid".into());
        for remembered in [false, true] {
            let error = store_authorized_identity(key.clone(), remembered, || {
                Transport::forget_source(&key.0);
                // Simulate the completed native-store save without touching it.
                Ok(IdentityMaterial {
                    certificates: vec![],
                    private_key: Zeroizing::new(vec![1]),
                })
            })
            .err()
            .unwrap();
            assert!(cached_identity(&key).is_err());
            assert_eq!(error.contains("was saved in Cadence storage"), remembered);
        }
    }
    #[test]
    fn portable_identity_import_unlocks_only_the_chosen_source_and_origin() {
        let source = config("portable-import-fixture");
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mtls/client.p12");
        assert!(
            Transport::import_client_identity(&source, &path, "wrong-password", false).is_err()
        );
        assert_eq!(Transport::for_source(&source).err().unwrap(), LOCKED);
        Transport::import_client_identity(&source, &path, "test-fixture-only", false).unwrap();
        let origin = identity_origin(&source).unwrap();
        let identity = cached_identity(&cache_key(&source, &origin)).unwrap();
        assert!(!identity.private_key.is_empty());
        #[cfg(not(target_os = "scarlet"))]
        assert!(Transport::for_source(&source).is_ok());
        let mut other = source.clone();
        other.settings["url"] = "https://other.invalid".into();
        assert_eq!(Transport::for_source(&other).err().unwrap(), LOCKED);
        other = config("another-source");
        assert_eq!(Transport::for_source(&other).err().unwrap(), LOCKED);
        Transport::forget_source(&source.id);
        assert_eq!(Transport::for_source(&source).err().unwrap(), LOCKED);
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
    #[test]
    fn invalid_identity_errors_do_not_disclose_password_contents_or_path() {
        let error = decode_identity(b"private-certificate-data", "private-password")
            .err()
            .unwrap();
        assert!(!error.contains("private-certificate-data"));
        assert!(!error.contains("private-password"));
        assert!(
            !read_pkcs12(Path::new("/private/missing-certificate.p12"))
                .err()
                .unwrap()
                .contains("/private")
        );
    }
    #[test]
    fn secure_store_record_roundtrip_and_malformed_bounds() {
        let bytes = b"encrypted-pkcs12-fixture";
        let record = encode_saved_record(bytes, "fixture-password").unwrap();
        assert_eq!(
            decode_saved_record(&record).unwrap(),
            (bytes.as_slice(), "fixture-password")
        );
        assert!(decode_saved_record(&record[..RECORD_HEADER.len() + 3]).is_err());
        let mut broken = record.to_vec();
        broken[RECORD_HEADER.len()..RECORD_HEADER.len() + 4]
            .copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(decode_saved_record(&broken).is_err());
        assert!(decode_saved_record(b"unrelated-data").is_err());
    }
}

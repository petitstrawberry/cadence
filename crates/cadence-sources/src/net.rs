use crate::*;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use url::Url;

pub const MAX_MEDIA_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_CACHE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);
static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);
static DOWNLOAD_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn http_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).map_err(|_| "Invalid source URL")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("Source URL must use HTTP or HTTPS".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("Credentials in source URLs are not supported".into());
    }
    #[cfg(target_os = "scarlet")]
    if url.scheme() == "https" {
        return Err(
            "HTTPS is not available on Scarlet yet; configure an HTTP endpoint on your trusted LAN"
                .into(),
        );
    }
    Ok(url)
}

fn agent() -> ureq::Agent {
    // Redirects are followed explicitly so platform and protocol checks apply to
    // every destination, including a signed media endpoint's redirect.
    ureq::AgentBuilder::new()
        .timeout_connect(TIMEOUT)
        .timeout_read(TIMEOUT)
        .timeout_write(TIMEOUT)
        .redirects(0)
        .build()
}

fn response(result: std::result::Result<ureq::Response, ureq::Error>) -> Result<ureq::Response> {
    match result {
        Ok(response) => Ok(response),
        Err(ureq::Error::Status(code, _)) => Err(format!("Source returned HTTP {code}")),
        // Transport diagnostics contain signed URLs. Never put them in UI errors.
        Err(ureq::Error::Transport(_)) => Err("Source connection failed or timed out".into()),
    }
}

fn get(url: &str) -> Result<ureq::Response> {
    get_with_client(url, &agent(), false)
}

fn get_with_client(
    url: &str,
    client: &ureq::Agent,
    restrict_origin: bool,
) -> Result<ureq::Response> {
    let mut current = http_url(url)?;
    let origin = current.origin();
    for _ in 0..=5 {
        let result = response(client.get(current.as_str()).call())?;
        if matches!(result.status(), 301 | 302 | 303 | 307 | 308) {
            let destination = result
                .header("Location")
                .ok_or("Source redirect has no destination")?;
            let destination = current
                .join(destination)
                .map_err(|_| "Source redirect is invalid")?;
            if current.scheme() == "https" && destination.scheme() != "https" {
                return Err("Source tried to redirect HTTPS media to an insecure endpoint".into());
            }
            if restrict_origin && destination.origin() != origin {
                return Err(
                    "Kanade media redirect must stay on the authorized server origin".into(),
                );
            }
            current = http_url(destination.as_str())?;
            continue;
        }
        if result.status() != 200 {
            return Err(format!("Source returned HTTP {}", result.status()));
        }
        return Ok(result);
    }
    Err("Source redirected too many times".into())
}

fn read_bounded(
    mut reader: impl Read,
    max: usize,
    cancel: &AtomicBool,
    pump: &mut dyn FnMut() -> Result<()>,
) -> Result<Vec<u8>> {
    let started = Instant::now();
    let mut data = Vec::new();
    let mut buffer = [0u8; 32 * 1024];
    loop {
        check_progress(cancel, pump, started)?;
        let count = reader
            .read(&mut buffer)
            .map_err(|_| "Source body read failed or timed out")?;
        if count == 0 {
            return Ok(data);
        }
        if data.len().saturating_add(count) > max {
            return Err("Source response exceeds the size limit".into());
        }
        data.extend_from_slice(&buffer[..count]);
    }
}

fn check_progress(
    cancel: &AtomicBool,
    pump: &mut dyn FnMut() -> Result<()>,
    started: Instant,
) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        return Err("Source request cancelled".into());
    }
    if started.elapsed() > DOWNLOAD_TIMEOUT {
        return Err("Source download exceeded the time limit".into());
    }
    pump()
}

pub(crate) fn get_text_cancellable(url: &str, max: usize, cancel: &AtomicBool) -> Result<String> {
    check_cancel(cancel)?;
    let bytes = read_bounded(get(url)?.into_reader(), max, cancel, &mut || Ok(()))?;
    String::from_utf8(bytes).map_err(|_| "Source returned text with invalid UTF-8".into())
}

pub(crate) fn post_xml_cancellable(
    url: &str,
    action: &str,
    body: &str,
    max: usize,
    cancel: &AtomicBool,
) -> Result<String> {
    check_cancel(cancel)?;
    let url = http_url(url)?;
    let reply = match agent()
        .post(url.as_str())
        .set("Content-Type", "text/xml; charset=\"utf-8\"")
        .set("SOAPAction", &format!("\"{action}\""))
        .send_string(body)
    {
        // UPnP action faults use HTTP 500 with a structured SOAP body.
        Err(ureq::Error::Status(500, reply)) => reply,
        other => response(other)?,
    };
    if !matches!(reply.status(), 200 | 500) {
        return Err(format!("Source returned HTTP {}", reply.status()));
    }
    let bytes = read_bounded(reply.into_reader(), max, cancel, &mut || Ok(()))?;
    String::from_utf8(bytes).map_err(|_| "Source returned XML with invalid UTF-8".into())
}

fn cache_path(key: &str, cache: &Path) -> PathBuf {
    let digest = Sha256::digest(key.as_bytes());
    cache.join(format!("{:x}.media", digest))
}

pub(crate) fn cached(key: &str, cache: &Path) -> Option<PathBuf> {
    let path = cache_path(key, cache);
    fs::metadata(&path)
        .ok()
        .filter(|m| m.is_file() && m.len() > 0 && m.len() <= MAX_MEDIA_BYTES)
        .map(|_| path)
}

fn ensure_capacity(cache: &Path, reserve: u64) -> Result<()> {
    let mut total = 0u64;
    for entry in fs::read_dir(cache)
        .map_err(|_| "Cannot inspect media cache")?
        .flatten()
    {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "media" && e != "part") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        total = total.saturating_add(metadata.len());
    }
    // Queued tracks retain these paths. Never unlink one behind the player.
    if total.saturating_add(reserve) > MAX_CACHE_BYTES {
        return Err("Media cache is full (2 GiB); stop playback, clear the queue, and remove unused cache files".into());
    }
    Ok(())
}

struct TempFile(PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub(crate) fn download(
    url: &str,
    key: &str,
    cache: &Path,
    cancel: &AtomicBool,
    pump: &mut dyn FnMut() -> Result<()>,
) -> Result<PathBuf> {
    download_inner(url, key, cache, cancel, pump, &agent(), false)
}

pub(crate) fn download_with_client(
    url: &str,
    key: &str,
    cache: &Path,
    cancel: &AtomicBool,
    pump: &mut dyn FnMut() -> Result<()>,
    client: &ureq::Agent,
) -> Result<PathBuf> {
    download_inner(url, key, cache, cancel, pump, client, true)
}

fn download_inner(
    url: &str,
    key: &str,
    cache: &Path,
    cancel: &AtomicBool,
    pump: &mut dyn FnMut() -> Result<()>,
    client: &ureq::Agent,
    restrict_origin: bool,
) -> Result<PathBuf> {
    if cancel.load(Ordering::Relaxed) {
        return Err("Source request cancelled".into());
    }
    if let Some(path) = cached(key, cache) {
        return Ok(path);
    }
    // Serialize capacity checks and writes. Keep leased source sessions alive
    // while another download owns the cache, and allow cancellation while waiting.
    let started = Instant::now();
    let _guard = loop {
        check_progress(cancel, pump, started)?;
        match DOWNLOAD_LOCK.try_lock() {
            Ok(guard) => break guard,
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err("Media cache worker failed".into());
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                std::thread::sleep(Duration::from_millis(20))
            }
        }
    };
    if let Some(path) = cached(key, cache) {
        return Ok(path);
    }
    fs::create_dir_all(cache).map_err(|_| "Cannot create media cache")?;
    let reply = get_with_client(url, client, restrict_origin)?;
    let declared = reply
        .header("Content-Length")
        .and_then(|s| s.parse::<u64>().ok());
    if declared.is_some_and(|length| length > MAX_MEDIA_BYTES) {
        return Err("Media exceeds the 512 MiB download limit".into());
    }
    ensure_capacity(cache, declared.unwrap_or(MAX_MEDIA_BYTES))?;
    let destination = cache_path(key, cache);
    let temp = TempFile(destination.with_extension(format!(
        "{}.{}.part",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    )));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp.0)
        .map_err(|_| "Cannot create media cache entry")?;
    let mut reader = reply.into_reader();
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    let started = Instant::now();
    loop {
        check_progress(cancel, pump, started)?;
        let count = reader
            .read(&mut buffer)
            .map_err(|_| "Media download failed or timed out")?;
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > MAX_MEDIA_BYTES {
            return Err("Media exceeds the 512 MiB download limit".into());
        }
        file.write_all(&buffer[..count])
            .map_err(|_| "Cannot write media cache entry")?;
    }
    check_progress(cancel, pump, started)?;
    if total == 0 || declared.is_some_and(|length| length != total) {
        return Err("Source returned incomplete or empty media".into());
    }
    file.sync_all()
        .map_err(|_| "Cannot synchronize media cache entry")?;
    drop(file);
    fs::rename(&temp.0, &destination).map_err(|_| "Cannot finish media cache entry")?;
    Ok(destination)
}

pub(crate) fn artwork(
    url: &str,
    cancel: &AtomicBool,
    pump: &mut dyn FnMut() -> Result<()>,
) -> Result<ArtworkData> {
    artwork_inner(url, cancel, pump, &agent(), false)
}

pub(crate) fn artwork_with_client(
    url: &str,
    cancel: &AtomicBool,
    pump: &mut dyn FnMut() -> Result<()>,
    client: &ureq::Agent,
) -> Result<ArtworkData> {
    artwork_inner(url, cancel, pump, client, true)
}

fn artwork_inner(
    url: &str,
    cancel: &AtomicBool,
    pump: &mut dyn FnMut() -> Result<()>,
    client: &ureq::Agent,
    restrict_origin: bool,
) -> Result<ArtworkData> {
    if cancel.load(Ordering::Relaxed) {
        return Err("Source request cancelled".into());
    }
    let reply = get_with_client(url, client, restrict_origin)?;
    let media_type = reply
        .header("Content-Type")
        .unwrap_or("application/octet-stream")
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();
    let bytes = read_bounded(
        reply.into_reader(),
        cadence_library::MAX_ARTWORK_BYTES,
        cancel,
        pump,
    )?;
    Ok(ArtworkData { bytes, media_type })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    fn server(response: String) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut data = [0; 1024];
            let _ = socket.read(&mut data);
            socket.write_all(response.as_bytes()).unwrap();
        });
        url
    }
    #[test]
    fn download_is_atomic_cached_and_cancelled_before_network() {
        let url =
            server("HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nwave".into());
        let cache = std::env::temp_dir().join(format!(
            "cadence-source-test-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let path = download(
            &url,
            "source/track",
            &cache,
            &AtomicBool::new(false),
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"wave");
        assert_eq!(
            download(
                "bad",
                "source/track",
                &cache,
                &AtomicBool::new(false),
                &mut || Ok(())
            )
            .unwrap(),
            path
        );
        assert!(
            download(
                "bad",
                "source/track",
                &cache,
                &AtomicBool::new(true),
                &mut || Ok(())
            )
            .unwrap_err()
            .contains("cancelled")
        );
        fs::remove_dir_all(cache).unwrap();
    }
    #[test]
    fn truncated_download_never_becomes_cache_entry() {
        let url = server(
            "HTTP/1.1 200 OK\r\nContent-Length: 99\r\nConnection: close\r\n\r\nshort".into(),
        );
        let cache = std::env::temp_dir().join(format!(
            "cadence-source-short-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        assert!(download(&url, "bad", &cache, &AtomicBool::new(false), &mut || Ok(())).is_err());
        assert!(cached("bad", &cache).is_none());
        assert_eq!(fs::read_dir(&cache).unwrap().count(), 0);
        fs::remove_dir_all(cache).unwrap();
    }
    #[test]
    fn unknown_and_credential_urls_are_rejected() {
        assert!(http_url("file:///etc/passwd").is_err());
        assert!(http_url("http://user:password@example.com").is_err());
    }
    #[test]
    fn authenticated_artwork_never_follows_a_redirect_to_another_origin() {
        let destination = TcpListener::bind("127.0.0.1:0").unwrap();
        destination.set_nonblocking(true).unwrap();
        let url = server(format!(
            "HTTP/1.1 302 Found\r\nLocation: http://{}/private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            destination.local_addr().unwrap()
        ));
        let error = artwork_with_client(&url, &AtomicBool::new(false), &mut || Ok(()), &agent())
            .unwrap_err();
        assert!(error.contains("authorized server origin"));
        assert_eq!(
            destination.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}

//! Explicitly configured executable providers using the v1 JSON-lines protocol.
//!
//! Each operation has its own child. Playback retains the child for the lifetime
//! of the stream, including pauses, so the provider can keep a remote lease open.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use cadence_library::{ArtworkData, ArtworkRef, LibraryTrack, MediaLocation, track_id};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    PluginDescriptor, Result, SOURCE_API_VERSION, SourceCapability, SourceConfig, SourceContainer,
    SourcePage, SourcePlugin, net,
};

const MAX_REQUEST_BYTES: usize = 128 * 1024;
const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(25);
const MAX_PIPE_WORKERS: usize = 8;
static PIPE_WORKERS: AtomicUsize = AtomicUsize::new(0);

pub struct ProcessPlugin;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    executable: PathBuf,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    config: Value,
}

#[derive(Serialize)]
struct Request<'a> {
    api_version: u32,
    id: u64,
    method: &'a str,
    params: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    api_version: u32,
    id: u64,
    result: Option<Value>,
    error: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    #[serde(default)]
    containers: Vec<SourceContainer>,
    #[serde(default)]
    tracks: Vec<Track>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Track {
    item_id: String,
    title: String,
    artist: Option<String>,
    album: Option<String>,
    #[serde(default)]
    album_id: Option<String>,
    #[serde(default)]
    album_artist: Option<String>,
    #[serde(default)]
    track_number: Option<u32>,
    #[serde(default)]
    disc_number: Option<u32>,
    duration_seconds: Option<f64>,
    artwork_item_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolvedUrl {
    url: String,
}

impl SourcePlugin for ProcessPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            id: "process".into(),
            name: "External source plugin (JSON-lines v1)".into(),
            api_version: SOURCE_API_VERSION,
            capabilities: vec![
                SourceCapability::Browse,
                SourceCapability::Artwork,
                SourceCapability::Stream,
                SourceCapability::Download,
            ],
        }
    }

    fn browse(&self, source: &SourceConfig, container: Option<&str>) -> Result<SourcePage> {
        self.browse_cancellable(source, container, &AtomicBool::new(false))
    }

    fn browse_cancellable(
        &self,
        source: &SourceConfig,
        container: Option<&str>,
        cancel: &AtomicBool,
    ) -> Result<SourcePage> {
        let (_session, result) =
            Session::request(source, "browse", json!({"container": container}), cancel)?;
        let page: Page = serde_json::from_value(result)
            .map_err(|error| format!("Invalid plugin page: {error}"))?;
        normalize_page(source, page)
    }

    fn open_stream(
        &self,
        source: &SourceConfig,
        track: &LibraryTrack,
        cancel: Arc<AtomicBool>,
    ) -> Result<Box<dyn cadence_core::MediaSource>> {
        if track.source_id != source.id {
            return Err("Track belongs to a different source".into());
        }
        if !matches!(track.location, MediaLocation::Provider) {
            return Err("External source tracks must use provider identities".into());
        }
        let (mut session, result) = Session::request(
            source,
            "resolve",
            json!({"item_id": track.item_id}),
            &cancel,
        )?;
        let url = resolved_url(result)?;
        let lease =
            crate::stream::SessionLease::spawn(cancel.clone(), move || session.check_status())?;
        Ok(Box::new(
            crate::stream::HttpStream::open(&url, cancel)?.with_lease(lease),
        ))
    }

    fn materialize(
        &self,
        source: &SourceConfig,
        track: &LibraryTrack,
        cache: &Path,
        cancel: &AtomicBool,
    ) -> Result<PathBuf> {
        if track.source_id != source.id {
            return Err("Track belongs to a different source".into());
        }
        if !matches!(track.location, MediaLocation::Provider) {
            return Err("External source tracks must use provider identities".into());
        }
        if cancel.load(Ordering::Relaxed) {
            return Err("Source request cancelled".into());
        }
        if let Some(path) = net::cached(&track.id, cache) {
            return Ok(path);
        }
        let (mut session, result) =
            Session::request(source, "resolve", json!({"item_id": track.item_id}), cancel)?;
        let url = resolved_url(result)?;
        net::download(&url, &track.id, cache, cancel, &mut || {
            session.check_status()
        })
    }

    fn fetch_artwork(
        &self,
        source: &SourceConfig,
        artwork: &ArtworkRef,
        _cache: &Path,
        cancel: &AtomicBool,
    ) -> Result<ArtworkData> {
        let ArtworkRef::Provider { source_id, item_id } = artwork else {
            return Err("External source artwork must use a provider identity".into());
        };
        if source_id != &source.id {
            return Err("Artwork belongs to a different source".into());
        }
        let (mut session, result) =
            Session::request(source, "artwork", json!({"item_id": item_id}), cancel)?;
        let url = resolved_url(result)?;
        net::artwork(&url, cancel, &mut || session.check_status())
    }
}

fn resolved_url(value: Value) -> Result<String> {
    let result: ResolvedUrl =
        serde_json::from_value(value).map_err(|_| "Invalid plugin URL response")?;
    let parsed = url::Url::parse(&result.url).map_err(|_| "Plugin returned an invalid URL")?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(
            "Plugin URLs must use HTTP(S), have a host, and omit embedded credentials".into(),
        );
    }
    if result.url.len() > 32_768 {
        return Err("Plugin URL is too long".into());
    }
    Ok(result.url)
}

fn normalize_page(source: &SourceConfig, page: Page) -> Result<SourcePage> {
    if source.id.is_empty() || source.id.len() > 32_768 {
        return Err("Invalid source instance ID".into());
    }
    if page.containers.len() + page.tracks.len() > 10_000 {
        return Err("Plugin page exceeds 10000 entries".into());
    }
    let mut ids = HashSet::new();
    for container in &page.containers {
        validate_string(&container.id, true)?;
        validate_string(&container.title, false)?;
        if !ids.insert(&container.id) {
            return Err("Plugin page contains duplicate container IDs".into());
        }
    }
    let mut ids = HashSet::new();
    let mut tracks = Vec::with_capacity(page.tracks.len());
    for track in page.tracks {
        validate_string(&track.item_id, true)?;
        validate_string(&track.title, false)?;
        for value in track
            .artist
            .iter()
            .chain(track.album.iter())
            .chain(track.album_artist.iter())
        {
            validate_string(value, false)?;
        }
        if let Some(album_id) = &track.album_id {
            validate_string(album_id, true)?;
        }
        if !ids.insert(track.item_id.clone()) {
            return Err("Plugin page contains duplicate item IDs".into());
        }
        if track
            .duration_seconds
            .is_some_and(|duration| Duration::try_from_secs_f64(duration).is_err())
        {
            return Err("Plugin track duration is outside the supported range".into());
        }
        if let Some(item_id) = &track.artwork_item_id {
            validate_string(item_id, true)?;
        }
        tracks.push(LibraryTrack {
            id: track_id(&source.id, &track.item_id),
            source_id: source.id.clone(),
            item_id: track.item_id,
            title: track.title,
            artist: track.artist,
            album: track.album,
            album_id: track.album_id,
            album_artist: track.album_artist,
            track_number: track.track_number.filter(|number| *number > 0),
            disc_number: track.disc_number.filter(|number| *number > 0),
            duration_seconds: track.duration_seconds,
            location: MediaLocation::Provider,
            artwork: track.artwork_item_id.map(|item_id| ArtworkRef::Provider {
                source_id: source.id.clone(),
                item_id,
            }),
        });
    }
    Ok(SourcePage {
        containers: page.containers,
        tracks,
    })
}

fn validate_string(value: &str, required: bool) -> Result<()> {
    if value.len() > 32_768 || (required && value.is_empty()) {
        Err("Plugin metadata contains an empty identity or an oversized string".into())
    } else {
        Ok(())
    }
}

/// A bounded number of detached I/O workers prevents a plugin descendant that
/// incorrectly retains an inherited pipe from creating unlimited blocked threads.
/// We never join a pipe read: the main worker must retain its timeout guarantee.
struct PipePermit;

impl PipePermit {
    fn acquire() -> Result<Self> {
        PIPE_WORKERS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_PIPE_WORKERS).then_some(count + 1)
            })
            .map(|_| Self)
            .map_err(|_| "External plugin pipe worker limit reached".into())
    }
}

impl Drop for PipePermit {
    fn drop(&mut self) {
        PIPE_WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}

struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
}

impl Session {
    fn request(
        source: &SourceConfig,
        method: &str,
        params: Value,
        cancel: &AtomicBool,
    ) -> Result<(Self, Value)> {
        Self::request_with_timeout(source, method, params, cancel, RESPONSE_TIMEOUT)
    }

    fn request_with_timeout(
        source: &SourceConfig,
        method: &str,
        mut params: Value,
        cancel: &AtomicBool,
        timeout: Duration,
    ) -> Result<(Self, Value)> {
        if cancel.load(Ordering::Relaxed) {
            return Err("Source request cancelled".into());
        }
        let configuration: Configuration = serde_json::from_value(source.settings.clone())
            .map_err(|error| format!("Invalid process plugin configuration: {error}"))?;
        if !configuration.executable.is_absolute() {
            return Err("Plugin executable must be an absolute path".into());
        }
        if configuration.args.len() > 128
            || configuration.args.iter().map(String::len).sum::<usize>() > MAX_REQUEST_BYTES
        {
            return Err("Plugin arguments exceed the size limit".into());
        }
        params["settings"] = configuration.config;
        params["source_id"] = Value::String(source.id.clone());
        let mut request = serde_json::to_vec(&Request {
            api_version: SOURCE_API_VERSION,
            id: 1,
            method,
            params,
        })
        .map_err(|error| error.to_string())?;
        if request.len() >= MAX_REQUEST_BYTES {
            return Err("Plugin request exceeds 128 KiB".into());
        }
        request.push(b'\n');
        // Scarlet currently implements spawning and waiting, but Child::kill
        // returns Unsupported. Launching here would make cancellation/timeouts
        // unable to stop a stuck plugin or safely reap it.
        if cfg!(target_os = "scarlet") {
            return Err("External executable sources require Child::kill support, which Scarlet does not yet provide; use a built-in Kanade or DLNA source".into());
        }
        let permit = PipePermit::acquire()?;
        let child = Command::new(&configuration.executable)
            .args(&configuration.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("Unable to launch source plugin: {error}"))?;
        let mut session = Self { child, stdin: None };
        let mut stdin = session
            .child
            .stdin
            .take()
            .ok_or("Plugin stdin pipe is missing")?;
        let stdout = session
            .child
            .stdout
            .take()
            .ok_or("Plugin stdout pipe is missing")?;
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("cadence-plugin-pipe".into())
            .spawn(move || {
                let _permit = permit;
                let result = (|| -> Result<(ChildStdin, Value)> {
                    stdin
                        .write_all(&request)
                        .and_then(|()| stdin.flush())
                        .map_err(|error| format!("Unable to send plugin request: {error}"))?;
                    let mut bytes = Vec::new();
                    BufReader::new(stdout)
                        .take(MAX_RESPONSE_BYTES + 1)
                        .read_until(b'\n', &mut bytes)
                        .map_err(|error| format!("Unable to read plugin response: {error}"))?;
                    if bytes.len() as u64 > MAX_RESPONSE_BYTES {
                        return Err("Plugin response exceeds 4 MiB".into());
                    }
                    if bytes.last() != Some(&b'\n') {
                        return Err("Plugin closed stdout before a complete JSON line".into());
                    }
                    Ok((stdin, parse_response(&bytes)?))
                })();
                // A timed-out caller has already dropped the receiver. Dropping a
                // failed send also releases stdin and closes the child's lease.
                let _ = sender.send(result);
            })
            .map_err(|error| format!("Unable to start plugin pipe worker: {error}"))?;
        let deadline = Instant::now() + timeout;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err("Source request cancelled".into());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("Source plugin response timed out".into());
            }
            match receiver.recv_timeout(remaining.min(POLL_INTERVAL)) {
                Ok(result) => {
                    let (stdin, result) = result?;
                    session.stdin = Some(stdin);
                    session.check_status()?;
                    return Ok((session, result));
                }
                Err(RecvTimeoutError::Timeout) => (),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err("Plugin pipe worker stopped unexpectedly".into());
                }
            }
        }
    }

    fn check_status(&mut self) -> Result<()> {
        match self
            .child
            .try_wait()
            .map_err(|error| format!("Unable to check source plugin: {error}"))?
        {
            Some(status) if !status.success() => Err(format!("Source plugin exited with {status}")),
            _ => Ok(()),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // EOF lets a provider release a remote lease. Always kill and reap our
        // direct child as well: a plugin that ignores EOF must not stall Cadence.
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn parse_response(bytes: &[u8]) -> Result<Value> {
    let response: Response =
        serde_json::from_slice(bytes).map_err(|_| "Invalid plugin response")?;
    if response.api_version != SOURCE_API_VERSION {
        return Err("Unsupported source plugin protocol version".into());
    }
    if response.id != 1 {
        return Err("Source plugin response ID does not match request".into());
    }
    match (response.result, response.error) {
        (Some(result), None) => Ok(result),
        // Provider diagnostics may echo signed URLs or credentials. Keep raw
        // provider messages out of playback snapshots and UI errors.
        (None, Some(_)) => Err("Source plugin reported an error".into()),
        _ => Err("Plugin response must contain exactly one result or error".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(settings: Value) -> SourceConfig {
        SourceConfig {
            id: "fixture".into(),
            name: "Fixture".into(),
            plugin_id: "process".into(),
            settings,
        }
    }

    #[test]
    fn rejects_wrong_versions_ids_and_ambiguous_results() {
        for response in [
            json!({"api_version": 2, "id": 1, "result": {}}),
            json!({"api_version": 1, "id": 2, "result": {}}),
            json!({"api_version": 1, "id": 1, "result": {}, "error": "no"}),
            json!({"api_version": 1, "id": 1}),
        ] {
            assert!(parse_response(&serde_json::to_vec(&response).unwrap()).is_err());
        }
        assert!(parse_response(br#"{"api_version":1,"id":1,"result":{}}"#).is_ok());
        let error =
            parse_response(br#"{"api_version":1,"id":1,"error":"https://host/media?sig=secret"}"#)
                .unwrap_err();
        assert_eq!(error, "Source plugin reported an error");
    }

    #[test]
    fn validates_resolved_transport_and_metadata() {
        for url in [
            "file:///etc/passwd",
            "http://user:secret@example.com/music",
            "not a URL",
        ] {
            assert!(resolved_url(json!({"url": url})).is_err());
        }
        assert!(resolved_url(json!({"url": "https://example.com/music?id=1"})).is_ok());
        let invalid =
            serde_json::from_value(json!({"tracks": [{"item_id": "", "title": "Title"}]})).unwrap();
        assert!(normalize_page(&source(Value::Null), invalid).is_err());
    }

    #[test]
    fn normalizes_stable_provider_identities() {
        let page: Page = serde_json::from_value(json!({
            "containers": [{"id": "album", "title": "Album"}],
            "tracks": [{"item_id": "song/7", "title": "Song", "artist": "Artist", "duration_seconds": 123.5, "artwork_item_id": "cover/7"}]
        })).unwrap();
        let page = normalize_page(&source(Value::Null), page).unwrap();
        assert_eq!(page.tracks[0].id, track_id("fixture", "song/7"));
        assert_eq!(page.tracks[0].location, MediaLocation::Provider);
        assert_eq!(
            page.tracks[0].artwork,
            Some(ArtworkRef::Provider {
                source_id: "fixture".into(),
                item_id: "cover/7".into()
            })
        );
    }

    #[test]
    fn optional_album_metadata_is_backward_compatible_and_preserved() {
        let page: Page = serde_json::from_value(json!({"tracks":[
            {"item_id":"one","title":"First","album_id":"album-a","album_artist":"Various Artists","track_number":4,"disc_number":2},
            {"item_id":"two","title":"Legacy"}
        ]})).unwrap();
        let page = normalize_page(&source(Value::Null), page).unwrap();
        assert_eq!(page.tracks[0].album_id.as_deref(), Some("album-a"));
        assert_eq!(
            page.tracks[0].album_artist.as_deref(),
            Some("Various Artists")
        );
        assert_eq!(page.tracks[0].track_number, Some(4));
        assert_eq!(page.tracks[0].disc_number, Some(2));
        assert_eq!(page.tracks[1].album_id, None);
        assert_eq!(page.tracks[1].album_artist, None);
        assert_eq!(page.tracks[1].track_number, None);
        assert_eq!(page.tracks[1].disc_number, None);
        let invalid: Page = serde_json::from_value(
            json!({"tracks":[{"item_id":"one","title":"First","album_id":""}]}),
        )
        .unwrap();
        assert!(normalize_page(&source(Value::Null), invalid).is_err());
    }

    #[cfg(unix)]
    fn shell(script: &str) -> SourceConfig {
        source(
            json!({"executable": "/bin/sh", "args": ["-c", script], "config": {"token": "fixture"}}),
        )
    }

    #[test]
    #[cfg(unix)]
    fn executable_receives_settings_and_returns_a_browsable_page() {
        let source = shell(
            "IFS= read -r request; case \"$request\" in *'\"settings\":{\"token\":\"fixture\"}'*) ;; *) exit 7;; esac; printf '%s\\n' '{\"api_version\":1,\"id\":1,\"result\":{\"tracks\":[{\"item_id\":\"one\",\"title\":\"One\"}]}}'; IFS= read -r unused",
        );
        let page = ProcessPlugin.browse(&source, None).unwrap();
        assert_eq!(page.tracks[0].title, "One");
    }

    #[test]
    #[cfg(unix)]
    fn resolved_media_and_artwork_are_downloaded_with_the_child_retained() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            for _ in 0..2 {
                let mut socket = loop {
                    match listener.accept() {
                        Ok((socket, _)) => {
                            socket.set_nonblocking(false).unwrap();
                            break socket;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "plugin never requested fixture");
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("fixture accept: {error}"),
                    }
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut buffer = [0; 4096];
                let received = socket.read(&mut buffer).unwrap();
                assert!(received > 0);
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 7\r\nConnection: close\r\n\r\nfixture").unwrap();
            }
        });
        let source = shell(&format!(
            "IFS= read -r request; printf '%s\\n' '{{\"api_version\":1,\"id\":1,\"result\":{{\"url\":\"{endpoint}\"}}}}'; IFS= read -r unused"
        ));
        let page = normalize_page(&source, serde_json::from_value(json!({"tracks": [{"item_id": "media", "title": "Media", "artwork_item_id": "image"}]})).unwrap()).unwrap();
        let track = &page.tracks[0];
        let cache =
            std::env::temp_dir().join(format!("cadence-process-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        let cancel = AtomicBool::new(false);
        let path = ProcessPlugin
            .materialize(&source, track, &cache, &cancel)
            .unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"fixture");
        let artwork = ProcessPlugin
            .fetch_artwork(&source, track.artwork.as_ref().unwrap(), &cache, &cancel)
            .unwrap();
        assert_eq!(artwork.bytes, b"fixture");
        assert_eq!(artwork.media_type, "image/png");
        server.join().unwrap();
        std::fs::remove_dir_all(cache).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn streaming_retains_provider_while_paused_and_releases_it_on_drop() {
        use std::net::TcpListener;

        fn process_alive(pid: &str) -> bool {
            Command::new("/bin/kill")
                .args(["-0", pid])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (pid_sender, pid_receiver) = mpsc::channel();
        let (closed_sender, closed_receiver) = mpsc::channel();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let deadline = Instant::now() + Duration::from_secs(3);
                let mut socket = loop {
                    match listener.accept() {
                        Ok((socket, _)) => {
                            socket.set_nonblocking(false).unwrap();
                            break socket;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "plugin did not open its URL");
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("fixture accept: {error}"),
                    }
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    socket.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                    assert!(request.len() < 8192);
                }
                let request = String::from_utf8(request).unwrap();
                let pid = request
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .strip_prefix('/')
                    .unwrap()
                    .to_owned();
                assert!(pid.chars().all(|character| character.is_ascii_digit()));
                assert!(
                    process_alive(&pid),
                    "provider exited before its URL was opened"
                );
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\nContent-Type: audio/flac\r\nConnection: close\r\n\r\nfixture").unwrap();
                pid_sender.send(pid.clone()).unwrap();
                // The caller has a partial stream and need not read another
                // byte while paused. Its provider must retain the URL lease.
                std::thread::sleep(Duration::from_millis(100));
                assert!(
                    process_alive(&pid),
                    "provider exited while playback was paused"
                );
                pid_sender.send(pid.clone()).unwrap();
                closed_receiver
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap();
                assert!(!process_alive(&pid), "dropped stream retained its provider");
                drop(socket);
            }
        });
        let source = shell(&format!(
            "IFS= read -r request; printf '{{\"api_version\":1,\"id\":1,\"result\":{{\"url\":\"{endpoint}/%s\"}}}}\\n' \"$$\"; IFS= read -r unused"
        ));
        let page = normalize_page(
            &source,
            serde_json::from_value(json!({"tracks": [{"item_id": "media", "title": "Media"}]}))
                .unwrap(),
        )
        .unwrap();
        let track = &page.tracks[0];
        let mut previous_pid = None;
        for _ in 0..2 {
            let mut stream = ProcessPlugin
                .open_stream(&source, track, Arc::new(AtomicBool::new(false)))
                .unwrap();
            let pid = pid_receiver.recv_timeout(Duration::from_secs(3)).unwrap();
            assert_ne!(previous_pid.as_ref(), Some(&pid));
            let mut bytes = [0; 7];
            stream.read_exact(&mut bytes).unwrap();
            assert_eq!(&bytes, b"fixture");
            assert_eq!(
                pid_receiver.recv_timeout(Duration::from_secs(3)).unwrap(),
                pid
            );
            drop(stream);
            assert!(!process_alive(&pid));
            closed_sender.send(()).unwrap();
            previous_pid = Some(pid);
        }
        assert_eq!(track.location, MediaLocation::Provider);
        server.join().unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn hangs_are_timed_out_and_partial_json_is_rejected() {
        let cancel = AtomicBool::new(false);
        let started = Instant::now();
        let result = Session::request_with_timeout(
            &shell("IFS= read -r request; while :; do :; done"),
            "browse",
            json!({}),
            &cancel,
            Duration::from_millis(100),
        );
        assert!(matches!(result, Err(message) if message.contains("timed out")));
        assert!(started.elapsed() < Duration::from_secs(3));
        let partial = Session::request(
            &shell("IFS= read -r request; printf '{'"),
            "browse",
            json!({}),
            &cancel,
        );
        assert!(matches!(partial, Err(message) if message.contains("complete JSON line")));
    }

    #[test]
    #[cfg(unix)]
    fn cancellation_interrupts_a_waiting_plugin() {
        let cancel = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(100));
                cancel.store(true, Ordering::Relaxed);
            });
            let result = ProcessPlugin.browse_cancellable(
                &shell("IFS= read -r request; while :; do :; done"),
                None,
                &cancel,
            );
            assert!(matches!(result, Err(message) if message.contains("cancelled")));
        });
    }

    #[test]
    #[cfg(unix)]
    fn oversized_response_is_rejected_without_waiting_for_eof() {
        // Shell builtins only: no external fixture runtime and no pipe-holding descendants.
        let source = shell(
            "IFS= read -r request; block=xxxxxxxxxxxxxxxx; i=0; while [ $i -lt 17 ]; do block=$block$block; i=$((i+1)); done; printf '%s%sx' \"$block\" \"$block\"; IFS= read -r unused",
        );
        let result = Session::request(&source, "browse", json!({}), &AtomicBool::new(false));
        assert!(matches!(result, Err(message) if message.contains("exceeds 4 MiB")));
    }

    #[test]
    fn rejects_relative_executables_and_pre_cancelled_requests() {
        let result = Session::request(
            &source(json!({"executable": "plugin"})),
            "browse",
            json!({}),
            &AtomicBool::new(false),
        );
        assert!(matches!(result, Err(message) if message.contains("absolute path")));
        let result = Session::request(
            &source(Value::Null),
            "browse",
            json!({}),
            &AtomicBool::new(true),
        );
        assert!(matches!(result, Err(message) if message.contains("cancelled")));
    }

    #[test]
    #[cfg(target_os = "scarlet")]
    fn scarlet_refuses_to_spawn_until_child_kill_is_available() {
        let config = source(json!({"executable": "/plugin"}));
        let result = ProcessPlugin.browse(&config, None);
        assert!(matches!(result, Err(message) if message.contains("Child::kill")));
    }
}

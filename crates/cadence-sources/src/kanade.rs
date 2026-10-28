use crate::*;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    io::ErrorKind,
    net::{TcpStream, ToSocketAddrs},
    sync::{Arc, Mutex, atomic::Ordering},
    time::{Duration, Instant},
};
use tungstenite::{
    Message, WebSocket, client::IntoClientRequest, protocol::WebSocketConfig,
    stream::MaybeTlsStream,
};
use url::Url;

const MAX_ENTRIES: usize = 20_000;
const MAX_CATALOG_TRACKS: usize = 100_000;
const MAX_CATALOG_BYTES: usize = 64 * 1024 * 1024;
const CATALOG_PAGE_SIZE: usize = 500;
const RPC_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) struct KanadePlugin;

impl SourcePlugin for KanadePlugin {
    fn validate_config(&self, config: &SourceConfig) -> Result<()> {
        let url = websocket_url(config)?;
        if let Some(identity) = config.settings.get("kanade_app_identity") {
            let enabled = identity
                .as_bool()
                .ok_or("Kanade certificate setting must be true or false")?;
            if enabled && url.scheme() != "wss" {
                return Err(
                    "Kanade client certificates require an HTTPS or WSS server address".into(),
                );
            }
        }
        Ok(())
    }
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            id: "kanade".into(),
            name: "Kanade".into(),
            api_version: SOURCE_API_VERSION,
            capabilities: vec![
                SourceCapability::Browse,
                SourceCapability::Artwork,
                SourceCapability::Stream,
                SourceCapability::Download,
            ],
        }
    }
    fn browse(&self, config: &SourceConfig, container: Option<&str>) -> Result<SourcePage> {
        self.browse_cancellable(config, container, &AtomicBool::new(false))
    }
    fn catalog_cancellable(
        &self,
        config: &SourceConfig,
        cancel: &AtomicBool,
    ) -> Result<Vec<LibraryTrack>> {
        let mut session = Session::open(config, cancel)?;
        let mut tracks = Vec::new();
        let mut identities = HashSet::new();
        let mut catalog_bytes = 0usize;
        loop {
            check_cancel(cancel)?;
            let response = session.request(
                json!({"req":"get_tracks", "offset":tracks.len(), "limit":CATALOG_PAGE_SIZE}),
                cancel,
            )?;
            let page = response
                .pointer("/tracks/tracks")
                .and_then(Value::as_array)
                .ok_or("Kanade returned an invalid library page")?;
            if page.len() > CATALOG_PAGE_SIZE || tracks.len() + page.len() > MAX_CATALOG_TRACKS {
                return Err(
                    "Kanade library exceeds the 100,000 track limit or returned an oversized page"
                        .into(),
                );
            }
            for value in page {
                check_cancel(cancel)?;
                let track = library_track(config, value, "")?;
                catalog_bytes = catalog_bytes.saturating_add(
                    serde_json::to_vec(&track)
                        .map_err(|_| "Cannot encode Kanade library metadata")?
                        .len(),
                );
                if catalog_bytes > MAX_CATALOG_BYTES {
                    return Err("Kanade library metadata exceeds the 64 MiB limit".into());
                }
                if !identities.insert(track.item_id.clone()) {
                    return Err(
                        "Kanade library changed while loading or repeated a page; refresh to retry"
                            .into(),
                    );
                }
                tracks.push(track);
            }
            if page.len() < CATALOG_PAGE_SIZE {
                return Ok(tracks);
            }
        }
    }
    fn browse_cancellable(
        &self,
        config: &SourceConfig,
        container: Option<&str>,
        cancel: &AtomicBool,
    ) -> Result<SourcePage> {
        check_cancel(cancel)?;
        let mut session = Session::open(config, cancel)?;
        match container {
            None => {
                let response = session.request(json!({"req":"get_albums"}), cancel)?;
                let albums = response
                    .pointer("/albums/albums")
                    .and_then(Value::as_array)
                    .ok_or("Kanade returned an invalid album list")?;
                if albums.len() > MAX_ENTRIES {
                    return Err("Kanade album list exceeds the 20,000 entry limit".into());
                }
                let mut containers = Vec::with_capacity(albums.len());
                for album in albums {
                    let id = required_text(album, "id")?;
                    let title = optional_text(album, "title").unwrap_or("Untitled album");
                    let title = match optional_text(album, "artist") {
                        Some(artist) => format!("{title} — {artist}"),
                        None => title.to_owned(),
                    };
                    containers.push(SourceContainer {
                        id: id.to_owned(),
                        title,
                    });
                }
                Ok(SourcePage {
                    containers,
                    tracks: Vec::new(),
                })
            }
            Some(album_id) => {
                let response = session.request(
                    json!({"req":"get_album_tracks","album_id":album_id}),
                    cancel,
                )?;
                let tracks = response
                    .pointer("/album_tracks/tracks")
                    .and_then(Value::as_array)
                    .ok_or("Kanade returned an invalid track list")?;
                if tracks.len() > MAX_ENTRIES {
                    return Err("Kanade album exceeds the 20,000 track limit".into());
                }
                let tracks = tracks
                    .iter()
                    .map(|track| library_track(config, track, album_id))
                    .collect::<Result<Vec<_>>>()?;
                Ok(SourcePage {
                    containers: Vec::new(),
                    tracks,
                })
            }
        }
    }
    fn open_stream(
        &self,
        config: &SourceConfig,
        track: &LibraryTrack,
        cancel: Arc<AtomicBool>,
    ) -> Result<Box<dyn cadence_core::MediaSource>> {
        if track.source_id != config.id {
            return Err("Track belongs to a different source".into());
        }
        if !matches!(track.location, MediaLocation::Provider) {
            return Err("Kanade tracks must use provider identities".into());
        }
        let session = Session::open(config, &cancel)?;
        let client = session.transport.http_agent();
        let session = Arc::new(Mutex::new(session));
        let path = media_path("tracks", &track.item_id)?;
        let resolver_session = session.clone();
        let stream = crate::stream::HttpStream::open_resolving_with_client(
            cancel.clone(),
            client,
            move |cancel| {
                resolver_session
                    .lock()
                    .map_err(|_| "Kanade session failed")?
                    .sign(&path, cancel)
            },
        )?;
        // The signature is scoped to this WebSocket, including HTTP range
        // requests after a seek. Heartbeats must continue while audio is paused.
        let lease = crate::stream::SessionLease::spawn(cancel, move || {
            session.lock().map_err(|_| "Kanade session failed")?.pump()
        })?;
        Ok(Box::new(stream.with_lease(lease)))
    }
    fn materialize(
        &self,
        config: &SourceConfig,
        track: &LibraryTrack,
        cache: &Path,
        cancel: &AtomicBool,
    ) -> Result<PathBuf> {
        if cancel.load(Ordering::Relaxed) {
            return Err("Source request cancelled".into());
        }
        if let Some(path) = net::cached(&track.id, cache) {
            return Ok(path);
        }
        let mut session = Session::open(config, cancel)?;
        let signed = session.sign(&media_path("tracks", &track.item_id)?, cancel)?;
        session.nonblocking()?;
        // Kanade revokes the signature on disconnect. Retain the session until
        // all HTTP bytes arrive and service its heartbeat between reads.
        let client = session.transport.http_agent();
        net::download_with_client(
            &signed,
            &track.id,
            cache,
            cancel,
            &mut || session.pump(),
            &client,
        )
    }
    fn fetch_artwork(
        &self,
        config: &SourceConfig,
        artwork: &ArtworkRef,
        _: &Path,
        cancel: &AtomicBool,
    ) -> Result<ArtworkData> {
        let ArtworkRef::Provider { item_id, .. } = artwork else {
            return Err("Kanade artwork requires a provider reference".into());
        };
        if cancel.load(Ordering::Relaxed) {
            return Err("Source request cancelled".into());
        }
        let mut session = Session::open(config, cancel)?;
        let signed = session.sign(&media_path("art", item_id)?, cancel)?;
        session.nonblocking()?;
        let client = session.transport.http_agent();
        net::artwork_with_client(&signed, cancel, &mut || session.pump(), &client)
    }
}
fn optional_text<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}
fn required_text<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    optional_text(value, field).ok_or_else(|| format!("Kanade response is missing {field}"))
}
fn positive_number(value: &Value, field: &str) -> Option<u32> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
}
fn library_track(config: &SourceConfig, value: &Value, album_id: &str) -> Result<LibraryTrack> {
    let item_id = required_text(value, "id")?.to_owned();
    let album_id = optional_text(value, "album_id").unwrap_or(album_id);
    Ok(LibraryTrack {
        id: cadence_library::track_id(&config.id, &item_id),
        source_id: config.id.clone(),
        title: optional_text(value, "title")
            .unwrap_or("Untitled track")
            .to_owned(),
        artist: optional_text(value, "artist").map(str::to_owned),
        album: optional_text(value, "album_title").map(str::to_owned),
        album_id: (!album_id.is_empty()).then(|| album_id.to_owned()),
        album_artist: optional_text(value, "album_artist").map(str::to_owned),
        track_number: positive_number(value, "track_number"),
        disc_number: positive_number(value, "disc_number"),
        duration_seconds: value
            .get("duration_secs")
            .and_then(Value::as_f64)
            .filter(|v| Duration::try_from_secs_f64(*v).is_ok()),
        item_id,
        location: MediaLocation::Provider,
        artwork: (!album_id.is_empty()).then(|| ArtworkRef::Provider {
            source_id: config.id.clone(),
            item_id: album_id.to_owned(),
        }),
    })
}
fn media_path(kind: &str, id: &str) -> Result<String> {
    if id.is_empty() {
        return Err("Kanade item ID is empty".into());
    }
    let mut url = Url::parse("http://localhost/media/").expect("constant media URL");
    url.path_segments_mut()
        .map_err(|_| "Invalid Kanade media path")?
        .pop_if_empty()
        .push(kind)
        .push(id);
    Ok(url.path().to_owned())
}
fn websocket_url(config: &SourceConfig) -> Result<Url> {
    let value = config
        .settings
        .get("url")
        .and_then(Value::as_str)
        .ok_or("Kanade source needs a URL")?;
    let mut url = Url::parse(value).map_err(|_| "Invalid Kanade URL")?;
    match url.scheme() {
        "ws" => {
            url.set_scheme("http").map_err(|_| "Invalid Kanade URL")?;
        }
        "wss" => {
            url.set_scheme("https").map_err(|_| "Invalid Kanade URL")?;
        }
        _ => {}
    }
    let mut url = net::http_url(url.as_str())?;
    if url.query().is_some() || url.fragment().is_some() {
        return Err("Kanade URL must not contain query parameters or a fragment".into());
    }
    if !url.path().trim_end_matches('/').ends_with("/ws") {
        url.path_segments_mut()
            .map_err(|_| "Invalid Kanade URL")?
            .pop_if_empty()
            .push("ws");
    }
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme)
        .map_err(|_| "Invalid Kanade WebSocket URL")?;
    Ok(url)
}
struct Session {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
    next_id: u64,
    secure: bool,
    transport: crate::kanade_tls::Transport,
}
impl Session {
    fn open(config: &SourceConfig, cancel: &AtomicBool) -> Result<Self> {
        check_cancel(cancel)?;
        let url = websocket_url(config)?;
        let transport = crate::kanade_tls::Transport::for_source(config)?;
        // url's socket_addrs helper is gated to its known OS list. Scarlet's
        // standard library implements ToSocketAddrs, including IPv6 literals.
        let host = url
            .host_str()
            .ok_or("Kanade URL has no host")?
            .trim_start_matches('[')
            .trim_end_matches(']');
        let port = url
            .port_or_known_default()
            .ok_or("Kanade URL has no port")?;
        let deadline = Instant::now() + RPC_TIMEOUT;
        let host = host.to_owned();
        let addresses = crate::stream::bounded_open(
            cancel,
            deadline,
            "Kanade hostname resolution timed out",
            move || {
                (host.as_str(), port)
                    .to_socket_addrs()
                    .map(|addresses| addresses.take(8).collect::<Vec<_>>())
                    .map_err(|_| "Kanade hostname could not be resolved".into())
            },
        )?;
        let mut connected = None;
        for address in addresses {
            check_cancel(cancel)?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            if let Ok(socket) =
                TcpStream::connect_timeout(&address, remaining.min(Duration::from_secs(3)))
            {
                connected = Some(socket);
                break;
            }
        }
        let stream = connected.ok_or("Cannot connect to Kanade")?;
        stream
            .set_read_timeout(Some(RPC_TIMEOUT))
            .map_err(|_| "Cannot set Kanade timeout")?;
        stream
            .set_write_timeout(Some(RPC_TIMEOUT))
            .map_err(|_| "Cannot set Kanade timeout")?;
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|_| "Invalid Kanade WebSocket request")?;
        if let Some(token) = config.settings.get("token").and_then(Value::as_str) {
            request.headers_mut().insert(
                "Authorization",
                format!("Bearer {token}")
                    .parse()
                    .map_err(|_| "Invalid Kanade token")?,
            );
        }
        let ws_config = WebSocketConfig::default()
            .max_message_size(Some(8 * 1024 * 1024))
            .max_frame_size(Some(8 * 1024 * 1024))
            .max_write_buffer_size(256 * 1024);
        #[cfg(not(target_os = "scarlet"))]
        let (socket, _) = tungstenite::client_tls_with_config(
            request,
            stream,
            Some(ws_config),
            transport.ws_connector(),
        )
        .map_err(|error| match error {
            tungstenite::HandshakeError::Failure(error) => handshake_error(error),
            tungstenite::HandshakeError::Interrupted(_) => {
                "Kanade WebSocket handshake timed out".into()
            }
        })?;
        #[cfg(target_os = "scarlet")]
        let (socket, _) = tungstenite::client::client_with_config(
            request,
            MaybeTlsStream::Plain(stream),
            Some(ws_config),
        )
        .map_err(|_| "Kanade WebSocket handshake failed")?;
        Ok(Self {
            socket,
            next_id: 1,
            secure: url.scheme() == "wss",
            transport,
        })
    }
    fn request(&mut self, mut request: Value, cancel: &AtomicBool) -> Result<Value> {
        check_cancel(cancel)?;
        let id = self.next_id;
        self.next_id += 1;
        request["req_id"] = id.into();
        // The first request also registers the UI session: waiting for media_auth
        // before sending would cause Kanade's 10 second handshake to time out.
        self.socket
            .send(Message::Text(request.to_string().into()))
            .map_err(|_| "Cannot send Kanade request")?;
        self.nonblocking()?;
        let deadline = Instant::now() + RPC_TIMEOUT;
        for _ in 0..1024 {
            check_cancel(cancel)?;
            if Instant::now() > deadline {
                return Err("Kanade request timed out".into());
            }
            let message = match self.socket.read() {
                Ok(message) => message,
                Err(tungstenite::Error::Io(error)) if error.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                Err(_) => return Err("Kanade connection closed or timed out".into()),
            };
            match message {
                Message::Text(text) => {
                    let response: Value =
                        serde_json::from_str(&text).map_err(|_| "Kanade returned invalid JSON")?;
                    if response.get("type").and_then(Value::as_str) == Some("response")
                        && response.get("req_id").and_then(Value::as_u64) == Some(id)
                    {
                        return response
                            .get("data")
                            .cloned()
                            .ok_or_else(|| "Kanade returned an empty response".into());
                    }
                }
                Message::Close(_) => return Err("Kanade closed the source session".into()),
                _ => {}
            }
        }
        Err("Kanade sent too many events before its response".into())
    }
    fn sign(&mut self, path: &str, cancel: &AtomicBool) -> Result<String> {
        let response = self.request(json!({"req":"sign_urls","paths":[path]}), cancel)?;
        let signed = response
            .pointer("/signed_urls/urls")
            .and_then(|urls| urls.get(path))
            .and_then(Value::as_str)
            .ok_or("Kanade did not sign the requested media")?;
        let url = net::http_url(signed)?;
        self.transport.validate_media_url(signed)?;
        if self.secure && url.scheme() != "https" {
            return Err("Kanade returned insecure media for a secure connection".into());
        }
        Ok(signed.to_owned())
    }
    fn nonblocking(&mut self) -> Result<()> {
        let stream = match self.socket.get_mut() {
            MaybeTlsStream::Plain(stream) => stream,
            #[cfg(not(target_os = "scarlet"))]
            MaybeTlsStream::Rustls(stream) => &mut stream.sock,
            _ => return Err("Unsupported Kanade WebSocket transport".into()),
        };
        stream
            .set_nonblocking(true)
            .map_err(|_| "Cannot service Kanade media session".into())
    }
    fn pump(&mut self) -> Result<()> {
        for _ in 0..32 {
            match self.socket.read() {
                Ok(Message::Close(_)) => {
                    return Err("Kanade media session expired during playback".into());
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(error)) if error.kind() == ErrorKind::WouldBlock => {
                    break;
                }
                Err(_) => return Err("Kanade media session disconnected during playback".into()),
            }
        }
        match self.socket.flush() {
            Ok(()) => Ok(()),
            Err(tungstenite::Error::Io(error)) if error.kind() == ErrorKind::WouldBlock => Ok(()),
            Err(_) => Err("Kanade heartbeat failed during playback".into()),
        }
    }
}
#[cfg(not(target_os = "scarlet"))]
fn handshake_error(error: tungstenite::Error) -> String {
    // Never expose headers, response bodies, or token-bearing URLs in UI errors.
    match error {
        tungstenite::Error::Http(response) => match response.status().as_u16() {
            status @ (401 | 403) => format!(
                "Kanade access was denied (HTTP {status}). Check the server's client certificate or proxy authorization, then refresh."
            ),
            status => format!("Kanade WebSocket handshake failed (HTTP {status})"),
        },
        _ => "Kanade WebSocket TLS connection or handshake failed".into(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn server_paths_and_signed_urls_never_enter_library_tracks() {
        let config = SourceConfig {
            id: "home".into(),
            name: "Home".into(),
            plugin_id: "kanade".into(),
            settings: json!({"url":"http://localhost:3000"}),
        };
        let value = json!({"id":"track-1","file_path":"/private/server/music.flac","album_id":"album-1","title":"Song","album_artist":"Album Artist","track_number":7,"disc_number":2,"duration_secs":42});
        let track = library_track(&config, &value, "fallback").unwrap();
        assert_eq!(track.location, MediaLocation::Provider);
        assert_eq!(track.album_id.as_deref(), Some("album-1"));
        assert_eq!(track.album_artist.as_deref(), Some("Album Artist"));
        assert_eq!(track.track_number, Some(7));
        assert_eq!(track.disc_number, Some(2));
        let fallback = library_track(&config, &json!({"id":"no-tags"}), "container-album").unwrap();
        assert_eq!(fallback.album_id.as_deref(), Some("container-album"));
        assert_eq!(fallback.album_artist, None);
        assert_eq!(fallback.track_number, None);
        assert_eq!(fallback.disc_number, None);
        assert_eq!(
            track.artwork,
            Some(ArtworkRef::Provider {
                source_id: "home".into(),
                item_id: "album-1".into()
            })
        );
        assert!(!serde_json::to_string(&track).unwrap().contains("private"));
    }
    #[test]
    fn websocket_endpoint_and_media_ids_are_encoded() {
        let mut config = SourceConfig {
            id: "home".into(),
            name: "Home".into(),
            plugin_id: "kanade".into(),
            settings: json!({"url":"http://localhost:3000/kanade/"}),
        };
        assert_eq!(
            websocket_url(&config).unwrap().as_str(),
            "ws://localhost:3000/kanade/ws"
        );
        config.settings = json!({"url":"ws://localhost:3000/ws"});
        assert_eq!(
            websocket_url(&config).unwrap().as_str(),
            "ws://localhost:3000/ws"
        );
        assert_eq!(
            media_path("tracks", "a/b ?").unwrap(),
            "/media/tracks/a%2Fb%20%3F"
        );
    }
    #[test]
    fn albumless_tracks_have_no_empty_artwork_reference() {
        let config = SourceConfig {
            id: "home".into(),
            name: "Home".into(),
            plugin_id: "kanade".into(),
            settings: json!({"url":"https://example.com"}),
        };
        let track = library_track(&config, &json!({"id":"single", "album_id":null}), "").unwrap();
        assert_eq!(track.album_id, None);
        assert_eq!(track.artwork, None);
        for url in [
            "https://user:secret@example.com",
            "https://example.com/?token=secret",
            "file:///music",
        ] {
            let mut invalid = config.clone();
            invalid.settings = json!({"url":url});
            assert!(SourceRegistry::builtin().validate_config(&invalid).is_err());
        }
    }
}

#[cfg(test)]
mod protocol_tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{Arc, mpsc},
        thread,
    };

    fn config(url: String) -> SourceConfig {
        SourceConfig {
            id: "fixture".into(),
            name: "Fixture".into(),
            plugin_id: "kanade".into(),
            settings: json!({"url":url}),
        }
    }
    fn response(socket: &mut WebSocket<TcpStream>, request: &Value, data: Value) {
        socket
            .send(Message::Text(
                json!({"type":"response", "req_id":request["req_id"],"data":data})
                    .to_string()
                    .into(),
            ))
            .unwrap();
    }
    #[test]
    fn browse_sends_request_before_auth_and_decodes_actual_enum_shape() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let source = config(format!("http://{}", listener.local_addr().unwrap()));
        let server = thread::spawn(move || {
            for album in [false, true] {
                let (stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut socket = tungstenite::accept(stream).unwrap();
                // Kanade waits for this request before assigning the UI role.
                let request: Value =
                    serde_json::from_str(socket.read().unwrap().to_text().unwrap()).unwrap();
                socket
                    .send(Message::Text(
                        json!({"type":"media_auth","media_auth_key_id":"fixture-key"})
                            .to_string()
                            .into(),
                    ))
                    .unwrap();
                if album {
                    assert_eq!(request["req"], "get_album_tracks");
                    assert_eq!(request["album_id"], "album-1");
                    response(
                        &mut socket,
                        &request,
                        json!({"album_tracks":{"tracks":[{"id":"song-1","title":"Song","album_id":"album-1","album_title":"Album","duration_secs":3.5,"file_path":"/server/private/song.flac"}]}}),
                    );
                } else {
                    assert_eq!(request["req"], "get_albums");
                    response(
                        &mut socket,
                        &request,
                        json!({"albums":{"albums":[{"id":"album-1","title":"Album","artist":"Artist"}]}}),
                    );
                }
            }
        });
        let root = KanadePlugin.browse(&source, None).unwrap();
        assert_eq!(root.containers[0].id, "album-1");
        let album = KanadePlugin.browse(&source, Some("album-1")).unwrap();
        assert_eq!(album.tracks[0].title, "Song");
        assert_eq!(album.tracks[0].duration_seconds, Some(3.5));
        assert_eq!(album.tracks[0].location, MediaLocation::Provider);
        server.join().unwrap();
    }

    fn media_fixture(kind: &'static str) -> (SourceConfig, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let source = config(base.clone());
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            let request: Value =
                serde_json::from_str(socket.read().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(request["req"], "sign_urls");
            let path = format!("/media/{kind}/item-1");
            assert_eq!(request["paths"], json!([path]));
            response(
                &mut socket,
                &request,
                json!({"signed_urls":{"urls":{path.clone():format!("{base}{path}?sig=ephemeral")}}}),
            );
            let alive = Arc::new(AtomicBool::new(true));
            let pong = Arc::new(AtomicBool::new(false));
            let (ping_now, ping_request) = mpsc::channel();
            let lease_alive = alive.clone();
            let saw_pong = pong.clone();
            let heartbeat = thread::spawn(move || {
                ping_request.recv_timeout(Duration::from_secs(3)).unwrap();
                socket.send(Message::Ping(vec![7].into())).unwrap();
                while let Ok(message) = socket.read() {
                    if let Message::Pong(bytes) = message {
                        assert_eq!(bytes.as_ref(), &[7]);
                        saw_pong.store(true, Ordering::SeqCst);
                    }
                }
                lease_alive.store(false, Ordering::SeqCst);
            });
            let (mut http, _) = listener.accept().unwrap();
            http.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let mut request = [0u8; 4096];
            let count = http.read(&mut request).unwrap();
            let request = std::str::from_utf8(&request[..count]).unwrap();
            assert!(request.starts_with(&format!("GET {path}?sig=ephemeral HTTP/1.1")));
            assert!(
                alive.load(Ordering::SeqCst),
                "signature revoked before HTTP fetch"
            );
            // Trigger the heartbeat after HTTP begins, then deliver one chunk so
            // the client's lease pump must service the ping before the next chunk.
            ping_now.send(()).unwrap();
            http.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 131072\r\nContent-Type: image/png\r\nConnection: close\r\n\r\n").unwrap();
            http.write_all(&vec![42; 65536]).unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while !pong.load(Ordering::SeqCst) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            assert!(
                pong.load(Ordering::SeqCst),
                "heartbeat must be serviced during media download"
            );
            assert!(
                alive.load(Ordering::SeqCst),
                "session must outlive complete download"
            );
            http.write_all(&vec![42; 65536]).unwrap();
            drop(http);
            heartbeat.join().unwrap();
        });
        (source, server)
    }
    #[test]
    fn signed_media_and_artwork_keep_session_alive_and_answer_heartbeat() {
        let (source, server) = media_fixture("tracks");
        let track = library_track(&source, &json!({"id":"item-1"}), "album").unwrap();
        let cache =
            std::env::temp_dir().join(format!("cadence-kanade-fixture-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        let file = KanadePlugin
            .materialize(&source, &track, &cache, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), vec![42; 131072]);
        server.join().unwrap();
        std::fs::remove_dir_all(cache).unwrap();
        let (source, server) = media_fixture("art");
        let artwork = ArtworkRef::Provider {
            source_id: source.id.clone(),
            item_id: "item-1".into(),
        };
        let bytes = KanadePlugin
            .fetch_artwork(
                &source,
                &artwork,
                Path::new("unused"),
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(bytes.bytes, vec![42; 131072]);
        assert_eq!(bytes.media_type, "image/png");
        server.join().unwrap();
    }

    #[test]
    fn streaming_services_heartbeats_during_reads_and_pause_and_resigns_after_drop() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let source = config(base.clone());
        let (pause_sender, pause_receiver) = mpsc::channel();
        let (event_sender, event_receiver) = mpsc::channel();
        let server = thread::spawn(move || {
            let accept = || {
                let deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            // Darwin inherits O_NONBLOCK from the listener.
                            // This fixture uses timed blocking WebSocket I/O.
                            stream.set_nonblocking(false).unwrap();
                            stream
                                .set_read_timeout(Some(Duration::from_secs(3)))
                                .unwrap();
                            stream
                                .set_write_timeout(Some(Duration::from_secs(3)))
                                .unwrap();
                            break stream;
                        }
                        Err(error) if error.kind() == ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "stream never connected");
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("fixture accept: {error}"),
                    }
                }
            };
            for generation in 0..2 {
                let mut socket = tungstenite::accept(accept()).unwrap();
                let request: Value =
                    serde_json::from_str(socket.read().unwrap().to_text().unwrap()).unwrap();
                assert_eq!(request["req"], "sign_urls");
                let path = "/media/tracks/item-1";
                assert_eq!(request["paths"], json!([path]));
                response(
                    &mut socket,
                    &request,
                    json!({"signed_urls":{"urls":{path:format!("{base}{path}?sig=session-{generation}")}}}),
                );
                let mut http = accept();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    http.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                    assert!(request.len() < 8192);
                }
                assert!(
                    String::from_utf8(request)
                        .unwrap()
                        .starts_with(&format!("GET {path}?sig=session-{generation} HTTP/1.1"))
                );
                // An intentionally unfinished body proves opening a stream does
                // not download the entire track before playback may start.
                http.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\nContent-Type: audio/flac\r\nConnection: close\r\n\r\n").unwrap();
                http.write_all(&[42; 16]).unwrap();
                socket.send(Message::Ping(vec![1].into())).unwrap();
                assert!(
                    matches!(socket.read().unwrap(), Message::Pong(bytes) if bytes.as_ref() == [1])
                );
                http.write_all(&[42; 16]).unwrap();
                event_sender.send((generation, "reading")).unwrap();
                pause_receiver.recv_timeout(Duration::from_secs(3)).unwrap();
                socket.send(Message::Ping(vec![2].into())).unwrap();
                assert!(
                    matches!(socket.read().unwrap(), Message::Pong(bytes) if bytes.as_ref() == [2])
                );
                event_sender.send((generation, "paused")).unwrap();
                // Dropping the reader must revoke this session promptly, even
                // though the HTTP body has not reached EOF.
                match socket.read() {
                    Ok(Message::Close(_)) => (),
                    Err(tungstenite::Error::Protocol(_))
                    | Err(tungstenite::Error::ConnectionClosed)
                    | Err(tungstenite::Error::AlreadyClosed) => (),
                    Err(tungstenite::Error::Io(error)) => {
                        assert!(!matches!(
                            error.kind(),
                            ErrorKind::WouldBlock | ErrorKind::TimedOut
                        ));
                    }
                    result => panic!("session did not close with stream: {result:?}"),
                }
                drop(http);
                event_sender.send((generation, "closed")).unwrap();
            }
        });
        let track = library_track(&source, &json!({"id":"item-1"}), "album").unwrap();
        for generation in 0..2 {
            let mut stream = KanadePlugin
                .open_stream(&source, &track, Arc::new(AtomicBool::new(false)))
                .unwrap();
            let mut bytes = [0; 32];
            stream.read_exact(&mut bytes).unwrap();
            assert_eq!(bytes, [42; 32]);
            assert_eq!(
                event_receiver.recv_timeout(Duration::from_secs(3)).unwrap(),
                (generation, "reading")
            );
            pause_sender.send(()).unwrap();
            assert_eq!(
                event_receiver.recv_timeout(Duration::from_secs(3)).unwrap(),
                (generation, "paused")
            );
            drop(stream);
            assert_eq!(
                event_receiver.recv_timeout(Duration::from_secs(3)).unwrap(),
                (generation, "closed")
            );
        }
        assert!(!serde_json::to_string(&track).unwrap().contains("sig="));
        server.join().unwrap();
    }
    #[test]
    fn certificate_configuration_requires_https_before_persistence() {
        let mut insecure_identity = config("http://localhost".into());
        insecure_identity.settings = json!({"url":"http://localhost", "kanade_app_identity":true});
        assert!(
            SourceRegistry::builtin()
                .validate_config(&insecure_identity)
                .is_err()
        );
    }
    #[test]
    fn cancelled_browse_does_not_connect() {
        let source = config("http://127.0.0.1:1".into());
        assert!(
            KanadePlugin
                .browse_cancellable(&source, None, &AtomicBool::new(true))
                .unwrap_err()
                .contains("cancelled")
        );
    }
    #[test]
    fn catalog_pages_all_tracks_in_one_session_including_albumless_songs() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let source = config(format!("http://{}", listener.local_addr().unwrap()));
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            for page in 0..2 {
                let request: Value =
                    serde_json::from_str(socket.read().unwrap().to_text().unwrap()).unwrap();
                assert_eq!(request["req"], "get_tracks");
                assert_eq!(request["offset"], page * CATALOG_PAGE_SIZE);
                assert_eq!(request["limit"], CATALOG_PAGE_SIZE);
                let tracks = if page == 0 {
                    (0..CATALOG_PAGE_SIZE).map(|id| json!({
                        "id":format!("song-{id}"), "title":"Song", "album_id":"album",
                        "album_title":"Album", "album_artist":"Album Artist", "artist":"Artist",
                        "disc_number":2, "track_number":id+1, "duration_secs":42.5,
                        "file_path":"/private/server/audio.flac"
                    })).collect::<Vec<_>>()
                } else {
                    vec![json!({"id":"single", "title":"Single", "album_id":null})]
                };
                response(&mut socket, &request, json!({"tracks":{"tracks":tracks}}));
            }
        });
        let tracks = SourceRegistry::builtin()
            .catalog_cancellable(&source, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(tracks.len(), CATALOG_PAGE_SIZE + 1);
        assert_eq!(tracks[0].album.as_deref(), Some("Album"));
        assert_eq!(tracks[0].album_artist.as_deref(), Some("Album Artist"));
        assert_eq!(tracks[0].track_number, Some(1));
        assert_eq!(tracks[0].disc_number, Some(2));
        assert_eq!(tracks.last().unwrap().artwork, None);
        assert!(
            !serde_json::to_string(&tracks)
                .unwrap()
                .contains("/private/")
        );
        server.join().unwrap();
    }
    #[test]
    fn catalog_rejects_repeated_pages_and_malformed_results_without_partial_success() {
        for invalid in [
            json!({"albums":{"albums":[]}}),
            json!({"tracks":{"tracks":[{"id":"same"},{"id":"same"}]}}),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let source = config(format!("http://{}", listener.local_addr().unwrap()));
            let server = thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                let mut socket = tungstenite::accept(stream).unwrap();
                let request: Value =
                    serde_json::from_str(socket.read().unwrap().to_text().unwrap()).unwrap();
                response(&mut socket, &request, invalid);
            });
            assert!(
                KanadePlugin
                    .catalog_cancellable(&source, &AtomicBool::new(false))
                    .is_err()
            );
            server.join().unwrap();
        }
        let source = config("http://127.0.0.1:1".into());
        assert!(
            KanadePlugin
                .catalog_cancellable(&source, &AtomicBool::new(true))
                .unwrap_err()
                .contains("cancelled")
        );
    }
    #[test]
    fn denied_handshakes_explain_access_without_echoing_server_details() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let source = config(format!("http://{}", listener.local_addr().unwrap()));
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = [0; 4096];
            stream.read(&mut bytes).unwrap();
            stream.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 13\r\nConnection: close\r\n\r\nprivate-token").unwrap();
        });
        let error = KanadePlugin
            .catalog_cancellable(&source, &AtomicBool::new(false))
            .unwrap_err();
        assert!(error.contains("403"));
        assert!(error.contains("client certificate"));
        assert!(!error.contains("private-token"));
        server.join().unwrap();
    }
}

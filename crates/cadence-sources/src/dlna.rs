//! DLNA/UPnP ContentDirectory source, configured by a device or control URL.
//!
//! SSDP discovery is deliberately independent of the source contract. A device
//! description URL or an explicit ContentDirectory control URL is sufficient.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use cadence_library::{ArtworkData, ArtworkRef, LibraryTrack, MediaLocation, track_id};
use roxmltree::{Document, Node, ParsingOptions};
use sha2::{Digest, Sha256};
use url::Url;

use crate::{
    PluginDescriptor, SOURCE_API_VERSION, SourceCapability, SourceConfig, SourceContainer,
    SourcePage, SourcePlugin, net,
};

const CONTENT_DIRECTORY_PREFIX: &str = "urn:schemas-upnp-org:service:ContentDirectory:";
const DEFAULT_SERVICE_TYPE: &str = "urn:schemas-upnp-org:service:ContentDirectory:1";
const PAGE_SIZE: usize = 200;
const MAX_OBJECTS: usize = 20_000;
const MAX_REQUESTS: usize = 1_000;
const MAX_XML_BYTES: usize = 8 * 1024 * 1024;
const XML_OPTIONS: ParsingOptions = ParsingOptions {
    allow_dtd: false,
    nodes_limit: 200_000,
};

pub(crate) struct DlnaPlugin;

impl SourcePlugin for DlnaPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            id: "dlna".into(),
            name: "DLNA / UPnP".into(),
            api_version: SOURCE_API_VERSION,
            capabilities: vec![
                SourceCapability::Browse,
                SourceCapability::Artwork,
                SourceCapability::Download,
                SourceCapability::Stream,
            ],
        }
    }

    fn browse(&self, config: &SourceConfig, container: Option<&str>) -> crate::Result<SourcePage> {
        self.browse_cancellable(config, container, &AtomicBool::new(false))
    }

    fn browse_cancellable(
        &self,
        config: &SourceConfig,
        container: Option<&str>,
        cancel: &AtomicBool,
    ) -> crate::Result<SourcePage> {
        check_cancel(cancel)?;
        let object_id = container
            .or_else(|| setting(config, "root_id"))
            .unwrap_or("0");
        validate_object_id(object_id)?;
        let service = configured_service(config, cancel)?;
        let mut result = SourcePage::default();
        let mut start = 0usize;
        let mut update_id = None;
        let mut seen = HashSet::new();
        for request in 0..MAX_REQUESTS {
            check_cancel(cancel)?;
            let body = browse_request(&service.service_type, object_id, start, false);
            let action = format!("{}#Browse", service.service_type);
            let response = net::post_xml_cancellable(
                service.control_url.as_str(),
                &action,
                &body,
                MAX_XML_BYTES,
                cancel,
            )?;
            let page = parse_browse(&response, &service.control_url)?;
            check_cancel(cancel)?;
            if request > 0 && page.update_id != update_id {
                return Err("DLNA container changed while browsing; refresh to try again".into());
            }
            update_id = page.update_id;
            start = start
                .checked_add(page.number_returned)
                .ok_or("DLNA object count overflow")?;
            if start > MAX_OBJECTS {
                return Err(format!(
                    "DLNA container exceeds the {MAX_OBJECTS}-object browse limit"
                ));
            }
            for container in page.containers {
                if !seen.insert(container.id.clone()) {
                    return Err("DLNA server repeated an object while paging".into());
                }
                result.containers.push(container);
            }
            for track in page.tracks {
                if !seen.insert(track.id.clone()) {
                    return Err("DLNA server repeated an object while paging".into());
                }
                let album_id = album_identity(&track, Some(object_id));
                result.tracks.push(LibraryTrack {
                    id: track_id(&config.id, &track.id),
                    source_id: config.id.clone(),
                    item_id: track.id.clone(),
                    title: track.title,
                    artist: track.artist,
                    album: track.album,
                    album_id,
                    album_artist: track.album_artist,
                    track_number: track.track_number,
                    disc_number: track.disc_number,
                    duration_seconds: track.duration_ms.map(|ms| ms as f64 / 1000.0),
                    // Resources can expire or include credentials. Persist only
                    // the source/object identity and resolve a fresh URL at use.
                    location: MediaLocation::Provider,
                    artwork: track.artwork.map(|_| ArtworkRef::Provider {
                        source_id: config.id.clone(),
                        item_id: track.id,
                    }),
                });
            }
            if page.number_returned == 0 {
                if page.total_matches > start {
                    return Err("DLNA server stopped before returning all container objects".into());
                }
                return Ok(result);
            }
            // UPnP uses zero for an unknown TotalMatches, even after a short page.
            if page.total_matches > 0 && start >= page.total_matches {
                return Ok(result);
            }
        }
        Err("DLNA server exceeded the browse request limit".into())
    }

    fn open_stream(
        &self,
        config: &SourceConfig,
        track: &LibraryTrack,
        cancel: Arc<AtomicBool>,
    ) -> crate::Result<Box<dyn cadence_core::MediaSource>> {
        check_cancel(&cancel)?;
        if track.source_id != config.id {
            return Err("Track belongs to a different source".into());
        }
        match &track.location {
            MediaLocation::Provider => {
                let resolved = resolve_track(config, &track.item_id, &cancel)?;
                Ok(Box::new(crate::stream::HttpStream::open(
                    &resolved.url,
                    cancel,
                )?))
            }
            // Retain compatibility with libraries created before provider
            // identities were used for DLNA media.
            MediaLocation::Remote { url } => {
                Ok(Box::new(crate::stream::HttpStream::open(url, cancel)?))
            }
            MediaLocation::Local(path) => std::fs::File::open(path)
                .map(|file| Box::new(file) as Box<dyn cadence_core::MediaSource>)
                .map_err(|_| "Cannot open source media file".into()),
        }
    }

    fn materialize(
        &self,
        config: &SourceConfig,
        track: &LibraryTrack,
        cache: &Path,
        cancel: &AtomicBool,
    ) -> crate::Result<PathBuf> {
        check_cancel(cancel)?;
        if track.source_id != config.id {
            return Err("Track belongs to a different source".into());
        }
        match &track.location {
            MediaLocation::Provider => {
                if let Some(path) = net::cached(&track.id, cache) {
                    return Ok(path);
                }
                let resolved = resolve_track(config, &track.item_id, cancel)?;
                net::download(&resolved.url, &track.id, cache, cancel, &mut || Ok(()))
            }
            MediaLocation::Remote { url } => {
                net::download(url, &track.id, cache, cancel, &mut || Ok(()))
            }
            MediaLocation::Local(path) => Ok(path.clone()),
        }
    }

    fn fetch_artwork(
        &self,
        config: &SourceConfig,
        artwork: &ArtworkRef,
        _: &Path,
        cancel: &AtomicBool,
    ) -> crate::Result<ArtworkData> {
        check_cancel(cancel)?;
        match artwork {
            ArtworkRef::Provider { source_id, item_id } => {
                if source_id != &config.id {
                    return Err("Artwork belongs to a different source".into());
                }
                let resolved = resolve_track(config, item_id, cancel)?;
                let url = resolved.artwork.ok_or("DLNA item no longer has artwork")?;
                net::artwork(&url, cancel, &mut || Ok(()))
            }
            ArtworkRef::Remote { url } => net::artwork(url, cancel, &mut || Ok(())),
            ArtworkRef::File(_) | ArtworkRef::Embedded { .. } => {
                cadence_library::read_artwork(artwork, cadence_library::MAX_ARTWORK_BYTES)
            }
        }
    }
}

fn check_cancel(cancel: &AtomicBool) -> crate::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err("DLNA request cancelled".into())
    } else {
        Ok(())
    }
}

fn validate_object_id(object_id: &str) -> crate::Result<()> {
    if object_id.is_empty()
        || object_id.len() > 32_768
        || object_id.chars().any(|character| character.is_control())
    {
        Err("DLNA object ID is invalid".into())
    } else {
        Ok(())
    }
}

fn resolve_track(
    config: &SourceConfig,
    object_id: &str,
    cancel: &AtomicBool,
) -> crate::Result<DlnaTrack> {
    check_cancel(cancel)?;
    validate_object_id(object_id)?;
    let service = configured_service(config, cancel)?;
    let body = browse_request(&service.service_type, object_id, 0, true);
    let response = net::post_xml_cancellable(
        service.control_url.as_str(),
        &format!("{}#Browse", service.service_type),
        &body,
        MAX_XML_BYTES,
        cancel,
    )?;
    let mut page = parse_browse(&response, &service.control_url)?;
    check_cancel(cancel)?;
    if page.number_returned != 1
        || page.total_matches > 1
        || !page.containers.is_empty()
        || page.tracks.len() != 1
        || page.tracks[0].id != object_id
    {
        return Err("DLNA metadata did not identify the requested audio item".into());
    }
    Ok(page.tracks.remove(0))
}

fn setting<'a>(config: &'a SourceConfig, key: &str) -> Option<&'a str> {
    config
        .settings
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn configured_service(config: &SourceConfig, cancel: &AtomicBool) -> crate::Result<Service> {
    check_cancel(cancel)?;
    if let Some(control_url) = setting(config, "control_url") {
        let url = Url::parse(control_url).map_err(|_| "Invalid DLNA control URL")?;
        let service_type = setting(config, "service_type").unwrap_or(DEFAULT_SERVICE_TYPE);
        if !valid_service_type(service_type) {
            return Err("Invalid UPnP ContentDirectory service type".into());
        }
        return Ok(Service {
            control_url: http_url(&url, control_url)?,
            service_type: service_type.to_owned(),
        });
    }
    let description = setting(config, "description_url")
        .or_else(|| setting(config, "url"))
        .ok_or("DLNA source requires a device description_url or ContentDirectory control_url")?;
    let url = Url::parse(description).map_err(|_| "Invalid DLNA device description URL")?;
    let url = http_url(&url, description)?;
    let xml = net::get_text_cancellable(url.as_str(), MAX_XML_BYTES, cancel)?;
    let service = parse_service(&xml, &url)?;
    check_cancel(cancel)?;
    Ok(service)
}

#[derive(Debug)]
struct Service {
    control_url: Url,
    service_type: String,
}

#[derive(Debug, PartialEq)]
struct DlnaTrack {
    id: String,
    parent_id: Option<String>,
    title: String,
    artist: Option<String>,
    album: Option<String>,
    album_artist: Option<String>,
    track_number: Option<u32>,
    disc_number: Option<u32>,
    duration_ms: Option<u64>,
    url: String,
    artwork: Option<String>,
}

fn album_identity(track: &DlnaTrack, container: Option<&str>) -> Option<String> {
    let parent = track
        .parent_id
        .as_deref()
        .or(container)
        .filter(|parent| !parent.is_empty() && !matches!(*parent, "0" | "-1"));
    if parent.is_none() && track.album.is_none() {
        return None;
    }
    // A parent can be an artist, folder, or all-music container, rather than an
    // album. Include album metadata so those views cannot merge different albums.
    // Conversely, do not include track artists: compilations may vary per track.
    let identity = serde_json::to_vec(&(parent, &track.album, &track.album_artist))
        .expect("album identity contains only strings");
    Some(format!("dlna-album:{:x}", Sha256::digest(identity)))
}

#[derive(Debug)]
struct BrowsePage {
    containers: Vec<SourceContainer>,
    tracks: Vec<DlnaTrack>,
    number_returned: usize,
    total_matches: usize,
    update_id: Option<String>,
}

fn child_text<'a>(node: Node<'a, '_>, name: &str) -> Option<&'a str> {
    node.children()
        .find(|child| child.is_element() && child.tag_name().name() == name)
        .and_then(|child| child.text())
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

fn http_url(base: &Url, reference: &str) -> Result<Url, String> {
    let url = base
        .join(reference)
        .map_err(|_| "Invalid DLNA URL".to_string())?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("DLNA resources must use HTTP or HTTPS".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("DLNA URLs must not contain credentials".into());
    }
    Ok(url)
}

fn parse_service(xml: &str, description_url: &Url) -> Result<Service, String> {
    let document = Document::parse_with_options(xml, XML_OPTIONS)
        .map_err(|_| "Invalid UPnP description XML")?;
    let base = child_text(document.root_element(), "URLBase")
        .map(|reference| http_url(description_url, reference))
        .transpose()?
        .unwrap_or_else(|| description_url.clone());
    for node in document
        .descendants()
        .filter(|node| node.has_tag_name("service"))
    {
        let Some(service_type) = child_text(node, "serviceType") else {
            continue;
        };
        if !valid_service_type(service_type) {
            continue;
        }
        let control =
            child_text(node, "controlURL").ok_or("ContentDirectory service has no controlURL")?;
        return Ok(Service {
            control_url: http_url(&base, control)?,
            service_type: service_type.into(),
        });
    }
    Err("Device does not advertise a UPnP ContentDirectory service".into())
}

fn valid_service_type(value: &str) -> bool {
    value
        .strip_prefix(CONTENT_DIRECTORY_PREFIX)
        .and_then(|version| version.parse::<u32>().ok())
        .is_some_and(|version| version > 0)
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn browse_request(service_type: &str, object_id: &str, start: usize, metadata: bool) -> String {
    let flag = if metadata {
        "BrowseMetadata"
    } else {
        "BrowseDirectChildren"
    };
    let count = if metadata { 1 } else { PAGE_SIZE };
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>\
         <u:Browse xmlns:u=\"{}\"><ObjectID>{}</ObjectID>\
         <BrowseFlag>{flag}</BrowseFlag><Filter>*</Filter>\
         <StartingIndex>{start}</StartingIndex><RequestedCount>{count}</RequestedCount>\
         <SortCriteria></SortCriteria></u:Browse></s:Body></s:Envelope>",
        escape_xml(service_type),
        escape_xml(object_id)
    )
}

fn parse_browse(xml: &str, base: &Url) -> Result<BrowsePage, String> {
    let document =
        Document::parse_with_options(xml, XML_OPTIONS).map_err(|_| "Invalid UPnP SOAP XML")?;
    if let Some(fault) = document
        .descendants()
        .find(|node| node.has_tag_name("Fault"))
    {
        let code = fault
            .descendants()
            .find(|node| node.has_tag_name("errorCode"))
            .and_then(|node| node.text())
            .and_then(|code| code.trim().parse::<u32>().ok());
        // Fault descriptions sometimes echo the requested signed resource.
        // A numeric UPnP code remains useful without retaining those secrets.
        return Err(match code {
            Some(code) => format!("ContentDirectory Browse failed (UPnP {code})"),
            None => "ContentDirectory Browse failed".into(),
        });
    }
    let response = document
        .descendants()
        .find(|node| node.has_tag_name("BrowseResponse"))
        .ok_or("Missing ContentDirectory BrowseResponse")?;
    let count = |name| -> Result<usize, String> {
        child_text(response, name)
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| format!("Invalid ContentDirectory {name}"))
    };
    let number_returned = count("NumberReturned")?;
    let total_matches = count("TotalMatches")?;
    let result = response
        .children()
        .find(|node| node.has_tag_name("Result"))
        .ok_or("Missing ContentDirectory Result")?;
    let (containers, tracks, actual_count) = if let Some(didl) = result
        .children()
        .find(|node| node.has_tag_name("DIDL-Lite"))
    {
        // A few servers return embedded XML instead of the SOAP string type.
        parse_didl(didl, base)?
    } else {
        let didl = result
            .children()
            .filter_map(|node| node.text().filter(|_| node.is_text()))
            .collect::<String>();
        if didl.trim().is_empty() && number_returned == 0 {
            (Vec::new(), Vec::new(), 0)
        } else {
            let didl_document = Document::parse_with_options(&didl, XML_OPTIONS)
                .map_err(|_| "Invalid DIDL-Lite XML")?;
            parse_didl(didl_document.root_element(), base)?
        }
    };
    if actual_count != number_returned {
        return Err("ContentDirectory NumberReturned does not match the DIDL-Lite objects".into());
    }
    Ok(BrowsePage {
        containers,
        tracks,
        number_returned,
        total_matches,
        update_id: child_text(response, "UpdateID").map(str::to_owned),
    })
}

fn parse_didl(
    root: Node<'_, '_>,
    base: &Url,
) -> Result<(Vec<SourceContainer>, Vec<DlnaTrack>, usize), String> {
    if !root.has_tag_name("DIDL-Lite") {
        return Err("Expected DIDL-Lite document".into());
    }
    let mut containers = Vec::new();
    let mut tracks = Vec::new();
    let mut count = 0;
    for node in root.children().filter(|node| node.is_element()) {
        match node.tag_name().name() {
            "container" => {
                count += 1;
                if let Some(id) = node.attribute("id").filter(|id| !id.is_empty()) {
                    containers.push(SourceContainer {
                        id: id.to_owned(),
                        title: child_text(node, "title").unwrap_or(id).to_owned(),
                    });
                }
            }
            "item" => {
                count += 1;
                if let Some(track) = parse_track(node, base) {
                    tracks.push(track);
                }
            }
            _ => {}
        }
    }
    Ok((containers, tracks, count))
}

fn parse_track(node: Node<'_, '_>, base: &Url) -> Option<DlnaTrack> {
    if child_text(node, "class").is_some_and(|class| !class.starts_with("object.item.audioItem")) {
        return None;
    }
    let id = node.attribute("id").filter(|id| !id.is_empty())?.to_owned();
    let (resource, url) = node
        .children()
        .filter(|node| node.has_tag_name("res"))
        .find_map(|resource| {
            let mut protocol = resource.attribute("protocolInfo")?.splitn(4, ':');
            if protocol.next()? != "http-get" {
                return None;
            }
            protocol.next()?;
            let mime = protocol
                .next()?
                .split(';')
                .next()?
                .trim()
                .to_ascii_lowercase();
            protocol.next()?;
            let reference = resource.text()?.trim();
            if reference.is_empty() {
                return None;
            }
            let url = http_url(base, reference).ok()?;
            if !supported_resource(&mime, &url) {
                return None;
            }
            Some((resource, url))
        })?;
    Some(DlnaTrack {
        title: child_text(node, "title").unwrap_or(&id).to_owned(),
        id,
        parent_id: node
            .attribute("parentID")
            .filter(|id| !id.is_empty())
            .map(str::to_owned),
        artist: artist_text(node, false)
            .or_else(|| child_text(node, "creator"))
            .map(str::to_owned),
        album: child_text(node, "album").map(str::to_owned),
        album_artist: child_text(node, "albumArtist")
            .or_else(|| artist_text(node, true))
            .map(str::to_owned),
        track_number: child_text(node, "originalTrackNumber").and_then(positive_number),
        // Some servers expose this extension; ContentDirectory does not define
        // a standard disc number. Keep it absent rather than guessing from a title.
        disc_number: child_text(node, "originalDiscNumber").and_then(positive_number),
        duration_ms: resource.attribute("duration").and_then(parse_duration_ms),
        url: url.into(),
        artwork: node
            .children()
            .filter(|node| node.has_tag_name("albumArtURI"))
            .filter_map(|node| node.text().map(str::trim).filter(|text| !text.is_empty()))
            .find_map(|reference| http_url(base, reference).ok())
            .map(Into::into),
    })
}

fn artist_text<'a>(node: Node<'a, '_>, album_artist: bool) -> Option<&'a str> {
    node.children()
        .filter(|child| child.has_tag_name("artist"))
        .filter(|child| {
            let is_album_artist = child.attribute("role").is_some_and(|role| {
                role.eq_ignore_ascii_case("AlbumArtist")
                    || role.eq_ignore_ascii_case("Album Artist")
            });
            is_album_artist == album_artist
        })
        .filter_map(|child| child.text().map(str::trim).filter(|text| !text.is_empty()))
        .next()
}

fn positive_number(value: &str) -> Option<u32> {
    value.parse::<u32>().ok().filter(|number| *number > 0)
}

fn supported_resource(mime: &str, url: &Url) -> bool {
    match mime {
        "audio/mpeg" | "audio/mp3" | "audio/flac" | "audio/x-flac" | "audio/wav"
        | "audio/x-wav" | "audio/wave" | "audio/vnd.wave" | "audio/aac" | "audio/aacp"
        | "audio/mp4" | "audio/x-m4a" | "audio/ogg" | "application/ogg" | "audio/aiff"
        | "audio/x-aiff" | "audio/x-caf" => true,
        "application/octet-stream" | "*" => cadence_library::is_audio_path(Path::new(url.path())),
        _ => false,
    }
}

fn parse_duration_ms(text: &str) -> Option<u64> {
    let mut parts = text.split(':');
    let hours: u64 = parts.next()?.parse().ok()?;
    let minutes: u64 = parts.next()?.parse().ok()?;
    let seconds: f64 = parts.next()?.parse().ok()?;
    if parts.next().is_some()
        || minutes >= 60
        || !seconds.is_finite()
        || !(0.0..60.0).contains(&seconds)
    {
        return None;
    }
    hours
        .checked_mul(3_600_000)?
        .checked_add(minutes * 60_000)?
        .checked_add((seconds * 1000.0).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    const DIDL_HEAD: &str = "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:u=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">";

    fn response(items: &str, count: usize, total: usize, update: u32) -> String {
        let didl = format!("{DIDL_HEAD}{items}</DIDL-Lite>");
        format!(
            "<soap:Envelope xmlns:soap=\"http://schemas.xmlsoap.org/soap/envelope/\"><soap:Body><cd:BrowseResponse xmlns:cd=\"{DEFAULT_SERVICE_TYPE}\"><Result>{}</Result><NumberReturned>{count}</NumberReturned><TotalMatches>{total}</TotalMatches><UpdateID>{update}</UpdateID></cd:BrowseResponse></soap:Body></soap:Envelope>",
            escape_xml(&didl)
        )
    }

    #[test]
    fn description_resolves_urlbase_and_relative_control_urls() {
        let url = Url::parse("http://server:8200/device/description.xml").unwrap();
        let xml = "<root xmlns=\"urn:schemas-upnp-org:device-1-0\"><URLBase>http://media:9000/device/</URLBase><device><serviceList><service><serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType><controlURL>/wrong</controlURL></service><service><serviceType>urn:schemas-upnp-org:service:ContentDirectory:2</serviceType><controlURL>../control/content</controlURL></service></serviceList></device></root>";
        let service = parse_service(xml, &url).unwrap();
        assert_eq!(
            service.control_url.as_str(),
            "http://media:9000/control/content"
        );
        assert!(service.service_type.ends_with(":2"));
        let without_base = xml.replace("<URLBase>http://media:9000/device/</URLBase>", "");
        assert_eq!(
            parse_service(&without_base, &url)
                .unwrap()
                .control_url
                .as_str(),
            "http://server:8200/control/content"
        );
    }

    #[test]
    fn didl_selects_supported_resource_and_decodes_entities_once() {
        let xml = response(
            r#"<container id="folder"><dc:title>Albums &amp; songs</dc:title></container>
            <item id="song" parentID="album-container"><dc:title>Fish &amp; Chips</dc:title><u:class>object.item.audioItem.musicTrack</u:class><u:artist>Artist</u:artist><u:album>Album</u:album><u:albumArtist>Ensemble</u:albumArtist><u:originalTrackNumber>7</u:originalTrackNumber><u:originalDiscNumber>2</u:originalDiscNumber>
            <res protocolInfo="http-get:*:audio/x-ms-wma:*">/unsupported.wma</res>
            <res protocolInfo="rtsp-rtp-udp:*:audio/mpeg:*">rtsp://server/song.mp3</res>
            <res protocolInfo="http-get:*:audio/flac:*" duration="01:02:03.125">/song.flac?a=1&amp;b=literal%26amp%3B</res>
            <u:albumArtURI>ftp://server/art.jpg</u:albumArtURI><u:albumArtURI>/art.jpg</u:albumArtURI></item>
            <item id="video"><u:class>object.item.videoItem</u:class><res protocolInfo="http-get:*:audio/mpeg:*">/video.mp3</res></item>"#,
            3,
            3,
            7,
        );
        let page = parse_browse(&xml, &Url::parse("http://server/control").unwrap()).unwrap();
        assert_eq!(
            page.containers,
            vec![SourceContainer {
                id: "folder".into(),
                title: "Albums & songs".into()
            }]
        );
        assert_eq!(page.number_returned, 3);
        assert_eq!(page.tracks.len(), 1);
        let track = &page.tracks[0];
        assert_eq!(track.title, "Fish & Chips");
        assert_eq!(track.artist.as_deref(), Some("Artist"));
        assert_eq!(track.album.as_deref(), Some("Album"));
        assert_eq!(track.parent_id.as_deref(), Some("album-container"));
        assert_eq!(track.album_artist.as_deref(), Some("Ensemble"));
        assert_eq!(track.track_number, Some(7));
        assert_eq!(track.disc_number, Some(2));
        assert_eq!(track.duration_ms, Some(3_723_125));
        assert_eq!(track.url, "http://server/song.flac?a=1&b=literal%26amp%3B");
        assert_eq!(track.artwork.as_deref(), Some("http://server/art.jpg"));
    }

    #[test]
    fn album_identity_keeps_compilations_together_and_separates_generic_container_albums() {
        let xml = response(
            r#"<item id="song" parentID="all-music"><dc:title>Song</dc:title><u:artist role="AlbumArtist">Ensemble</u:artist><u:artist>Singer A</u:artist><u:album>Album A</u:album><res protocolInfo="http-get:*:audio/flac:*">/song.flac</res></item>"#,
            1,
            1,
            1,
        );
        let mut track = parse_browse(&xml, &Url::parse("http://server/control").unwrap())
            .unwrap()
            .tracks
            .remove(0);
        assert_eq!(track.artist.as_deref(), Some("Singer A"));
        assert_eq!(track.album_artist.as_deref(), Some("Ensemble"));
        assert_eq!(track.track_number, None);
        assert_eq!(track.disc_number, None);
        let first = album_identity(&track, Some("some-view"));
        assert!(first.is_some());
        track.artist = Some("Singer B".into());
        assert_eq!(album_identity(&track, Some("other-view")), first);
        track.album = Some("Album B".into());
        assert_ne!(album_identity(&track, Some("some-view")), first);
        track.album = Some("Album A".into());
        track.album_artist = Some("Another ensemble".into());
        assert_ne!(album_identity(&track, Some("some-view")), first);
        track.parent_id = Some("0".into());
        let root_album = album_identity(&track, Some("0"));
        assert!(root_album.is_some());
        track.album = Some("Album B".into());
        assert_ne!(album_identity(&track, Some("0")), root_album);
        track.parent_id = None;
        track.album = None;
        track.album_artist = None;
        assert_eq!(album_identity(&track, Some("0")), None);
        assert_eq!(album_identity(&track, None), None);
        assert!(album_identity(&track, Some("album-container")).is_some());
    }

    #[test]
    fn invalid_disc_and_track_numbers_remain_absent() {
        for number in ["0", "-1", "1/10", "4294967296", "NaN", ""] {
            assert_eq!(positive_number(number), None);
        }
        assert_eq!(positive_number("7"), Some(7));
    }

    #[test]
    fn browse_request_escapes_opaque_ids_and_preserves_service_version() {
        let body = browse_request(
            "urn:schemas-upnp-org:service:ContentDirectory:2",
            "A<&\"'",
            34,
            false,
        );
        let document = Document::parse(&body).unwrap();
        let action = document
            .descendants()
            .find(|node| node.has_tag_name("Browse"))
            .unwrap();
        assert_eq!(
            action.tag_name().namespace(),
            Some("urn:schemas-upnp-org:service:ContentDirectory:2")
        );
        assert_eq!(child_text(action, "ObjectID"), Some("A<&\"'"));
        assert_eq!(child_text(action, "StartingIndex"), Some("34"));
        assert_eq!(child_text(action, "RequestedCount"), Some("200"));
    }

    #[test]
    fn rejects_faults_inconsistent_counts_and_unsafe_resource_schemes() {
        let base = Url::parse("http://server/control").unwrap();
        let fault = "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><s:Fault><faultstring>UPnPError</faultstring><detail><UPnPError><errorCode>701</errorCode><errorDescription>No such object</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>";
        assert!(parse_browse(fault, &base).unwrap_err().contains("701"));
        let sensitive_fault = fault.replace("No such object", "https://host/media?sig=secret");
        assert!(
            !parse_browse(&sensitive_fault, &base)
                .unwrap_err()
                .contains("secret")
        );
        assert!(parse_browse(&response("", 1, 1, 1), &base).is_err());
        assert!(http_url(&base, "file:///etc/passwd").is_err());
        assert!(http_url(&base, "http://user:password@server/song").is_err());
        assert!(!valid_service_type(
            "urn:schemas-upnp-org:service:ContentDirectory:1\r\nBad: value"
        ));
        assert!(parse_duration_ms("00:99:01").is_none());
        assert!(parse_duration_ms("00:01:NaN").is_none());
        assert!(parse_duration_ms("18446744073709551615:01:01").is_none());
    }

    // Real loopback HTTP exercises discovery, SOAP action headers and page offsets.
    fn server(responses: Vec<String>) -> (String, JoinHandle<Vec<String>>) {
        server_with_hook(responses, |_| {})
    }

    fn server_with_hook(
        responses: Vec<String>,
        before_response: impl FnMut(usize) + Send + 'static,
    ) -> (String, JoinHandle<Vec<String>>) {
        server_responses(
            responses
                .into_iter()
                .map(|body| ("text/xml", body))
                .collect(),
            before_response,
        )
    }

    fn server_responses(
        responses: Vec<(&'static str, String)>,
        mut before_response: impl FnMut(usize) + Send + 'static,
    ) -> (String, JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let worker = thread::spawn(move || {
            let mut requests = Vec::new();
            for (index, (content_type, response)) in responses.into_iter().enumerate() {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            stream.set_nonblocking(false).unwrap();
                            break stream;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "DLNA test server timed out");
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("{error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut buffer = [0u8; 4096];
                    let read = stream.read(&mut buffer).unwrap();
                    assert!(read > 0);
                    bytes.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&bytes);
                    if let Some(end) = text.find("\r\n\r\n") {
                        let content_length = text[..end]
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + content_length {
                            break;
                        }
                    }
                }
                requests.push(String::from_utf8(bytes).unwrap());
                before_response(index);
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
            }
            requests
        });
        (url, worker)
    }

    #[test]
    fn browses_unknown_totals_after_short_and_non_audio_pages() {
        let description = format!(
            "<root><device><serviceList><service><serviceType>{DEFAULT_SERVICE_TYPE}</serviceType><controlURL>/control</controlURL></service></serviceList></device></root>"
        );
        let (url, worker) = server(vec![
            description,
            response(
                "<container id=\"album\"><dc:title>Album</dc:title></container><item id=\"video\"><u:class>object.item.videoItem</u:class></item>",
                2,
                0,
                3,
            ),
            response(
                "<item id=\"song\"><dc:title>Song</dc:title><res protocolInfo=\"http-get:*:audio/mpeg:*\">/song.mp3</res></item>",
                1,
                0,
                3,
            ),
            response("", 0, 0, 3),
        ]);
        let config = SourceConfig {
            id: "nas".into(),
            name: "NAS".into(),
            plugin_id: "dlna".into(),
            settings: serde_json::json!({ "url": format!("{url}/description.xml") }),
        };
        let result = DlnaPlugin.browse(&config, None).unwrap();
        assert_eq!(result.containers.len(), 1);
        assert_eq!(result.tracks.len(), 1);
        assert_eq!(result.tracks[0].source_id, "nas");
        assert_eq!(result.tracks[0].item_id, "song");
        assert_eq!(result.tracks[0].location, MediaLocation::Provider);
        assert_eq!(result.tracks[0].album_id, None);
        assert_eq!(result.tracks[0].album_artist, None);
        assert_eq!(result.tracks[0].track_number, None);
        assert_eq!(result.tracks[0].disc_number, None);
        let requests = worker.join().unwrap();
        assert!(requests[0].starts_with("GET /description.xml "));
        for (request, index) in requests[1..].iter().zip([0, 2, 3]) {
            assert!(request.starts_with("POST /control "));
            assert!(request.to_ascii_lowercase().contains(
                &format!("soapaction: \"{DEFAULT_SERVICE_TYPE}#Browse\"").to_ascii_lowercase()
            ));
            assert!(request.contains(&format!("<StartingIndex>{index}</StartingIndex>")));
        }
    }

    #[test]
    fn provider_identities_resolve_fresh_metadata_before_streams_artwork_and_downloads() {
        let metadata = |media: &str, artwork: &str| {
            response(
                &format!(
                    "<item id=\"song\" parentID=\"album-1\"><dc:title>Song</dc:title><u:album>Album</u:album><u:albumArtist>Ensemble</u:albumArtist><u:originalTrackNumber>7</u:originalTrackNumber><u:originalDiscNumber>2</u:originalDiscNumber><res protocolInfo=\"http-get:*:audio/flac:*\">{}</res><u:albumArtURI>{}</u:albumArtURI></item>",
                    escape_xml(media),
                    escape_xml(artwork)
                ),
                1,
                1,
                1,
            )
        };
        let (url, worker) = server_responses(
            vec![
                (
                    "text/xml",
                    metadata(
                        "/stale.flac?token=browse-secret",
                        "/stale.png?token=old-art-secret",
                    ),
                ),
                (
                    "text/xml",
                    metadata("/play.flac?token=play-secret", "/unused.png"),
                ),
                ("audio/flac", "first stream".into()),
                (
                    "text/xml",
                    metadata("/replay.flac?token=replay-secret", "/unused.png"),
                ),
                ("audio/flac", "second stream".into()),
                (
                    "text/xml",
                    metadata("/unused.flac", "/art.png?token=art-secret"),
                ),
                ("image/png", "artwork".into()),
                (
                    "text/xml",
                    metadata("/download.flac?token=download-secret", "/unused.png"),
                ),
                ("audio/flac", "explicit download".into()),
            ],
            |_| {},
        );
        let config = SourceConfig {
            id: "nas".into(),
            name: "NAS".into(),
            plugin_id: "dlna".into(),
            settings: serde_json::json!({"control_url": format!("{url}/control")}),
        };
        let page = DlnaPlugin.browse(&config, None).unwrap();
        let track = &page.tracks[0];
        assert_eq!(track.location, MediaLocation::Provider);
        assert!(track.album_id.is_some());
        assert_eq!(track.album_artist.as_deref(), Some("Ensemble"));
        assert_eq!(track.track_number, Some(7));
        assert_eq!(track.disc_number, Some(2));
        assert_eq!(
            track.artwork,
            Some(ArtworkRef::Provider {
                source_id: config.id.clone(),
                item_id: "song".into(),
            })
        );
        let persisted = serde_json::to_string(&page).unwrap();
        assert!(!persisted.contains("secret"));
        assert!(!persisted.contains("stale"));
        assert!(!persisted.contains("http"));
        for expected in ["first stream", "second stream"] {
            let mut stream = DlnaPlugin
                .open_stream(&config, track, Arc::new(AtomicBool::new(false)))
                .unwrap();
            let mut bytes = Vec::new();
            stream.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, expected.as_bytes());
        }
        let artwork = DlnaPlugin
            .fetch_artwork(
                &config,
                track.artwork.as_ref().unwrap(),
                Path::new("unused"),
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(artwork.bytes, b"artwork");
        let cache =
            std::env::temp_dir().join(format!("cadence-dlna-resolve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        let downloaded = DlnaPlugin
            .materialize(&config, track, &cache, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(std::fs::read(downloaded).unwrap(), b"explicit download");
        std::fs::remove_dir_all(cache).unwrap();
        let requests = worker.join().unwrap();
        assert!(requests[0].contains("<BrowseFlag>BrowseDirectChildren</BrowseFlag>"));
        for index in [1, 3, 5, 7] {
            assert!(requests[index].starts_with("POST /control "));
            assert!(requests[index].contains("<ObjectID>song</ObjectID>"));
            assert!(requests[index].contains("<BrowseFlag>BrowseMetadata</BrowseFlag>"));
            assert!(requests[index].contains("<RequestedCount>1</RequestedCount>"));
        }
        for (index, path) in [
            (2, "/play.flac?token=play-secret"),
            (4, "/replay.flac?token=replay-secret"),
            (6, "/art.png?token=art-secret"),
            (8, "/download.flac?token=download-secret"),
        ] {
            assert!(requests[index].starts_with(&format!("GET {path} ")));
        }
    }

    #[test]
    fn metadata_resolution_rejects_wrong_ids_multiple_objects_and_non_audio_items() {
        let audio = "<item id=\"other\"><res protocolInfo=\"http-get:*:audio/flac:*\">/song.flac</res></item>";
        let (url, worker) = server(vec![
            response(audio, 1, 1, 1),
            response(&format!("{audio}{audio}"), 2, 2, 1),
            response("<container id=\"song\" />", 1, 1, 1),
            response(
                "<item id=\"song\"><u:class>object.item.videoItem</u:class><res protocolInfo=\"http-get:*:audio/flac:*\">/song.flac</res></item>",
                1,
                1,
                1,
            ),
        ]);
        let config = SourceConfig {
            id: "nas".into(),
            name: "NAS".into(),
            plugin_id: "dlna".into(),
            settings: serde_json::json!({"control_url": format!("{url}/control")}),
        };
        for _ in 0..4 {
            assert!(resolve_track(&config, "song", &AtomicBool::new(false)).is_err());
        }
        assert_eq!(worker.join().unwrap().len(), 4);
        for id in ["", "bad\nobject"] {
            assert!(validate_object_id(id).is_err());
        }
    }

    #[test]
    fn container_update_during_pagination_is_reported() {
        let (url, worker) = server(vec![
            response("<container id=\"a\" />", 1, 2, 1),
            response("<container id=\"b\" />", 1, 2, 2),
        ]);
        let config = SourceConfig {
            id: "nas".into(),
            name: "NAS".into(),
            plugin_id: "dlna".into(),
            settings: serde_json::json!({ "control_url": format!("{url}/control") }),
        };
        assert!(
            DlnaPlugin
                .browse(&config, Some("album"))
                .unwrap_err()
                .contains("changed while browsing")
        );
        assert_eq!(worker.join().unwrap().len(), 2);
    }

    #[test]
    fn cancellation_during_pagination_discards_partial_results() {
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let server_cancel = cancel.clone();
        let (url, worker) = server_with_hook(
            vec![
                response("<container id=\"a\" />", 1, 3, 1),
                response("<container id=\"b\" />", 1, 3, 1),
            ],
            move |index| {
                if index == 1 {
                    server_cancel.store(true, Ordering::Relaxed);
                }
            },
        );
        let config = SourceConfig {
            id: "nas".into(),
            name: "NAS".into(),
            plugin_id: "dlna".into(),
            settings: serde_json::json!({ "control_url": format!("{url}/control") }),
        };
        let error = DlnaPlugin
            .browse_cancellable(&config, None, &cancel)
            .unwrap_err();
        assert!(error.contains("cancelled"), "{error}");
        assert_eq!(worker.join().unwrap().len(), 2);
    }

    #[test]
    fn pre_cancelled_browse_stops_before_discovery() {
        let config = SourceConfig {
            id: "nas".into(),
            name: "NAS".into(),
            plugin_id: "dlna".into(),
            settings: serde_json::json!({ "url": "invalid url" }),
        };
        let error = DlnaPlugin
            .browse_cancellable(&config, None, &AtomicBool::new(true))
            .unwrap_err();
        assert!(error.contains("cancelled"), "{error}");
    }
}

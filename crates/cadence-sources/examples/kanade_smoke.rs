//! Read-only end-to-end check against an explicitly supplied Kanade server.
//! Prints aggregate results only and keeps catalogue, artwork and PCM in memory.
//! No audio is sent to the device and no server playback commands are issued.
//!
//! ./scripts/dev cargo run --locked -p cadence-sources --example kanade_smoke -- https://server
//! Add --authorize-kanade-app-identity only when the owner explicitly approves
//! using their existing KanadeApp certificate. This can show a system prompt.
use cadence_core::{DecoderRegistry, Track};
use cadence_library::Library;
use cadence_sources::{SourceConfig, SourceRegistry};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let url = args.next().ok_or("Pass the Kanade server base URL")?;
    let identity = match args.next().as_deref() {
        None => false,
        Some("--authorize-kanade-app-identity") if args.next().is_none() => true,
        _ => {
            return Err(
                "Usage: kanade_smoke <server-base-url> [--authorize-kanade-app-identity]".into(),
            );
        }
    };
    let config = SourceConfig {
        id: "kanade-smoke".into(),
        name: "Kanade integration check".into(),
        plugin_id: "kanade".into(),
        settings: serde_json::json!({"url": url, "kanade_app_identity": identity}),
    };
    let registry = SourceRegistry::builtin();
    if identity {
        registry.authorize_source_identity(&config)?;
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let tracks = registry.catalog_cancellable(&config, &cancel)?;
    let albums = tracks
        .iter()
        .filter_map(|track| track.album_id.as_deref())
        .collect::<HashSet<_>>()
        .len();
    let artists = tracks
        .iter()
        .filter_map(|track| track.artist.as_deref())
        .collect::<HashSet<_>>()
        .len();
    let count = tracks.len();
    let mut library = Library::new();
    library.upsert_many(tracks)?;
    if library.tracks().len() != count || library.search("").len() != count {
        return Err("Catalogue did not round-trip through the application library".into());
    }
    println!(
        "Catalogue: {count} tracks, {albums} album identities, {artists} artists; library import/search passed"
    );
    if count == 0 {
        println!("Empty catalogue; artwork and playback checks skipped");
        return Ok(());
    }

    if let Some(artwork) = library
        .tracks()
        .iter()
        .find_map(|track| track.artwork.as_ref())
    {
        // Kanade fetch_artwork does not use this path or write image data to disk.
        let bytes = registry.fetch_artwork(&config, artwork, Path::new("unused"), &cancel)?;
        let bitmap = cadence_library::decode_artwork(&bytes, 512)?;
        println!(
            "Artwork: {} bytes; decoded {} x {}",
            bytes.bytes.len(),
            bitmap.width,
            bitmap.height
        );
    } else {
        println!("No artwork references; artwork check skipped");
    }

    // Probe a bounded sample because a library may also contain codecs the
    // application's decoder does not support. Never print server track metadata.
    let decoders = DecoderRegistry::default();
    for (index, entry) in library.tracks().iter().take(8).enumerate() {
        let input = registry.stream_input(&config, entry)?;
        let track = Track {
            path: PathBuf::from(input.id()),
            title: "Integration sample".into(),
            artist: None,
            album: None,
            duration: entry
                .duration_seconds
                .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok()),
            media_input: Some(input),
        };
        let mut decoder = match decoders.open_track(&track, cancel.clone()) {
            Ok(decoder) => decoder,
            Err(_) => {
                println!("Playback sample {} could not be decoded", index + 1);
                continue;
            }
        };
        let frames = decoder.read_frames(4096)?;
        if frames.is_empty() {
            return Err("Playback sample returned no PCM frames".into());
        }
        println!(
            "Playback: {} PCM frames at {} Hz; seekable={}",
            frames.len(),
            decoder.metadata().sample_rate,
            decoder.is_seekable()
        );
        if decoder.is_seekable() {
            let duration = decoder
                .metadata()
                .duration
                .unwrap_or(Duration::from_secs(2));
            let target = duration.div_f64(2.).min(Duration::from_secs(5));
            let position = decoder.seek(target)?;
            if decoder.read_frames(1024)?.is_empty() {
                return Err("Playback returned no PCM after seek".into());
            }
            println!("Seek: decoded PCM at {:.3} seconds", position.as_secs_f64());
        }
        drop(decoder);
        let mut reopened = decoders.open_track(&track, cancel.clone())?;
        if reopened.read_frames(1024)?.is_empty() {
            return Err("Reopened playback returned no PCM".into());
        }
        println!("Reopen: fresh session decoded PCM; no media files or credentials saved");
        return Ok(());
    }
    Err("None of the first eight catalogue tracks could be decoded".into())
}

//! Read library tags and artwork without constructing an audio decoder.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use symphonia::core::common::Limit;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatReader, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{
    MetadataContainer, MetadataOptions, RawValue, StandardTag, StandardVisualKey, Visual,
};

use crate::ArtworkData;

const MAX_TAG_BYTES: usize = 64 * 1024;
const METADATA_OVERHEAD_BYTES: usize = 1024 * 1024;
const MAX_METADATA_PACKETS: usize = 4;

pub(crate) struct Metadata {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub duration_seconds: Option<f64>,
    pub artwork_index: Option<usize>,
}

pub(crate) fn read_metadata(path: &Path, max_artwork_bytes: usize) -> Result<Metadata, String> {
    Ok(scan(path, max_artwork_bytes, None)?.metadata)
}

pub(crate) fn embedded_artwork(
    path: &Path,
    index: usize,
    max_bytes: usize,
) -> Result<ArtworkData, String> {
    scan(path, max_bytes, Some(index))?
        .artwork
        .ok_or_else(|| "Embedded artwork is absent, unsupported, or exceeds the size limit".into())
}

struct Scan {
    metadata: Metadata,
    artwork: Option<ArtworkData>,
    next_visual_index: usize,
    selected_visual_score: u8,
    requested_index: Option<usize>,
    max_artwork_bytes: usize,
}

fn scan(
    path: &Path,
    max_artwork_bytes: usize,
    requested_index: Option<usize>,
) -> Result<Scan, String> {
    let mut file = File::open(path).map_err(|error| error.to_string())?;
    check_leading_id3(&mut file, max_artwork_bytes)?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|value| value.to_str()) {
        hint.with_extension(extension);
    }
    // Reader support for these options varies; also check tags and pictures before
    // retaining them. The catalog stores an index, never a copy of the image bytes.
    let options = MetadataOptions::default()
        .limit_tag_bytes(Limit::Maximum(MAX_TAG_BYTES))
        .limit_visual_bytes(Limit::Maximum(max_artwork_bytes));
    let mut format = symphonia::default::get_probe()
        .probe(&hint, stream, Default::default(), options)
        .map_err(|error| error.to_string())?;
    let track_id = format
        .default_track(TrackType::Audio)
        .ok_or("No audio track found")?
        .id;
    let mut scan = Scan {
        metadata: Metadata {
            title: path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            artist: None,
            album: None,
            album_artist: None,
            track_number: None,
            disc_number: None,
            duration_seconds: None,
            artwork_index: None,
        },
        artwork: None,
        next_visual_index: 0,
        selected_visual_score: 0,
        requested_index,
        max_artwork_bytes,
    };
    let mut seen_current = scan.revisions(format.as_mut(), track_id, false);
    // Some containers publish their metadata only as their first packets are read.
    // Keep this bounded; scanning a library must never decode a complete track.
    for _ in 0..MAX_METADATA_PACKETS {
        let packet = format.next_packet();
        seen_current = scan.revisions(format.as_mut(), track_id, seen_current);
        if !matches!(packet, Ok(Some(_))) {
            break;
        }
    }
    if let Some(track) = format.tracks().iter().find(|track| track.id == track_id) {
        scan.metadata.duration_seconds = track
            .time_base
            .zip(track.duration)
            .and_then(|(base, duration)| base.calc_duration(duration))
            .map(|duration| duration.as_nanos() as f64 / 1_000_000_000.0)
            .or_else(|| {
                let rate = track.codec_params.as_ref()?.audio()?.sample_rate?;
                (rate > 0).then(|| {
                    track
                        .num_frames
                        .map(|frames| frames as f64 / f64::from(rate))
                })?
            })
            .filter(|duration| duration.is_finite() && *duration >= 0.0);
    }
    Ok(scan)
}

/// Symphonia 0.6.1 does not enforce `MetadataOptions` while reading ID3 frames.
/// Reject oversized or truncated leading tags before its reader allocates them.
/// Container-specific tags still depend on their readers' own allocation limits.
fn check_leading_id3(file: &mut File, max_artwork_bytes: usize) -> Result<(), String> {
    let file_len = file.metadata().map_err(|error| error.to_string())?.len();
    let budget = max_artwork_bytes.saturating_add(METADATA_OVERHEAD_BYTES) as u64;
    let mut position = 0_u64;
    while file_len.saturating_sub(position) >= 10 {
        let mut header = [0_u8; 10];
        file.read_exact(&mut header)
            .map_err(|error| error.to_string())?;
        if !header.starts_with(b"ID3") {
            break;
        }
        if header[6..10].iter().any(|byte| byte & 0x80 != 0) {
            return Err("Invalid ID3 metadata size".into());
        }
        let payload = header[6..10]
            .iter()
            .fold(0_u64, |size, byte| (size << 7) | u64::from(*byte));
        let footer = u64::from(header[3] == 4 && header[5] & 0x10 != 0) * 10;
        position = position
            .checked_add(10 + payload + footer)
            .ok_or("ID3 metadata size overflow")?;
        if position > budget {
            return Err("ID3 metadata exceeds the library scan size limit".into());
        }
        if position > file_len {
            return Err("Truncated ID3 metadata".into());
        }
        file.seek(SeekFrom::Start(position))
            .map_err(|error| error.to_string())?;
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    Ok(())
}

impl Scan {
    /// Symphonia keeps the last revision in its log. Skip that already consumed
    /// revision when packets add new revisions, so image indices remain stable.
    fn revisions(&mut self, format: &mut dyn FormatReader, track_id: u32, seen: bool) -> bool {
        let mut metadata = format.metadata();
        if seen {
            if metadata.is_latest() {
                return true;
            }
            metadata.pop();
        }
        loop {
            let Some(revision) = metadata.current() else {
                return false;
            };
            self.container(&revision.media);
            for track in &revision.per_track {
                if track.track_id == u64::from(track_id) {
                    self.container(&track.metadata);
                }
            }
            if metadata.is_latest() {
                return true;
            }
            metadata.pop();
        }
    }

    fn container(&mut self, container: &MetadataContainer) {
        for tag in &container.tags {
            match tag.std.as_ref() {
                Some(StandardTag::TrackTitle(value)) if useful_tag(value) => {
                    self.metadata.title = value.to_string();
                }
                Some(StandardTag::Artist(value)) if value.len() <= MAX_TAG_BYTES => {
                    self.metadata
                        .artist
                        .get_or_insert_with(|| value.to_string());
                }
                Some(StandardTag::Album(value)) if value.len() <= MAX_TAG_BYTES => {
                    self.metadata.album.get_or_insert_with(|| value.to_string());
                }
                Some(StandardTag::AlbumArtist(value)) if value.len() <= MAX_TAG_BYTES => {
                    self.metadata
                        .album_artist
                        .get_or_insert_with(|| value.to_string());
                }
                Some(StandardTag::TrackNumber(value)) => {
                    if let Ok(value) = u32::try_from(*value) {
                        self.metadata.track_number.get_or_insert(value);
                    }
                }
                Some(StandardTag::DiscNumber(value)) => {
                    if let Ok(value) = u32::try_from(*value) {
                        self.metadata.disc_number.get_or_insert(value);
                    }
                }
                None => {
                    // Symphonia leaves ID3 text lists without a standard tag.
                    // Kanade/Lofty uses the first string rather than joining or
                    // splitting artist names; keep that behavior for these tags.
                    let RawValue::StringList(values) = &tag.raw.value else {
                        continue;
                    };
                    let Some(value) = values.first().filter(|value| value.len() <= MAX_TAG_BYTES)
                    else {
                        continue;
                    };
                    match tag.raw.key.as_str() {
                        "TPE1" | "TP1" => {
                            self.metadata.artist.get_or_insert_with(|| value.clone());
                        }
                        "TPE2" | "TP2" => {
                            self.metadata
                                .album_artist
                                .get_or_insert_with(|| value.clone());
                        }
                        "TALB" | "TAL" => {
                            self.metadata.album.get_or_insert_with(|| value.clone());
                        }
                        "TRCK" | "TRK" => {
                            if let Some(value) = text_number(value) {
                                self.metadata.track_number.get_or_insert(value);
                            }
                        }
                        "TPOS" | "TPA" => {
                            if let Some(value) = text_number(value) {
                                self.metadata.disc_number.get_or_insert(value);
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        for visual in &container.visuals {
            let index = self.next_visual_index;
            self.next_visual_index += 1;
            if visual.data.is_empty() || visual.data.len() > self.max_artwork_bytes {
                continue;
            }
            let Some(media_type) = visual_media_type(visual) else {
                continue;
            };
            let score = if visual.usage == Some(StandardVisualKey::FrontCover) {
                2
            } else {
                1
            };
            if score > self.selected_visual_score {
                self.metadata.artwork_index = Some(index);
                self.selected_visual_score = score;
            }
            if self.requested_index == Some(index) {
                self.artwork = Some(ArtworkData {
                    bytes: visual.data.to_vec(),
                    media_type: media_type.into(),
                });
            }
        }
    }
}

fn useful_tag(value: &str) -> bool {
    value.len() <= MAX_TAG_BYTES && !value.trim().is_empty()
}

fn text_number(value: &str) -> Option<u32> {
    value.split('/').next()?.parse().ok()
}

fn visual_media_type(visual: &Visual) -> Option<&'static str> {
    // Container MIME tags are often missing or wrong. Inspect the encoded data;
    // the artwork decoder performs the complete format and dimensions validation.
    if visual.data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if visual.data.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

    struct TestFile(PathBuf);

    impl Drop for TestFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn fixture(id3_frames: &[u8]) -> TestFile {
        let path = std::env::temp_dir().join(format!(
            "cadence-library-metadata-{}-{}.wav",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut bytes = id3_tag(id3_frames);
        // One second of 8 kHz, mono, 16-bit PCM. Constructing the fixture needs no
        // codec or additional test dependency.
        bytes.extend(b"RIFF");
        bytes.extend(16_036_u32.to_le_bytes());
        bytes.extend(b"WAVEfmt ");
        bytes.extend(16_u32.to_le_bytes());
        bytes.extend(1_u16.to_le_bytes());
        bytes.extend(1_u16.to_le_bytes());
        bytes.extend(8000_u32.to_le_bytes());
        bytes.extend(16_000_u32.to_le_bytes());
        bytes.extend(2_u16.to_le_bytes());
        bytes.extend(16_u16.to_le_bytes());
        bytes.extend(b"data");
        bytes.extend(16_000_u32.to_le_bytes());
        bytes.resize(bytes.len() + 16_000, 0);
        std::fs::write(&path, bytes).unwrap();
        TestFile(path)
    }

    fn id3_tag(frames: &[u8]) -> Vec<u8> {
        if frames.is_empty() {
            return Vec::new();
        }
        let mut bytes = b"ID3\x03\x00\x00".to_vec();
        let size = frames.len() as u32;
        bytes.extend([
            (size >> 21) as u8 & 0x7f,
            (size >> 14) as u8 & 0x7f,
            (size >> 7) as u8 & 0x7f,
            size as u8 & 0x7f,
        ]);
        bytes.extend(frames);
        bytes
    }

    fn encoded_fixture(extension: &str, bytes: &[u8]) -> TestFile {
        let path = std::env::temp_dir().join(format!(
            "cadence-library-metadata-{}-{}.{}",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed),
            extension
        ));
        std::fs::write(&path, bytes).unwrap();
        TestFile(path)
    }

    // Keep the checked-in mathematical audio unchanged, replacing only its
    // comment block. This exercises real FLAC metadata without a test encoder.
    fn flac_fixture(comments: &[&str]) -> TestFile {
        let original = include_bytes!("../../cadence-core/tests/fixtures/tone.flac");
        let mut comment_bytes = Vec::new();
        comment_bytes.extend(7_u32.to_le_bytes());
        comment_bytes.extend(b"Cadence");
        comment_bytes.extend((comments.len() as u32).to_le_bytes());
        for comment in comments {
            comment_bytes.extend((comment.len() as u32).to_le_bytes());
            comment_bytes.extend(comment.as_bytes());
        }
        let mut bytes = b"fLaC".to_vec();
        let mut offset = 4;
        let mut replaced = false;
        loop {
            let header = &original[offset..offset + 4];
            let length = (usize::from(header[1]) << 16)
                | (usize::from(header[2]) << 8)
                | usize::from(header[3]);
            if header[0] & 0x7f == 4 {
                bytes.push(header[0]);
                bytes.extend([
                    (comment_bytes.len() >> 16) as u8,
                    (comment_bytes.len() >> 8) as u8,
                    comment_bytes.len() as u8,
                ]);
                bytes.extend(&comment_bytes);
                replaced = true;
            } else {
                bytes.extend(&original[offset..offset + 4 + length]);
            }
            offset += 4 + length;
            if header[0] & 0x80 != 0 {
                break;
            }
        }
        assert!(replaced, "FLAC fixture must contain a comment block");
        bytes.extend(&original[offset..]);
        encoded_fixture("flac", &bytes)
    }

    fn frame(frames: &mut Vec<u8>, key: &[u8; 4], data: &[u8]) {
        frames.extend(key);
        frames.extend((data.len() as u32).to_be_bytes());
        frames.extend([0, 0]);
        frames.extend(data);
    }

    fn picture(frames: &mut Vec<u8>, kind: u8, image: &[u8]) {
        let mut body = b"\x00image/png\x00".to_vec();
        body.extend([kind, 0]);
        body.extend(image);
        frame(frames, b"APIC", &body);
    }

    #[test]
    fn reads_tags_duration_and_prefers_front_cover_with_stable_index() {
        let mut frames = Vec::new();
        frame(&mut frames, b"TIT2", b"\x00Library title");
        frame(&mut frames, b"TPE1", b"\x00Library artist");
        frame(&mut frames, b"TALB", b"\x00Library album");
        // A second front-cover picture should win over the earlier back cover.
        picture(&mut frames, 4, b"\xff\xd8\xffback");
        let image = image::RgbaImage::from_pixel(1, 1, image::Rgba([10, 20, 30, 255]));
        let mut front = std::io::Cursor::new(Vec::new());
        image.write_to(&mut front, image::ImageFormat::Png).unwrap();
        let front = front.into_inner();
        picture(&mut frames, 3, &front);
        let file = fixture(&frames);
        let metadata = read_metadata(&file.0, 4096).unwrap();
        assert_eq!(metadata.title, "Library title");
        assert_eq!(metadata.artist.as_deref(), Some("Library artist"));
        assert_eq!(metadata.album.as_deref(), Some("Library album"));
        assert_eq!(metadata.duration_seconds, Some(1.0));
        assert_eq!(metadata.artwork_index, Some(1));
        let artwork = embedded_artwork(&file.0, 1, 4096).unwrap();
        assert_eq!(artwork.bytes, front);
        assert_eq!(artwork.media_type, "image/png");
        assert_eq!(
            crate::decode_artwork(&artwork, 128).unwrap().pixels,
            [0xff0a_141e]
        );
        let artwork = embedded_artwork(&file.0, 0, 4096).unwrap();
        assert_eq!(artwork.media_type, "image/jpeg");
        assert!(embedded_artwork(&file.0, 2, 4096).is_err());
    }

    #[test]
    fn skips_large_or_unsupported_pictures_and_empty_tags() {
        let mut frames = Vec::new();
        frame(&mut frames, b"TIT2", b"\x00  ");
        picture(&mut frames, 3, b"\x89PNG\r\n\x1a\ntoo large");
        picture(&mut frames, 3, b"GIF89a");
        let file = fixture(&frames);
        let metadata = read_metadata(&file.0, 8).unwrap();
        assert_eq!(
            metadata.title,
            file.0.file_stem().unwrap().to_string_lossy()
        );
        assert_eq!(metadata.artwork_index, None);
        assert!(embedded_artwork(&file.0, 0, 8).is_err());
        assert!(embedded_artwork(&file.0, 1, 4096).is_err());
    }

    #[test]
    fn reads_plain_audio_without_artwork_and_rejects_invalid_files() {
        let file = fixture(&[]);
        let metadata = read_metadata(&file.0, 4096).unwrap();
        assert_eq!(
            metadata.title,
            file.0.file_stem().unwrap().to_string_lossy()
        );
        assert_eq!(metadata.duration_seconds, Some(1.0));
        assert_eq!(metadata.artist, None);
        assert_eq!(metadata.album, None);
        assert_eq!(metadata.artwork_index, None);
        std::fs::write(&file.0, b"invalid media").unwrap();
        assert!(read_metadata(&file.0, 4096).is_err());
    }

    #[test]
    fn reads_container_metadata_without_audio_decoding() {
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../cadence-core/tests/fixtures");
        for name in [
            "tone-mono.mp3",
            "tone.flac",
            "tone.ogg",
            "tone-aac.m4a",
            "tone-alac.m4a",
            "tone.aiff",
        ] {
            let metadata = read_metadata(&fixtures.join(name), 4096).unwrap();
            assert_eq!(metadata.title, "Cadence synthetic tone", "{name}");
            assert_eq!(metadata.artist.as_deref(), Some("Cadence tests"), "{name}");
            assert_eq!(
                metadata.album.as_deref(),
                Some("Decoder fixtures"),
                "{name}"
            );
        }
    }

    #[test]
    fn flac_keeps_first_raw_artist_and_album_artist_and_disc_order() {
        let file = flac_fixture(&[
            "TITLE=Movement",
            "ARTIST= Soloist; Guest ",
            "ARTIST=Another soloist",
            "ALBUMARTIST=Ensemble",
            "ALBUMARTIST=Another ensemble",
            "ALBUM= Concert ",
            "TRACKNUMBER=7/12",
            "DISCNUMBER=2/3",
            "COMPILATION=1",
        ]);
        let metadata = read_metadata(&file.0, 4096).unwrap();
        assert_eq!(metadata.artist.as_deref(), Some(" Soloist; Guest "));
        assert_eq!(metadata.album_artist.as_deref(), Some("Ensemble"));
        assert_eq!(metadata.album.as_deref(), Some(" Concert "));
        assert_eq!(metadata.track_number, Some(7));
        assert_eq!(metadata.disc_number, Some(2));
        let track = crate::inspect_track(&file.0, &crate::ScanOptions::default()).unwrap();
        assert_eq!(track.album_artist, metadata.album_artist);
        assert_eq!(track.track_number, Some(7));
        assert_eq!(track.disc_number, Some(2));
        assert_eq!(track.album_id, None);

        let mut legacy_track = track.clone();
        legacy_track.album_artist = None;
        legacy_track.track_number = None;
        legacy_track.disc_number = None;
        let mut library = crate::Library::new();
        library.upsert(legacy_track).unwrap();
        let report = library.scan(
            std::slice::from_ref(&file.0),
            &crate::ScanOptions::default(),
            &std::sync::atomic::AtomicBool::new(false),
        );
        assert_eq!((report.added, report.updated), (0, 1));
        assert_eq!(library.tracks(), &[track]);
    }

    #[test]
    fn mp3_distinguishes_album_artist_from_first_track_artist() {
        let original = include_bytes!("../../cadence-core/tests/fixtures/tone-mono.mp3");
        assert!(original.starts_with(b"ID3"));
        let tag_length = original[6..10]
            .iter()
            .fold(0_usize, |length, byte| (length << 7) | usize::from(*byte));
        let mut frames = Vec::new();
        frame(&mut frames, b"TIT2", b"\x00Movement");
        frame(
            &mut frames,
            b"TPE1",
            b"\x00Soloist; Guest\x00Additional performer",
        );
        frame(&mut frames, b"TPE1", b"\x00Another soloist");
        frame(&mut frames, b"TPE2", b"\x00Ensemble\x00Additional ensemble");
        frame(&mut frames, b"TALB", b"\x00Concert");
        frame(&mut frames, b"TRCK", b"\x0007/12");
        frame(&mut frames, b"TPOS", b"\x002/3");
        let mut bytes = id3_tag(&frames);
        bytes.extend(&original[10 + tag_length..]);
        let file = encoded_fixture("mp3", &bytes);
        let metadata = read_metadata(&file.0, 4096).unwrap();
        assert_eq!(metadata.artist.as_deref(), Some("Soloist; Guest"));
        assert_eq!(metadata.album_artist.as_deref(), Some("Ensemble"));
        assert_eq!(metadata.track_number, Some(7));
        assert_eq!(metadata.disc_number, Some(2));
        assert!(metadata.duration_seconds.is_some());
    }

    #[test]
    fn invalid_or_oversized_track_numbers_do_not_wrap() {
        let file = flac_fixture(&[
            "ARTIST=",
            "ALBUMARTIST= ",
            "TRACKNUMBER=4294967296",
            "DISCNUMBER=invalid",
        ]);
        let metadata = read_metadata(&file.0, 4096).unwrap();
        assert_eq!(metadata.artist.as_deref(), Some(""));
        assert_eq!(metadata.album_artist.as_deref(), Some(" "));
        assert_eq!(metadata.track_number, None);
        assert_eq!(metadata.disc_number, None);
    }

    #[test]
    fn rejects_oversized_and_truncated_id3_before_parsing() {
        let file = fixture(&[]);
        std::fs::write(&file.0, b"ID3\x03\x00\x00\x7f\x7f\x7f\x7f").unwrap();
        let error = read_metadata(&file.0, 4096).err().unwrap();
        assert!(error.contains("size limit"), "{error}");
        std::fs::write(&file.0, b"ID3\x03\x00\x00\x00\x00\x01\x00").unwrap();
        let error = read_metadata(&file.0, 4096).err().unwrap();
        assert!(error.contains("Truncated"), "{error}");
        std::fs::write(&file.0, b"ID3\x03\x00\x00\xff\x00\x00\x00").unwrap();
        let error = read_metadata(&file.0, 4096).err().unwrap();
        assert!(error.contains("Invalid"), "{error}");
    }
}

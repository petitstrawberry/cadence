//! Persistent, platform-neutral music catalog and local metadata scanning.
//!
//! The catalog stores provider identities, never credentials or temporary stream
//! URLs. Source providers resolve [`MediaLocation::Provider`] when playback starts.
//! Scanning, artwork I/O and saving are synchronous; run them on a worker thread.

mod artwork;
mod metadata;

pub use artwork::{
    ArtworkBitmap, ArtworkData, MAX_ARTWORK_BYTES, MAX_ARTWORK_PIXELS, decode_artwork, read_artwork,
};

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, String>;
pub const SCHEMA_VERSION: u32 = 1;
pub const LOCAL_SOURCE_ID: &str = "local";
const MAX_CATALOG_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TRACKS: usize = 100_000;
const MAX_STRING_BYTES: usize = 32_768;
const MAX_SCAN_ERRORS: usize = 1024;
const EXTENSIONS: &[&str] = &[
    "wav", "wave", "aif", "aiff", "aifc", "caf", "flac", "mp3", "ogg", "oga", "m4a", "mp4", "m4b",
    "aac",
];

/// Stable identity without hash collisions, including for arbitrary provider IDs.
pub fn track_id(source_id: &str, item_id: &str) -> String {
    format!("{}:{source_id}{item_id}", source_id.len())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum MediaLocation {
    Local(PathBuf),
    /// A stable public URL; authentication belongs in provider configuration.
    Remote {
        url: String,
    },
    /// Resolve the track's source/item identity afresh before playback.
    Provider,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ArtworkRef {
    File(PathBuf),
    Embedded { path: PathBuf, index: usize },
    Remote { url: String },
    Provider { source_id: String, item_id: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LibraryTrack {
    pub id: String,
    pub source_id: String,
    pub item_id: String,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// Optional provider album identity. Local albums use the file's parent
    /// directory, so their identity does not depend on artist or title tags.
    #[serde(default)]
    pub album_id: Option<String>,
    #[serde(default)]
    pub album_artist: Option<String>,
    #[serde(default)]
    pub track_number: Option<u32>,
    #[serde(default)]
    pub disc_number: Option<u32>,
    pub duration_seconds: Option<f64>,
    pub location: MediaLocation,
    pub artwork: Option<ArtworkRef>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Library {
    schema_version: u32,
    /// Missing in catalogs created before album ordering tags were stored.
    #[serde(default)]
    ordering_metadata_version: u32,
    #[serde(default)]
    roots: Vec<PathBuf>,
    tracks: Vec<LibraryTrack>,
    #[serde(skip)]
    index: HashMap<String, usize>,
}

impl Default for Library {
    fn default() -> Self {
        Self::new()
    }
}

impl Library {
    pub fn new() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            ordering_metadata_version: 1,
            roots: Vec::new(),
            tracks: Vec::new(),
            index: HashMap::new(),
        }
    }

    pub fn tracks(&self) -> &[LibraryTrack] {
        &self.tracks
    }
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    pub fn needs_ordering_metadata_refresh(&self) -> bool {
        self.ordering_metadata_version == 0
            && self
                .tracks
                .iter()
                .any(|track| matches!(track.location, MediaLocation::Local(_)))
    }

    /// Refresh legacy ordering tags on an I/O worker. Identity, catalog
    /// membership, and existing title/artwork data remain unchanged. Call on a
    /// candidate catalog and commit it only when the operation is not cancelled.
    pub fn refresh_ordering_metadata(
        &mut self,
        cancel: &AtomicBool,
        mut progress: impl FnMut(&ScanProgress),
    ) -> ScanReport {
        let mut report = ScanReport::default();
        for track in &mut self.tracks {
            if cancel.load(Ordering::Relaxed) {
                report.cancelled = true;
                break;
            }
            let MediaLocation::Local(path) = &track.location else {
                continue;
            };
            report.entries_seen += 1;
            report.audio_files_processed += 1;
            progress(&report.progress(Some(path.clone()), false));
            match metadata::read_metadata(path, MAX_ARTWORK_BYTES) {
                Ok(metadata) => {
                    track.album_artist = metadata.album_artist;
                    track.track_number = metadata.track_number;
                    track.disc_number = metadata.disc_number;
                    report.updated += 1;
                }
                Err(error) => report.error(path.clone(), error),
            }
        }
        report.cancelled |= cancel.load(Ordering::Relaxed);
        // Failed/offline files are retried on a later startup. Valid entries
        // can still be committed without deleting unavailable music.
        if !report.cancelled && report.error_count == 0 {
            self.ordering_metadata_version = 1;
        }
        progress(&report.progress(None, true));
        report
    }
    pub fn get(&self, id: &str) -> Option<&LibraryTrack> {
        self.index
            .get(id)
            .and_then(|index| self.tracks.get(*index))
            .or_else(|| self.tracks.iter().find(|track| track.id == id))
    }

    /// Case-insensitive substring search across title, artists and album.
    pub fn search(&self, query: &str) -> Vec<&LibraryTrack> {
        let query = query.trim().to_lowercase();
        self.tracks
            .iter()
            .filter(|track| {
                [&track.title]
                    .into_iter()
                    .chain(track.artist.iter())
                    .chain(track.album_artist.iter())
                    .chain(track.album.iter())
                    .any(|value| value.to_lowercase().contains(&query))
            })
            .collect()
    }

    /// A missing catalog is an empty library. Corruption/version errors are
    /// reported, allowing the caller to preserve the original file.
    pub fn load(path: &Path) -> Result<Self> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Self::new()),
            Err(error) => return Err(format!("Unable to read library: {error}")),
        };
        if file.metadata().map_err(|error| error.to_string())?.len() > MAX_CATALOG_BYTES {
            return Err("Library exceeds the catalog size limit".into());
        }
        let mut bytes = Vec::new();
        file.take(MAX_CATALOG_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        if bytes.len() as u64 > MAX_CATALOG_BYTES {
            return Err("Library exceeds the catalog size limit".into());
        }
        let mut library: Self =
            serde_json::from_slice(&bytes).map_err(|error| format!("Invalid library: {error}"))?;
        library.validate()?;
        library.rebuild_index();
        Ok(library)
    }

    /// Save via a unique same-directory temporary file and atomic rename. Failed
    /// validation/serialization/writes leave an existing catalog untouched.
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let mut buffer = LimitedBuffer {
            bytes: Vec::new(),
            limit: MAX_CATALOG_BYTES as usize,
        };
        serde_json::to_writer_pretty(&mut buffer, self).map_err(|error| error.to_string())?;
        let bytes = buffer.bytes;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .map_err(|error| format!("Unable to create library directory: {error}"))?;
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let (temporary, mut file) = (0..32)
            .find_map(|_| {
                let nonce = SEQUENCE.fetch_add(1, Ordering::Relaxed);
                let temporary = parent.join(format!(
                    ".cadence-library-{}-{nonce}.tmp",
                    std::process::id()
                ));
                match OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temporary)
                {
                    Ok(file) => Some(Ok((temporary, file))),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                    Err(error) => Some(Err(error.to_string())),
                }
            })
            .unwrap_or_else(|| Err("Unable to allocate a library temporary file".into()))?;
        let write_result = (|| -> Result<()> {
            file.write_all(&bytes).map_err(|error| error.to_string())?;
            file.sync_all().map_err(|error| error.to_string())?;
            drop(file);
            fs::rename(&temporary, path)
                .map_err(|error| format!("Unable to replace library: {error}"))?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        write_result
    }

    /// Insert or replace by the stable source/item ID. Returns true for insertion.
    pub fn upsert(&mut self, track: LibraryTrack) -> Result<bool> {
        validate_track(&track)?;
        if self.index.len() != self.tracks.len() {
            self.rebuild_index();
        }
        if let Some(index) = self.index.get(&track.id) {
            self.tracks[*index] = track;
            Ok(false)
        } else if self.tracks.len() >= MAX_TRACKS {
            Err("Library track limit reached".into())
        } else {
            self.index.insert(track.id.clone(), self.tracks.len());
            self.tracks.push(track);
            Ok(true)
        }
    }

    /// Validate the entire batch before changing the catalog. Returns new count.
    pub fn upsert_many(&mut self, tracks: impl IntoIterator<Item = LibraryTrack>) -> Result<usize> {
        let mut candidate = self.clone();
        let mut added = 0;
        for track in tracks {
            added += usize::from(candidate.upsert(track)?);
        }
        *self = candidate;
        Ok(added)
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.tracks.len();
        self.tracks.retain(|track| track.id != id);
        self.rebuild_index();
        self.tracks.len() != before
    }

    pub fn remove_source(&mut self, source_id: &str) -> usize {
        let before = self.tracks.len();
        self.tracks.retain(|track| track.source_id != source_id);
        self.rebuild_index();
        before - self.tracks.len()
    }

    pub fn set_roots(&mut self, roots: Vec<PathBuf>) -> Result<()> {
        for root in &roots {
            validate_path(root)?;
        }
        self.roots = roots;
        self.roots.sort();
        self.roots.dedup();
        Ok(())
    }

    /// Stop remembering a folder for rescans. Catalog entries and audio files
    /// are retained, including when the folder is no longer mounted.
    pub fn forget_root(&mut self, root: &Path) -> Result<bool> {
        validate_path(root)?;
        let before = self.roots.len();
        self.roots.retain(|registered| registered != root);
        Ok(self.roots.len() != before)
    }

    /// Remove a folder source and its catalog entries without touching files.
    /// Tracks covered by another registered root remain in that source.
    /// Works for disconnected roots and already-unregistered sources.
    pub fn remove_root(&mut self, root: &Path) -> Result<usize> {
        self.forget_root(root)?;
        let canonical = root.canonicalize().unwrap_or_else(|_| root.to_owned());
        self.roots
            .retain(|other| other.canonicalize().unwrap_or_else(|_| other.clone()) != canonical);
        let roots = self
            .roots
            .iter()
            .map(|other| {
                (
                    other.clone(),
                    other.canonicalize().unwrap_or_else(|_| other.clone()),
                )
            })
            .collect::<Vec<_>>();
        let before = self.tracks.len();
        self.tracks.retain(|track| match &track.location {
            MediaLocation::Local(path) => {
                (!path.starts_with(root) && !path.starts_with(&canonical))
                    || roots.iter().any(|(original, canonical)| {
                        path.starts_with(original) || path.starts_with(canonical)
                    })
            }
            _ => true,
        });
        self.rebuild_index();
        Ok(before - self.tracks.len())
    }

    /// Add/update tracks recursively. Existing tracks, including missing files,
    /// remain until explicitly removed. Symlinks are never followed. Successful
    /// directory roots are remembered for a later rescan.
    pub fn scan(
        &mut self,
        roots: &[PathBuf],
        options: &ScanOptions,
        cancel: &AtomicBool,
    ) -> ScanReport {
        self.scan_with_progress(roots, options, cancel, |_| {})
    }

    /// Recursively scan while reporting actual work performed. The total is not
    /// known in advance, so progress deliberately has no percentage. Callbacks
    /// run on this thread; UI clients should publish a throttled latest value.
    pub fn scan_with_progress(
        &mut self,
        roots: &[PathBuf],
        options: &ScanOptions,
        cancel: &AtomicBool,
        mut progress: impl FnMut(&ScanProgress),
    ) -> ScanReport {
        let mut report = ScanReport::default();
        let mut pending: Vec<_> = roots.iter().rev().map(|root| (root.clone(), 0)).collect();
        let mut visited = HashSet::new();
        let mut current_path = None;
        progress(&report.progress(None, false));
        while let Some((path, depth)) = pending.pop() {
            if cancel.load(Ordering::Relaxed) {
                report.cancelled = true;
                break;
            }
            if report.entries_seen >= options.max_entries {
                report.limit_reached = true;
                break;
            }
            report.entries_seen += 1;
            current_path = Some(path.clone());
            progress(&report.progress(current_path.clone(), false));
            if cancel.load(Ordering::Relaxed) {
                report.cancelled = true;
                break;
            }
            let file_type = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata.file_type(),
                Err(error) => {
                    report.error(path, error.to_string());
                    continue;
                }
            };
            if file_type.is_symlink() {
                report.skipped += 1;
                continue;
            }
            let canonical = match fs::canonicalize(&path) {
                Ok(path) => path,
                Err(error) => {
                    report.error(path, error.to_string());
                    continue;
                }
            };
            if let Err(error) = validate_path(&canonical) {
                report.error(path, error);
                continue;
            }
            if !visited.insert(canonical.clone()) {
                report.skipped += 1;
                continue;
            }
            if file_type.is_dir() {
                if depth == 0 && !self.roots.contains(&canonical) {
                    self.roots.push(canonical.clone());
                }
                if depth >= options.max_depth {
                    report.limit_reached = true;
                    continue;
                }
                let children = match fs::read_dir(&canonical) {
                    Ok(children) => children,
                    Err(error) => {
                        report.error(canonical, error.to_string());
                        continue;
                    }
                };
                let mut paths = Vec::new();
                let room = options
                    .max_entries
                    .saturating_sub(report.entries_seen + pending.len());
                for child in children {
                    if cancel.load(Ordering::Relaxed) {
                        report.cancelled = true;
                        break;
                    }
                    if paths.len() >= room {
                        report.limit_reached = true;
                        break;
                    }
                    match child {
                        Ok(child) => paths.push(child.path()),
                        Err(error) => report.error(canonical.clone(), error.to_string()),
                    }
                }
                paths.sort();
                pending.extend(paths.into_iter().rev().map(|path| (path, depth + 1)));
                continue;
            }
            if !file_type.is_file() || !is_audio_path(&canonical) {
                report.skipped += 1;
                continue;
            }
            if report.audio_files_processed >= options.max_files {
                report.limit_reached = true;
                break;
            }
            report.audio_files_processed += 1;
            let track = match inspect_track(&canonical, options) {
                Ok(track) => track,
                Err(error) => {
                    report.error(canonical, error);
                    continue;
                }
            };
            match self.upsert(track) {
                Ok(true) => report.added += 1,
                Ok(false) => report.updated += 1,
                Err(error) => report.error(canonical, error),
            }
        }
        self.roots.sort();
        self.roots.dedup();
        progress(&report.progress(current_path, true));
        report
    }

    fn validate(&self) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(format!(
                "Unsupported library schema {} (expected {SCHEMA_VERSION})",
                self.schema_version
            ));
        }
        if self.tracks.len() > MAX_TRACKS {
            return Err("Library track limit reached".into());
        }
        for root in &self.roots {
            validate_path(root)?;
        }
        let mut ids = HashSet::new();
        for track in &self.tracks {
            validate_track(track)?;
            if !ids.insert(&track.id) {
                return Err("Library contains duplicate track IDs".into());
            }
        }
        Ok(())
    }

    fn rebuild_index(&mut self) {
        self.index = self
            .tracks
            .iter()
            .enumerate()
            .map(|(index, track)| (track.id.clone(), index))
            .collect();
    }
}

/// Stop serialization at the file limit instead of first allocating an entire
/// oversized JSON document in memory.
struct LimitedBuffer {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for LimitedBuffer {
    fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
        if input.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other(
                "Library exceeds the catalog size limit",
            ));
        }
        self.bytes.extend_from_slice(input);
        Ok(input.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ScanOptions {
    pub max_files: usize,
    pub max_entries: usize,
    pub max_depth: usize,
    pub max_artwork_bytes: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            max_files: 50_000,
            max_entries: 200_000,
            max_depth: 32,
            max_artwork_bytes: MAX_ARTWORK_BYTES,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanReport {
    pub entries_seen: usize,
    pub audio_files_processed: usize,
    pub added: usize,
    pub updated: usize,
    pub skipped: usize,
    pub errors: Vec<ScanError>,
    /// All errors encountered, even if the bounded detail list is full.
    pub error_count: usize,
    pub cancelled: bool,
    pub limit_reached: bool,
}

impl ScanReport {
    fn error(&mut self, path: PathBuf, message: String) {
        self.error_count += 1;
        if self.errors.len() < MAX_SCAN_ERRORS {
            self.errors.push(ScanError { path, message });
        }
    }

    fn progress(&self, current_path: Option<PathBuf>, finished: bool) -> ScanProgress {
        ScanProgress {
            entries_seen: self.entries_seen,
            audio_files_processed: self.audio_files_processed,
            current_path,
            added: self.added,
            updated: self.updated,
            skipped: self.skipped,
            error_count: self.error_count,
            finished,
            cancelled: self.cancelled,
            limit_reached: self.limit_reached,
        }
    }
}

/// Lightweight scan state, independent of the catalog and the error detail list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanProgress {
    pub entries_seen: usize,
    pub audio_files_processed: usize,
    pub current_path: Option<PathBuf>,
    pub added: usize,
    pub updated: usize,
    pub skipped: usize,
    pub error_count: usize,
    pub finished: bool,
    pub cancelled: bool,
    pub limit_reached: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanError {
    pub path: PathBuf,
    pub message: String,
}

pub fn is_audio_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            EXTENSIONS
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known))
        })
}

/// Inspect one local audio file without adding it to a catalog. Paths are
/// canonicalized, so this identity matches the one returned by a folder scan.
pub fn inspect_track(path: &Path, options: &ScanOptions) -> Result<LibraryTrack> {
    let file_type = fs::symlink_metadata(path)
        .map_err(|error| error.to_string())?
        .file_type();
    if !file_type.is_file() {
        return Err("Music path must be a regular file, not a symlink".into());
    }
    let canonical = fs::canonicalize(path).map_err(|error| error.to_string())?;
    validate_path(&canonical)?;
    let data =
        metadata::read_metadata(&canonical, options.max_artwork_bytes.min(MAX_ARTWORK_BYTES))?;
    let item_id = canonical.to_str().expect("path was validated").to_owned();
    let artwork = data
        .artwork_index
        .map(|index| ArtworkRef::Embedded {
            path: canonical.clone(),
            index,
        })
        .or_else(|| sidecar_artwork(&canonical, options.max_artwork_bytes.min(MAX_ARTWORK_BYTES)));
    let track = LibraryTrack {
        id: track_id(LOCAL_SOURCE_ID, &item_id),
        source_id: LOCAL_SOURCE_ID.into(),
        item_id,
        title: data.title,
        artist: data.artist,
        album: data.album,
        album_id: None,
        album_artist: data.album_artist,
        track_number: data.track_number,
        disc_number: data.disc_number,
        duration_seconds: data.duration_seconds,
        location: MediaLocation::Local(canonical),
        artwork,
    };
    validate_track(&track)?;
    Ok(track)
}

fn validate_path(path: &Path) -> Result<()> {
    match path.to_str() {
        Some(value) if !value.is_empty() && value.len() <= MAX_STRING_BYTES => Ok(()),
        Some(_) => Err("Library path is empty or too long".into()),
        None => Err("Library paths must be valid UTF-8 for portable, lossless persistence".into()),
    }
}

fn validate_track(track: &LibraryTrack) -> Result<()> {
    if track.source_id.is_empty()
        || track.item_id.is_empty()
        || track.id != track_id(&track.source_id, &track.item_id)
    {
        return Err("Track ID must match its nonempty source and item IDs".into());
    }
    for value in [&track.source_id, &track.item_id, &track.title]
        .into_iter()
        .chain(track.artist.iter())
        .chain(track.album.iter())
        .chain(track.album_id.iter())
        .chain(track.album_artist.iter())
    {
        if value.len() > MAX_STRING_BYTES {
            return Err("Track metadata exceeds the string size limit".into());
        }
    }
    if track
        .duration_seconds
        .is_some_and(|duration| std::time::Duration::try_from_secs_f64(duration).is_err())
    {
        return Err("Track duration must be finite, nonnegative, and representable".into());
    }
    match &track.location {
        MediaLocation::Local(path) => validate_path(path)?,
        MediaLocation::Remote { url } if url.len() > MAX_STRING_BYTES => {
            return Err("Media URL is too long".into());
        }
        _ => (),
    }
    match &track.artwork {
        Some(ArtworkRef::File(path) | ArtworkRef::Embedded { path, .. }) => validate_path(path)?,
        Some(ArtworkRef::Remote { url }) if url.len() > MAX_STRING_BYTES => {
            return Err("Artwork URL is too long".into());
        }
        Some(ArtworkRef::Provider { source_id, item_id })
            if source_id.len() > MAX_STRING_BYTES || item_id.len() > MAX_STRING_BYTES =>
        {
            return Err("Artwork provider identity is too long".into());
        }
        _ => (),
    }
    Ok(())
}

fn sidecar_artwork(track: &Path, max_bytes: usize) -> Option<ArtworkRef> {
    let directory = track.parent()?;
    for name in [
        "cover.jpg",
        "cover.jpeg",
        "cover.png",
        "folder.jpg",
        "folder.jpeg",
        "folder.png",
        "Cover.jpg",
        "Cover.png",
        "Folder.jpg",
        "Folder.png",
    ] {
        let path = directory.join(name);
        if fs::symlink_metadata(&path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() <= max_bytes as u64)
        {
            return Some(ArtworkRef::File(path));
        }
    }
    None
}

#[cfg(test)]
mod tests;

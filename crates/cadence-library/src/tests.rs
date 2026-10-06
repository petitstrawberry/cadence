use super::*;

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "cadence-library-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
    fn wav(&self, name: &str) -> PathBuf {
        let path = self.path(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 8000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for _ in 0..800 {
            writer.write_sample(0_i16).unwrap();
        }
        writer.finalize().unwrap();
        path
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn remote(item: &str) -> LibraryTrack {
    LibraryTrack {
        id: track_id("test", item),
        source_id: "test".into(),
        item_id: item.into(),
        title: "Test Song".into(),
        artist: Some("Artist".into()),
        album: Some("Album".into()),
        album_id: None,
        album_artist: None,
        track_number: None,
        disc_number: None,
        duration_seconds: Some(12.5),
        location: MediaLocation::Provider,
        artwork: Some(ArtworkRef::Provider {
            source_id: "test".into(),
            item_id: item.into(),
        }),
    }
}

#[test]
fn recursive_scan_deduplicates_and_keeps_valid_files_after_errors() {
    let temporary = TempDir::new();
    let wav = temporary.wav("album/song.wav");
    temporary.wav("another.WAV");
    fs::write(temporary.path("broken.mp3"), b"broken").unwrap();
    fs::write(temporary.path("notes.txt"), b"not audio").unwrap();
    let mut library = Library::new();
    let cancel = AtomicBool::new(false);
    let report = library.scan(
        &[temporary.0.clone(), wav.clone()],
        &ScanOptions::default(),
        &cancel,
    );
    assert_eq!(report.added, 2);
    assert_eq!(report.errors.len(), 1);
    assert_eq!(library.tracks.len(), 2);
    assert_eq!(library.roots(), &[fs::canonicalize(&temporary.0).unwrap()]);
    let again = library.scan(
        std::slice::from_ref(&temporary.0),
        &ScanOptions::default(),
        &cancel,
    );
    assert_eq!((again.added, again.updated), (0, 2));
    assert_eq!(library.tracks.len(), 2);
    let inspected = inspect_track(&wav, &ScanOptions::default()).unwrap();
    assert_eq!(library.get(&inspected.id), Some(&inspected));
    assert_eq!(inspected.duration_seconds, Some(0.1));
}

#[test]
fn progress_reports_actual_work_and_always_publishes_completion() {
    let temporary = TempDir::new();
    temporary.wav("album/song.wav");
    fs::write(temporary.path("broken.mp3"), b"broken").unwrap();
    fs::write(temporary.path("notes.txt"), b"not audio").unwrap();
    let mut progress = Vec::new();
    let report = Library::new().scan_with_progress(
        std::slice::from_ref(&temporary.0),
        &ScanOptions::default(),
        &AtomicBool::new(false),
        |value| progress.push(value.clone()),
    );
    assert_eq!(progress.first(), Some(&ScanProgress::default()));
    assert!(progress.windows(2).all(|pair| {
        pair[0].entries_seen <= pair[1].entries_seen
            && pair[0].audio_files_processed <= pair[1].audio_files_processed
    }));
    let final_progress = progress.last().unwrap();
    assert!(final_progress.finished);
    assert!(!final_progress.cancelled);
    assert!(final_progress.current_path.as_ref().unwrap().exists());
    assert_eq!(final_progress.entries_seen, 5);
    assert_eq!(final_progress.audio_files_processed, 2);
    assert_eq!((final_progress.added, final_progress.skipped), (1, 1));
    assert_eq!(final_progress.error_count, 1);
    assert_eq!(final_progress.entries_seen, report.entries_seen);
    assert_eq!(
        final_progress.audio_files_processed,
        report.audio_files_processed
    );
    assert_eq!(final_progress.error_count, report.error_count);
    assert_eq!(
        report.errors[0].path,
        fs::canonicalize(temporary.path("broken.mp3")).unwrap()
    );
}

#[test]
fn progress_remains_final_after_cancellation_and_bounds_error_details() {
    let temporary = TempDir::new();
    temporary.wav("one.wav");
    temporary.wav("two.wav");
    let cancel = AtomicBool::new(false);
    let mut last = ScanProgress::default();
    let report = Library::new().scan_with_progress(
        std::slice::from_ref(&temporary.0),
        &ScanOptions::default(),
        &cancel,
        |value| {
            last = value.clone();
            if value.added == 1 {
                cancel.store(true, Ordering::Relaxed);
            }
        },
    );
    assert!(report.cancelled);
    assert!(last.finished && last.cancelled);
    assert_eq!(last.audio_files_processed, 1);
    assert_eq!(last.added, 1);

    let missing: Vec<_> = (0..MAX_SCAN_ERRORS + 9)
        .map(|index| temporary.path(&format!("missing-{index}")))
        .collect();
    let report = Library::new().scan_with_progress(
        &missing,
        &ScanOptions::default(),
        &AtomicBool::new(false),
        |value| last = value.clone(),
    );
    assert_eq!(report.error_count, MAX_SCAN_ERRORS + 9);
    assert_eq!(report.errors.len(), MAX_SCAN_ERRORS);
    assert!(last.finished);
    assert_eq!(last.error_count, report.error_count);
    assert_eq!(last.entries_seen, missing.len());
}

#[test]
fn forgetting_a_root_retains_catalog_and_files_and_roundtrips() {
    let temporary = TempDir::new();
    let wav = temporary.wav("album/song.wav");
    let mut library = Library::new();
    library.scan(
        std::slice::from_ref(&temporary.0),
        &ScanOptions::default(),
        &AtomicBool::new(false),
    );
    let tracks = library.tracks().to_vec();
    let root = library.roots()[0].clone();
    assert!(library.forget_root(Path::new("")).is_err());
    assert_eq!(library.roots(), std::slice::from_ref(&root));
    assert!(library.forget_root(&root).unwrap());
    assert!(!library.forget_root(&root).unwrap());
    assert_eq!(library.tracks(), tracks);
    assert!(wav.exists());
    let path = temporary.path("library.json");
    library.save(&path).unwrap();
    let restored = Library::load(&path).unwrap();
    assert!(restored.roots().is_empty());
    assert_eq!(restored.tracks(), tracks);

    let unavailable = temporary.path("disconnected-volume");
    library.set_roots(vec![unavailable.clone()]).unwrap();
    assert!(library.forget_root(&unavailable).unwrap());
}

#[test]
fn sidecar_artwork_is_discovered_and_decoded() {
    let temporary = TempDir::new();
    let wav = temporary.wav("song.wav");
    let cover = temporary.path("cover.png");
    image::RgbaImage::from_pixel(2, 1, image::Rgba([255, 0, 0, 255]))
        .save(&cover)
        .unwrap();
    let track = inspect_track(&wav, &ScanOptions::default()).unwrap();
    let reference = track.artwork.unwrap();
    assert_eq!(
        reference,
        ArtworkRef::File(fs::canonicalize(&cover).unwrap())
    );
    let bitmap =
        decode_artwork(&read_artwork(&reference, MAX_ARTWORK_BYTES).unwrap(), 256).unwrap();
    assert_eq!(bitmap.pixels, vec![0xffff0000; 2]);
    assert!(read_artwork(&reference, 1).is_err());
}

#[test]
fn catalog_roundtrip_upsert_remove_search_and_roots() {
    let temporary = TempDir::new();
    let path = temporary.path("nested/library.json");
    let mut library = Library::load(&path).unwrap();
    assert!(library.upsert(remote("one")).unwrap());
    assert!(!library.upsert(remote("one")).unwrap());
    assert_eq!(
        library
            .upsert_many([remote("two"), remote("three")])
            .unwrap(),
        2
    );
    library
        .set_roots(vec![temporary.0.clone(), temporary.0.clone()])
        .unwrap();
    library.save(&path).unwrap();
    assert_eq!(Library::load(&path).unwrap(), library);
    assert_eq!(library.search("ARTIST").len(), 3);
    assert!(library.search("absent").is_empty());
    assert!(library.remove(&track_id("test", "two")));
    assert!(!library.remove("missing"));
    assert_eq!(library.remove_source("test"), 2);
    assert_eq!(library.roots().len(), 1);
}

#[test]
fn album_metadata_roundtrips_and_old_catalogs_load_without_rewriting() {
    let temporary = TempDir::new();
    let path = temporary.path("library.json");
    let mut track = remote("one");
    track.album_id = Some("opaque-album-id".into());
    track.album_artist = Some("Album ensemble".into());
    track.track_number = Some(7);
    track.disc_number = Some(2);
    let mut library = Library::new();
    library.upsert(track.clone()).unwrap();
    assert_eq!(library.search("album ensemble"), vec![&track]);
    library.save(&path).unwrap();
    assert_eq!(Library::load(&path).unwrap().tracks(), &[track]);

    let mut legacy = serde_json::to_value(&library).unwrap();
    let legacy_track = legacy["tracks"][0].as_object_mut().unwrap();
    for key in ["album_id", "album_artist", "track_number", "disc_number"] {
        legacy_track.remove(key);
    }
    let original = serde_json::to_vec(&legacy).unwrap();
    fs::write(&path, &original).unwrap();
    let loaded = Library::load(&path).unwrap();
    assert_eq!(loaded.schema_version, SCHEMA_VERSION);
    let track = &loaded.tracks()[0];
    assert_eq!(track.album_id, None);
    assert_eq!(track.album_artist, None);
    assert_eq!(track.track_number, None);
    assert_eq!(track.disc_number, None);
    assert_eq!(fs::read(&path).unwrap(), original);
}

#[test]
fn legacy_ordering_refresh_reads_tags_preserves_identity_and_cancels_transactionally() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../cadence-core/tests/fixtures/tone-order.flac");
    let mut track = inspect_track(&fixture, &ScanOptions::default()).unwrap();
    track.track_number = None;
    track.disc_number = None;
    track.album_artist = None;
    track.title = "Preserved catalog title".into();
    let mut legacy = Library::new();
    legacy.upsert(track.clone()).unwrap();
    legacy.ordering_metadata_version = 0;
    assert!(legacy.needs_ordering_metadata_refresh());
    let mut cancelled = legacy.clone();
    assert!(
        cancelled
            .refresh_ordering_metadata(&AtomicBool::new(true), |_| {})
            .cancelled
    );
    assert_eq!(cancelled, legacy);
    let report = legacy.refresh_ordering_metadata(&AtomicBool::new(false), |_| {});
    assert_eq!(report.error_count, 0);
    assert!(!legacy.needs_ordering_metadata_refresh());
    assert_eq!(legacy.tracks()[0].track_number, Some(7));
    assert_eq!(legacy.tracks()[0].disc_number, Some(2));
    assert_eq!(
        legacy.tracks()[0].album_artist.as_deref(),
        Some("Cadence album ensemble")
    );
    assert_eq!(legacy.tracks()[0].id, track.id);
    assert_eq!(legacy.tracks()[0].title, track.title);
    assert_eq!(legacy.tracks()[0].artwork, track.artwork);
}

#[test]
fn album_identity_and_artist_follow_existing_metadata_size_limits() {
    for artist in [false, true] {
        let mut track = remote("one");
        let field = if artist {
            &mut track.album_artist
        } else {
            &mut track.album_id
        };
        *field = Some("x".repeat(MAX_STRING_BYTES + 1));
        let mut library = Library::new();
        library.upsert(remote("original")).unwrap();
        assert!(library.upsert_many([remote("valid"), track]).is_err());
        assert_eq!(library.tracks().len(), 1);
    }
}

#[test]
fn invalid_batch_is_atomic_and_does_not_replace_catalog() {
    let temporary = TempDir::new();
    let path = temporary.path("library.json");
    let mut library = Library::new();
    library.upsert(remote("original")).unwrap();
    library.save(&path).unwrap();
    let original = fs::read(&path).unwrap();
    let mut bad = remote("bad");
    bad.duration_seconds = Some(f64::INFINITY);
    assert!(library.upsert_many([remote("valid"), bad.clone()]).is_err());
    assert_eq!(library.tracks.len(), 1);
    library.tracks.push(bad);
    assert!(library.save(&path).is_err());
    assert_eq!(fs::read(&path).unwrap(), original);
    let mut too_long = remote("too-long");
    too_long.duration_seconds = Some(1e30);
    assert!(Library::new().upsert(too_long).is_err());
}

#[test]
fn malformed_future_schema_and_duplicate_ids_are_rejected() {
    let temporary = TempDir::new();
    let path = temporary.path("library.json");
    for contents in [
        "{bad",
        r#"{"schema_version":999,"tracks":[]}"#,
        r#"{"schema_version":0,"tracks":[]}"#,
    ] {
        fs::write(&path, contents).unwrap();
        assert!(Library::load(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), contents);
    }
    let duplicate = Library {
        schema_version: SCHEMA_VERSION,
        ordering_metadata_version: 1,
        roots: vec![],
        tracks: vec![remote("same"), remote("same")],
        index: HashMap::new(),
    };
    fs::write(&path, serde_json::to_vec(&duplicate).unwrap()).unwrap();
    assert!(Library::load(&path).is_err());
}

#[test]
fn rename_failure_preserves_existing_directory_and_cleans_temporary_file() {
    let temporary = TempDir::new();
    let path = temporary.path("catalog-directory");
    fs::create_dir(&path).unwrap();
    fs::write(path.join("keep"), b"preserved").unwrap();
    assert!(Library::new().save(&path).is_err());
    assert_eq!(fs::read(path.join("keep")).unwrap(), b"preserved");
    assert_eq!(fs::read_dir(&temporary.0).unwrap().count(), 1);
}

#[test]
fn cancellation_and_scan_bounds_preserve_existing_catalog() {
    let temporary = TempDir::new();
    temporary.wav("one.wav");
    temporary.wav("two.wav");
    let mut library = Library::new();
    library.upsert(remote("original")).unwrap();
    let cancelled = library.scan(
        std::slice::from_ref(&temporary.0),
        &ScanOptions::default(),
        &AtomicBool::new(true),
    );
    assert!(cancelled.cancelled);
    assert_eq!(library.tracks.len(), 1);
    let options = ScanOptions {
        max_files: 1,
        ..ScanOptions::default()
    };
    let bounded = library.scan(
        std::slice::from_ref(&temporary.0),
        &options,
        &AtomicBool::new(false),
    );
    assert!(bounded.limit_reached);
    assert_eq!(bounded.added, 1);
    assert_eq!(library.tracks.len(), 2);
}

#[test]
fn identities_do_not_collide_on_separator_boundaries() {
    assert_ne!(track_id("a", "bc"), track_id("ab", "c"));
    assert_ne!(track_id("a:b", "c"), track_id("a", "b:c"));
}

#[test]
fn serialization_stops_at_the_byte_budget() {
    let mut buffer = LimitedBuffer {
        bytes: Vec::new(),
        limit: 64,
    };
    assert!(
        serde_json::to_writer_pretty(&mut buffer, &remote("large".repeat(100).as_str())).is_err()
    );
    assert!(buffer.bytes.len() <= 64);
}

#[cfg(unix)]
#[test]
fn symlink_loops_and_non_utf8_paths_are_skipped() {
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::symlink;
    let temporary = TempDir::new();
    let wav = temporary.wav("song.wav");
    symlink(&temporary.0, temporary.path("loop")).unwrap();
    symlink(&wav, temporary.path("linked.wav")).unwrap();
    let invalid = temporary
        .0
        .join(std::ffi::OsString::from_vec(b"invalid-\xff.wav".to_vec()));
    assert!(validate_path(&invalid).unwrap_err().contains("UTF-8"));
    // APFS rejects invalid UTF-8 names itself; Unix filesystems such as ext4
    // allow them and exercise the scan's error-reporting path as well.
    let invalid_created = fs::copy(&wav, &invalid).is_ok();
    let mut library = Library::new();
    let report = library.scan(
        std::slice::from_ref(&temporary.0),
        &ScanOptions::default(),
        &AtomicBool::new(false),
    );
    assert_eq!(report.added, 1);
    assert_eq!(report.errors.len(), usize::from(invalid_created));
    if invalid_created {
        assert!(report.errors[0].message.contains("UTF-8"));
    }
    assert!(!report.limit_reached);
}

#[test]
fn removing_folder_source_purges_only_its_unshared_local_tracks_and_keeps_files() {
    let temporary = TempDir::new();
    let root = temporary.path("music");
    let inside = temporary.wav("music/album/song.wav");
    let shared = temporary.wav("music/shared/song.wav");
    let sibling = temporary.wav("music-other/song.wav");
    let mut library = Library::new();
    for path in [&inside, &shared, &sibling] {
        library
            .upsert(inspect_track(path, &ScanOptions::default()).unwrap())
            .unwrap();
    }
    library.upsert(remote("provider-song")).unwrap();
    library
        .set_roots(vec![
            root.clone(),
            root.join("shared"),
            temporary.path("music-other"),
        ])
        .unwrap();
    assert!(library.remove_root(Path::new("")).is_err());
    assert_eq!(library.tracks().len(), 4);
    assert_eq!(library.remove_root(&root).unwrap(), 1);
    assert_eq!(library.tracks().len(), 3);
    assert!(
        library
            .tracks()
            .iter()
            .any(|t| t.location == MediaLocation::Local(shared.canonicalize().unwrap()))
    );
    assert!(
        library
            .tracks()
            .iter()
            .any(|t| t.location == MediaLocation::Local(sibling.canonicalize().unwrap()))
    );
    assert!(library.tracks().iter().any(|t| t.source_id == "test"));
    assert!(inside.exists() && shared.exists() && sibling.exists());
    assert_eq!(library.remove_root(&root).unwrap(), 0);
}

#[test]
fn removing_disconnected_or_previously_unregistered_folder_needs_no_filesystem_access() {
    let mut library = Library::new();
    let root = PathBuf::from("/unmounted/cadence-removal-test");
    let mut track = remote("offline");
    track.source_id = LOCAL_SOURCE_ID.into();
    track.item_id = root.join("album/song.flac").to_string_lossy().into_owned();
    track.id = track_id(&track.source_id, &track.item_id);
    track.location = MediaLocation::Local(PathBuf::from(&track.item_id));
    library.upsert(track).unwrap();
    assert_eq!(library.remove_root(&root).unwrap(), 1);
    assert!(library.tracks().is_empty());
}

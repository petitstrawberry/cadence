use super::*;
use cadence_core::Snapshot;
use cadence_library::{LibraryTrack, MediaLocation, track_id};
use cadence_sources::SourceConfig;
use scarlet_ui::{
    ElementTree, EventDispatcher, LayoutConstraints,
    event::KeyModifiers,
    renderer::{PaintCommand, PaintContext},
    views::BitmapImage,
};
use std::sync::Arc;
use std::{
    io::Write,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

const MODES: [&str; 15] = [
    "empty",
    "queue",
    "artwork",
    "browser",
    "library-empty",
    "library",
    "albums",
    "artists",
    "folders-empty",
    "folders",
    "folders-scanning",
    "folders-readonly",
    "sources-empty",
    "sources",
    "effects",
];

fn native_effect_fixture() -> Option<PathBuf> {
    std::env::var_os("CADENCE_TEST_CLAP")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

fn modes() -> impl Iterator<Item = &'static str> {
    MODES
        .into_iter()
        .chain(native_effect_fixture().map(|_| "effects-loaded"))
}

fn app() -> Cadence {
    // No audio device is opened by UI tests, including playback error paths.
    let (controller, factory) = cadence_effects::EffectRack::new(Default::default());
    let player = Player::with_processor_factories(
        Box::new(|| Err("Test output unavailable".into())),
        cadence_core::DecoderRegistry::default(),
        vec![factory],
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while controller.snapshot().pending {
        assert!(
            Instant::now() < deadline,
            "Empty effect rack did not initialize"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let mut app = Cadence::new(player);
    app.effects = Some(EffectPane::new(controller));
    app
}

fn tracks() -> Vec<Track> {
    ["First light", "Quiet streets", "The long way home"]
        .into_iter()
        .map(|title| Track {
            path: PathBuf::from(format!("/cadence-test/{title}.flac")),
            title: title.into(),
            artist: Some("Cadence Ensemble".into()),
            album: Some("Small moments".into()),
            duration: Some(Duration::from_secs(213)),
            media_input: None,
        })
        .collect()
}

fn populate(app: &Cadence) {
    app.playback.queue.tracks.set(tracks());
    app.playback.queue.current.set(Some(0));
    let mut snapshot = app.playback.snapshot.borrow_mut();
    snapshot.queue = tracks();
    snapshot.current = Some(0);
    snapshot.duration = Some(Duration::from_secs(213));
    snapshot.seekable = true;
    app.playback.transport.time.set("0:00 / 3:33".into());
    app.playback.status.set("Ready".into());
}

fn library_tracks() -> Vec<LibraryTrack> {
    tracks()
        .into_iter()
        .map(|track| {
            let item_id = track.path.to_string_lossy().into_owned();
            LibraryTrack {
                id: track_id("local", &item_id),
                source_id: "local".into(),
                item_id,
                title: track.title,
                artist: track.artist,
                album: track.album,
                album_id: None,
                album_artist: None,
                track_number: None,
                disc_number: None,
                duration_seconds: track.duration.map(|duration| duration.as_secs_f64()),
                location: MediaLocation::Local(track.path),
                artwork: None,
            }
        })
        .collect()
}

fn library_entries(app: &Cadence) {
    app.workspace.tab.set(Tab::Library);
    app.workspace.library.set(library_tracks());
    app.workspace.filtered.set(library_tracks());
    app.workspace.library_selection.set(Some(1));
    app.workspace
        .library_status
        .set("3 library tracks · ready".into());
}

fn folder_entries(app: &Cadence) {
    library_entries(app);
    app.workspace.tab.set(Tab::Sources);
    app.workspace.source_page.set(extension::SourcePage::Browse);
    app.workspace.roots.set(vec![
        PathBuf::from("/cadence-test"),
        PathBuf::from("/Archive"),
    ]);
    app.workspace.open_provider(
        app,
        extension::ProviderId::Local(PathBuf::from("/cadence-test")),
    );
    app.workspace
        .folders
        .summary
        .set("Scan finished · 3 added · 0 updated · 0 skipped · 1 errors".into());
    app.workspace.folders.progress_text.set(
        "4 entries checked · 4 audio files · 3 added · 0 updated · 0 skipped · 1 errors".into(),
    );
    app.workspace
        .folders
        .current_path
        .set("/cadence-test/unreadable.flac".into());
    app.workspace
        .folders
        .errors
        .set(vec![cadence_library::ScanError {
            path: PathBuf::from("/cadence-test/unreadable.flac"),
            message: "Permission denied".into(),
        }]);
}

fn source_entries(app: &Cadence) {
    app.workspace.tab.set(Tab::Sources);
    app.workspace.sources.set(vec![
        SourceConfig {
            id: "kanade-home".into(),
            name: "Kanade home".into(),
            plugin_id: "kanade".into(),
            settings: serde_json::json!({"url":"https://kanade.invalid"}),
        },
        SourceConfig {
            id: "dlna-living".into(),
            name: "Living room DLNA".into(),
            plugin_id: "dlna".into(),
            settings: serde_json::json!({"url":"http://dlna.invalid/device.xml"}),
        },
        SourceConfig {
            id: "archive-plugin".into(),
            name: "Archive plugin".into(),
            plugin_id: "process".into(),
            settings: serde_json::json!({"executable":"/cadence-test/archive-source","args":[]}),
        },
    ]);
    let mut entries = vec![extension::SourceItem::Folder {
        id: "albums".into(),
        title: "Albums".into(),
    }];
    entries.extend(library_tracks().into_iter().map(|mut track| {
        track.source_id = "kanade-home".into();
        track.id = track_id(&track.source_id, &track.item_id);
        track.location = MediaLocation::Provider;
        extension::SourceItem::Track(Arc::new(track))
    }));
    app.workspace.source_entries.set(entries);
    app.workspace
        .source_selection
        .set(Some(extension::ProviderId::Remote("kanade-home".into())));
    app.workspace.source_page.set(extension::SourcePage::Browse);
    app.workspace.item_selection.set(Some(2));
    app.workspace
        .source_status
        .set("4 items · open folders or select a track".into());
}

fn prepare_mode(app: &Cadence, mode: &str) {
    match mode {
        "empty" => {}
        "queue" => {
            populate(app);
            app.workspace.tab.set(Tab::Playing);
        }
        "artwork" => {
            populate(app);
            // An actual decoded bitmap exercises ScarletUI's image paint path.
            let pixels = (0..96)
                .flat_map(|y| {
                    (0..96).map(move |x| {
                        let inside = (x - 48_i32).pow(2) + (y - 48_i32).pow(2) < 35 * 35;
                        if inside { 0xffdfad71 } else { 0xff243c59 }
                    })
                })
                .collect();
            *app.workspace.artwork.borrow_mut() = Some(BitmapImage::from_bgra(pixels, 96, 96));
        }
        "browser" => browser_entries(app),
        "library-empty" => app.workspace.tab.set(Tab::Library),
        "library" => library_entries(app),
        "albums" | "artists" => {
            library_entries(app);
            app.collections.poll(&app.workspace);
            app.workspace.tab.set(if mode == "albums" {
                Tab::Albums
            } else {
                Tab::Artists
            });
        }
        "folders-empty" => app.workspace.tab.set(Tab::Sources),
        "folders" => folder_entries(app),
        "folders-scanning" => {
            folder_entries(app);
            app.workspace.folders.busy.set(true);
            app.workspace
                .folders
                .summary
                .set("Scanning folders…".into());
        }
        "folders-readonly" => {
            folder_entries(app);
            app.workspace.read_only.set(true);
        }
        "sources-empty" => app.workspace.tab.set(Tab::Sources),
        "sources" => source_entries(app),
        "effects" => app.workspace.tab.set(Tab::Effects),
        "effects-loaded" => {
            app.workspace.tab.set(Tab::Effects);
            let path =
                native_effect_fixture().expect("Native fixture mode needs CADENCE_TEST_CLAP");
            let effects = app.effects.as_ref().unwrap();
            effects.path.set(path.to_string_lossy().into_owned());
            effects.controller.discover(path.clone()).unwrap();
            let discovered = wait_for_effects(app, |snapshot| !snapshot.discovered.is_empty());
            let plugin = &discovered.discovered[0];
            assert_eq!(plugin.id, "org.resonara.gain");
            effects
                .controller
                .add(path, Some(plugin.id.clone()))
                .unwrap();
            wait_for_effects(app, |snapshot| snapshot.effects.len() == 1);
        }
        _ => panic!("Unknown UI fixture {mode}"),
    }
}

fn browser_entries(app: &Cadence) {
    app.browser.visible.set(true);
    app.browser.directory.set("/Music".into());
    app.browser.entries.set(
        ["Albums", "First light.flac", "Quiet streets.wav"]
            .into_iter()
            .enumerate()
            .map(|(index, name)| browser::Entry {
                path: PathBuf::from("/Music").join(name),
                name: name.into(),
                directory: index == 0,
            })
            .collect(),
    );
    app.browser
        .status
        .set("2 audio file(s) · click a file to select it".into());
}

// Fixture sizes refer to content, independent of the platform's decoration.
fn content_window(app: &Cadence, size: Size) -> Window<Cadence> {
    let window = app.window(size);
    let chrome = scarlet_ui::views::WindowContentLayout::for_decoration(window.get_decoration())
        .decoration_size();
    window.size(Size::new(
        size.width + chrome.width,
        size.height + chrome.height,
    ))
}

fn layout(app: &Cadence, size: Size) -> ElementTree {
    app.size.set(size);
    let mut tree = ElementTree::new();
    tree.set_root(app.create_element());
    rebuild(&mut tree, size);
    tree
}

fn rebuild(tree: &mut ElementTree, size: Size) {
    for _ in 0..3 {
        tree.root_mut().unwrap().rebuild();
        tree.layout(LayoutConstraints::tight(size.width, size.height));
    }
}

#[derive(Debug)]
struct Control {
    kind: &'static str,
    label: String,
    origin: Point,
    size: Size,
}

fn controls(element: &dyn Element, parent: Point, found: &mut Vec<Control>, controls_only: bool) {
    let origin = Point::new(
        parent.x + element.position().x,
        parent.y + element.position().y,
    );
    if let Some(render) = element.render_object() {
        let is_button = render
            .as_any()
            .is::<scarlet_ui::views::ButtonRenderObject>();
        let kind = if is_button {
            "Button"
        } else if render
            .as_any()
            .is::<scarlet_ui::views::SliderRenderObject>()
        {
            "Slider"
        } else if render
            .as_any()
            .is::<scarlet_ui::views::TextFieldRenderObject>()
        {
            "TextField"
        } else if render
            .as_any()
            .is::<scarlet_ui::views::ToggleRenderObject>()
        {
            "Toggle"
        } else if render
            .as_any()
            .is::<scarlet_ui::views::SelectRenderObject>()
        {
            "Select"
        } else {
            "Text"
        };
        let is_control = kind != "Text";
        let mut paint = PaintContext::new();
        render.paint(&mut paint, origin);
        if is_control {
            let label = paint
                .commands()
                .iter()
                .find_map(|command| match command {
                    PaintCommand::DrawText { text, .. } if !text.is_empty() => Some(text.clone()),
                    _ => None,
                })
                .or_else(|| {
                    paint.commands().iter().find_map(|command| match command {
                        PaintCommand::DrawIcon { icon, .. } if is_button => {
                            Some(format!("icon:{icon:?}"))
                        }
                        _ => None,
                    })
                })
                .unwrap_or_else(|| kind.to_owned());
            found.push(Control {
                kind,
                label,
                origin,
                size: element.bounds().size,
            });
        }
        // Rows and NavigationView entries are native click targets, not Buttons.
        // Derive their hit points from the text the retained renderer paints.
        if !controls_only && (!is_control || kind == "Select") {
            for command in paint.commands() {
                if let PaintCommand::DrawText {
                    position,
                    text,
                    font_size_px,
                    ..
                } = command
                {
                    let (width, height) =
                        scarlet_ui::graphics::measure_text_sized(text, *font_size_px);
                    found.push(Control {
                        kind: "Text",
                        label: text.clone(),
                        origin: *position,
                        size: Size::new(width as f32, height as f32),
                    });
                }
            }
        }
    }
    for child in element.children() {
        controls(child.as_ref(), origin, found, controls_only);
    }
}

fn all_controls(tree: &ElementTree) -> Vec<Control> {
    let mut found = Vec::new();
    controls(tree.root().unwrap(), Point::ZERO, &mut found, true);
    found
}

fn all_targets(tree: &ElementTree) -> Vec<Control> {
    let mut found = Vec::new();
    controls(tree.root().unwrap(), Point::ZERO, &mut found, false);
    found
}

fn target_point(tree: &ElementTree, label: &str) -> Point {
    let targets = all_targets(tree);
    let control = targets
        .iter()
        .find(|control| control.label == label)
        .unwrap_or_else(|| panic!("Missing control {label:?}: {targets:?}"));
    Point::new(
        control.origin.x + control.size.width / 2.,
        control.origin.y + control.size.height / 2.,
    )
}

fn click_at(tree: &mut ElementTree, dispatcher: &mut EventDispatcher, point: Point) {
    let x = point.x as i32;
    let y = point.y as i32;
    assert!(
        dispatcher
            .hit_test_with_path(tree, Point::new(x as f32, y as f32))
            .is_some(),
        "Control cannot be hit at ({x}, {y})"
    );
    dispatcher.dispatch(tree, &Event::Mouse(MouseEvent::Moved { x, y }));
    for event in [
        MouseEvent::ButtonPressed {
            button: MouseButton::Left,
            x,
            y,
            click_count: 1,
        },
        MouseEvent::ButtonReleased {
            button: MouseButton::Left,
            x,
            y,
            click_count: 1,
        },
    ] {
        dispatcher.dispatch(tree, &Event::Mouse(event));
    }
}

fn click(tree: &mut ElementTree, label: &str) {
    let _ = click_with_dispatcher(tree, label);
}

fn click_with_dispatcher(tree: &mut ElementTree, label: &str) -> EventDispatcher {
    let point = target_point(tree, label);
    let mut dispatcher = EventDispatcher::new();
    click_at(tree, &mut dispatcher, point);
    dispatcher
}

fn select_option(tree: &mut ElementTree, current: &str, option: &str) {
    let selector = all_controls(tree)
        .into_iter()
        .find(|control| control.kind == "Select")
        .expect("Source type must use native Select");
    // ScarletUI clips the selected label to the field width; the popup retains
    // the complete option. Target the actual Select bounds, not clipped copy.
    assert!(
        current.starts_with(&selector.label),
        "Unexpected selected option {:?}",
        selector.label
    );
    let mut dispatcher = EventDispatcher::new();
    click_at(
        tree,
        &mut dispatcher,
        Point::new(
            selector.origin.x + selector.size.width / 2.,
            selector.origin.y + selector.size.height / 2.,
        ),
    );
    let point = target_point(tree, option);
    click_at(tree, &mut dispatcher, point);
}

fn wait_for(app: &Cadence, condition: impl Fn(&Snapshot) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        app.playback
            .last_poll
            .set(Instant::now() - Duration::from_millis(101));
        app.poll();
        if condition(&app.playback.snapshot.borrow()) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "Playback worker did not settle: {:?}",
            app.playback.snapshot.borrow()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn wait_for_effects(
    app: &Cadence,
    condition: impl Fn(&cadence_effects::RackSnapshot) -> bool,
) -> cadence_effects::RackSnapshot {
    let effects = app.effects.as_ref().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        app.playback
            .last_poll
            .set(Instant::now() - Duration::from_millis(101));
        app.poll();
        let snapshot = effects.controller.snapshot();
        assert!(
            snapshot.error.is_none(),
            "Native effect failed: {:?}",
            snapshot.error
        );
        if !snapshot.pending && condition(&snapshot) {
            // A command can finish just after app.poll reads the controller.
            // Publish that final snapshot to the pane before a layout or click.
            let _ = effects.poll();
            return snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "Native effect did not settle: {snapshot:?}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn assert_control_frames(
    element: &dyn Element,
    parent: Point,
    frame: Option<(Point, Size)>,
    context: &str,
) {
    let origin = Point::new(
        parent.x + element.position().x,
        parent.y + element.position().y,
    );
    let mut frame = frame;
    if let Some(render) = element.render_object() {
        if render
            .as_any()
            .is::<scarlet_ui::views::modifiers::FrameRenderObject>()
        {
            frame = Some((origin, element.bounds().size));
        }
        let control = render
            .as_any()
            .is::<scarlet_ui::views::ButtonRenderObject>()
            || render
                .as_any()
                .is::<scarlet_ui::views::SliderRenderObject>()
            || render
                .as_any()
                .is::<scarlet_ui::views::TextFieldRenderObject>()
            || render
                .as_any()
                .is::<scarlet_ui::views::ToggleRenderObject>()
            || render
                .as_any()
                .is::<scarlet_ui::views::SelectRenderObject>();
        if control && let Some((frame_origin, frame_size)) = frame {
            let size = element.bounds().size;
            assert!(
                origin.x >= frame_origin.x - 0.5
                    && origin.y >= frame_origin.y - 0.5
                    && origin.x + size.width <= frame_origin.x + frame_size.width + 0.5
                    && origin.y + size.height <= frame_origin.y + frame_size.height + 0.5,
                "Native control exceeds its allocated frame at {context}: {} at {origin:?}/{size:?}, frame {frame_origin:?}/{frame_size:?}",
                element.type_name_debug()
            );
        }
    }
    for child in element.children() {
        assert_control_frames(child.as_ref(), origin, frame, context);
    }
}

#[test]
fn controls_fit_minimum_and_default_content_sizes() {
    for size in [
        Size::new(737., 768.),
        Size::new(900., 568.),
        Size::new(1280., 768.),
    ] {
        for mode in modes() {
            let app = app();
            prepare_mode(&app, mode);
            let mut tree = layout(&app, size);
            assert_control_frames(
                tree.root().unwrap(),
                Point::ZERO,
                None,
                &format!("{size:?}/{mode}"),
            );
            let found = all_controls(&tree);
            let transport = transport_boundary(tree.root().unwrap(), Point::ZERO)
                .expect("Every page must retain its player bar");
            let player: Vec<_> = found
                .iter()
                .filter(|control| {
                    transport.contains(Point::new(
                        control.origin.x + control.size.width / 2.,
                        control.origin.y + control.size.height / 2.,
                    ))
                })
                .collect();
            let compact = transport.size.width < 640.;
            let required: &[&str] = if compact {
                &["icon:PlayerPlay", "icon:List"]
            } else {
                &[
                    "icon:ChevronLeft",
                    "icon:PlayerPlay",
                    "icon:ChevronRight",
                    "icon:Repeat",
                    "icon:List",
                ]
            };
            for label in required {
                assert!(
                    player.iter().any(|control| control.label == *label),
                    "Missing player control {label} in {mode}: {player:?}"
                );
            }
            assert_eq!(
                player
                    .iter()
                    .filter(|control| control.kind == "Slider")
                    .count(),
                if app.playback.snapshot.borrow().seekable {
                    2
                } else {
                    1
                },
                "Player must expose volume and only supported seek controls in {mode}"
            );
            if compact {
                for (index, left) in player.iter().enumerate() {
                    for right in player.iter().skip(index + 1) {
                        let overlap_x = (left.origin.x + left.size.width)
                            .min(right.origin.x + right.size.width)
                            - left.origin.x.max(right.origin.x);
                        let overlap_y = (left.origin.y + left.size.height)
                            .min(right.origin.y + right.size.height)
                            - left.origin.y.max(right.origin.y);
                        assert!(
                            overlap_x <= 0.5 || overlap_y <= 0.5,
                            "Compact player controls overlap in {mode}: {left:?} and {right:?}"
                        );
                    }
                }
            }
            if mode == "folders-scanning" {
                assert!(found.iter().any(|control| control.label == "Cancel scan"));
                assert!(
                    !found.iter().any(|control| control.label == "Add source"),
                    "Adding folders must be unavailable during an active scan"
                );
            }
            if mode == "folders-readonly" {
                assert!(
                    !found
                        .iter()
                        .any(|control| ["Refresh", "Remove source"]
                            .contains(&control.label.as_str()))
                );
                assert!(
                    !found.iter().any(|control| [
                        "Add folder",
                        "Rescan all",
                        "Rescan folder",
                        "Remove folder"
                    ]
                    .contains(&control.label.as_str())),
                    "Read-only catalog exposed a mutating folder action"
                );
            }
            for control in &found {
                assert!(
                    control.size.width > 0. && control.size.height > 0.,
                    "Empty control at {size:?} / {mode}: {control:?}"
                );
                assert!(
                    control.origin.x >= 0.
                        && control.origin.y >= 0.
                        && control.origin.x + control.size.width <= size.width + 0.5
                        && control.origin.y + control.size.height <= size.height + 0.5,
                    "Clipped control at {size:?} / {mode}: {control:?}"
                );
            }
            tree.clear_root();
        }
    }
}

#[test]
fn pointer_opens_closes_browser_and_fallback_picker() {
    let app = app();
    app.workspace.tab.set(Tab::Library);
    let size = Size::new(900., 568.);
    let mut tree = layout(&app, size);
    click(&mut tree, "Sources");
    rebuild(&mut tree, size);
    click(&mut tree, "Add source");
    rebuild(&mut tree, size);
    click(&mut tree, "Choose folder…");
    assert!(app.browser.visible.get());
    rebuild(&mut tree, size);
    click(&mut tree, "Done");
    assert!(!app.browser.visible.get());
    app.workspace.tab.set(Tab::Playing);
    rebuild(&mut tree, size);
    click(&mut tree, "Add files…");
    assert!(
        app.browser.visible.get(),
        "No native owner must use folder browser"
    );
    tree.clear_root();
}

#[test]
fn narrow_queue_button_closes_browser_and_opens_play_queue() {
    let app = app();
    populate(&app);
    browser_entries(&app);
    let size = Size::new(900., 568.);
    let mut tree = layout(&app, size);
    click(&mut tree, "icon:List");
    assert!(
        !app.browser.visible.get(),
        "Queue navigation left the browser open"
    );
    assert!(app.workspace.tab.get() == Tab::Playing);
    rebuild(&mut tree, size);
    let visible = all_targets(&tree);
    assert!(visible.iter().any(|control| control.label == "Clear"));
    assert!(visible.iter().any(|control| control.label == "First light"));
    assert!(!visible.iter().any(|control| control.label == "Done"));
    tree.clear_root();
}

#[test]
fn browser_pointer_selects_file_and_folder_import_excludes_directories() {
    for all_files in [false, true] {
        let app = app();
        browser_entries(&app);
        let size = Size::new(900., 568.);
        let mut tree = layout(&app, size);
        if all_files {
            click(&mut tree, "Add folder to queue");
        } else {
            click(&mut tree, "First light.flac");
            assert_eq!(app.browser.selection.get(), Some(1));
            rebuild(&mut tree, size);
            click(&mut tree, "Add selected");
        }
        assert!(!app.browser.visible.get());
        wait_for(&app, |snapshot| {
            snapshot.queue.len() == if all_files { 2 } else { 1 }
        });
        assert_eq!(
            app.playback.queue.tracks.get()[0].path,
            PathBuf::from("/Music/First light.flac")
        );
        assert!(
            app.playback
                .queue
                .tracks
                .get()
                .iter()
                .all(|track| track.path.extension().is_some())
        );
        tree.clear_root();
    }
}

#[test]
fn queue_transport_clicks_reach_worker_and_surface_playback_errors() {
    let app = app();
    app.add(tracks().into_iter().map(|track| track.path).collect());
    wait_for(&app, |snapshot| snapshot.queue.len() == 3);
    app.workspace.tab.set(Tab::Playing);
    let size = Size::new(1280., 768.);
    let mut tree = layout(&app, size);
    click(&mut tree, "icon:PlayerPlay");
    wait_for(&app, |snapshot| snapshot.state == PlaybackState::Error);
    assert!(app.playback.snapshot.borrow().error.is_some());
    rebuild(&mut tree, size);
    click(&mut tree, "icon:Repeat");
    wait_for(&app, |snapshot| snapshot.repeat == RepeatMode::All);
    rebuild(&mut tree, size);
    click(&mut tree, "Quiet streets");
    assert_eq!(app.playback.queue.selected.get(), Some(1));
    rebuild(&mut tree, size);
    wait_for(&app, |snapshot| {
        snapshot.current == Some(1) && snapshot.state == PlaybackState::Error
    });
    assert!(!app.playback.status.get().is_empty());
    rebuild(&mut tree, size);
    click(&mut tree, "Clear");
    wait_for(&app, |snapshot| snapshot.queue.is_empty());
    assert_eq!(app.playback.queue.current.get(), None);
    tree.clear_root();
}

#[test]
fn browser_suppresses_transport_shortcuts_and_escape_closes_it() {
    let app = app();
    app.browser.visible.set(true);
    for keycode in [KeyCode::Space, KeyCode::Right, KeyCode::Left] {
        assert!(!app.handle_key(KeyEvent::Pressed {
            keycode,
            modifiers: KeyModifiers::default(),
        }));
    }
    assert!(app.handle_key(KeyEvent::Pressed {
        keycode: KeyCode::Escape,
        modifiers: KeyModifiers::default(),
    }));
    assert!(!app.browser.visible.get());
}

#[test]
fn pointer_switches_all_workspace_tabs() {
    let app = app();
    let size = Size::new(900., 568.);
    let mut tree = layout(&app, size);
    for (label, expected, visible) in [
        ("Albums", Tab::Albums, "Search albums"),
        ("Artists", Tab::Artists, "Search artists"),
        ("All songs", Tab::Library, "Search title, artist, or album"),
        ("Sources", Tab::Sources, "Add source"),
        ("Effects", Tab::Effects, "Discover"),
        ("Play queue", Tab::Playing, "Add files…"),
    ] {
        click(&mut tree, label);
        assert!(
            app.workspace.tab.get() == expected,
            "Tab {label} did not activate"
        );
        rebuild(&mut tree, size);
        assert!(
            all_targets(&tree)
                .iter()
                .any(|control| control.label == visible)
        );
    }
    click(&mut tree, "All songs");
    rebuild(&mut tree, size);
    click(&mut tree, "Sources");
    assert!(app.workspace.tab.get() == Tab::Sources);
    rebuild(&mut tree, size);
    assert!(
        all_controls(&tree)
            .iter()
            .any(|control| control.label == "Add source")
    );
    click(&mut tree, "Add source");
    rebuild(&mut tree, size);
    assert!(
        all_controls(&tree)
            .iter()
            .any(|control| control.label == "Choose folder…")
    );
    click(&mut tree, "Back");
    rebuild(&mut tree, size);
    assert!(
        all_controls(&tree)
            .iter()
            .any(|control| control.label == "Add source")
    );
    tree.clear_root();
}

#[test]
fn library_search_selection_and_queue_preserve_catalog_metadata() {
    let app = app();
    library_entries(&app);
    let mut catalog = app.workspace.library.get();
    catalog[0].album_artist = Some("Guest conductor".into());
    app.workspace.library.set(catalog);
    let size = Size::new(1280., 768.);
    let mut tree = layout(&app, size);
    let mut dispatcher = click_with_dispatcher(&mut tree, "Search title, artist, or album");
    for c in "QUIET".chars() {
        dispatcher.dispatch(&mut tree, &Event::Keyboard(KeyEvent::Char { c }));
    }
    assert_eq!(app.workspace.query.get(), "QUIET");
    app.workspace.poll(&app);
    assert_eq!(app.workspace.filtered.get().len(), 1);
    assert_eq!(app.workspace.library_selection.get(), None);
    rebuild(&mut tree, size);
    click(&mut tree, "Quiet streets");
    assert_eq!(app.workspace.library_selection.get(), Some(0));
    rebuild(&mut tree, size);
    wait_for(&app, |snapshot| snapshot.queue.len() == 1);
    let queued = &app.playback.queue.tracks.get()[0];
    assert_eq!(
        queued.path,
        PathBuf::from("/cadence-test/Quiet streets.flac")
    );
    assert_eq!(queued.title, "Quiet streets");
    assert_eq!(queued.artist.as_deref(), Some("Cadence Ensemble"));
    assert_eq!(queued.album.as_deref(), Some("Small moments"));
    assert_eq!(queued.duration, Some(Duration::from_secs(213)));
    for (query, count) in [
        ("ensemble", 3),
        ("SMALL MOMENTS", 3),
        ("GUEST CONDUCTOR", 1),
        ("no matching track", 0),
    ] {
        app.workspace.query.set(query.into());
        app.workspace.poll(&app);
        assert_eq!(app.workspace.filtered.get().len(), count);
        assert_eq!(app.workspace.library_selection.get(), None);
    }
    rebuild(&mut tree, size);
    assert!(
        !all_controls(&tree)
            .iter()
            .any(|control| control.label == "Add to queue")
    );
    assert_eq!(app.playback.player.snapshot().queue.len(), 1);
    tree.clear_root();
}

#[test]
fn source_type_switching_and_input_errors_do_not_connect() {
    let app = app();
    app.workspace.tab.set(Tab::Sources);
    let size = Size::new(900., 568.);
    let mut tree = layout(&app, size);
    click(&mut tree, "Add source");
    rebuild(&mut tree, size);
    select_option(&mut tree, "Local folder", "Kanade");
    rebuild(&mut tree, size);
    click(&mut tree, "Connect");
    assert_eq!(
        app.workspace.source_status.get(),
        "Enter the source address first"
    );
    assert!(app.workspace.sources.get().is_empty());
    select_option(&mut tree, "Kanade", "DLNA");
    assert_eq!(app.workspace.source_kind.get(), 2);
    rebuild(&mut tree, size);
    select_option(&mut tree, "DLNA", "Source plugin");
    assert_eq!(app.workspace.source_kind.get(), 3);
    app.workspace.source_url.set("relative-plugin".into());
    rebuild(&mut tree, size);
    click(&mut tree, "Connect");
    assert_eq!(
        app.workspace.source_status.get(),
        "Source plugin executable must be an absolute path"
    );
    assert!(app.workspace.sources.get().is_empty());
    select_option(&mut tree, "Source plugin", "Kanade");
    assert_eq!(app.workspace.source_kind.get(), 1);
    tree.clear_root();
}

#[test]
fn source_track_selection_and_disconnected_library_track_are_truthful() {
    let app = app();
    source_entries(&app);
    let size = Size::new(1280., 768.);
    let mut tree = layout(&app, size);
    app.workspace.sources.set(vec![]);
    rebuild(&mut tree, size);
    click(&mut tree, "First light");
    assert_eq!(app.workspace.item_selection.get(), Some(1));
    assert_eq!(
        app.playback.status.get(),
        "Reconnect this track's source in Sources before playing"
    );
    assert!(app.playback.player.snapshot().queue.is_empty());
    tree.clear_root();
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "cadence-ui-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn library_folder_import_opens_browser_and_reports_missing_directory() {
    let directory = TestDirectory::new();
    let mut app = app();
    app.workspace = Workspace::persistent(
        directory.0.clone(),
        persistence::LoadedConfig {
            config: persistence::AppConfig::default(),
            writable: true,
            error: None,
        },
    );
    wait_for(&app, |_| {
        app.workspace.library_status.get().starts_with("Loaded")
    });
    app.workspace.tab.set(Tab::Library);
    app.browser.directory.set(
        directory
            .0
            .join("missing-music-folder")
            .to_string_lossy()
            .into_owned(),
    );
    let size = Size::new(900., 568.);
    let mut tree = layout(&app, size);
    click(&mut tree, "Sources");
    rebuild(&mut tree, size);
    click(&mut tree, "Add source");
    rebuild(&mut tree, size);
    click(&mut tree, "Choose folder…");
    assert!(app.browser.visible.get());
    rebuild(&mut tree, size);
    click(&mut tree, "Import to library");
    assert!(!app.browser.visible.get());
    assert!(
        app.workspace.tab.get() == Tab::Sources
            && app.workspace.source_page.get() == extension::SourcePage::Browse
    );
    wait_for(&app, |_| {
        app.workspace
            .library_status
            .get()
            .contains("missing-music-folder")
    });
    assert!(app.workspace.library.get().is_empty());
    assert!(app.playback.player.snapshot().queue.is_empty());
    assert!(!app.workspace.library_status.get().contains("Scan complete"));
    tree.clear_root();
    app.workspace.shutdown();
}

#[test]
fn folder_sources_browse_and_manage_in_the_same_page_as_remote_sources() {
    let app = app();
    folder_entries(&app);
    app.workspace
        .source_page
        .set(extension::SourcePage::Overview);
    let size = Size::new(900., 568.);
    let mut tree = layout(&app, size);
    let controls = all_controls(&tree);
    assert!(controls.iter().any(|c| c.label == "Add source"));
    assert!(
        !controls
            .iter()
            .any(|c| ["Add folder", "Scan progress", "Rescan all"].contains(&c.label.as_str()))
    );
    click(&mut tree, "cadence-test");
    assert_eq!(
        app.workspace.source_selection.get(),
        Some(extension::ProviderId::Local(PathBuf::from("/cadence-test")))
    );
    assert!(app.workspace.source_page.get() == extension::SourcePage::Browse);
    rebuild(&mut tree, size);
    for label in ["Refresh", "Remove source", "Quiet streets"] {
        assert!(
            all_targets(&tree).iter().any(|c| c.label == label),
            "Missing peer source control {label}"
        );
    }
    assert!(
        !all_controls(&tree)
            .iter()
            .any(|c| ["Play all", "Add to library", "icon:Plus"].contains(&c.label.as_str()))
    );
    click(&mut tree, "Quiet streets");
    wait_for(&app, |snapshot| {
        snapshot.queue.len() == 3 && snapshot.current == Some(1)
    });
    app.workspace.folders.busy.set(true);
    rebuild(&mut tree, size);
    assert!(all_controls(&tree).iter().any(|c| c.label == "Cancel scan"));
    assert!(
        !all_controls(&tree)
            .iter()
            .any(|c| ["Refresh", "Remove source"].contains(&c.label.as_str()))
    );
    click(&mut tree, "Details");
    rebuild(&mut tree, size);
    assert!(
        all_targets(&tree)
            .iter()
            .any(|c| c.label == "Permission denied")
    );
    assert!(app.workspace.source_page.get() == extension::SourcePage::Browse);
    tree.clear_root();
}

#[test]
fn effects_discover_empty_path_and_add_without_discovery_show_errors() {
    let app = app();
    app.workspace.tab.set(Tab::Effects);
    let size = Size::new(900., 568.);
    let mut tree = layout(&app, size);
    click(&mut tree, "Discover");
    let effects = app.effects.as_ref().unwrap();
    assert_eq!(
        effects.status.get(),
        "Enter the path to a trusted .clap file or bundle"
    );
    assert!(effects.controller.snapshot().discovered.is_empty());
    assert!(!effects.controller.snapshot().pending);
    click(&mut tree, "Add");
    assert_eq!(
        effects.status.get(),
        "Select an available plug-in, or refresh discovery"
    );
    assert!(effects.controller.snapshot().effects.is_empty());
    tree.clear_root();
}

/// Opt in with CADENCE_TEST_CLAP pointing to the built Resonara Gain fixture.
/// Discovery, parameters, bypass and removal use real native CLAP instances.
#[test]
fn native_effect_pointer_discovery_parameter_bypass_and_removal() {
    let Some(path) = native_effect_fixture() else {
        return;
    };
    assert!(
        path.is_file(),
        "Native fixture does not exist: {}",
        path.display()
    );
    for size in [Size::new(900., 568.), Size::new(1280., 768.)] {
        let app = app();
        app.workspace.tab.set(Tab::Effects);
        let effects = app.effects.as_ref().unwrap();
        effects.path.set(path.to_string_lossy().into_owned());
        let mut tree = layout(&app, size);
        click(&mut tree, "Discover");
        let discovered = wait_for_effects(&app, |snapshot| snapshot.discovered.len() == 1);
        assert_eq!(discovered.discovered[0].name, "Resonara Gain");
        rebuild(&mut tree, size);
        click(&mut tree, "Resonara Gain");
        click(&mut tree, "Add");
        let loaded = wait_for_effects(&app, |snapshot| snapshot.effects.len() == 1);
        assert_eq!(loaded.effects[0].parameters[0].name, "Gain");
        assert_eq!(loaded.effects[0].parameters[0].value, 1.0);
        rebuild(&mut tree, size);
        click(&mut tree, "Resonara Gain");
        rebuild(&mut tree, size);

        let controls = all_controls(&tree);
        // The parameter slider is above the volume and transport sliders.
        let slider = controls
            .iter()
            .filter(|control| control.label.contains("Slider"))
            .min_by(|left, right| left.origin.y.total_cmp(&right.origin.y))
            .expect("Loaded effect must expose a Gain slider");
        assert!(
            slider.origin.y < size.height / 2.,
            "Expected an effect parameter: {slider:?}"
        );
        let start_x = (slider.origin.x + slider.size.width / 2.) as i32;
        let end_x = (slider.origin.x + slider.size.width - 1.) as i32;
        let y = (slider.origin.y + slider.size.height / 2.) as i32;
        let mut dispatcher = EventDispatcher::new();
        for event in [
            MouseEvent::Moved { x: start_x, y },
            MouseEvent::ButtonPressed {
                button: MouseButton::Left,
                x: start_x,
                y,
                click_count: 1,
            },
            MouseEvent::Moved { x: end_x, y },
            MouseEvent::ButtonReleased {
                button: MouseButton::Left,
                x: end_x,
                y,
                click_count: 1,
            },
        ] {
            dispatcher.dispatch(&mut tree, &Event::Mouse(event));
        }
        let changed = wait_for_effects(&app, |snapshot| {
            (snapshot.effects[0].parameters[0].value - 2.0).abs() < 1e-9
        });
        assert_eq!(changed.effects[0].parameters[0].max, 2.0);
        assert!(!changed.effects[0].bypass);
        rebuild(&mut tree, size);
        click(&mut tree, "Toggle");
        wait_for_effects(&app, |snapshot| snapshot.effects[0].bypass);
        rebuild(&mut tree, size);
        click(&mut tree, "Toggle");
        wait_for_effects(&app, |snapshot| !snapshot.effects[0].bypass);
        rebuild(&mut tree, size);
        click(&mut tree, "icon:Trash");
        let removed = wait_for_effects(&app, |snapshot| snapshot.effects.is_empty());
        assert!(removed.config.effects.is_empty());
        tree.clear_root();
    }
}

/// Set CADENCE_RENDER_UI to a directory to export the real ScarletUI CPU renderer.
/// With CADENCE_TEST_CLAP also set, include the real populated CLAP effects pane.
/// PPM keeps this diagnostic path dependency-free; no native window is opened.
#[test]
fn render_offscreen_ui_for_visual_review() {
    let Some(directory) = std::env::var_os("CADENCE_RENDER_UI") else {
        return;
    };
    let directory = Path::new(&directory);
    std::fs::create_dir_all(directory).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while scarlet_ui::graphics::default_font_stack().is_none() {
        assert!(Instant::now() < deadline, "No UI font found");
        std::thread::sleep(Duration::from_millis(1));
    }
    for (name, size) in [
        ("minimum", Size::new(900., 568.)),
        ("default", Size::new(1280., 768.)),
    ] {
        for mode in modes().chain([
            "source-overview",
            "source-connect",
            "kanade-connect",
            "kanade-authorization",
            "artist-albums",
            "album-detail",
            "library-long",
        ]) {
            let mut app = app();
            app.size.set(size);
            match mode {
                "source-overview" => {
                    source_entries(&app);
                    app.workspace
                        .roots
                        .set(vec![PathBuf::from("/Music"), PathBuf::from("/Archive")]);
                    app.workspace
                        .source_page
                        .set(extension::SourcePage::Overview);
                }
                "source-connect" => {
                    app.workspace.tab.set(Tab::Sources);
                    app.workspace
                        .source_page
                        .set(extension::SourcePage::Connect);
                }
                "kanade-connect" => {
                    app.workspace.tab.set(Tab::Sources);
                    app.workspace
                        .source_page
                        .set(extension::SourcePage::Connect);
                    app.workspace.source_kind.set(1);
                    app.workspace.source_name.set("Kanade home".into());
                    app.workspace
                        .source_url
                        .set("https://kanade.example.com".into());
                }
                "kanade-authorization" => {
                    // Render the actual authorization controls without opening
                    // a native window, reading Keychain or dispatching network I/O.
                    app.workspace = Workspace::persistent(
                        directory.join(format!("{name}-render-state")),
                        persistence::LoadedConfig {
                            config: Default::default(),
                            writable: true,
                            error: None,
                        },
                    );
                    source_entries(&app);
                    let mut sources = app.workspace.sources.get();
                    sources[0].settings["kanade_app_identity"] = true.into();
                    app.workspace.sources.set(sources);
                    app.workspace.source_entries.set(vec![]);
                    app.workspace.source_status.set("HTTP 403 · Check server access, or authorize the selected client certificate.".into());
                }
                "artist-albums" => prepare_mode(&app, "artists"),
                "album-detail" => prepare_mode(&app, "albums"),
                "library-long" => {
                    app.workspace.library.set(scroll_catalog(120));
                    app.workspace.query.set(" ".into());
                    app.workspace.poll(&app);
                    app.workspace.tab.set(Tab::Library);
                }
                _ => prepare_mode(&app, mode),
            }
            let mut pipeline = scarlet_ui::RenderingPipeline::new();
            pipeline.set_root(content_window(&app, size).create_element());
            pipeline.layout_initial();
            let mut pixels = None;
            for _ in 0..4 {
                if let Some(buffer) = pipeline.render() {
                    pixels = Some((buffer.width(), buffer.height(), buffer.data().to_vec()));
                }
            }
            if mode == "artist-albums" {
                click(pipeline.element_tree_mut(), "Cadence Ensemble");
            }
            if mode == "album-detail" {
                click(pipeline.element_tree_mut(), "Small moments");
            }
            for _ in 0..4 {
                if let Some(buffer) = pipeline.render() {
                    pixels = Some((buffer.width(), buffer.height(), buffer.data().to_vec()));
                }
            }
            let (width, height, pixels) = pixels.expect("CPU renderer returned no frame");
            let mut file =
                std::fs::File::create(directory.join(format!("{name}-{mode}.ppm"))).unwrap();
            write!(file, "P6\n{width} {height}\n255\n").unwrap();
            let rgb: Vec<u8> = pixels
                .chunks_exact(4)
                .flat_map(|pixel| [pixel[2], pixel[1], pixel[0]])
                .collect();
            file.write_all(&rgb).unwrap();
            pipeline.teardown();
        }
    }
}

#[test]
fn unchanged_transport_polls_do_not_invalidate_or_repaint_the_window() {
    let app = app();
    // Initial catalog observation publishes the first albums/artists snapshot.
    // Idle polling is measured after that legitimate startup work has settled.
    app.poll();
    app.size.set(Size::new(900., 568.));
    let mut pipeline = scarlet_ui::RenderingPipeline::new();
    pipeline.set_root(app.window(Size::new(900., 600.)).create_element());
    pipeline.layout_initial();
    for _ in 0..4 {
        pipeline.render_for_present().unwrap();
    }
    assert!(!pipeline.has_dirty(), "Initial window did not settle");
    for _ in 0..12 {
        app.playback
            .last_poll
            .set(Instant::now() - Duration::from_millis(101));
        app.poll();
        assert!(
            !pipeline.has_dirty(),
            "An unchanged transport poll invalidated the window"
        );
        assert!(
            matches!(
                pipeline.render_for_present().unwrap(),
                scarlet_ui::renderer::PresentedFrame::Idle
            ),
            "An unchanged transport poll produced a new frame"
        );
    }
    // A changed displayed value must still be restored from the worker, then settle.
    app.playback.transport.position.set(1.25);
    app.playback.transport.time.set("stale position".into());
    app.playback
        .last_poll
        .set(Instant::now() - Duration::from_millis(101));
    app.poll();
    assert_eq!(app.playback.transport.position.get(), 0.);
    assert_eq!(app.playback.transport.time.get(), "0:00 / —:——");
    assert!(pipeline.has_dirty());
    pipeline.teardown();
}

#[test]
fn scrubbing_updates_time_only_when_the_displayed_second_changes() {
    let app = app();
    app.playback.transport.scrubbing.set(true);
    let notifications = std::sync::Arc::new(AtomicU64::new(0));
    let count = notifications.clone();
    let subscription = app
        .playback
        .transport
        .time
        .subscribe_any(std::sync::Arc::new(move || {
            count.fetch_add(1, Ordering::Relaxed);
        }));
    for seconds in [0.1, 0.2, 0.9, 1.0, 1.1, 1.9] {
        app.playback.transport.position.set(seconds);
        app.playback
            .last_poll
            .set(Instant::now() - Duration::from_millis(101));
        app.poll();
    }
    assert_eq!(
        app.playback.transport.position.get(),
        1.9,
        "Polling overwrote the scrub position"
    );
    assert_eq!(app.playback.transport.time.get(), "0:01 / —:——");
    assert_eq!(notifications.load(Ordering::Relaxed), 1);
    app.playback.transport.time.unsubscribe(subscription);
}

fn build_counts(app: &Cadence) -> [u64; 6] {
    [
        app.builds.root.get(),
        app.builds.navigation.get(),
        app.builds.main_column.get(),
        app.builds.queue.get(),
        app.builds.transport.get(),
        app.builds.track_info.get(),
    ]
}

fn settle_render(pipeline: &mut scarlet_ui::RenderingPipeline) -> Option<Vec<u8>> {
    let mut pixels = None;
    for _ in 0..12 {
        if let Some(buffer) = pipeline.render() {
            pixels = Some(buffer.data().to_vec());
        }
        if !pipeline.has_dirty() {
            return pixels;
        }
    }
    panic!("Retained UI did not settle after twelve frames");
}

#[derive(Default)]
struct RasterFrame {
    chunks: Vec<(Rect, std::sync::Arc<scarlet_ui::buffer::Buffer>)>,
}

struct ObservedCpuBackend {
    cpu: scarlet_ui::renderer::CpuPaintBackend,
    frames: Rc<RefCell<Vec<RasterFrame>>>,
}

impl scarlet_ui::renderer::PaintBackend for ObservedCpuBackend {
    fn uses_cpu_picture_cache(&self) -> bool {
        true
    }
    fn resize(&mut self, size: Size, scale_milli: u32) {
        scarlet_ui::renderer::PaintBackend::resize(&mut self.cpu, size, scale_milli);
    }

    fn render<'a>(
        &'a mut self,
        context: &PaintContext<'_>,
        background_color: Color,
        logical_damage: Option<&[Rect]>,
        physical_damage: Option<&[scarlet_ui::compositor::DamageRect]>,
    ) -> scarlet_ui::error::Result<scarlet_ui::renderer::BackendFrame<'a>> {
        let mut frame = RasterFrame::default();
        for command in context.commands() {
            let (dst, index) = match command {
                PaintCommand::DrawBuffer { dst, buffer_idx }
                | PaintCommand::DrawBufferRect {
                    dst, buffer_idx, ..
                } => (*dst, *buffer_idx),
                _ => continue,
            };
            if let scarlet_ui::renderer::PaintBuffer::Shared(buffer) = &context.buffers()[index] {
                frame.chunks.push((dst, buffer.clone()));
            }
        }
        self.frames.borrow_mut().push(frame);
        scarlet_ui::renderer::PaintBackend::render(
            &mut self.cpu,
            context,
            background_color,
            logical_damage,
            physical_damage,
        )
    }
}

fn transport_boundary(element: &dyn Element, parent: Point) -> Option<Rect> {
    let origin = Point::new(
        parent.x + element.position().x,
        parent.y + element.position().y,
    );
    if element.type_name_debug().contains("TransportFrame")
        && let Some(size) = element
            .render_object()
            .and_then(|render| render.repaint_boundary_size())
    {
        return Some(Rect::new(origin, size));
    }
    element
        .children()
        .iter()
        .find_map(|child| transport_boundary(child.as_ref(), origin))
}

fn assert_reused_raster_outside_transport(
    previous: &RasterFrame,
    next: &RasterFrame,
    transport: Rect,
) {
    assert!(
        !next.chunks.is_empty(),
        "CPU backend did not receive retained raster buffers"
    );
    // ScarletUI publishes a new Arc whenever it rasterizes a retained chunk.
    // Holding the previous Arc makes reuse measurable even when pixels match.
    for (rect, buffer) in &next.chunks {
        let reused = previous.chunks.iter().any(|(old_rect, old_buffer)| {
            old_rect == rect && std::sync::Arc::ptr_eq(old_buffer, buffer)
        });
        if !reused {
            assert!(
                rect.origin.x >= transport.origin.x - 0.5
                    && rect.origin.y >= transport.origin.y - 0.5
                    && rect.origin.x + rect.size.width
                        <= transport.origin.x + transport.size.width + 0.5
                    && rect.origin.y + rect.size.height
                        <= transport.origin.y + transport.size.height + 0.5,
                "Transport update rerasterized an unrelated layer: {rect:?}; transport: {transport:?}"
            );
        }
    }
}

#[test]
fn transport_ticks_update_pixels_without_rebuilding_the_shell_or_catalog() {
    // Exercise both the compact layout and the SplitView layout with its visible
    // queue sidebar. Both catalog and album gallery must stay retained during ticks.
    for size in [Size::new(900., 568.), Size::new(1280., 768.)] {
        for tab in [Tab::Playing, Tab::Library, Tab::Albums] {
            let app = app();
            app.size.set(size);
            populate(&app);
            if tab != Tab::Playing {
                library_entries(&app);
                app.collections.poll(&app.workspace);
            }
            app.workspace.tab.set(tab);
            let mut pipeline = scarlet_ui::RenderingPipeline::new();
            let frames = Rc::new(RefCell::new(Vec::new()));
            pipeline.set_paint_backend(Box::new(ObservedCpuBackend {
                cpu: scarlet_ui::renderer::CpuPaintBackend::new(
                    size,
                    1000,
                    ColorPalette::default().background(),
                ),
                frames: frames.clone(),
            }));
            pipeline.set_root(content_window(&app, size).create_element());
            pipeline.layout_initial();
            let mut pixels = settle_render(&mut pipeline).expect("Initial UI did not render");
            let transport =
                transport_boundary(pipeline.element_tree().root().unwrap(), Point::ZERO)
                    .expect("Transport must have its own repaint boundary");
            let mut raster_before = frames
                .borrow_mut()
                .pop()
                .expect("Initial CPU raster was not observed");
            frames.borrow_mut().clear();
            let before = build_counts(&app);
            assert!(
                before[0] > 0 && before[1] > 0 && before[2] > 0 && before[4] > 0 && before[5] > 0
            );
            if tab == Tab::Playing {
                assert!(before[3] > 0, "Populated queue must be rendered");
            }
            // Use actual retained geometry so player-bar redesigns keep the
            // unchanged-page assertion meaningful without a fixed height guess.
            let page_pixels = transport.origin.y.max(0.) as usize * size.width as usize * 4;
            for tick in 1..=6 {
                let seconds = tick as f32 * 7.;
                let time = format!("0:{:02} / 3:33", tick * 7);
                let volume = 0.1 + tick as f32 * 0.1;
                let volume_text = format!("{:.0}%", volume * 100.);
                app.playback.transport.position.set(seconds);
                app.playback.transport.time.set(time.clone());
                app.playback.transport.volume.set(volume);
                app.playback.transport.volume_text.set(volume_text.clone());
                assert!(
                    pipeline.has_dirty(),
                    "Changed transport did not invalidate its controls"
                );
                let next =
                    settle_render(&mut pipeline).expect("Changed controls did not produce pixels");
                for frame in frames.borrow_mut().drain(..) {
                    assert_reused_raster_outside_transport(&raster_before, &frame, transport);
                    // A partial present may omit unchanged chunks. Keep every
                    // previously observed Arc alive so allocator reuse cannot
                    // disguise a later rerasterization of that static layer.
                    for chunk in frame.chunks {
                        if !raster_before.chunks.iter().any(|(rect, buffer)| {
                            *rect == chunk.0 && std::sync::Arc::ptr_eq(buffer, &chunk.1)
                        }) {
                            raster_before.chunks.push(chunk);
                        }
                    }
                }
                assert!(
                    pixels != next,
                    "Tick {tick} left the displayed transport unchanged"
                );
                assert!(
                    pixels[..page_pixels] == next[..page_pixels],
                    "Transport tick repainted different pixels in the unchanged page"
                );
                assert_eq!(
                    build_counts(&app),
                    before,
                    "Tick {tick} rebuilt a structural component at {size:?}, page index={}",
                    tab as usize
                );
                let visible = all_targets(pipeline.element_tree());
                assert!(
                    visible
                        .iter()
                        .any(|target| target.label == time.split_once(" / ").unwrap().0),
                    "Displayed elapsed time did not update"
                );
                assert!(
                    visible.iter().any(|target| target.label == "3:33"),
                    "Displayed duration did not update"
                );
                assert!(
                    visible.iter().any(|target| target.label == volume_text),
                    "Displayed volume did not update"
                );
                pixels = next;
            }
            pipeline.teardown();
        }
    }
}

fn render_bounds<T: 'static>(element: &dyn Element, parent: Point, found: &mut Vec<Rect>) {
    let origin = Point::new(
        parent.x + element.position().x,
        parent.y + element.position().y,
    );
    if element
        .render_object()
        .is_some_and(|render| render.as_any().is::<T>())
    {
        found.push(Rect::new(origin, element.bounds().size));
    }
    for child in element.children() {
        render_bounds::<T>(child.as_ref(), origin, found);
    }
}

#[test]
fn forward_only_stream_has_readonly_progress_and_working_volume() {
    let app = app();
    let size = Size::new(1280., 768.);
    app.size.set(size);
    populate(&app);
    app.playback.snapshot.borrow_mut().seekable = false;
    library_entries(&app);
    app.collections.poll(&app.workspace);
    let frames = Rc::new(RefCell::new(Vec::new()));
    let mut pipeline = scarlet_ui::RenderingPipeline::new();
    pipeline.set_paint_backend(Box::new(ObservedCpuBackend {
        cpu: scarlet_ui::renderer::CpuPaintBackend::new(
            size,
            1000,
            ColorPalette::default().background(),
        ),
        frames: frames.clone(),
    }));
    pipeline.set_root(content_window(&app, size).create_element());
    pipeline.layout_initial();
    settle_render(&mut pipeline).expect("Stream UI did not render");

    let controls = all_controls(pipeline.element_tree());
    let sliders: Vec<_> = controls
        .iter()
        .filter(|control| control.kind == "Slider")
        .collect();
    assert_eq!(
        sliders.len(),
        1,
        "Forward-only media must expose only the volume slider"
    );
    let mut progress = Vec::new();
    render_bounds::<scarlet_ui::views::ProgressViewRenderObject>(
        pipeline.element_tree().root().unwrap(),
        Point::ZERO,
        &mut progress,
    );
    assert_eq!(
        progress.len(),
        1,
        "Stream position must use native readonly ProgressView"
    );
    let progress = progress[0];
    let mut dispatcher = EventDispatcher::new();
    click_at(
        pipeline.element_tree_mut(),
        &mut dispatcher,
        Point::new(
            progress.origin.x + progress.size.width * 0.75,
            progress.origin.y + progress.size.height / 2.,
        ),
    );
    assert_eq!(
        app.playback.transport.position.get(),
        0.,
        "Clicking progress changed the stream position"
    );
    assert!(!app.playback.transport.scrubbing.get());
    assert!(
        app.playback.transport.seek_pending.get().is_none(),
        "Readonly progress requested a seek"
    );

    let volume = sliders[0];
    click_at(
        pipeline.element_tree_mut(),
        &mut dispatcher,
        Point::new(
            volume.origin.x + volume.size.width * 0.25,
            volume.origin.y + volume.size.height / 2.,
        ),
    );
    let volume = app.playback.transport.volume.get();
    assert!(
        volume > 0. && volume < 0.5,
        "Volume slider did not respond: {volume}"
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while (app.playback.player.snapshot().volume - volume).abs() > 1e-6 {
        assert!(
            Instant::now() < deadline,
            "Volume did not reach playback worker"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let before_pixels = settle_render(&mut pipeline).expect("Volume edit did not repaint");
    let raster_before = frames
        .borrow_mut()
        .pop()
        .expect("CPU raster was not observed");
    frames.borrow_mut().clear();
    let before_builds = build_counts(&app);
    let transport =
        transport_boundary(pipeline.element_tree().root().unwrap(), Point::ZERO).unwrap();
    app.playback.transport.position.set(45.);
    app.playback.transport.time.set("0:45 / 3:33".into());
    let after_pixels = settle_render(&mut pipeline).expect("Readonly progress did not repaint");
    assert!(
        before_pixels != after_pixels,
        "Stream progress stayed visually unchanged"
    );
    let page_pixels = transport.origin.y as usize * size.width as usize * 4;
    assert!(
        before_pixels[..page_pixels] == after_pixels[..page_pixels],
        "Stream progress changed the library pixels"
    );
    assert_eq!(
        build_counts(&app),
        before_builds,
        "Stream progress rebuilt unrelated views"
    );
    for frame in frames.borrow().iter() {
        assert_reused_raster_outside_transport(&raster_before, frame, transport);
    }
    assert!(
        all_targets(pipeline.element_tree())
            .iter()
            .any(|target| target.label == "0:45")
    );
    assert!(app.playback.transport.seek_pending.get().is_none());
    pipeline.teardown();
}

/// Keeps a real decoder playing while never opening a host audio device.
struct HoldingOutput {
    pending: usize,
}
impl cadence_core::AudioOutput for HoldingOutput {
    fn sample_rate(&self) -> u32 {
        8_000
    }
    fn write(&mut self, frames: &[cadence_core::Frame]) -> cadence_core::Result<usize> {
        let count = frames.len().min(256usize.saturating_sub(self.pending));
        self.pending += count;
        Ok(count)
    }
    fn pending_frames(&self) -> usize {
        self.pending
    }
    fn check_error(&self) -> cadence_core::Result<()> {
        Ok(())
    }
}

fn silent_wav(path: &Path) {
    let data_bytes = 8_000_u32 * 10 * 2;
    let mut file = std::fs::File::create(path).unwrap();
    file.write_all(b"RIFF").unwrap();
    file.write_all(&(36 + data_bytes).to_le_bytes()).unwrap();
    file.write_all(b"WAVEfmt ").unwrap();
    file.write_all(&16_u32.to_le_bytes()).unwrap();
    file.write_all(&1_u16.to_le_bytes()).unwrap();
    file.write_all(&1_u16.to_le_bytes()).unwrap();
    file.write_all(&8_000_u32.to_le_bytes()).unwrap();
    file.write_all(&16_000_u32.to_le_bytes()).unwrap();
    file.write_all(&2_u16.to_le_bytes()).unwrap();
    file.write_all(&16_u16.to_le_bytes()).unwrap();
    file.write_all(b"data").unwrap();
    file.write_all(&data_bytes.to_le_bytes()).unwrap();
    file.write_all(&vec![0; data_bytes as usize]).unwrap();
}

fn rects_overlap(left: Rect, right: Rect) -> bool {
    left.origin.x < right.origin.x + right.size.width
        && right.origin.x < left.origin.x + left.size.width
        && left.origin.y < right.origin.y + right.size.height
        && right.origin.y < left.origin.y + left.size.height
}

fn assert_pixels_unchanged_in_rect(
    before: &[u8],
    after: &[u8],
    width: usize,
    scale: f32,
    region: Rect,
    except: Option<Rect>,
) {
    assert_eq!(before.len(), after.len());
    for y in (region.origin.y * scale).ceil() as usize
        ..((region.origin.y + region.size.height) * scale).floor() as usize
    {
        for x in (region.origin.x * scale).ceil() as usize
            ..((region.origin.x + region.size.width) * scale).floor() as usize
        {
            if except
                .is_some_and(|rect| rect.contains(Point::new(x as f32 / scale, y as f32 / scale)))
            {
                continue;
            }
            let offset = (y * width + x) * 4;
            assert_eq!(
                &before[offset..offset + 4],
                &after[offset..offset + 4],
                "Static navigation/detail pixel changed at physical ({x}, {y})"
            );
        }
    }
}

#[test]
fn play_all_preserves_album_detail_and_navigation_at_native_retina_size() {
    for (window, scale_milli) in [
        (Size::new(1280., 800.), 1000),
        (Size::new(1512., 1429.), 2000),
    ] {
        let directory = TestDirectory::new();
        let app = Cadence::new(
            Player::new(Box::new(|| Ok(Box::new(HoldingOutput { pending: 0 })))).unwrap(),
        );
        let entries: Vec<_> = (0..6)
            .map(|index| {
                let path = directory.0.join(format!("track-{index}.wav"));
                silent_wav(&path);
                let item_id = path.to_string_lossy().into_owned();
                let mut track = library_tracks()[0].clone();
                track.id = track_id("local", &item_id);
                track.item_id = item_id;
                track.title = format!("Album track {}", index + 1);
                track.track_number = Some(index + 1);
                track.location = MediaLocation::Local(path);
                track.duration_seconds = Some(10.);
                track
            })
            .collect();
        app.workspace.library.set(entries.clone());
        app.workspace.filtered.set(entries);
        app.poll();
        let mut pipeline = scarlet_ui::RenderingPipeline::new();
        let frames = Rc::new(RefCell::new(Vec::new()));
        pipeline.set_paint_backend(Box::new(ObservedCpuBackend {
            cpu: scarlet_ui::renderer::CpuPaintBackend::new(
                window,
                scale_milli,
                ColorPalette::default().background(),
            ),
            frames: frames.clone(),
        }));
        pipeline.set_scale_milli(scale_milli);
        pipeline.set_root(app.window(window).create_element());
        pipeline.layout_initial();
        settle_render(&mut pipeline).unwrap();
        click(pipeline.element_tree_mut(), "Albums");
        settle_render(&mut pipeline);
        click(pipeline.element_tree_mut(), "Small moments");
        settle_render(&mut pipeline).unwrap();
        let button = all_controls(pipeline.element_tree())
            .into_iter()
            .find(|control| control.label == "Play all")
            .unwrap();
        let point = Point::new(
            button.origin.x + button.size.width / 2.,
            button.origin.y + button.size.height / 2.,
        );
        let mut dispatcher = EventDispatcher::new();
        dispatcher.dispatch(
            pipeline.element_tree_mut(),
            &Event::Mouse(MouseEvent::Moved {
                x: point.x as i32,
                y: point.y as i32,
            }),
        );
        let before_click = settle_render(&mut pipeline).expect("Hover did not render");
        let transport =
            transport_boundary(pipeline.element_tree().root().unwrap(), Point::ZERO).unwrap();
        let static_region = Rect::from_xywh(
            0.,
            32.,
            transport.origin.x + transport.size.width + 19.,
            transport.origin.y - 44.,
        );
        let button_region = Rect::from_xywh(
            button.origin.x - 3.,
            button.origin.y - 3.,
            button.size.width + 6.,
            button.size.height + 6.,
        );
        click_at(pipeline.element_tree_mut(), &mut dispatcher, point);
        let after_click = settle_render(&mut pipeline).expect("Play all did not render");
        let scale = scale_milli as f32 / 1000.;
        let physical_width = (window.width * scale) as usize;
        assert_pixels_unchanged_in_rect(
            &before_click,
            &after_click,
            physical_width,
            scale,
            static_region,
            Some(button_region),
        );
        let before_worker = frames
            .borrow_mut()
            .pop()
            .expect("CPU frame was not observed");
        frames.borrow_mut().clear();
        wait_for(&app, |snapshot| {
            snapshot.queue.len() == 6 && snapshot.state == PlaybackState::Playing
        });
        assert_eq!(app.playback.snapshot.borrow().current, Some(0));
        let after_worker = settle_render(&mut pipeline).expect("Playback changes did not render");
        assert_pixels_unchanged_in_rect(
            &after_click,
            &after_worker,
            physical_width,
            scale,
            static_region,
            None,
        );
        for frame in frames.borrow().iter() {
            for (rect, buffer) in &frame.chunks {
                if rects_overlap(*rect, static_region) {
                    assert!(
                        before_worker
                            .chunks
                            .iter()
                            .any(|(old_rect, old)| old_rect == rect
                                && std::sync::Arc::ptr_eq(old, buffer)),
                        "Starting album playback rerasterized static navigation/detail: {rect:?}"
                    );
                }
            }
        }
        assert!(
            all_targets(pipeline.element_tree())
                .iter()
                .any(|control| control.label == "Album track 6")
        );
        assert!(
            all_controls(pipeline.element_tree())
                .iter()
                .any(|control| control.label == "Play all")
        );
        pipeline.teardown();
    }
}

#[derive(Clone, Copy, Debug)]
struct ListScroll {
    id: scarlet_ui::ElementId,
    viewport: Size,
    content_height: f32,
    offset: f32,
}

fn list_scrolls(element: &dyn Element, found: &mut Vec<ListScroll>) {
    let name = element.type_name_debug();
    if name.starts_with("RenderElement<scarlet_ui_core::views::scroll::ScrollView<")
        && (name.contains("ListContentView<") || name.contains("containers::vstack::VStack<"))
        && let Some(child) = element.children().first()
    {
        found.push(ListScroll {
            id: element.id(),
            viewport: element.bounds().size,
            content_height: child.bounds().size.height
                + if name.contains("cadence_library::LibraryTrack")
                    || name.contains("cadence_core::player::Track")
                    || name.contains("extension::SourceItem")
                    || name.contains("extension::ProviderRow")
                    || name.contains("containers::vstack::VStack<")
                {
                    PLAYER_CLEARANCE
                } else {
                    0.
                },
            offset: -child.position().y,
        });
    }
    for child in element.children() {
        list_scrolls(child.as_ref(), found);
    }
}

fn scroll_list_to_end(
    pipeline: &mut scarlet_ui::RenderingPipeline,
    id: scarlet_ui::ElementId,
    row_count: usize,
    row_height: f32,
    bottom_inset: f32,
) -> ListScroll {
    pipeline
        .element_tree_mut()
        .find_element_mut(id)
        .unwrap()
        .handle_event(
            &Event::Mouse(MouseEvent::Wheel {
                delta_x: 0,
                delta_y: -3_000_000,
                x: 10,
                y: 10,
                phase: scarlet_ui::event::WheelPhase::Moved,
                source: scarlet_ui::event::ScrollSource::Trackpad,
            }),
            scarlet_ui::event::Phase::Target,
        );
    settle_render(pipeline);
    let mut scrolls = Vec::new();
    list_scrolls(pipeline.element_tree().root().unwrap(), &mut scrolls);
    let scroll = *scrolls
        .iter()
        .find(|scroll| scroll.id == id)
        .expect("List scroll identity was lost");
    let expected_content = row_count as f32 * row_height + bottom_inset;
    let expected_offset = (expected_content - scroll.viewport.height).max(0.);
    assert!(
        (scroll.content_height - expected_content).abs() < 0.5,
        "Wrong list extent: {scroll:?}; expected {expected_content}"
    );
    assert!(
        (scroll.offset - expected_offset).abs() < 0.5,
        "Scrolled beyond the actual rows: {scroll:?}; expected max {expected_offset}"
    );
    scroll
}

fn scroll_catalog(count: usize) -> Vec<LibraryTrack> {
    (0..count)
        .map(|index| {
            let mut track = library_tracks()[0].clone();
            track.item_id = format!("/cadence-scroll/{index:03}.wav");
            track.id = track_id("local", &track.item_id);
            track.location = MediaLocation::Local(PathBuf::from(&track.item_id));
            track.title = format!("{} {index:03}", if index < 6 { "Keep" } else { "Song" });
            track.track_number = Some(index as u32 + 1);
            track
        })
        .collect()
}

#[test]
fn list_scroll_extent_clamps_after_queue_filter_folder_and_source_shrink() {
    for (mode, row_height) in [
        ("queue", 52.),
        ("library", 40.),
        ("detail", 52.),
        ("folders", 76.),
        ("sources", 52.),
    ] {
        let bottom_inset = if mode == "detail" {
            164. + PLAYER_CLEARANCE
        } else if ["queue", "library", "folders", "sources"].contains(&mode) {
            PLAYER_CLEARANCE
        } else {
            0.
        };
        let app = app();
        app.size.set(Size::new(1280., 1168.));
        let entries = scroll_catalog(100);
        match mode {
            "queue" => {
                app.workspace.tab.set(Tab::Playing);
                app.playback.queue.tracks.set(
                    entries
                        .iter()
                        .map(|entry| Track {
                            path: PathBuf::from(&entry.item_id),
                            media_input: None,
                            title: entry.title.clone(),
                            artist: entry.artist.clone(),
                            album: entry.album.clone(),
                            duration: Some(Duration::from_secs(213)),
                        })
                        .collect(),
                );
            }
            "library" | "detail" => {
                app.workspace.library.set(entries.clone());
                app.workspace.query.set(" ".into());
                app.workspace.poll(&app);
                app.collections.poll(&app.workspace);
                app.workspace.tab.set(if mode == "detail" {
                    Tab::Albums
                } else {
                    Tab::Library
                });
            }
            "folders" => {
                app.workspace.tab.set(Tab::Sources);
                app.workspace
                    .source_page
                    .set(extension::SourcePage::Overview);
                app.workspace.roots.set(
                    (0..100)
                        .map(|i| PathBuf::from(format!("/cadence-scroll/folder-{i}")))
                        .collect(),
                );
            }
            "sources" => {
                source_entries(&app);
                app.workspace.item_selection.set(None);
                app.workspace.source_entries.set(
                    entries
                        .iter()
                        .cloned()
                        .map(|track| extension::SourceItem::Track(Arc::new(track)))
                        .collect(),
                );
            }
            _ => unreachable!(),
        }
        let mut pipeline = scarlet_ui::RenderingPipeline::new();
        pipeline.set_root(app.window(Size::new(1280., 1200.)).create_element());
        pipeline.layout_initial();
        settle_render(&mut pipeline).unwrap();
        if mode == "detail" {
            click(pipeline.element_tree_mut(), "Small moments");
            settle_render(&mut pipeline).unwrap();
        }
        let mut scrolls = Vec::new();
        list_scrolls(pipeline.element_tree().root().unwrap(), &mut scrolls);
        let list = *scrolls
            .iter()
            .find(|scroll| (scroll.content_height - 100. * row_height - bottom_inset).abs() < 0.5)
            .unwrap_or_else(|| panic!("Missing 100-row {mode} list: {scrolls:?}"));
        scroll_list_to_end(&mut pipeline, list.id, 100, row_height, bottom_inset);
        match mode {
            "queue" => app
                .playback
                .queue
                .tracks
                .set(app.playback.queue.tracks.get()[..6].to_vec()),
            "library" => {
                app.workspace.query.set("Keep".into());
                app.workspace.poll(&app);
                assert_eq!(app.workspace.filtered.get().len(), 6);
            }
            "detail" => {
                app.workspace.library.set(entries[..6].to_vec());
                app.collections.poll(&app.workspace);
            }
            "folders" => app
                .workspace
                .roots
                .set(app.workspace.roots.get()[..6].to_vec()),
            "sources" => app.workspace.source_entries.set(
                entries[..6]
                    .iter()
                    .cloned()
                    .map(|track| extension::SourceItem::Track(Arc::new(track)))
                    .collect(),
            ),
            _ => unreachable!(),
        }
        settle_render(&mut pipeline);
        let small = scroll_list_to_end(&mut pipeline, list.id, 6, row_height, bottom_inset);
        assert!(
            small.viewport.height >= 6. * row_height,
            "Fixture should fit six {mode} rows: {small:?}"
        );
        assert_eq!(small.offset, 0., "Shrinking {mode} left empty overscroll");
        pipeline.teardown();
    }
}

#[test]
fn album_detail_displays_tag_numbers_in_disc_then_track_order() {
    let app = app();
    app.size.set(Size::new(1280., 768.));
    let mut entries = scroll_catalog(4);
    for (track, (disc, number, title)) in entries.iter_mut().zip([
        (2, 1, "First on second disc"),
        (1, 7, "Seventh on first disc"),
        (1, 3, "Third on first disc"),
        (2, 4, "Fourth on second disc"),
    ]) {
        track.disc_number = Some(disc);
        track.track_number = Some(number);
        track.title = title.into();
    }
    app.workspace.library.set(entries);
    app.collections.poll(&app.workspace);
    let mut pipeline = scarlet_ui::RenderingPipeline::new();
    pipeline.set_root(app.window(Size::new(1280., 800.)).create_element());
    pipeline.layout_initial();
    settle_render(&mut pipeline);
    click(pipeline.element_tree_mut(), "Small moments");
    settle_render(&mut pipeline);
    let targets = all_targets(pipeline.element_tree());
    let ordered = [
        ("Third on first disc", "1", "3"),
        ("Seventh on first disc", "1", "7"),
        ("First on second disc", "2", "1"),
        ("Fourth on second disc", "2", "4"),
    ];
    let mut previous_y = 0.;
    for (title, disc, number) in ordered {
        let row = targets.iter().find(|target| target.label == title).unwrap();
        assert!(
            row.origin.y > previous_y,
            "Wrong tagged track order: {title}"
        );
        previous_y = row.origin.y;
        for label in [format!("{disc} · {number}")] {
            assert!(
                targets
                    .iter()
                    .any(|target| target.label == label
                        && (target.origin.y - row.origin.y).abs() < 20.),
                "Missing tag {label} on {title}"
            );
        }
    }
    pipeline.teardown();
}

#[test]
fn native_effect_short_lists_do_not_scroll_into_empty_space() {
    if native_effect_fixture().is_none() {
        return;
    }
    let app = app();
    app.size.set(Size::new(1280., 1168.));
    prepare_mode(&app, "effects-loaded");
    let mut pipeline = scarlet_ui::RenderingPipeline::new();
    pipeline.set_root(app.window(Size::new(1280., 1200.)).create_element());
    pipeline.layout_initial();
    settle_render(&mut pipeline).unwrap();
    let mut scrolls = Vec::new();
    list_scrolls(pipeline.element_tree().root().unwrap(), &mut scrolls);
    assert!(
        scrolls.len() >= 3,
        "Expected chain, available plugins, and parameters lists"
    );
    for scroll in scrolls {
        if scroll.content_height > 0. && scroll.content_height <= scroll.viewport.height {
            scroll_list_to_end(&mut pipeline, scroll.id, 1, scroll.content_height, 0.);
        }
    }
    pipeline.teardown();
}

#[test]
fn gpu_scroll_command_work_is_bounded_by_visible_gallery_and_detail_rows() {
    use scarlet_ui::renderer::{BackendFrame, PaintBackend};
    struct CommandBackend(Rc<Cell<usize>>);
    impl PaintBackend for CommandBackend {
        fn resize(&mut self, _: Size, _: u32) {}
        fn render<'a>(
            &'a mut self,
            ctx: &PaintContext<'_>,
            _: Color,
            _: Option<&[Rect]>,
            _: Option<&[scarlet_ui::compositor::DamageRect]>,
        ) -> scarlet_ui::error::Result<BackendFrame<'a>> {
            self.0.set(ctx.commands().len());
            Ok(BackendFrame::External)
        }
    }
    let app = app();
    let template = library_tracks()[0].clone();
    let entries: Vec<_> = (0..5381)
        .map(|index| {
            let mut entry = template.clone();
            let album = index / 15;
            let path = format!("/cadence-test/albums/{album:04}/track-{index:04}.flac");
            entry.id = track_id("local", &path);
            entry.item_id = path.clone();
            entry.location = MediaLocation::Local(PathBuf::from(path));
            entry.album = Some(format!("Album {album:04}"));
            entry.title = format!("Track {index:04}");
            entry.track_number = Some(index as u32 % 15 + 1);
            entry.disc_number = Some(1);
            entry.artwork = None;
            entry
        })
        .collect();
    app.workspace.library.set(entries.clone());
    app.workspace.filtered.set(entries);
    app.poll();
    let commands = Rc::new(Cell::new(0));
    let mut pipeline = scarlet_ui::RenderingPipeline::new();
    pipeline.set_paint_backend(Box::new(CommandBackend(commands.clone())));
    pipeline.set_root(app.window(Size::new(1280.0, 800.0)).create_element());
    pipeline.resize(Size::new(1280.0, 800.0));
    pipeline.layout_initial();
    pipeline.render_for_present().unwrap();
    click(pipeline.element_tree_mut(), "Albums");
    pipeline.render_for_present().unwrap();
    let mut elapsed = Vec::new();
    for step in 0..120 {
        let started = Instant::now();
        pipeline.handle_event(&Event::Mouse(MouseEvent::Wheel {
            x: 800,
            y: 450,
            delta_x: 0,
            delta_y: if step < 60 { -1600 } else { 1600 },
            phase: scarlet_ui::event::WheelPhase::Moved,
            source: scarlet_ui::event::ScrollSource::Trackpad,
        }));
        pipeline.render_for_present().unwrap();
        elapsed.push(started.elapsed().as_micros());
        assert!(
            commands.get() < 2500,
            "Gallery emitted {} commands for offscreen rows",
            commands.get()
        );
    }
    click(pipeline.element_tree_mut(), "All songs");
    pipeline.render_for_present().unwrap();
    for _ in 0..120 {
        let started = Instant::now();
        pipeline.handle_event(&Event::Mouse(MouseEvent::Wheel {
            x: 800,
            y: 450,
            delta_x: 0,
            delta_y: -1600,
            phase: scarlet_ui::event::WheelPhase::Moved,
            source: scarlet_ui::event::ScrollSource::Trackpad,
        }));
        pipeline.render_for_present().unwrap();
        elapsed.push(started.elapsed().as_micros());
        assert!(
            commands.get() < 2500,
            "Track list emitted {} commands for offscreen rows",
            commands.get()
        );
    }
    elapsed.sort_unstable();
    eprintln!(
        "GPU command preparation (5381 tracks; 240 scroll frames; excludes GPU execution): median={}us p95={}us max={}us; last frame commands={}",
        elapsed[elapsed.len() / 2],
        elapsed[elapsed.len() * 95 / 100],
        elapsed.last().unwrap(),
        commands.get()
    );
}

#[test]
fn artists_navigate_via_albums_and_track_activation_starts_in_album_context() {
    let app = app();
    library_entries(&app);
    app.collections.poll(&app.workspace);
    let size = Size::new(1280., 768.);
    let mut tree = layout(&app, size);
    click(&mut tree, "Artists");
    rebuild(&mut tree, size);
    click(&mut tree, "Cadence Ensemble");
    rebuild(&mut tree, size);
    assert!(
        all_targets(&tree)
            .iter()
            .any(|target| target.label == "Small moments")
    );
    assert!(
        !all_targets(&tree)
            .iter()
            .any(|target| target.label == "Quiet streets")
    );
    click(&mut tree, "Small moments");
    rebuild(&mut tree, size);
    click(&mut tree, "Quiet streets");
    wait_for(&app, |snapshot| {
        snapshot.queue.len() == 3 && snapshot.current == Some(1)
    });
    let snapshot = app.playback.player.snapshot();
    assert_eq!(
        snapshot
            .queue
            .iter()
            .map(|track| track.title.as_str())
            .collect::<Vec<_>>(),
        vec!["First light", "Quiet streets", "The long way home"]
    );
    rebuild(&mut tree, size);
    click(&mut tree, "Back");
    rebuild(&mut tree, size);
    assert!(
        all_targets(&tree)
            .iter()
            .any(|target| target.label == "Small moments")
    );
    assert!(
        !all_controls(&tree)
            .iter()
            .any(|target| target.label == "Play all")
    );
    click(&mut tree, "Back");
    rebuild(&mut tree, size);
    assert!(
        all_targets(&tree)
            .iter()
            .any(|target| target.label == "Cadence Ensemble")
    );
    tree.clear_root();
}

#[test]
fn library_search_headers_share_geometry_and_artist_detail_controls_fit() {
    let app = app();
    library_entries(&app);
    app.collections.poll(&app.workspace);
    for size in [Size::new(900., 568.), Size::new(1280., 768.)] {
        let mut tree = layout(&app, size);
        let mut origin = None;
        for (tab, placeholder) in [
            ("Albums", "Search albums"),
            ("Artists", "Search artists"),
            ("All songs", "Search title, artist, or album"),
        ] {
            click(&mut tree, tab);
            rebuild(&mut tree, size);
            let field = all_controls(&tree)
                .into_iter()
                .find(|control| control.label == placeholder)
                .unwrap();
            if let Some((position, extent)) = origin {
                assert_eq!(field.origin, position);
                assert_eq!(field.size, extent);
            } else {
                origin = Some((field.origin, field.size));
            }
        }
        click(&mut tree, "Artists");
        rebuild(&mut tree, size);
        click(&mut tree, "Cadence Ensemble");
        rebuild(&mut tree, size);
        assert_control_frames(tree.root().unwrap(), Point::ZERO, None, "artist albums");
        click(&mut tree, "Small moments");
        rebuild(&mut tree, size);
        assert_control_frames(
            tree.root().unwrap(),
            Point::ZERO,
            None,
            "artist album detail",
        );
        tree.clear_root();
    }
}

#[test]
fn all_songs_click_plays_a_catalog_larger_than_the_previous_queue_limit() {
    let app = app();
    app.workspace.library.set(scroll_catalog(10_762));
    app.workspace.query.set(" ".into());
    app.workspace.poll(&app);
    app.workspace.tab.set(Tab::Library);
    let size = Size::new(900., 568.);
    let mut tree = layout(&app, size);
    click(&mut tree, "Keep 000");
    wait_for(&app, |snapshot| {
        snapshot.queue.len() == 10_762 && snapshot.current == Some(0)
    });
    assert_eq!(
        app.playback.player.snapshot().queue[10_761].title,
        "Song 10761"
    );
    tree.clear_root();
}

#[test]
fn page_and_queue_headers_have_one_shared_height_and_player_has_its_own_width() {
    fn headers(element: &dyn Element, parent: Point, found: &mut Vec<Rect>) {
        let origin = Point::new(
            parent.x + element.position().x,
            parent.y + element.position().y,
        );
        if element
            .render_object()
            .is_some_and(|r| r.as_any().is::<scarlet_ui::views::HeaderBarRenderObject>())
            && origin.y.abs() < 0.5
        {
            found.push(Rect::new(origin, element.bounds().size));
        }
        for child in element.children() {
            headers(child.as_ref(), origin, found);
        }
    }
    for size in [
        Size::new(900., 568.),
        Size::new(1280., 768.),
        Size::new(1600., 1000.),
    ] {
        for mode in modes() {
            let app = app();
            prepare_mode(&app, mode);
            let mut tree = layout(&app, size);
            let mut found = Vec::new();
            headers(tree.root().unwrap(), Point::ZERO, &mut found);
            assert!(!found.is_empty(), "Missing top header for {mode}");
            let title = match mode {
                "albums" | "empty" => Some("Albums"),
                "artists" => Some("Artists"),
                "library" | "library-empty" => Some("All songs"),
                "queue" => Some("Play queue"),
                "effects" | "effects-loaded" => Some("Effects"),
                "folders-empty" => Some("Sources"),
                _ => None,
            };
            let main = found
                .iter()
                .min_by(|a, b| a.origin.x.total_cmp(&b.origin.x))
                .unwrap();
            if let Some(title) = title {
                let label = all_targets(&tree)
                    .into_iter()
                    .find(|c| {
                        c.label == title
                            && c.origin.x >= main.origin.x
                            && c.origin.y < LIBRARY_HEADER_HEIGHT
                    })
                    .unwrap();
                assert!(
                    (label.origin.x - main.origin.x - PAGE_PADDING).abs() < 0.5,
                    "Header title is not aligned in {mode}: {label:?}"
                );
                assert!(
                    (label.origin.y + label.size.height * 0.5 - LIBRARY_HEADER_HEIGHT * 0.5).abs()
                        < 0.5,
                    "Header title is not vertically centered in {mode}: {label:?}"
                );
            }
            let main_width = found
                .iter()
                .min_by(|a, b| a.origin.x.total_cmp(&b.origin.x))
                .unwrap()
                .size
                .width;
            for header in found {
                assert_eq!(
                    header.size.height, 48.,
                    "Unmatched header in {mode}: {header:?}"
                );
            }
            let player = transport_boundary(tree.root().unwrap(), Point::ZERO).unwrap();
            assert_eq!(player.size.width, player_width(main_width));
            assert!(
                player.size.width < main_width - 40.,
                "Player should have more space around it than the artwork grid"
            );
            assert!(player.size.width <= 960.);
            tree.clear_root();
        }
    }
}

#[test]
fn local_source_navigation_preserves_scope_and_uses_back_for_each_level() {
    let app = app();
    let mut entries = library_tracks();
    for entry in &mut entries {
        let MediaLocation::Local(path) = &mut entry.location else {
            unreachable!()
        };
        *path = PathBuf::from("/cadence-test/Artist/Small moments").join(path.file_name().unwrap());
    }
    let mut outsider = entries[0].clone();
    outsider.id = "outside".into();
    outsider.location = MediaLocation::Local(PathBuf::from("/another-root/outside.flac"));
    entries.push(outsider);
    app.workspace.library.set(entries);
    app.workspace.open_provider(
        &app,
        extension::ProviderId::Local(PathBuf::from("/cadence-test")),
    );
    app.workspace.tab.set(Tab::Sources);
    let size = Size::new(900., 568.);
    let mut tree = layout(&app, size);
    assert_eq!(app.workspace.source_entries.get().len(), 1);
    assert!(!all_controls(&tree).iter().any(|c| c.label == "Play all"));
    click(&mut tree, "Artist");
    rebuild(&mut tree, size);
    assert!(
        all_targets(&tree)
            .iter()
            .any(|c| c.label == "Small moments")
    );
    click(&mut tree, "Small moments");
    rebuild(&mut tree, size);
    assert_eq!(app.workspace.source_entries.get().len(), 3);
    assert!(
        app.workspace
            .source_status
            .get()
            .starts_with("3 items · /cadence-test/Artist/Small moments")
    );
    click(&mut tree, "Back");
    rebuild(&mut tree, size);
    assert!(app.workspace.source_page.get() == extension::SourcePage::Browse);
    assert!(
        all_targets(&tree)
            .iter()
            .any(|c| c.label == "Small moments")
    );
    click(&mut tree, "Back");
    rebuild(&mut tree, size);
    assert!(all_targets(&tree).iter().any(|c| c.label == "Artist"));
    click(&mut tree, "Back");
    assert!(app.workspace.source_page.get() == extension::SourcePage::Overview);
    tree.clear_root();
}

#[test]
fn album_information_scrolls_with_virtualized_tracks_and_remains_above_player_at_end() {
    let app = app();
    app.workspace.library.set(scroll_catalog(10_000));
    app.collections.poll(&app.workspace);
    app.workspace.tab.set(Tab::Albums);
    let mut pipeline = scarlet_ui::RenderingPipeline::new();
    pipeline.set_root(app.window(Size::new(1280., 800.)).create_element());
    pipeline.layout_initial();
    settle_render(&mut pipeline);
    click(pipeline.element_tree_mut(), "Small moments");
    settle_render(&mut pipeline);
    let info_before = all_targets(pipeline.element_tree())
        .into_iter()
        .find(|c| c.label == "Small moments")
        .unwrap();
    let mut scrolls = Vec::new();
    list_scrolls(pipeline.element_tree().root().unwrap(), &mut scrolls);
    let scroll = scrolls
        .iter()
        .find(|s| s.content_height > 500_000.)
        .unwrap();
    scroll_list_to_end(
        &mut pipeline,
        scroll.id,
        10_000,
        52.,
        164. + PLAYER_CLEARANCE,
    );
    let targets = all_targets(pipeline.element_tree());
    let info_after = targets.iter().find(|c| c.label == "Small moments").unwrap();
    assert!(
        info_before.origin.y > 0. && info_after.origin.y < 0.,
        "Information must move with the track scroll"
    );
    let rendered_tracks = targets
        .iter()
        .filter(|c| c.label.starts_with("Song ") || c.label.starts_with("Keep "))
        .count();
    assert!(
        rendered_tracks < 60,
        "Mounted {rendered_tracks} tracks instead of virtualizing"
    );
    let last = targets.iter().find(|c| c.label == "Song 9999").unwrap();
    let player = transport_boundary(pipeline.element_tree().root().unwrap(), Point::ZERO).unwrap();
    assert!(
        last.origin.y + last.size.height < player.origin.y,
        "The last song is covered by the player"
    );
    pipeline.teardown();
}

#[test]
fn removing_local_and_remote_sources_updates_library_collections_and_restart() {
    for remote in [false, true] {
        let directory = TestDirectory::new();
        let root = directory.0.join("fixture");
        std::fs::create_dir_all(&root).unwrap();
        let audio = root.join("keep-on-disk.wav");
        std::fs::write(&audio, b"test source file").unwrap();
        let mut removed = library_tracks()[0].clone();
        removed.source_id = if remote { "remove" } else { "local" }.into();
        removed.item_id = if remote {
            "remote-song".into()
        } else {
            audio.to_string_lossy().into_owned()
        };
        removed.id = track_id(&removed.source_id, &removed.item_id);
        removed.location = if remote {
            MediaLocation::Provider
        } else {
            MediaLocation::Local(audio.clone())
        };
        removed.title = "Removed song".into();
        removed.album = Some("Removed album".into());
        removed.artist = Some("Removed artist".into());
        removed.album_artist = removed.artist.clone();
        let mut keep = removed.clone();
        keep.source_id = "keep".into();
        keep.item_id = "keep-song".into();
        keep.id = track_id("keep", "keep-song");
        keep.location = MediaLocation::Provider;
        keep.title = "Remaining song".into();
        keep.album = Some("Remaining album".into());
        keep.artist = Some("Remaining artist".into());
        keep.album_artist = keep.artist.clone();
        let mut library = cadence_library::Library::new();
        library.upsert_many(vec![removed, keep.clone()]).unwrap();
        if !remote {
            library.set_roots(vec![root.clone()]).unwrap();
        }
        library.save(&directory.0.join("library.json")).unwrap();
        let source = SourceConfig {
            id: "remove".into(),
            name: "Remove source test".into(),
            plugin_id: "kanade".into(),
            settings: serde_json::json!({"url":"https://kanade.invalid"}),
        };
        let config = persistence::AppConfig {
            sources: if remote { vec![source] } else { vec![] },
            ..Default::default()
        };
        let mut app = app();
        app.workspace = Workspace::persistent(
            directory.0.clone(),
            persistence::LoadedConfig {
                config,
                writable: true,
                error: None,
            },
        );
        wait_for(&app, |_| app.workspace.library.get().len() == 2);
        app.workspace.source_selection.set(Some(if remote {
            extension::ProviderId::Remote("remove".into())
        } else {
            extension::ProviderId::Local(root.clone())
        }));
        app.workspace.source_page.set(extension::SourcePage::Browse);
        app.workspace.tab.set(Tab::Sources);
        let size = Size::new(900., 568.);
        let mut tree = layout(&app, size);
        click(&mut tree, "Remove source");
        wait_for(&app, |_| app.workspace.library.get() == vec![keep.clone()]);
        assert!(app.workspace.roots.get().is_empty());
        assert!(app.workspace.sources.get().is_empty());
        assert_eq!(app.workspace.filtered.get(), vec![keep.clone()]);
        for tab in [Tab::Albums, Tab::Artists] {
            app.workspace.tab.set(tab);
            tree.clear_root();
            tree = layout(&app, size);
            let targets = all_targets(&tree);
            assert!(
                !targets
                    .iter()
                    .any(|c| ["Removed album", "Removed artist"].contains(&c.label.as_str()))
            );
            assert!(
                targets
                    .iter()
                    .any(|c| ["Remaining album", "Remaining artist"].contains(&c.label.as_str()))
            );
        }
        tree.clear_root();
        app.workspace.shutdown();
        assert_eq!(
            cadence_library::Library::load(&directory.0.join("library.json"))
                .unwrap()
                .tracks(),
            vec![keep]
        );
        assert!(persistence::load(&directory.0).config.sources.is_empty());
        assert_eq!(std::fs::read(&audio).unwrap(), b"test source file");
    }
}

#[test]
fn window_geometry_drives_content_size_without_titlebar_compensation() {
    use scarlet_ui::platform::WindowDecoration;
    for decoration in [
        WindowDecoration::SYSTEM,
        WindowDecoration::CUSTOM,
        WindowDecoration::NONE,
    ] {
        let app = app();
        let chrome =
            scarlet_ui::views::WindowContentLayout::for_decoration(decoration).decoration_size();
        let mut pipeline = scarlet_ui::RenderingPipeline::new();
        pipeline.set_root(
            app.window(Size::new(1280., 800.))
                .decoration(decoration)
                .create_element(),
        );
        pipeline.layout_initial();
        settle_render(&mut pipeline);
        assert_eq!(
            app.size.get(),
            Size::new(1280. - chrome.width, 800. - chrome.height)
        );
        pipeline.resize(Size::new(900., 650.));
        settle_render(&mut pipeline);
        assert_eq!(
            app.size.get(),
            Size::new(900. - chrome.width, 650. - chrome.height)
        );
        let builds = app.builds.root.get();
        for _ in 0..3 {
            pipeline.render_for_present().unwrap();
        }
        assert_eq!(
            app.builds.root.get(),
            builds,
            "unchanged geometry must not rebuild content"
        );
        pipeline.teardown();
    }
}

#[test]
fn playback_stays_centered_and_controls_do_not_overlap_across_compact_transition() {
    let app = app();
    populate(&app);
    for width in [320., 400., 449., 450., 500., 639., 640., 676., 900.] {
        let mut tree = ElementTree::new();
        tree.set_root(transport::view(&app, width).create_element());
        rebuild(&mut tree, Size::new(width, 64.));
        let controls = all_controls(&tree);
        let play = controls
            .iter()
            .find(|control| control.label == "icon:PlayerPlay")
            .unwrap();
        let play_center = play.origin.x + play.size.width / 2.;
        assert!(
            (play_center - width / 2.).abs() <= 0.5,
            "Play shifted off the bar center at {width}: {play:?}"
        );
        for (index, left) in controls.iter().enumerate() {
            for right in controls.iter().skip(index + 1) {
                let x = (left.origin.x + left.size.width).min(right.origin.x + right.size.width)
                    - left.origin.x.max(right.origin.x);
                let y = (left.origin.y + left.size.height).min(right.origin.y + right.size.height)
                    - left.origin.y.max(right.origin.y);
                assert!(
                    x <= 0.5 || y <= 0.5,
                    "Controls overlap at {width}: {left:?}, {right:?}"
                );
            }
        }
        tree.clear_root();
    }
}

#[test]
fn transport_stays_at_page_center_after_resize_queue_and_playback_changes() {
    let app = app();
    populate(&app);
    let mut tree = ElementTree::new();
    tree.set_root(app.create_element());
    // Exercise the retained application, including the sidebar and queue split,
    // rather than laying out only an isolated transport bar.
    for width in [1280., 900., 737., 1280., 1600., 900.] {
        for queue_visible in [true, false, true] {
            app.shell.queue_visible.set(queue_visible);
            for (state, repeat, seekable, title) in [
                (
                    PlaybackState::Playing,
                    RepeatMode::Off,
                    true,
                    "長い曲名でも中央の位置は変わらない",
                ),
                (PlaybackState::Paused, RepeatMode::One, false, "短い"),
                (PlaybackState::Playing, RepeatMode::All, true, "Song"),
            ] {
                {
                    let mut snapshot = app.playback.snapshot.borrow_mut();
                    snapshot.state = state;
                    snapshot.repeat = repeat;
                    snapshot.seekable = seekable;
                    snapshot.queue[0].title = title.into();
                }
                app.playback
                    .transport
                    .revision
                    .set(app.playback.transport.revision.get() + 1);
                app.size.set(Size::new(width, 650.));
                rebuild(&mut tree, Size::new(width, 650.));
                let controls = all_controls(&tree);
                let button = |label: &str| {
                    controls
                        .iter()
                        .find(|control| control.label == label && control.origin.y > 540.)
                        .unwrap_or_else(|| panic!("Missing transport {label}: {controls:?}"))
                };
                let center = |control: &Control| control.origin.x + control.size.width / 2.;
                let play = button(if state == PlaybackState::Playing {
                    "icon:PlayerPause"
                } else {
                    "icon:PlayerPlay"
                });
                let primary_width = width
                    - if queue_visible && width >= 1160. {
                        280.
                    } else {
                        0.
                    };
                let expected = 184. + (primary_width - 184.) / 2.;
                assert!(
                    (center(play) - expected).abs() <= 0.5,
                    "Transport moved at width={width}, queue={queue_visible}, repeat={repeat:?}: {play:?}, expected={expected}"
                );
                let previous = center(button("icon:ChevronLeft"));
                let next = center(button("icon:ChevronRight"));
                assert!(
                    ((previous + next) / 2. - expected).abs() <= 0.5,
                    "Previous/Next are not symmetric around Play"
                );
            }
        }
    }
    tree.clear_root();
}

#[test]
fn japanese_now_playing_labels_fit_their_column_after_resize() {
    let app = app();
    populate(&app);
    {
        let mut snapshot = app.playback.snapshot.borrow_mut();
        snapshot.queue[0].title = "脱落人生へようこそ".into();
        snapshot.queue[0].artist = Some("After the Rain".into());
        snapshot.queue[0].album = Some("アンチクロックワイズ - EP".into());
    }
    for width in [84., 110., 120., 150., 190., 240., 110.] {
        let mut tree = ElementTree::new();
        tree.set_root(transport::track_info(&app, width).create_element());
        rebuild(&mut tree, Size::new(width, 56.));
        let labels = all_targets(&tree);
        assert_eq!(labels.len(), 3);
        for label in &labels {
            assert!(label.origin.x >= 54. - 0.5);
            assert!(
                label.origin.x + label.size.width <= width + 0.5,
                "Label escaped the info column at width {width}: {label:?}"
            );
            assert!(!label.label.is_empty());
        }
        if width <= 150. {
            assert!(labels.iter().any(|label| label.label.ends_with('…')));
        }
        tree.clear_root();
    }
}

#[test]
fn automatic_queue_advance_paints_only_the_current_row() {
    fn highlights(element: &dyn Element, parent: Point, found: &mut Vec<Point>) {
        let origin = Point::new(
            parent.x + element.position().x,
            parent.y + element.position().y,
        );
        if let Some(render) = element.render_object()
            && render
                .as_any()
                .is::<scarlet_ui::views::BackgroundRenderObject>()
        {
            let mut paint = PaintContext::new();
            render.paint(&mut paint, origin);
            if paint.commands().iter().any(|command| {
                matches!(command,
                PaintCommand::FillPath { color, .. } if *color == selected_color())
            }) {
                found.push(origin);
            }
        }
        for child in element.children() {
            highlights(child.as_ref(), origin, found);
        }
    }
    let app = app();
    populate(&app);
    app.playback.queue.selected.set(Some(0));
    let mut tree = ElementTree::new();
    tree.set_root(
        presentation::QueueFrame {
            app: app.clone(),
            width: 900.,
            height: 650.,
        }
        .create_element(),
    );
    let mut first_y = None;
    for index in [0, 1, 2, 0] {
        // Automatic playback changes current; the old clicked selection may
        // still be present until presentation polling reconciles it.
        app.playback.queue.current.set(Some(index));
        rebuild(&mut tree, Size::new(900., 650.));
        let mut found = Vec::new();
        highlights(tree.root().unwrap(), Point::ZERO, &mut found);
        assert_eq!(
            found.len(),
            1,
            "old selection stayed highlighted: {found:?}"
        );
        let y = *first_y.get_or_insert(found[0].y);
        assert!((found[0].y - y - index as f32 * 52.).abs() <= 0.5);
    }
    tree.clear_root();
}

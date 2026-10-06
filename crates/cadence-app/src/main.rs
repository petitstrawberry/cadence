mod browser;
mod collection;
mod effect_pane;
mod extension;
mod jobs;
mod library_folders;
mod persistence;
mod playback;
mod presentation;
use presentation::*;
mod transport;
mod ui;

#[cfg(test)]
mod tests;

use cadence_core::{Command, Player, RepeatMode};
#[cfg(test)]
use cadence_core::{PlaybackState, Track};
use cadence_platform::folder_picker::{
    FolderPicker, FolderPickerError, FolderPickerOptions, FolderPickerOutcome,
};
use effect_pane::EffectPane;
use extension::{Tab, Workspace};
use scarlet_ui::{
    NavigationLink, NavigationView,
    event::KeyCode,
    file_dialog::{
        FileDialog, FileDialogError, FileDialogFilter, FileDialogFilterPolicy, FileDialogHandle,
        FileDialogMode, FileDialogOutcome,
    },
    hstack,
    prelude::*,
    vstack,
};
#[cfg(test)]
use std::time::Instant;
use std::{
    any::Any,
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    time::Duration,
};
use ui::*;

fn state<T: 'static>(value: T) -> State<T> {
    State::new(scarlet_ui::state::generate_state_id(), value)
}

#[derive(Clone)]
struct Cadence {
    playback: playback::Playback,
    size: State<Size>,
    focus: State<bool>,
    owner: Rc<Cell<Option<WindowId>>>,
    picker: Rc<RefCell<Option<FileDialogHandle>>>,
    folder_picker: Rc<RefCell<Option<FolderPicker>>>,
    browser: browser::BrowserState,
    workspace: Workspace,
    collections: collection::Collections,
    shell: ShellState,
    effects: Option<EffectPane>,
    startup_effects: Rc<RefCell<Option<cadence_effects::RackConfig>>>,
    effect_picker: Rc<Cell<bool>>,
    #[cfg(test)]
    builds: Rc<BuildCounts>,
}
impl Cadence {
    fn window(&self, size: Size) -> Window<Self> {
        Window::new("Cadence", self.clone())
            .size(size)
            .min_size(Size::new(737., 600.))
            .background_color(ColorPalette::default().background())
    }

    fn new(player: Player) -> Self {
        Self {
            playback: playback::Playback::new(player),
            size: state(Size::new(1280., 768.)),
            focus: state(true),
            owner: Rc::new(Cell::new(None)),
            picker: Rc::new(RefCell::new(None)),
            folder_picker: Rc::new(RefCell::new(None)),
            browser: browser::BrowserState::new(),
            workspace: Workspace::memory(),
            collections: collection::Collections::new(),
            shell: ShellState::new(),
            effects: None,
            startup_effects: Rc::new(RefCell::new(None)),
            effect_picker: Rc::new(Cell::new(false)),
            #[cfg(test)]
            builds: Rc::new(BuildCounts::default()),
        }
    }
    fn send(&self, command: Command) {
        if let Err(error) = self.playback.player.send(command) {
            self.playback.status.set(error);
        }
    }
    fn add(&self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        self.playback
            .status
            .set(format!("Added {} file(s) to the queue", paths.len()));
        self.send(Command::Add(paths));
    }
    fn open(&self) {
        if self.picker.borrow().is_some() || self.folder_picker.borrow().is_some() {
            return;
        }
        let Some(owner) = self.owner.get() else {
            self.open_browser();
            return;
        };
        // The SWS Files provider currently supports single file selection.
        let mode = if cfg!(target_os = "scarlet") {
            FileDialogMode::Open
        } else {
            FileDialogMode::OpenMultiple
        };
        let mut dialog = FileDialog::new(mode);
        dialog.title = "Add music to Cadence".into();
        dialog.filter_policy = FileDialogFilterPolicy::Optional;
        dialog.filters = vec![FileDialogFilter {
            name: "Audio files".into(),
            extensions: browser::EXTENSIONS.iter().map(|s| (*s).into()).collect(),
        }];
        let path = PathBuf::from(self.browser.directory.get());
        if path.is_absolute() {
            dialog.initial_directory = Some(path.into());
        }
        self.effect_picker.set(false);
        *self.picker.borrow_mut() = Some(dialog.show(owner));
    }
    fn open_effect(&self) {
        let Some(owner) = self.owner.get() else {
            self.playback
                .status
                .set("Enter a CLAP library path in Effects, then choose Discover".into());
            return;
        };
        if self.picker.borrow().is_some() || self.folder_picker.borrow().is_some() {
            return;
        }
        let mut dialog = FileDialog::new(FileDialogMode::Open);
        dialog.title = "Choose a CLAP effect".into();
        dialog.filter_policy = FileDialogFilterPolicy::Optional;
        dialog.filters = vec![FileDialogFilter {
            name: "CLAP effects".into(),
            extensions: vec!["clap".into()],
        }];
        self.effect_picker.set(true);
        *self.picker.borrow_mut() = Some(dialog.show(owner));
    }
    fn import_folder(&self) {
        if self.picker.borrow().is_some() || self.folder_picker.borrow().is_some() {
            return;
        }
        if self.owner.get().is_none() {
            self.open_browser();
            return;
        }
        let initial = PathBuf::from(self.browser.directory.get());
        let options = FolderPickerOptions {
            title: "Add a music folder to Cadence".into(),
            initial_directory: initial.is_absolute().then_some(initial),
        };
        match FolderPicker::show(options) {
            Ok(picker) => *self.folder_picker.borrow_mut() = Some(picker),
            Err(FolderPickerError::Unsupported) => self.open_browser(),
            Err(error) => self
                .playback
                .status
                .set(format!("Could not open the folder picker: {error}")),
        }
    }
    fn open_browser(&self) {
        self.browser.visible.set(true);
        self.read_directory(PathBuf::from(self.browser.directory.get()));
    }
    fn read_directory(&self, path: PathBuf) {
        self.browser.entries.set(vec![]);
        self.browser.selection.set(None);
        self.browser.status.set("Reading folder…".into());
        *self.browser.listing.borrow_mut() = Some(browser::read(path));
    }
    fn repeat(&self) {
        let mode = match self.playback.snapshot.borrow().repeat {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::One,
            RepeatMode::One => RepeatMode::Off,
        };
        self.send(Command::SetRepeat(mode));
    }
    fn poll(&self) {
        if let Some(effects) = &self.effects
            && let Some(config) = effects.poll()
        {
            self.workspace.save_effects(config);
        }
        self.workspace.poll(self);
        self.collections.poll(&self.workspace);
        let folder_result = self
            .folder_picker
            .borrow_mut()
            .as_mut()
            .and_then(FolderPicker::take_result);
        if let Some(result) = folder_result {
            self.folder_picker.borrow_mut().take();
            match result {
                Ok(FolderPickerOutcome::Selected(path)) => {
                    self.browser.directory.set(path.to_string_lossy().into());
                    self.workspace.add_folder_source(self, path);
                    self.workspace.tab.set(Tab::Sources);
                    self.browser.visible.set(false);
                }
                Ok(FolderPickerOutcome::Cancelled) => {}
                Err(error) => self
                    .playback
                    .status
                    .set(format!("Could not select the folder: {error}")),
            }
        }
        let result = self.picker.borrow().as_ref().and_then(|p| p.take_result());
        if let Some(result) = result {
            self.picker.borrow_mut().take();
            match result {
                Ok(FileDialogOutcome::Selected(paths)) => {
                    if self.effect_picker.replace(false) {
                        if let Some(effects) = &self.effects
                            && let Some(path) = paths.first()
                        {
                            effects
                                .path
                                .set(PathBuf::from(path.clone()).to_string_lossy().into());
                        }
                    } else {
                        self.add(paths.into_iter().map(PathBuf::from).collect());
                    }
                }
                Ok(FileDialogOutcome::Cancelled) => {}
                Err(FileDialogError::Unsupported(_)) => {
                    if self.effect_picker.replace(false) {
                        self.playback.status.set(
                            "Enter a CLAP library path in Effects, then choose Discover".into(),
                        );
                    } else {
                        self.open_browser();
                    }
                }
                Err(error) => self
                    .playback
                    .status
                    .set(format!("Could not open the file picker: {error}")),
                _ => {}
            }
        }
        let listing = self
            .browser
            .listing
            .borrow()
            .as_ref()
            .and_then(|r| r.try_recv().ok());
        if let Some(result) = listing {
            self.browser.listing.borrow_mut().take();
            match result {
                Ok((path, entries)) => {
                    self.browser.directory.set(path.to_string_lossy().into());
                    self.browser.status.set(format!(
                        "{} audio file(s) · click a file to select it",
                        entries.iter().filter(|e| !e.directory).count()
                    ));
                    self.browser.entries.set(entries);
                }
                Err(error) => self
                    .browser
                    .status
                    .set(format!("Could not read this folder: {error}")),
            }
        }
        self.playback.poll();
    }
    fn artwork(&self, side: f32) -> AnyView {
        if let Some(bitmap) = self.workspace.artwork.borrow().as_ref() {
            return AnyView::new(Image::from_bitmap(bitmap.clone()).frame(side, side));
        }
        AnyView::new(Surface::section(
            IconView::new(Icon::Music)
                .size(IconSize::Pixels((side * 0.45) as u16))
                .color(ColorPalette::default().text_secondary())
                .frame(side * 0.45, side * 0.45)
                .alignment(Alignment::Center)
                .frame(side, side),
        ))
    }
    fn track_info(&self, width: f32) -> AnyView {
        transport::track_info(self, width)
    }
    fn queue_view(&self, width: f32, height: f32) -> AnyView {
        #[cfg(test)]
        self.builds.queue.set(self.builds.queue.get() + 1);
        let clear = self.clone();
        let add = self.clone();
        let count = self.playback.queue.tracks.get().len();
        let inner = width - PAGE_PADDING * 2.;
        let album_width = ((inner - 152.) * 0.3).max(80.);
        let title_width = inner - album_width - 152.;
        let app = self.clone();
        let body_height = height - LIBRARY_HEADER_HEIGHT - if count == 0 { 0. } else { 28. };
        let content = if count == 0 {
            AnyView::new(vstack! {
                Spacer::new(),
                IconView::new(Icon::List).size(IconSize::Large).color(ColorPalette::default().text_secondary()),
                text("Your queue is empty").font_size(18.),
                caption("Play a song from your library to start listening."),
                Spacer::new(),
            }.spacing(12.).frame(width, (body_height - PLAYER_CLEARANCE).max(0.)))
        } else {
            AnyView::new(ListView::new(self.playback.queue.tracks.clone(), self.playback.queue.selected.clone(), 52., move |index, track, selected| {
                let choose = app.clone();
                let remove = app.clone();
                let active = app.playback.queue.current.get() == Some(index);
                hstack! {
                    hstack! {
                        queue_marker(index, active, 28., 44.),
                        vstack! {
                            text(truncate(&track.title,(title_width / 7.).max(8.) as usize)).font_size(14.)
                                .color(if active { ColorPalette::default().primary() } else { ColorPalette::default().text() }),
                            caption(truncate(track.artist.as_deref().unwrap_or("Unknown artist"),(title_width / 6.5).max(8.) as usize)),
                        }.spacing(3.).alignment(Alignment::Leading).frame_width(title_width),
                        caption(truncate(track.album.as_deref().unwrap_or("—"),(album_width / 6.5).max(6.) as usize))
                            .alignment(Alignment::Leading).frame(album_width,44.),
                        caption(track.duration.map(clock).unwrap_or_else(|| "—:——".into()))
                            .alignment(Alignment::Trailing).frame(48.,44.),
                    }.spacing(12.).on_click(move || {
                        choose.playback.select_queue_row(index);
                    }),
                    Button::icon_only(Icon::X).header_style().icon_size(IconSize::Small)
                        .on_click(move || remove.send(Command::Remove(index))).frame(28.,28.),
                }.spacing(12.).padding_insets(EdgeInsets::horizontal(PAGE_PADDING)).frame(width,52.)
                    .background(if selected == Some(index) || active { selected_color() } else { ColorPalette::default().background() })
            }).content_bottom_inset(PLAYER_CLEARANCE).frame(width,body_height))
        };
        let clear_action = if count == 0 {
            AnyView::new(Spacer::new().frame(0., 32.))
        } else {
            AnyView::new(
                button("Clear")
                    .header_style()
                    .on_click(move || clear.send(Command::Clear))
                    .frame(64., 32.),
            )
        };
        let columns = if count == 0 {
            AnyView::new(Spacer::new().frame(width, 0.))
        } else {
            AnyView::new(
                hstack! {
                    caption("#").alignment(Alignment::Center).frame(28.,28.),
                    caption("Song").alignment(Alignment::Leading).frame(title_width,28.),
                    caption("Album").alignment(Alignment::Leading).frame(album_width,28.),
                    caption("Time").alignment(Alignment::Trailing).frame(48.,28.),
                    Spacer::new().frame(28.,28.),
                }
                .spacing(12.)
                .padding_insets(EdgeInsets::horizontal(PAGE_PADDING))
                .frame(width, 28.),
            )
        };
        AnyView::new(Surface::new(vstack! {
            header(hstack! {
                header_title("Play queue"), caption(counted(count,"song")), Spacer::new(),
                button("Add files…").header_style().icon(Icon::Plus).on_click(move || add.open()).frame(116.,32.),
                clear_action,
            }.spacing(12.),width),
            columns,
            content,
        }.spacing(0.).alignment(Alignment::TopLeading),SurfaceRole::Canvas).frame(width,height))
    }
    fn transport(&self, width: f32) -> AnyView {
        #[cfg(test)]
        self.builds.transport.set(self.builds.transport.get() + 1);
        transport::view(self, width)
    }
    fn browser_view(&self, width: f32, height: f32) -> AnyView {
        let parent = self.clone();
        let go = self.clone();
        let folder = self.clone();
        let import = self.clone();
        let file = self.clone();
        let close = self.clone();
        let app = self.clone();
        AnyView::new(vstack! {
            header(hstack! {
                header_title("Browse files"), Spacer::new(),
                button("Done").header_style().on_click(move || close.browser.visible.set(false)).frame(64., 28.),
            }, width),
            hstack! {
                Button::icon_only(Icon::ArrowUp).header_style().on_click(move || { let path = PathBuf::from(parent.browser.directory.get()); if let Some(path) = path.parent() { parent.read_directory(path.into()); } }).frame(30., 30.),
                TextField::new(self.browser.directory.clone()).font_size(13.).on_submit(move || go.read_directory(PathBuf::from(go.browser.directory.get()))).on_key(|_| true).frame(width - 76., 30.),
            }.spacing(8.).padding_insets(EdgeInsets::new(16., 0., 16., 0.)),
            ListView::new(self.browser.entries.clone(), self.browser.selection.clone(), 32., move |index, entry, selected| {
                let app = app.clone();
                hstack! {
                    IconView::new(if entry.directory { Icon::Folder } else { Icon::FileMusic }).size(IconSize::Small),
                    text(truncate(&entry.name, ((width - 108.) / 7.) as usize)), Spacer::new(),
                    caption(if entry.directory { "Folder" } else { "Audio" }).alignment(Alignment::Trailing).frame(56., 28.),
                }.spacing(8.).padding_insets(EdgeInsets::new(12., 0., 12., 0.)).frame(width - 44., 32.)
                    .background(if selected == Some(index) { selected_color() } else { ColorPalette::default().background() })
                    .on_click(move || if entry.directory { app.read_directory(entry.path.clone()); } else { app.browser.selection.set(Some(index)); })
            }).frame(width - 32., height - 160.).padding_insets(EdgeInsets::new(16., 0., 16., 0.)),
            Text::from_state(self.browser.status.clone()).font_size(11.).color(ColorPalette::default().text_secondary()).alignment(Alignment::Leading).padding_insets(EdgeInsets::new(16., 0., 16., 0.)).frame(width, 20.),
            HeaderBar::new(hstack! {
                button("Add folder to queue").on_click(move || { folder.add(folder.browser.entries.get().into_iter().filter(|e| !e.directory).map(|e| e.path).collect()); folder.browser.visible.set(false); }).frame(154., 28.),
                button("Import to library").on_click(move || { import.workspace.add_folder_source(&import, PathBuf::from(import.browser.directory.get())); import.workspace.tab.set(Tab::Sources); import.browser.visible.set(false); }).frame(142., 28.),
                Spacer::new(),
                button("Add selected").on_click(move || if let Some(entry) = file.browser.selection.get().and_then(|i| file.browser.entries.get().get(i).cloned()) && !entry.directory { file.add(vec![entry.path]); file.browser.visible.set(false); }).frame(116., 28.),
            }.spacing(8.).padding_insets(EdgeInsets::new(16., 0., 16., 0.))).height(38.),
        }.spacing(6.).frame(width, height))
    }
    fn body(&self) -> AnyView {
        #[cfg(test)]
        self.builds.root.set(self.builds.root.get() + 1);
        let size = self.size.get();
        let key = self.clone();
        AnyView::new(vstack! {
            NavigationFrame { app: self.clone(), width: size.width, height: size.height - 22. }.frame(size.width, size.height - 22.),
            StatusFrame { app: self.clone(), width: size.width }.repaint_boundary().frame(size.width, 22.),
        }.spacing(0.).frame(size.width, size.height).background(ColorPalette::default().background())
            .focusable(self.focus.clone()).on_key(move |event| key.handle_key(event)))
    }
    fn handle_key(&self, event: KeyEvent) -> bool {
        if self.picker.borrow().is_some() || self.folder_picker.borrow().is_some() {
            return false;
        }
        let KeyEvent::Pressed { keycode, modifiers } = event else {
            return false;
        };
        if self.browser.visible.get() {
            if keycode == KeyCode::Escape {
                self.browser.visible.set(false);
                return true;
            }
            return false;
        }
        if modifiers.control || modifiers.super_key {
            if matches!(keycode, KeyCode::Char('o' | 'O')) {
                self.open();
                return true;
            }
            return false;
        }
        match keycode {
            KeyCode::Enter => match self.workspace.tab.get() {
                Tab::Library => self.selected_library(true),
                Tab::Playing => {
                    if let Some(i) = self.playback.queue.selected.get() {
                        self.send(Command::Select(i));
                    }
                }
                _ => return false,
            },
            KeyCode::Space => self.send(Command::Toggle),
            KeyCode::Right => self.send(Command::Seek(
                self.playback
                    .snapshot
                    .borrow()
                    .position
                    .saturating_add(Duration::from_secs(5)),
            )),
            KeyCode::Left => self.send(Command::Seek(
                self.playback
                    .snapshot
                    .borrow()
                    .position
                    .saturating_sub(Duration::from_secs(5)),
            )),
            _ => return false,
        }
        true
    }
}
impl View for Cadence {
    fn create_element(&self) -> Box<dyn Element> {
        Box::new(scarlet_ui::ComponentElement::new_with_builder(
            self.clone(),
            |app| {
                let content_size = app.size.clone();
                Box::new(
                    app.body()
                        .frame(f32::INFINITY, f32::INFINITY)
                        .on_geometry_change(
                            |geometry| geometry.size(),
                            move |size| {
                                if content_size.get() != size {
                                    content_size.set(size);
                                }
                            },
                        ),
                )
            },
        ))
    }
    fn listenables(&self) -> Vec<&dyn Listenable> {
        vec![&self.size]
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
impl Application for Cadence {
    fn scenes(&self) -> impl Scene {
        WindowGroup::new("cadence", self.window(Size::new(1280., 800.)))
    }
    fn scene_listenables(&self, _: &scarlet_ui::SceneWindowKey) -> Option<Vec<&dyn Listenable>> {
        Some(vec![])
    }
    fn on_window_created(
        &mut self,
        context: &WindowContext,
        _: &mut dyn scarlet_ui::PlatformWindow,
    ) {
        self.owner.set(Some(context.window_id));
    }
    fn on_idle(&mut self) {
        // The runner pumps AppKit on the main thread before its first idle.
        // Some native CLAP libraries initialize AppKit in entry.init: loading
        // them before that pump can bind its event queue to the audio worker.
        if let Some(config) = self.startup_effects.borrow_mut().take()
            && let Some(effects) = &self.effects
            && let Err(error) = effects.controller.restore(config)
        {
            effects.status.set(error);
        }
        if self.workspace.tab.get() == Tab::Effects
            && let Some(effects) = &self.effects
        {
            effects.ensure_catalog();
        }
        self.poll();
    }
    fn on_window_close_requested(&mut self, _: &WindowContext) -> bool {
        if let Some(picker) = self.picker.borrow_mut().take() {
            picker.cancel();
        }
        if let Some(mut picker) = self.folder_picker.borrow_mut().take() {
            picker.cancel();
        }
        self.workspace.shutdown();
        self.send(Command::Pause);
        true
    }
    fn on_render_error(&mut self, _: &WindowContext, error: &scarlet_ui::RenderFailure) {
        eprintln!("[Cadence UI] {error}");
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let mut paths = Vec::new();
    for argument in std::env::args_os().skip(1) {
        if argument == "--help" || argument == "-h" {
            println!(
                "Cadence — ScarletUI music player\n\nUsage: cadence [AUDIO_FILE ...]\n\nFiles are added to the queue. Press Play to listen.\nSpace: play/pause · Left/Right: seek 5 seconds · Ctrl/Cmd+O: add music"
            );
            return Ok(());
        }
        paths.push(PathBuf::from(argument));
    }
    let directory = persistence::data_directory();
    let loaded = persistence::load(&directory);
    let (controller, factory) = cadence_effects::EffectRack::new(Default::default());
    let player = Player::with_processor_factories(
        cadence_platform::output_factory(),
        cadence_core::DecoderRegistry::default(),
        vec![factory],
    )
    .map_err(std::io::Error::other)?;
    let mut app = Cadence::new(player);
    app.effects = Some(EffectPane::with_saved_config(
        controller,
        loaded.config.effects.clone(),
    ));
    *app.startup_effects.borrow_mut() = Some(loaded.config.effects.clone());
    app.workspace = Workspace::persistent(directory, loaded);
    app.send(Command::SetVolume(0.8));
    app.add(paths);
    app.run()
        .map_err(|error| format!("ScarletUI: {error:?}").into())
}

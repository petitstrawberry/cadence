//! Independent presentation boundaries. None forwards descendant listenables.
use super::*;

#[cfg(test)]
#[derive(Default)]
pub(crate) struct BuildCounts {
    pub root: Cell<u64>,
    pub navigation: Cell<u64>,
    pub main_column: Cell<u64>,
    pub queue: Cell<u64>,
    pub transport: Cell<u64>,
    pub track_info: Cell<u64>,
}

#[derive(Clone)]
pub(crate) struct ShellState {
    pub(super) queue_visible: State<bool>,
    primary_size: State<Size>,
    queue_size: State<Size>,
}
impl ShellState {
    pub fn new() -> Self {
        Self {
            queue_visible: state(true),
            primary_size: state(Size::new(1000., 746.)),
            queue_size: state(Size::new(280., 746.)),
        }
    }
}
#[derive(Clone)]
pub(crate) struct NavigationFrame {
    pub app: Cadence,
    pub width: f32,
    pub height: f32,
}
impl NavigationFrame {
    fn body(&self) -> AnyView {
        #[cfg(test)]
        self.app
            .builds
            .navigation
            .set(self.app.builds.navigation.get() + 1);
        if self.app.shell.queue_visible.get() && self.width >= 1160. {
            let first = self.app.shell.primary_size.clone();
            let second = self.app.shell.queue_size.clone();
            AnyView::new(
                SplitView::new(
                    MainColumn {
                        app: self.app.clone(),
                    }
                    .on_geometry_change(
                        |g| g.size(),
                        move |size| {
                            if first.get() != size {
                                first.set(size);
                            }
                        },
                    ),
                    QueueSidebar {
                        app: self.app.clone(),
                    }
                    .repaint_boundary()
                    .on_geometry_change(
                        |g| g.size(),
                        move |size| {
                            if second.get() != size {
                                second.set(size);
                            }
                        },
                    ),
                )
                .fraction((self.width - 280.) / self.width)
                .min_first(880.)
                .min_second(220.)
                .frame(self.width, self.height),
            )
        } else {
            let size = Size::new(self.width, self.height);
            if self.app.shell.primary_size.get() != size {
                self.app.shell.primary_size.set(size);
            }
            AnyView::new(
                MainColumn {
                    app: self.app.clone(),
                }
                .frame(self.width, self.height),
            )
        }
    }
}
impl View for NavigationFrame {
    fn create_element(&self) -> Box<dyn Element> {
        Box::new(scarlet_ui::ComponentElement::new_with_builder(
            self.clone(),
            |v| Box::new(v.body()),
        ))
    }
    fn listenables(&self) -> Vec<&dyn Listenable> {
        vec![&self.app.shell.queue_visible]
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
#[derive(Clone)]
struct MainColumn {
    app: Cadence,
}
impl MainColumn {
    fn page(&self, tab: Tab) -> AnyView {
        let size = self.app.shell.primary_size.get();
        let width = (size.width - 184.).max(500.);
        let height = size.height;
        let content = if self.app.browser.visible.get() {
            AnyView::new(BrowserFrame {
                app: self.app.clone(),
                width,
                height: height - PLAYER_CLEARANCE,
            })
        } else {
            match tab {
                Tab::Albums => self.app.collections.view(
                    &self.app,
                    collection::CollectionKind::Albums,
                    width,
                    height,
                ),
                Tab::Artists => self.app.collections.view(
                    &self.app,
                    collection::CollectionKind::Artists,
                    width,
                    height,
                ),
                Tab::Playing => AnyView::new(QueueFrame {
                    app: self.app.clone(),
                    width,
                    height,
                }),
                Tab::Library => self.app.workspace.library_view(&self.app, width, height),
                Tab::Sources => self.app.workspace.sources_view(&self.app, width, height),
                Tab::Effects => {
                    if let Some(effects) = &self.app.effects {
                        let app = self.app.clone();
                        effects.view(
                            width,
                            height - PLAYER_CLEARANCE,
                            Rc::new(move || app.open_effect()),
                        )
                    } else {
                        AnyView::new(caption("Effects are unavailable").frame(width, height))
                    }
                }
            }
        };
        AnyView::new(
            scarlet_ui::zstack! {
                content.repaint_boundary().alignment(Alignment::Top).frame(width,height),
                TransportFrame { app: self.app.clone(), width: player_width(width) }.repaint_boundary()
                    .frame(player_width(width),64.).padding_insets(EdgeInsets::new(0.,0.,0.,12.)),
            }
            .alignment(Alignment::Bottom)
            .frame(width, size.height),
        )
    }
    fn link(&self, title: &str, icon: Icon, tab: Tab) -> NavigationLink {
        let page = self.clone();
        let select = self.app.clone();
        NavigationLink::new(title, move || page.page(tab))
            .icon(icon)
            .on_select(move || {
                select.collections.return_to_root();
                if tab == Tab::Sources {
                    select
                        .workspace
                        .source_page
                        .set(extension::SourcePage::Overview);
                }
                select.workspace.tab.set(tab);
                select.browser.visible.set(false);
            })
    }
    fn body(&self) -> AnyView {
        #[cfg(test)]
        self.app
            .builds
            .main_column
            .set(self.app.builds.main_column.get() + 1);
        let nav = NavigationView::new((
            self.link("Albums", Icon::Photo, Tab::Albums),
            self.link("Artists", Icon::Microphone, Tab::Artists),
            self.link("All songs", Icon::FileMusic, Tab::Library),
            self.link("Play queue", Icon::List, Tab::Playing),
            self.link("Sources", Icon::Folder, Tab::Sources),
            self.link("Effects", Icon::Adjustments, Tab::Effects),
        ))
        .sidebar_width(184.)
        .shows_icons(true)
        .presentation(NavigationPresentation::Sidebar);
        let index = match self.app.workspace.tab.get() {
            Tab::Albums => 0,
            Tab::Artists => 1,
            Tab::Library => 2,
            Tab::Playing => 3,
            Tab::Sources => 4,
            Tab::Effects => 5,
        };
        nav.selected_index_state().set(index);
        AnyView::new(nav)
    }
}
impl View for MainColumn {
    fn create_element(&self) -> Box<dyn Element> {
        Box::new(scarlet_ui::ComponentElement::new_with_builder(
            self.clone(),
            |v| Box::new(v.body()),
        ))
    }
    fn listenables(&self) -> Vec<&dyn Listenable> {
        vec![
            &self.app.workspace.tab,
            &self.app.browser.visible,
            &self.app.shell.primary_size,
        ]
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
#[derive(Clone)]
struct QueueSidebar {
    app: Cadence,
}
impl QueueSidebar {
    fn body(&self) -> AnyView {
        let size = self.app.shell.queue_size.get();
        let app = self.app.clone();
        let close = self.app.clone();
        let current = self.app.playback.queue.current.clone();
        let count = self.app.playback.queue.tracks.get().len();
        AnyView::new(vstack! {
            header(hstack! {
                header_title("Queue"), caption(format!("{count}")), Spacer::new(),
                Button::icon_only(Icon::List).header_style().on_click(move || close.shell.queue_visible.set(false)).frame(28.,28.),
            }.spacing(12.), size.width),
            ListView::new(self.app.playback.queue.tracks.clone(),current,48.,move |index,track,playing| {
                let play=app.clone(); let width=size.width-24.;
                hstack! {
                    queue_marker(index, playing==Some(index), 24., 40.),
                    vstack! {
                        text(truncate(&track.title,((width-80.)/7.)as usize)).color(if playing==Some(index) {ColorPalette::default().primary()} else {ColorPalette::default().text()}),
                        caption(truncate(track.artist.as_deref().unwrap_or("Unknown artist"),((width-80.)/6.)as usize)).font_size(11.),
                    }.spacing(3.).alignment(Alignment::Leading).frame_width(width-80.),
                    caption(track.duration.map(clock).unwrap_or_else(||"—:——".into())).font_size(10.).alignment(Alignment::Trailing).frame(40.,40.),
                }.spacing(4.).padding_insets(EdgeInsets::new(6.,2.,6.,2.)).frame(size.width-12.,48.)
                    .background(if playing==Some(index) {selected_color()}else{ColorPalette::default().background()})
                    .on_click(move || play.send(Command::Select(index)))
            }).frame(size.width,size.height-LIBRARY_HEADER_HEIGHT),
        }.spacing(0.).frame(size.width,size.height).background(ColorPalette::default().background()))
    }
}
impl View for QueueSidebar {
    fn create_element(&self) -> Box<dyn Element> {
        Box::new(scarlet_ui::ComponentElement::new_with_builder(
            self.clone(),
            |v| Box::new(v.body()),
        ))
    }
    fn listenables(&self) -> Vec<&dyn Listenable> {
        vec![&self.app.shell.queue_size, &self.app.playback.queue.tracks]
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Clone)]
pub(crate) struct QueueFrame {
    pub app: Cadence,
    pub width: f32,
    pub height: f32,
}
impl View for QueueFrame {
    fn create_element(&self) -> Box<dyn Element> {
        Box::new(scarlet_ui::ComponentElement::new_with_builder(
            self.clone(),
            |v| Box::new(v.app.queue_view(v.width, v.height)),
        ))
    }
    fn listenables(&self) -> Vec<&dyn Listenable> {
        vec![
            &self.app.playback.queue.tracks,
            &self.app.playback.queue.current,
        ]
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
#[derive(Clone)]
pub(crate) struct BrowserFrame {
    pub app: Cadence,
    pub width: f32,
    pub height: f32,
}
impl View for BrowserFrame {
    fn create_element(&self) -> Box<dyn Element> {
        Box::new(scarlet_ui::ComponentElement::new_with_builder(
            self.clone(),
            |v| Box::new(v.app.browser_view(v.width, v.height)),
        ))
    }
    fn listenables(&self) -> Vec<&dyn Listenable> {
        vec![]
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
#[derive(Clone)]
pub(crate) struct TransportFrame {
    pub app: Cadence,
    pub width: f32,
}
impl View for TransportFrame {
    fn create_element(&self) -> Box<dyn Element> {
        Box::new(scarlet_ui::ComponentElement::new_with_builder(
            self.clone(),
            |v| Box::new(v.app.transport(v.width)),
        ))
    }
    fn listenables(&self) -> Vec<&dyn Listenable> {
        vec![&self.app.playback.transport.revision]
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
#[derive(Clone)]
pub(crate) struct TrackInfoFrame {
    pub app: Cadence,
    pub width: f32,
}
impl View for TrackInfoFrame {
    fn create_element(&self) -> Box<dyn Element> {
        Box::new(scarlet_ui::ComponentElement::new_with_builder(
            self.clone(),
            |v| {
                #[cfg(test)]
                v.app
                    .builds
                    .track_info
                    .set(v.app.builds.track_info.get() + 1);
                Box::new(v.app.track_info(v.width))
            },
        ))
    }
    fn listenables(&self) -> Vec<&dyn Listenable> {
        vec![
            &self.app.playback.queue.tracks,
            &self.app.playback.queue.current,
            &self.app.workspace.artwork_revision,
        ]
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
#[derive(Clone)]
pub(crate) struct StatusFrame {
    pub app: Cadence,
    pub width: f32,
}
impl View for StatusFrame {
    fn create_element(&self) -> Box<dyn Element> {
        Box::new(scarlet_ui::ComponentElement::new_with_builder(
            self.clone(),
            |v| {
                let warning = v.app.workspace.warning.get();
                let message = if warning.is_empty() {
                    v.app.playback.status.get()
                } else {
                    warning
                };
                Box::new(HeaderBar::new(hstack! {
            Text::new(truncate(&message, ((v.width - 240.) / 6.) as usize)).font_size(11.).color(ColorPalette::default().text_secondary()),
            Spacer::new(), caption("Space: play / pause   ·   ← →: seek").font_size(10.),
        }.padding_insets(EdgeInsets::new(12.,0.,12.,0.))).height(22.))
            },
        ))
    }
    fn listenables(&self) -> Vec<&dyn Listenable> {
        vec![&self.app.playback.status, &self.app.workspace.warning]
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

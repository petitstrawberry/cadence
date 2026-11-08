//! Independent presentation boundaries. None forwards descendant listenables.
use super::*;

const SIDEBAR_WIDTH: f32 = 168.;

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
    body_size: State<Size>,
}
impl ShellState {
    pub fn new() -> Self {
        Self {
            queue_visible: state(true),
            primary_size: state(Size::new(1000., 746.)),
            queue_size: state(Size::new(280., 746.)),
            body_size: state(Size::new(500., 500.)),
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
                    // Measure the allocated pane, not the page's previous
                    // fixed height, so growing the window can update it.
                    .frame(f32::INFINITY, f32::INFINITY)
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
    fn page(&self, tab: Tab, region: PageRegion) -> AnyView {
        let size = self.app.shell.primary_size.get();
        let available = self.app.shell.body_size.get();
        let width = available.width;
        let height = available.height;
        let content = if self.app.browser.visible.get() {
            AnyView::new(BrowserFrame {
                region,
                app: self.app.clone(),
                width,
                height,
            })
        } else {
            match tab {
                Tab::Albums => self.app.collections.view(
                    &self.app,
                    collection::CollectionKind::Albums,
                    width,
                    height,
                    region,
                ),
                Tab::Artists => self.app.collections.view(
                    &self.app,
                    collection::CollectionKind::Artists,
                    width,
                    height,
                    region,
                ),
                Tab::Playing => AnyView::new(QueueFrame {
                    region,
                    app: self.app.clone(),
                    width,
                    height,
                }),
                Tab::Library => self
                    .app
                    .workspace
                    .library_view(&self.app, width, height, region),
                Tab::Sources => self
                    .app
                    .workspace
                    .sources_view(&self.app, width, height, region),
                Tab::Effects => {
                    if let Some(effects) = &self.app.effects {
                        let app = self.app.clone();
                        effects.view(width, height, Rc::new(move || app.open_effect()), region)
                    } else {
                        page_in(
                            region,
                            header(header_title("Effects"), width),
                            AnyView::new(caption("Effects are unavailable")),
                            height,
                            false,
                        )
                    }
                }
            }
        };
        if region == PageRegion::Header {
            return content;
        }
        let player_width = player_width(width);
        let body_size = self.app.shell.body_size.clone();
        AnyView::new(Surface::new(
            scarlet_ui::zstack! {
                main_content(AnyView::new(content.on_geometry_change(
                    |g| { let s = g.size(); let i = g.safe_area_insets();
                        Size::new((s.width-i.left-i.right).max(0.), (s.height-i.top-i.bottom).max(0.)) },
                    move |s| body_size.set(s),
                ))).repaint_boundary().alignment(Alignment::Top).frame(f32::INFINITY,size.height),
                TransportFrame { app: self.app.clone(), width: player_width }.repaint_boundary()
                    .frame(player_width,PLAYER_HEIGHT).padding_insets(EdgeInsets::only_bottom(PLAYER_BOTTOM_MARGIN))
                    .safe_area_padding(SafeAreaEdges::LEFT | SafeAreaEdges::RIGHT),
            }
            .alignment(Alignment::Bottom)
            .frame(f32::INFINITY, size.height),
            SurfaceRole::Canvas,
        ))
    }
    fn link(&self, title: &str, icon: Icon, tab: Tab) -> NavigationLink {
        let page = self.clone();
        let select = self.app.clone();
        let heading = self.clone();
        NavigationLink::new(title, move || NavigationPage {
            column: page.clone(),
            tab,
            region: PageRegion::Content,
        })
        .custom_header(
            NavigationHeader::new(move || NavigationPage {
                column: heading.clone(),
                tab,
                region: PageRegion::Header,
            })
            .automatic_height(),
        )
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
        .sidebar_width(SIDEBAR_WIDTH)
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
// Geometry changes rebuild only the selected page regions, never the outer
// navigation. The measured safe size includes live header/player reservations.
#[derive(Clone)]
struct NavigationPage {
    column: MainColumn,
    tab: Tab,
    region: PageRegion,
}
impl View for NavigationPage {
    fn create_element(&self) -> Box<dyn Element> {
        Box::new(scarlet_ui::ComponentElement::new_with_builder(
            self.clone(),
            |v| Box::new(v.column.page(v.tab, v.region)),
        ))
    }
    fn listenables(&self) -> Vec<&dyn Listenable> {
        vec![&self.column.app.shell.body_size]
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
        page(
            header(hstack! {
                header_title("Queue"), caption(format!("{count}")), Spacer::new(),
                Button::icon_only(Icon::List).header_style().on_click(move || close.shell.queue_visible.set(false)).frame(28.,28.),
            }.spacing(12.), size.width),
            AnyView::new(ListView::new(self.app.playback.queue.tracks.clone(),current,48.,move |index,track,playing| {
                let play=app.clone(); let width=content_width(size.width)-24.;
                hstack! {
                    queue_marker(index, playing==Some(index), 24., 40.),
                    vstack! {
                        text(truncate(&track.title,((width-80.)/7.)as usize)).color(if playing==Some(index) {ColorPalette::default().primary()} else {ColorPalette::default().text()}),
                        caption(truncate(track.artist.as_deref().unwrap_or("Unknown artist"),((width-80.)/6.)as usize)).font_size(11.),
                    }.spacing(3.).alignment(Alignment::Leading).frame_width(width-80.),
                    caption(track.duration.map(clock).unwrap_or_else(||"—:——".into())).font_size(10.).alignment(Alignment::Trailing).frame(40.,40.),
                }.spacing(4.).padding_insets(EdgeInsets::new(6.,2.,6.,2.)).frame(content_width(size.width)-12.,48.)
                    .background(if playing==Some(index) {selected_color()}else{ColorPalette::default().background()})
                    .on_click(move || play.send(Command::Select(index)))
            }).frame(f32::INFINITY,f32::INFINITY)),
            size.height, true)
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
    pub region: PageRegion,
    pub app: Cadence,
    pub width: f32,
    pub height: f32,
}
impl View for QueueFrame {
    fn create_element(&self) -> Box<dyn Element> {
        Box::new(scarlet_ui::ComponentElement::new_with_builder(
            self.clone(),
            |v| Box::new(v.app.queue_view(v.width, v.height, v.region)),
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
    pub region: PageRegion,
    pub app: Cadence,
    pub width: f32,
    pub height: f32,
}
impl View for BrowserFrame {
    fn create_element(&self) -> Box<dyn Element> {
        Box::new(scarlet_ui::ComponentElement::new_with_builder(
            self.clone(),
            |v| Box::new(v.app.browser_view(v.width, v.height, v.region)),
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
        }.padding_insets(EdgeInsets::new(12.,0.,12.,0.))).chrome_style(ChromeStyle::Simple).height(22.))
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

//! Shared visual tokens and type-erased composition for ScarletUI.
use scarlet_ui::{
    element::{ElementRenderObject, RenderElement, UpdateResult},
    prelude::*,
};
use std::any::Any;

#[derive(Clone)]
pub struct AnyView(Box<dyn View>);
impl AnyView {
    pub fn new(view: impl View + 'static) -> Self {
        Self(Box::new(view))
    }
}
impl View for AnyView {
    fn create_element(&self) -> Box<dyn Element> {
        Box::new(scarlet_ui::ComponentElement::new_with_builder(
            self.clone(),
            |v| v.0.clone(),
        ))
    }
    fn listenables(&self) -> Vec<&dyn Listenable> {
        // The child owns its subscriptions. Forwarding all of a container's
        // descendants here turns a position tick into a whole-page rebuild.
        Vec::new()
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub fn queue_marker(index: usize, active: bool, width: f32, height: f32) -> AnyView {
    let content = if active {
        AnyView::new(
            IconView::new(Icon::PlayerPlay)
                .size(IconSize::Pixels(12))
                .color(ColorPalette::default().primary()),
        )
    } else {
        AnyView::new(caption(format!("{}", index + 1)))
    };
    AnyView::new(content.alignment(Alignment::Center).frame(width, height))
}

pub fn text(value: impl Into<String>) -> Text {
    Text::new(value).font_size(13.)
}
pub fn caption(value: impl Into<String>) -> Text {
    Text::new(value)
        .font_size(12.)
        .color(ColorPalette::default().text_secondary())
}
pub fn button(value: impl Into<String>) -> Button {
    Button::new(value).font_size(13.)
}
pub fn clock(duration: std::time::Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}
pub fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_owned()
    } else {
        value
            .chars()
            .take(max.saturating_sub(1))
            .chain(std::iter::once('…'))
            .collect()
    }
}

pub fn selected_color() -> Color {
    ColorPalette::default()
        .primary()
        .with_opacity(0.10)
        .blend_over(ColorPalette::default().background())
}

/// One set of metrics for every page and panel header.
pub const LIBRARY_HEADER_HEIGHT: f32 = 64.;
pub const COLUMN_HEADER_HEIGHT: f32 = 36.;
pub const PAGE_PADDING: f32 = 20.;
pub const PLAYER_HEIGHT: f32 = 64.;
pub const PLAYER_BOTTOM_MARGIN: f32 = 12.;
pub const FLOATING_CONTENT_GAP: f32 = 16.;
// Floating chrome owns its content reservation: body + outer margin + gap.
pub const PLAYER_CLEARANCE: f32 = PLAYER_HEIGHT + PLAYER_BOTTOM_MARGIN + FLOATING_CONTENT_GAP;
/// Gap outside the material outline. Shadows do not change the safe width.
pub const CHROME_MARGIN: f32 = 8.;
pub const CONTENT_PADDING: f32 = PAGE_PADDING - CHROME_MARGIN;

pub fn content_width(page_width: f32) -> f32 {
    (page_width - CHROME_MARGIN * 2.).max(0.)
}

/// Share the sidebar's trailing gap and reserve the floating player once.
/// Both reservations affect usable content, never the canvas dimensions.
pub fn main_content(content: AnyView) -> AnyView {
    AnyView::new(ContentInsets {
        content,
        adjustment: EdgeInsets::new(-CHROME_MARGIN, 0., 0., PLAYER_CLEARANCE),
    })
}

/// Gallery spacing inside the safe area, without narrowing its scroll canvas.
pub fn scroll_content_padding(content: AnyView, insets: EdgeInsets) -> AnyView {
    AnyView::new(ContentInsets {
        content,
        adjustment: insets,
    })
}

/// Shared material and horizontal geometry for full and compact chrome.
fn chrome_bar(content: impl View + Clone + 'static, width: f32, height: f32) -> AnyView {
    AnyView::new(
        chrome_surface(
            HeaderBar::new(content.padding_insets(EdgeInsets::horizontal(CONTENT_PADDING)))
                .height(height)
                .surface(Color::CLEAR)
                .separator(Color::CLEAR),
        )
        .frame(content_width(width), height)
        .padding_insets(EdgeInsets::horizontal(CHROME_MARGIN)),
    )
}

/// Headers and transport share the navigation chrome's radius and material.
pub fn chrome_surface<V: View>(content: V) -> Surface<V> {
    content
        .surface_style(SurfaceStyle::bubble_glass())
        .corner_radius(16.)
}

/// Shared inset chrome; its backing samples the content painted beneath it.
pub fn header(content: impl View + Clone + 'static, width: f32) -> AnyView {
    AnyView::new(chrome_bar(content, width, 48.).padding_insets(EdgeInsets::vertical(8.)))
}

/// Compact column labels use the same inset and rounded material as the toolbar.
pub fn column_header(content: impl View + Clone + 'static, width: f32) -> AnyView {
    AnyView::new(chrome_bar(content, width, 28.).padding_insets(EdgeInsets::only_bottom(8.)))
}

/// Select a page's navigation-owned header or body without adding another shell.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageRegion {
    Standalone,
    Header,
    Content,
}

pub(crate) fn page_in(
    region: PageRegion,
    header: AnyView,
    content: AnyView,
    height: f32,
    scrolling: bool,
) -> AnyView {
    page_with_header_height_in(
        region,
        header,
        content,
        height,
        LIBRARY_HEADER_HEIGHT,
        scrolling,
    )
}

pub(crate) fn page_with_header_height_in(
    region: PageRegion,
    header: AnyView,
    content: AnyView,
    height: f32,
    header_height: f32,
    scrolling: bool,
) -> AnyView {
    match region {
        PageRegion::Header => AnyView::new(header.frame_height(header_height)),
        PageRegion::Content => {
            let content = if scrolling {
                content
            } else {
                AnyView::new(content.safe_area_padding(SafeAreaEdges::ALL))
            };
            AnyView::new(
                Surface::new(
                    ContentInsets {
                        content: crate::layout_debug::inspect_safe_area(content),
                        adjustment: EdgeInsets::horizontal(CHROME_MARGIN),
                    },
                    SurfaceRole::Canvas,
                )
                .frame(f32::INFINITY, height),
            )
        }
        PageRegion::Standalone => {
            page_with_header_height(header, content, height, header_height, scrolling)
        }
    }
}

/// Keep fixed content safe; let scrollports fill the canvas behind chrome.
pub fn page(header: AnyView, content: AnyView, height: f32, scrolling: bool) -> AnyView {
    page_in(PageRegion::Standalone, header, content, height, scrolling)
}

pub fn page_with_header_height(
    header: AnyView,
    content: AnyView,
    height: f32,
    header_height: f32,
    scrolling: bool,
) -> AnyView {
    let content = if scrolling {
        content
    } else {
        AnyView::new(content.safe_area_padding(SafeAreaEdges::HORIZONTAL))
    };
    let content = ContentInsets {
        content: crate::layout_debug::inspect_safe_area(content),
        adjustment: EdgeInsets::horizontal(CHROME_MARGIN),
    };
    let shell = if scrolling {
        ContentShell::scrolling(content)
    } else {
        ContentShell::new(content)
    };
    AnyView::new(
        Surface::new(
            shell.header(header, header_height).underlap(header_height),
            SurfaceRole::Canvas,
        )
        .frame(f32::INFINITY, height),
    )
}

/// Reserve the chrome's outer margins without shrinking the paint viewport.
/// Scrollports consume these reservations; fixed content uses SafeArea above.
#[derive(Clone)]
struct ContentInsets {
    content: AnyView,
    adjustment: EdgeInsets,
}

impl View for ContentInsets {
    fn create_element(&self) -> Box<dyn Element> {
        Box::new(RenderElement::with_view_children_and_updater(
            self.clone(),
            |view| ContentInsetsRenderObject {
                adjustment: view.adjustment,
                ..Default::default()
            },
            |render, view| {
                if render.adjustment == view.adjustment {
                    return UpdateResult::NoChange;
                }
                render.adjustment = view.adjustment;
                UpdateResult::Updated
            },
            |view| vec![Box::new(view.content.clone())],
        ))
    }

    fn listenables(&self) -> Vec<&dyn Listenable> {
        Vec::new()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Default)]
pub(crate) struct ContentInsetsRenderObject {
    size: Size,
    insets: EdgeInsets,
    adjustment: EdgeInsets,
}

impl ContentInsetsRenderObject {
    #[cfg(test)]
    pub(crate) fn content_insets(&self) -> EdgeInsets {
        self.child_safe_area_insets(self.insets)
    }
}

impl ElementRenderObject for ContentInsetsRenderObject {
    fn set_safe_area_insets(&mut self, insets: EdgeInsets) {
        self.insets = insets;
    }

    fn child_safe_area_insets(&self, insets: EdgeInsets) -> EdgeInsets {
        EdgeInsets::new(
            (insets.left + self.adjustment.left).max(0.),
            (insets.top + self.adjustment.top).max(0.),
            (insets.right + self.adjustment.right).max(0.),
            (insets.bottom + self.adjustment.bottom).max(0.),
        )
    }

    fn layout(&mut self, constraints: LayoutConstraints) -> Size {
        self.size = Size::new(constraints.min_width, constraints.min_height);
        self.size
    }

    fn layout_with_children(
        &mut self,
        constraints: LayoutConstraints,
        children: &mut [Box<dyn Element>],
    ) -> Size {
        if let Some(child) = children.first_mut() {
            self.size = child.layout(constraints);
            child.set_position(Point::ZERO);
        } else {
            self.layout(constraints);
        }
        self.size
    }

    fn size(&self) -> Size {
        self.size
    }

    fn render(&mut self) {}
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

pub fn header_title(title: impl Into<String>) -> Text {
    text(title).font_size(18.)
}
pub fn page_header(title: &str, detail: String, actions: AnyView, width: f32) -> AnyView {
    header(
        scarlet_ui::hstack! {
            header_title(truncate(title, ((width - 400.).max(90.) / 9.) as usize)),
            caption(detail), Spacer::new(), actions,
        }
        .spacing(12.),
        width,
    )
}
pub fn back_header(
    title: &str,
    actions: AnyView,
    actions_width: f32,
    width: f32,
    back: impl Fn() + 'static,
) -> AnyView {
    header(
        scarlet_ui::hstack! {
            button("Back").icon(Icon::ArrowLeft).header_style().on_click(back).frame(78.,32.),
            header_title(truncate(title, ((width - actions_width - 156.).max(70.) / 9.) as usize)),
            Spacer::new(), actions,
        }
        .spacing(12.),
        width,
    )
}
pub fn player_width(width: f32) -> f32 {
    (width - if width >= 700. { 96. } else { 48. }).min(960.)
}
pub fn library_header(
    app: &crate::Cadence,
    title: &str,
    count: String,
    query: State<String>,
    placeholder: &str,
    width: f32,
) -> AnyView {
    let sources = app.workspace.clone();
    header(scarlet_ui::hstack! {
        scarlet_ui::hstack! {
            header_title(title), caption(count),
        }.spacing(8.).alignment(Alignment::Leading).frame(180., 32.),
        TextField::new(query).placeholder(placeholder).font_size(13.).padding(8.).on_key(|_| true)
            .frame((width - 180. - 108. - 72.).max(100.), 32.),
        button("Sources").icon(Icon::Folder).header_style().on_click(move || {
            sources.source_page.set(crate::extension::SourcePage::Overview);
            sources.tab.set(crate::extension::Tab::Sources);
        }).frame(108., 32.),
    }.spacing(16.), width)
}

pub fn counted(count: usize, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

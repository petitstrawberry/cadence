//! Shared visual tokens and type-erased composition for ScarletUI.
use scarlet_ui::prelude::*;
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
pub const LIBRARY_HEADER_HEIGHT: f32 = 48.;
pub const PAGE_PADDING: f32 = 20.;
pub const PLAYER_CLEARANCE: f32 = 92.;
pub fn header(content: impl View + Clone + 'static, width: f32) -> AnyView {
    AnyView::new(
        HeaderBar::new(content.padding_insets(EdgeInsets::horizontal(PAGE_PADDING)))
            .height(LIBRARY_HEADER_HEIGHT)
            .frame(width, LIBRARY_HEADER_HEIGHT),
    )
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

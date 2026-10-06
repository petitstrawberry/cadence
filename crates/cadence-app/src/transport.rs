//! Kanade-style transport: track information, centered playback, and queue/volume.
use crate::{Cadence, extension::Tab, presentation::TrackInfoFrame, ui::*};
use cadence_core::{Command, PlaybackState, RepeatMode};
use scarlet_ui::{hstack, prelude::*, vstack};
use std::any::Any;

pub(super) fn view(app: &Cadence, width: f32) -> AnyView {
    if width < 640. {
        return compact_view(app, width);
    }
    let snapshot = app.playback.snapshot.borrow();
    let playing = snapshot.state == PlaybackState::Playing;
    let duration = snapshot.duration.map(|d| d.as_secs_f32()).unwrap_or(0.);
    let repeat_mode = snapshot.repeat;
    let previous = app.clone();
    let play = app.clone();
    let next = app.clone();
    let repeat = app.clone();
    let seek = app.playback.clone();
    let volume = app.playback.clone();
    let palette = ColorPalette::default();
    let progress_width = (width - (width * 0.27).clamp(190., 240.) * 2. - 48.) - 96.;
    let progress = if snapshot.seekable {
        AnyView::new(
            Slider::new(app.playback.transport.position.clone())
                .max(duration.max(1.))
                .dragging_state(app.playback.transport.scrubbing.clone())
                .on_change(move |seconds| {
                    if duration > 0. {
                        seek.transport.seek_pending.set(Some(seconds));
                    }
                })
                .frame(progress_width, 20.),
        )
    } else {
        AnyView::new(
            StreamProgress {
                position: app.playback.transport.position.clone(),
                duration,
            }
            .frame(progress_width, 20.),
        )
    };

    // Equal side columns keep Play over the middle of the bar at every supported
    // width. At the 676px minimum the seek slider still receives 152px.
    let side_width = (width * 0.27).clamp(190., 240.);
    let center_width = width - side_width * 2. - 48.;
    let repeat_color = if repeat_mode == RepeatMode::Off {
        palette.text_secondary()
    } else {
        palette.primary()
    };
    let repeat_button = if repeat_mode == RepeatMode::One {
        Button::new("1")
            .icon(Icon::Repeat)
            .font_size(10.)
            .padding(2.)
    } else {
        Button::icon_only(Icon::Repeat)
    }
    .header_style()
    .padding(if repeat_mode == RepeatMode::One {
        2.
    } else {
        6.
    })
    .icon_size(IconSize::Small)
    .icon_color(repeat_color)
    .text_color(repeat_color)
    .on_click(move || repeat.repeat())
    .frame(32., 32.);

    AnyView::new(
        Surface::floating(
            hstack! {
                TrackInfoFrame { app: app.clone(), width: side_width }.repaint_boundary(),
                vstack! {
                    hstack! {
                        // This balances Repeat so Previous/Play/Next stay centered.
                        Spacer::new().frame(32., 32.),
                        Button::icon_only(Icon::ChevronLeft).header_style().icon_size(IconSize::Small)
                            .on_click(move || previous.send(Command::Previous)).frame(32., 32.),
                        Button::icon_only(if playing { Icon::PlayerPause } else { Icon::PlayerPlay })
                            .header_style().icon_size(IconSize::Medium).icon_color(palette.primary())
                            .on_click(move || play.send(Command::Toggle)).frame(36., 32.),
                        Button::icon_only(Icon::ChevronRight).header_style().icon_size(IconSize::Small)
                            .on_click(move || next.send(Command::Next)).frame(32., 32.),
                        repeat_button,
                    }.spacing(12.).frame(212., 32.).alignment(Alignment::Center).frame(center_width, 32.),
                    hstack! {
                        TimeLabel { time: app.playback.transport.time.clone(), part: TimePart::Elapsed }
                            .frame(40., 20.),
                        progress,
                        TimeLabel { time: app.playback.transport.time.clone(), part: TimePart::Duration }
                            .frame(40., 20.),
                    }.spacing(8.).frame(center_width, 20.),
                }.spacing(2.).frame(center_width, 54.),
                hstack! {
                    QueueButton { app: app.clone() },
                    IconView::new(Icon::Volume).size(IconSize::Small)
                        .color(palette.text_secondary()).frame(16., 20.),
                    Slider::new(app.playback.transport.volume.clone()).on_change(move |value| {
                        volume.transport.volume_text.set(format!("{:.0}%", value * 100.));
                        volume.send(Command::SetVolume(value));
                    }).frame(100., 20.),
                    Text::from_state(app.playback.transport.volume_text.clone()).font_size(10.)
                        .color(palette.text_secondary()).alignment(Alignment::Trailing).frame(28., 20.),
                }.spacing(6.).frame(190., 32.).alignment(Alignment::Trailing).frame(side_width, 56.),
            }
            .spacing(12.)
            .frame(width - 24., 56.)
            .padding_insets(EdgeInsets::new(12., 4., 12., 4.)),
        )
        .frame(width, 64.),
    )
}

// macOS can constrain a window below its requested minimum on a small display.
// Kanade uses a compact information/playback cluster in that case.
fn compact_view(app: &Cadence, width: f32) -> AnyView {
    let snapshot = app.playback.snapshot.borrow();
    let playing = snapshot.state == PlaybackState::Playing;
    let repeat_mode = snapshot.repeat;
    let duration = snapshot.duration.map(|d| d.as_secs_f32()).unwrap_or(0.);
    let play = app.clone();
    let previous = app.clone();
    let next = app.clone();
    let repeat = app.clone();
    let seek = app.playback.clone();
    let volume = app.playback.clone();
    // Reserve equal side columns so the playback trio stays centered on the
    // whole bar, including when the compact layout replaces the desktop one.
    let show_repeat = width >= 450.;
    let cluster_width = if show_repeat { 168. } else { 104. };
    let info_width = (width * 0.24)
        .clamp(110., 150.)
        .min(((width - 48. - cluster_width) / 2.).max(54.));
    let control_width = (width - info_width * 2. - 48.).max(0.);
    let progress = if snapshot.seekable {
        AnyView::new(
            Slider::new(app.playback.transport.position.clone())
                .max(duration.max(1.))
                .dragging_state(app.playback.transport.scrubbing.clone())
                .on_change(move |seconds| {
                    if duration > 0. {
                        seek.transport.seek_pending.set(Some(seconds));
                    }
                })
                .frame((control_width - 88.).max(8.), 20.),
        )
    } else {
        AnyView::new(
            StreamProgress {
                position: app.playback.transport.position.clone(),
                duration,
            }
            .frame((control_width - 88.).max(8.), 20.),
        )
    };
    let volume_control = if width >= 450. {
        AnyView::new(hstack! {
            Slider::new(app.playback.transport.volume.clone()).on_change(move |value| {
                volume.transport.volume_text.set(format!("{:.0}%",value * 100.));
                volume.send(Command::SetVolume(value));
            }).frame((info_width - 62.).max(24.),20.),
            Text::from_state(app.playback.transport.volume_text.clone()).font_size(10.)
                .color(ColorPalette::default().text_secondary()).alignment(Alignment::Trailing).frame(24.,20.),
        }.spacing(4.).frame(info_width - 34.,20.))
    } else {
        AnyView::new(Spacer::new().frame(0., 20.))
    };
    let repeat_color = if repeat_mode == RepeatMode::Off {
        ColorPalette::default().text_secondary()
    } else {
        ColorPalette::default().primary()
    };
    let repeat_button = if show_repeat {
        let button = if repeat_mode == RepeatMode::One {
            Button::new("1")
                .icon(Icon::Repeat)
                .font_size(10.)
                .padding(2.)
        } else {
            Button::icon_only(Icon::Repeat)
        };
        AnyView::new(
            button
                .header_style()
                .padding(if repeat_mode == RepeatMode::One {
                    2.
                } else {
                    6.
                })
                .icon_size(IconSize::Small)
                .icon_color(repeat_color)
                .on_click(move || repeat.repeat())
                .frame(32., 28.),
        )
    } else {
        AnyView::new(Spacer::new().frame(0., 28.))
    };
    let controls = hstack! {
        // A matching slot on the left balances Repeat on the right.
        Spacer::new().frame(if show_repeat {32.} else {0.},28.),
        Button::icon_only(Icon::ChevronLeft).header_style().icon_size(IconSize::Small).on_click(move || previous.send(Command::Previous)).frame(28.,28.),
        Button::icon_only(if playing { Icon::PlayerPause } else { Icon::PlayerPlay }).header_style().icon_size(IconSize::Medium).icon_color(ColorPalette::default().primary()).on_click(move || play.send(Command::Toggle)).frame(32.,28.),
        Button::icon_only(Icon::ChevronRight).header_style().icon_size(IconSize::Small).on_click(move || next.send(Command::Next)).frame(28.,28.),
        repeat_button,
    }.spacing(4.).frame(if show_repeat {168.} else {104.},28.);
    AnyView::new(Surface::floating(hstack! {
        TrackInfoFrame { app: app.clone(), width: info_width }.repaint_boundary(),
        vstack! {
            controls.alignment(Alignment::Center).frame(control_width,32.),
            hstack! {
                TimeLabel { time: app.playback.transport.time.clone(), part: TimePart::Elapsed }.frame(36.,20.),
                progress,
                TimeLabel { time: app.playback.transport.time.clone(), part: TimePart::Duration }.frame(36.,20.),
            }.spacing(8.).frame(control_width,20.),
        }.spacing(2.).frame(control_width,54.),
        hstack! { QueueButton { app: app.clone() }, volume_control }
            .spacing(6.).alignment(Alignment::Trailing).frame(info_width,32.),
    }.spacing(12.).frame(width - 24.,56.).padding_insets(EdgeInsets::new(12.,4.,12.,4.))).frame(width,64.))
}

fn fit_track_label(value: &str, font_size: f32, width: f32) -> String {
    let fits =
        |text: &str| scarlet_ui::graphics::measure_text_sized(text, font_size).0 as f32 <= width;
    if width <= 0. {
        return String::new();
    }
    if fits(value) {
        return value.to_owned();
    }
    if !fits("…") {
        return String::new();
    }
    let boundaries: Vec<usize> = value
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(value.len()))
        .collect();
    let mut low = 0;
    let mut high = boundaries.len() - 1;
    while low < high {
        let middle = (low + high + 1) / 2;
        if fits(&format!("{}…", &value[..boundaries[middle]])) {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    format!("{}…", &value[..boundaries[low]])
}

pub(super) fn track_info(app: &Cadence, width: f32) -> AnyView {
    let snapshot = app.playback.snapshot.borrow();
    let track = snapshot.current.and_then(|index| snapshot.queue.get(index));
    let title = track.map(|track| track.title.as_str()).unwrap_or("No song");
    let artist = track
        .and_then(|track| track.artist.as_deref())
        .unwrap_or(if track.is_some() {
            "Unknown artist"
        } else {
            "Ready"
        });
    let album = track
        .and_then(|track| track.album.as_deref())
        .filter(|album| !album.is_empty());
    let text_width = (width - 54.).max(0.);
    let primary = Text::new(fit_track_label(title, 11., text_width))
        .font_size(11.)
        .color(ColorPalette::default().text_primary());
    let secondary = Text::new(fit_track_label(artist, 10., text_width))
        .font_size(10.)
        .color(ColorPalette::default().text_secondary());
    let labels = if let Some(album) = album {
        AnyView::new(
            vstack! {
                primary,
                secondary,
                Text::new(fit_track_label(album, 10., text_width)).font_size(10.)
                    .color(ColorPalette::default().text_secondary()),
            }
            .spacing(2.)
            .alignment(Alignment::Leading)
            .frame_width(text_width),
        )
    } else {
        AnyView::new(
            vstack! {
                primary,
                secondary,
            }
            .spacing(3.)
            .alignment(Alignment::Leading)
            .frame_width(text_width),
        )
    };
    AnyView::new(
        hstack! {
            app.artwork(44.),
            labels,
        }
        .spacing(10.)
        .frame(width, 56.),
    )
}

#[derive(Clone, Copy)]
enum TimePart {
    Elapsed,
    Duration,
}

#[derive(Clone)]
struct StreamProgress {
    position: State<f32>,
    duration: f32,
}
impl View for StreamProgress {
    fn create_element(&self) -> Box<dyn Element> {
        Box::new(scarlet_ui::ComponentElement::new_with_builder(
            self.clone(),
            |view| {
                let value = if view.duration > 0. {
                    view.position.get() / view.duration
                } else {
                    0.
                };
                Box::new(ProgressView::new(value.clamp(0., 1.)))
            },
        ))
    }
    fn listenables(&self) -> Vec<&dyn Listenable> {
        vec![&self.position]
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

// Only these two labels subscribe to formatted time. A tick never reconstructs
// the transport buttons, track information, navigation, or catalog.
#[derive(Clone)]
struct TimeLabel {
    time: State<String>,
    part: TimePart,
}
impl View for TimeLabel {
    fn create_element(&self) -> Box<dyn Element> {
        Box::new(scarlet_ui::ComponentElement::new_with_builder(
            self.clone(),
            |view| {
                let time = view.time.get();
                let (elapsed, duration) = time.split_once(" / ").unwrap_or((&time, "—:——"));
                let (label, alignment) = match view.part {
                    TimePart::Elapsed => (elapsed, Alignment::Trailing),
                    TimePart::Duration => (duration, Alignment::Leading),
                };
                Box::new(
                    Text::new(label)
                        .font_size(11.)
                        .color(ColorPalette::default().text_secondary())
                        .alignment(alignment)
                        .frame(40., 20.),
                )
            },
        ))
    }
    fn listenables(&self) -> Vec<&dyn Listenable> {
        vec![&self.time]
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Clone)]
struct QueueButton {
    app: Cadence,
}
impl View for QueueButton {
    fn create_element(&self) -> Box<dyn Element> {
        Box::new(scarlet_ui::ComponentElement::new_with_builder(
            self.clone(),
            |view| {
                let app = view.app.clone();
                let active = if app.size.get().width >= 1160. {
                    app.shell.queue_visible.get()
                } else {
                    app.workspace.tab.get() == Tab::Playing && !app.browser.visible.get()
                };
                let palette = ColorPalette::default();
                Box::new(
                    Button::icon_only(Icon::List)
                        .header_style()
                        .icon_size(IconSize::Small)
                        .icon_color(if active {
                            palette.primary()
                        } else {
                            palette.text_secondary()
                        })
                        .on_click(move || {
                            if app.size.get().width >= 1160. {
                                app.shell.queue_visible.set(!app.shell.queue_visible.get());
                            } else {
                                app.browser.visible.set(false);
                                app.workspace.tab.set(Tab::Playing);
                            }
                        })
                        .frame(28., 28.),
                )
            },
        ))
    }
    fn listenables(&self) -> Vec<&dyn Listenable> {
        vec![
            &self.app.shell.queue_visible,
            &self.app.workspace.tab,
            &self.app.browser.visible,
        ]
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

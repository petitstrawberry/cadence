//! Generic CLAP controls. Active instances stay on the playback worker.
use crate::{state, ui::*};
use cadence_effects::{EffectController, ParameterSnapshot, RackConfig, RackSnapshot};
use scarlet_ui::{hstack, prelude::*, vstack};
use std::{
    any::Any,
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
};

#[derive(Clone)]
struct ParameterRow {
    parameter: ParameterSnapshot,
    value: State<f32>,
    label: State<String>,
    dragging: State<bool>,
    pending: Rc<Cell<Option<f64>>>,
}
impl ParameterRow {
    fn new(parameter: ParameterSnapshot) -> Self {
        Self {
            value: state(normalized(&parameter)),
            label: state(parameter_label(&parameter)),
            dragging: state(false),
            pending: Rc::new(Cell::new(None)),
            parameter,
        }
    }
}

#[derive(Clone)]
pub struct EffectPane {
    pub controller: EffectController,
    pub path: State<String>,
    pub revision: State<u64>,
    pub status: State<String>,
    snapshot: Rc<RefCell<RackSnapshot>>,
    saved: Rc<RefCell<RackConfig>>,
    selected: State<Option<usize>>,
    discovered_selection: State<Option<usize>>,
    parameters: State<Vec<ParameterRow>>,
    parameter_selection: State<Option<usize>>,
    enabled: State<bool>,
    observed_enabled: Rc<Cell<bool>>,
    catalog_scanned: Rc<Cell<bool>>,
    bypass_request: Rc<Cell<Option<(usize, bool)>>>,
    moved_selection: Rc<Cell<Option<usize>>>,
    chain_size: State<Size>,
    parameter_size: State<Size>,
}
impl EffectPane {
    pub fn new(controller: EffectController) -> Self {
        let snapshot = controller.snapshot();
        Self {
            controller,
            path: state(String::new()),
            revision: state(0),
            status: state("Discovering installed CLAP plug-ins".into()),
            saved: Rc::new(RefCell::new(snapshot.config.clone())),
            snapshot: Rc::new(RefCell::new(snapshot)),
            selected: state(None),
            discovered_selection: state(None),
            parameters: state(vec![]),
            parameter_selection: state(None),
            enabled: state(false),
            observed_enabled: Rc::new(Cell::new(false)),
            catalog_scanned: Rc::new(Cell::new(false)),
            bypass_request: Rc::new(Cell::new(None)),
            moved_selection: Rc::new(Cell::new(None)),
            chain_size: state(Size::ZERO),
            parameter_size: state(Size::ZERO),
        }
    }
    pub fn with_saved_config(controller: EffectController, config: RackConfig) -> Self {
        let pane = Self::new(controller);
        *pane.saved.borrow_mut() = config;
        pane
    }
    pub fn ensure_catalog(&self) {
        if !self.catalog_scanned.get() && !self.controller.is_pending() {
            self.scan_installed();
        }
    }
    fn scan_installed(&self) {
        if self.submit(|controller| controller.scan_installed()) {
            self.catalog_scanned.set(true);
            self.status.set("Scanning installed CLAP plug-ins…".into());
        }
    }
    fn submit(
        &self,
        action: impl FnOnce(&EffectController) -> std::result::Result<(), String>,
    ) -> bool {
        if self.controller.is_pending() {
            self.status.set("Applying effect changes…".into());
            return false;
        }
        if self
            .parameters
            .get()
            .iter()
            .any(|row| row.dragging.get() || row.pending.get().is_some())
        {
            self.status
                .set("Finish applying the parameter edit first".into());
            return false;
        }
        match action(&self.controller) {
            Ok(()) => {
                self.status.set("Applying effect changes…".into());
                true
            }
            Err(error) => {
                self.status.set(error);
                false
            }
        }
    }
    fn discover(&self) {
        let path = self.path.get();
        if path.trim().is_empty() {
            self.status
                .set("Enter the path to a trusted .clap file or bundle".into());
            return;
        }
        self.submit(|controller| controller.discover(PathBuf::from(path)));
    }
    fn select(&self, index: usize) {
        self.selected.set(Some(index));
        self.sync_enabled();
        self.refresh_parameters();
    }
    fn sync_enabled(&self) {
        let enabled = self
            .selected
            .get()
            .and_then(|index| {
                self.snapshot
                    .borrow()
                    .effects
                    .get(index)
                    .map(|effect| !effect.bypass)
            })
            .unwrap_or(false);
        self.observed_enabled.set(enabled);
        if self.enabled.get() != enabled {
            self.enabled.set(enabled);
        }
    }
    fn flush_bypass(&self) {
        if self.controller.is_pending() || self.bypass_request.get().is_some() {
            return;
        }
        let Some(index) = self.selected.get() else {
            return;
        };
        let enabled = self.enabled.get();
        if enabled == self.observed_enabled.get()
            || self
                .parameters
                .get()
                .iter()
                .any(|row| row.dragging.get() || row.pending.get().is_some())
        {
            return;
        }
        let changed = self
            .snapshot
            .borrow()
            .effects
            .get(index)
            .is_some_and(|effect| effect.bypass == enabled);
        if !changed {
            self.observed_enabled.set(enabled);
        } else if self.submit(|controller| controller.set_bypass(index, !enabled)) {
            self.observed_enabled.set(enabled);
            self.bypass_request.set(Some((index, enabled)));
        } else {
            self.sync_enabled();
        }
    }
    fn refresh_parameters(&self) {
        let snapshot = self.snapshot.borrow();
        let rows = self
            .selected
            .get()
            .and_then(|index| snapshot.effects.get(index))
            .map(|effect| {
                effect
                    .parameters
                    .iter()
                    .filter(|p| !p.hidden)
                    .cloned()
                    .map(ParameterRow::new)
                    .collect()
            })
            .unwrap_or_default();
        self.parameters.set(rows);
    }
    /// Poll from the app's idle hook; persist only a completed, changed config.
    pub fn poll(&self) -> Option<RackConfig> {
        let rows = self.parameters.get();
        let dragging = rows.iter().any(|row| row.dragging.get());
        if !dragging
            && !self.controller.is_pending()
            && let Some(index) = self.selected.get()
        {
            for row in &rows {
                if let Some(value) = row.pending.take() {
                    match self
                        .controller
                        .set_parameter(index, row.parameter.id, value)
                    {
                        Ok(()) => self.status.set("Applying parameter…".into()),
                        Err(error) => self.status.set(error),
                    }
                }
            }
        }
        if dragging {
            return None;
        }
        let previous = self.snapshot.borrow();
        let Some(next) = self
            .controller
            .snapshot_if_changed(previous.revision, previous.pending)
        else {
            drop(previous);
            self.flush_bypass();
            return None;
        };
        let effects_changed = previous.effects != next.effects;
        drop(previous);
        *self.snapshot.borrow_mut() = next.clone();
        if next.revision != 0 && !next.pending {
            self.status.set(next.error.clone().unwrap_or_else(|| {
                format!(
                    "{} effect(s) · {} available",
                    next.effects.len(),
                    next.discovered.len()
                )
            }));
        }
        if !next.pending {
            if let Some((index, requested)) = self.bypass_request.take()
                && self.selected.get() == Some(index)
                && self.enabled.get() == requested
            {
                // A failed worker command restores the confirmed state. A newer
                // user toggle remains queued until this command has completed.
                self.sync_enabled();
            }
            if let Some(index) = self.moved_selection.take()
                && next.error.is_none()
            {
                self.selected.set(Some(index));
                self.sync_enabled();
            }
        }
        let selected = self.selected.get();
        if selected.is_none_or(|i| i >= next.effects.len()) {
            self.selected.set(if next.effects.is_empty() {
                None
            } else {
                Some(0)
            });
            self.sync_enabled();
            self.refresh_parameters();
        } else if effects_changed {
            if self.bypass_request.get().is_none()
                && self.enabled.get() == self.observed_enabled.get()
            {
                self.sync_enabled();
            }
            // Do not overwrite a user change that has not yet reached the worker.
            if !rows.iter().any(|row| row.pending.get().is_some()) {
                self.refresh_parameters();
            }
        }
        if self
            .discovered_selection
            .get()
            .is_none_or(|i| i >= next.discovered.len())
        {
            self.discovered_selection
                .set(if next.discovered.is_empty() {
                    None
                } else {
                    Some(0)
                });
        }
        self.revision.set(self.revision.get().wrapping_add(1));
        self.flush_bypass();
        if !next.pending
            && next.config_persistable
            && next.error.is_none()
            && next.config != *self.saved.borrow()
        {
            *self.saved.borrow_mut() = next.config.clone();
            Some(next.config)
        } else {
            None
        }
    }
    pub fn view(&self, width: f32, height: f32, browse: Rc<dyn Fn()>) -> AnyView {
        AnyView::new(EffectFrame {
            pane: self.clone(),
            width,
            height,
            browse,
        })
    }
    fn body(&self, width: f32, height: f32, browse: Rc<dyn Fn()>) -> AnyView {
        let discover = self.clone();
        let scan = self.clone();
        let submit = self.clone();
        let body_height = (height - 80.).max(120.);
        let initial_left = ((width - 1.) * 0.4).clamp(280., (width - 301.).max(280.));
        let chain_size = self.chain_size.get();
        let parameter_size = self.parameter_size.get();
        let left_width = if chain_size.width > 0. {
            chain_size.width
        } else {
            initial_left
        };
        let right_width = if parameter_size.width > 0. {
            parameter_size.width
        } else {
            width - initial_left - 1.
        };
        let update_chain = self.chain_size.clone();
        let update_parameters = self.parameter_size.clone();
        let panes = SplitView::new(
            self.chain_view(left_width, body_height)
                .frame(f32::INFINITY, body_height)
                .on_geometry_change(
                    |geometry| geometry.size(),
                    move |size| {
                        if update_chain.get() != size {
                            update_chain.set(size);
                        }
                    },
                ),
            self.parameter_view(right_width, body_height)
                .frame(f32::INFINITY, body_height)
                .on_geometry_change(
                    |geometry| geometry.size(),
                    move |size| {
                        if update_parameters.get() != size {
                            update_parameters.set(size);
                        }
                    },
                ),
        )
        .fraction(0.4)
        .min_first(280.)
        .min_second(300.)
        .frame(width, body_height);
        AnyView::new(Surface::new(vstack! {
            header(hstack! {
                hstack! { header_title("Effects"), Spacer::new() }.frame(180.,32.),
                TextField::new(self.path.clone()).placeholder("Choose an effect plug-in…")
                    .font_size(12.).padding(6.).on_submit(move || submit.discover())
                    .on_key(|_| true).frame((width - 400.).max(100.),28.),
                Button::icon_only(Icon::FolderOpen).header_style().icon_size(IconSize::Small)
                    .on_click(move || browse()).frame(28.,28.),
                Button::new("Discover").header_style().font_size(12.)
                    .on_click(move || discover.discover()).frame(92.,28.),
                Button::icon_only(Icon::Refresh).header_style().icon_size(IconSize::Small)
                    .on_click(move || scan.scan_installed()).frame(28.,28.),
            }.spacing(8.), width),
            panes,
            HeaderBar::new(hstack! {
                Text::from_state(self.status.clone()).font_size(11.).color(ColorPalette::default().text_secondary()),
                Spacer::new(),
            }.padding_insets(EdgeInsets::horizontal(PAGE_PADDING))).height(32.),
        }.spacing(0.), SurfaceRole::Canvas).frame(width,height).repaint_boundary())
    }
    fn chain_view(&self, width: f32, height: f32) -> AnyView {
        let snapshot = self.snapshot.borrow().clone();
        let discovered = state(snapshot.discovered.clone());
        let effects = state(snapshot.effects.clone());
        let choose = self.clone();
        let add = self.clone();
        let pane = self.clone();
        let up = self.clone();
        let down = self.clone();
        let remove = self.clone();
        let available_height = if snapshot.discovered.is_empty() {
            44.
        } else {
            (snapshot.discovered.len() as f32 * 36.).clamp(36., 108.)
        };
        let chain_height = (height - available_height - 96.).max(40.);
        let chain = if snapshot.effects.is_empty() {
            AnyView::new(
                vstack! {
                    Text::new("No effects added").font_size(13.),
                    caption("Add an available plug-in below.").font_size(11.),
                }
                .spacing(6.)
                .alignment(Alignment::TopLeading)
                .padding(PAGE_PADDING)
                .frame(width, chain_height),
            )
        } else {
            AnyView::new(ListView::new(effects,self.selected.clone(),46.,move |index,effect,selected| {
                let select = pane.clone();
                let palette = ColorPalette::default();
                hstack! {
                    Text::new(format!("{}",index + 1)).font_size(11.).color(palette.text_secondary()).alignment(Alignment::Center).frame(22.,34.),
                    vstack! {
                        Text::new(truncate(&effect.name,((width - 104.) / 7.).max(8.) as usize)).font_size(13.),
                        Text::new(if effect.bypass { "Bypassed".to_owned() } else if effect.vendor.is_empty() { "Active".to_owned() } else { effect.vendor }).font_size(10.).color(palette.text_secondary()),
                    }.spacing(3.).alignment(Alignment::Leading).frame_width(width - 88.),
                    IconView::new(Icon::Adjustments).size(IconSize::Small).color(if effect.bypass { palette.text_tertiary() } else { palette.primary() }).alignment(Alignment::Center).frame(20.,24.),
                }.spacing(6.).padding(6.).frame(width - 8.,46.)
                    .background(if selected == Some(index) { selected_color() } else { palette.background() })
                    .on_click(move || select.select(index))
            }).frame(width,chain_height))
        };
        let available = if snapshot.discovered.is_empty() {
            AnyView::new(
                caption("No plug-ins discovered")
                    .font_size(11.)
                    .padding(PAGE_PADDING)
                    .frame(width, available_height),
            )
        } else {
            AnyView::new(ListView::new(discovered,self.discovered_selection.clone(),36.,move |index,plugin,selected| {
                let select = choose.clone();
                let palette = ColorPalette::default();
                hstack! {
                    Text::new(truncate(&plugin.name,((width - 40.) / 7.).max(8.) as usize)).font_size(12.),
                    Spacer::new(),
                }.padding_insets(EdgeInsets::horizontal(PAGE_PADDING)).frame(width,36.)
                    .background(if selected == Some(index) { selected_color() } else { palette.background() })
                    .on_click(move || select.discovered_selection.set(Some(index)))
            }).frame(width,available_height))
        };
        AnyView::new(Surface::new(vstack! {
            header(hstack! {
                header_title("Effect chain"), caption(format!("{}", snapshot.effects.len())),
                Spacer::new(),
                Button::icon_only(Icon::ArrowUp).header_style().icon_size(IconSize::Small).on_click(move || {
                    if let Some(index) = up.selected.get() && index > 0 && up.submit(|c| c.move_effect(index,index - 1)) {
                        up.moved_selection.set(Some(index - 1));
                    }
                }).frame(28.,28.),
                Button::icon_only(Icon::ArrowDown).header_style().icon_size(IconSize::Small).on_click(move || {
                    if let Some(index) = down.selected.get() && index + 1 < down.snapshot.borrow().effects.len() && down.submit(|c| c.move_effect(index,index + 1)) {
                        down.moved_selection.set(Some(index + 1));
                    }
                }).frame(28.,28.),
                Button::icon_only(Icon::Trash).header_style().icon_size(IconSize::Small).on_click(move || {
                    if let Some(index) = remove.selected.get() { remove.submit(|c| c.remove(index)); }
                }).frame(28.,28.),
            }.spacing(8.), width),
            chain,
            header(hstack! {
                header_title("Available"), caption(format!("{}",snapshot.discovered.len())),
                Spacer::new(),
                Button::new("Add").icon(Icon::Plus).icon_size(IconSize::Small).header_style().font_size(12.).on_click(move || {
                    let plugin = add.discovered_selection.get().and_then(|i| add.snapshot.borrow().discovered.get(i).cloned());
                    if let Some(plugin) = plugin { add.submit(|c| c.add(plugin.path,Some(plugin.id))); }
                    else { add.status.set("Select an available plug-in, or refresh discovery".into()); }
                }).frame(60.,26.),
            }.spacing(4.), width),
            available,
        }.spacing(0.),SurfaceRole::Canvas).frame(width,height).clip())
    }
    fn parameter_view(&self, width: f32, height: f32) -> AnyView {
        let selected = self
            .selected
            .get()
            .and_then(|index| self.snapshot.borrow().effects.get(index).cloned());
        let header_content = match selected.as_ref() {
            Some(effect) => AnyView::new(hstack! {
                Text::new(truncate(&effect.name,((width - 126.) / 8.).max(8.) as usize)).font_size(18.),
                Spacer::new(),
                caption("Enabled").font_size(11.),
                Toggle::new(self.enabled.clone()).frame(36.,22.),
            }.spacing(8.)),
            None => AnyView::new(hstack! { header_title("Parameters"), Spacer::new() }),
        };
        let list = if self.parameters.get().is_empty() {
            AnyView::new(vstack! {
                Spacer::new(),
                IconView::new(Icon::Adjustments).size(IconSize::Large).color(ColorPalette::default().text_tertiary()),
                Text::new(if selected.is_some() { "No adjustable parameters" } else { "Select an effect" }).font_size(14.),
                caption(if selected.is_some() { "This plug-in has no generic controls." } else { "Choose an effect from the chain." }).font_size(11.),
                Spacer::new(),
            }.spacing(10.).frame(width,(height - 48.).max(40.)))
        } else {
            AnyView::new(ListView::new(self.parameters.clone(),self.parameter_selection.clone(),68.,move |_,row,_| {
                let parameter = row.parameter.clone();
                let editable = !parameter.read_only && parameter.min.is_finite() && parameter.max.is_finite() && parameter.min < parameter.max;
                let control = if editable {
                    let changed = row.clone();
                    AnyView::new(Slider::new(row.value.clone()).dragging_state(row.dragging.clone()).on_change(move |normalized| {
                        let value = parameter_value(&changed.parameter,normalized);
                        changed.pending.set(Some(value));
                        changed.label.set(if changed.parameter.stepped { format!("{value:.0}") } else { format!("{value:.4}") });
                    }).frame(width - 40.,22.))
                } else {
                    AnyView::new(caption(if parameter.read_only { "Read only" } else { "Fixed value" }).font_size(10.).alignment(Alignment::Leading).frame(width - 40.,22.))
                };
                AnyView::new(vstack! {
                    hstack! {
                        Text::new(truncate(&parameter.name,((width - 136.) / 7.).max(8.) as usize)).font_size(12.),
                        Spacer::new(),
                        Text::from_state(row.label.clone()).font_size(11.).color(ColorPalette::default().text_secondary()).alignment(Alignment::Trailing).frame(88.,20.),
                    }.frame(width - 40.,22.),
                    control,
                }.spacing(4.).frame_width(width - 40.))
                    .alignment(Alignment::Leading).frame(width - 40.,68.)
                    .padding_insets(EdgeInsets::horizontal(PAGE_PADDING)).frame(width,68.)
            }).frame(width,(height - 48.).max(40.)))
        };
        AnyView::new(
            Surface::new(
                vstack! {
                    header(header_content,width),
                    list,
                }
                .spacing(0.),
                SurfaceRole::Canvas,
            )
            .frame(width, height)
            .clip(),
        )
    }
}

#[derive(Clone)]
struct EffectFrame {
    pane: EffectPane,
    width: f32,
    height: f32,
    browse: Rc<dyn Fn()>,
}
impl View for EffectFrame {
    fn create_element(&self) -> Box<dyn Element> {
        Box::new(scarlet_ui::ComponentElement::new_with_builder(
            self.clone(),
            |frame| {
                Box::new(
                    frame
                        .pane
                        .body(frame.width, frame.height, frame.browse.clone()),
                )
            },
        ))
    }
    fn listenables(&self) -> Vec<&dyn Listenable> {
        vec![
            &self.pane.revision,
            &self.pane.selected,
            &self.pane.discovered_selection,
            &self.pane.chain_size,
            &self.pane.parameter_size,
        ]
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
fn parameter_label(parameter: &ParameterSnapshot) -> String {
    if parameter.text.is_empty() {
        format!("{:.4}", parameter.value)
    } else {
        truncate(&parameter.text, 16)
    }
}
fn normalized(parameter: &ParameterSnapshot) -> f32 {
    if !parameter.min.is_finite()
        || !parameter.max.is_finite()
        || parameter.min >= parameter.max
        || !parameter.value.is_finite()
    {
        return 0.;
    }
    let value = parameter.value.clamp(parameter.min, parameter.max);
    let span = parameter.max - parameter.min;
    let position = if span.is_finite() {
        (value - parameter.min) / span
    } else {
        // Halving first handles valid f64 ranges wider than f64::MAX.
        (value / 2. - parameter.min / 2.) / (parameter.max / 2. - parameter.min / 2.)
    };
    position.clamp(0., 1.) as f32
}
fn parameter_value(parameter: &ParameterSnapshot, normalized: f32) -> f64 {
    let normalized = normalized.clamp(0., 1.) as f64;
    let value = parameter.min * (1. - normalized) + parameter.max * normalized;
    if parameter.stepped && parameter.min.ceil() <= parameter.max.floor() {
        value
            .round()
            .clamp(parameter.min.ceil(), parameter.max.floor())
    } else {
        value.clamp(parameter.min, parameter.max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parameter(min: f64, max: f64, value: f64, stepped: bool) -> ParameterSnapshot {
        ParameterSnapshot {
            id: 1,
            name: "Gain".into(),
            module: String::new(),
            value,
            min,
            max,
            stepped,
            read_only: false,
            hidden: false,
            text: String::new(),
        }
    }
    #[test]
    fn parameter_controls_cover_negative_and_wide_ranges() {
        let gain = parameter(-24., 24., 0., false);
        assert_eq!(normalized(&gain), 0.5);
        assert_eq!(parameter_value(&gain, 0.), -24.);
        assert_eq!(parameter_value(&gain, 1.), 24.);
        let wide = parameter(-f64::MAX, f64::MAX, 0., false);
        assert_eq!(normalized(&wide), 0.5);
        assert_eq!(parameter_value(&wide, 0.5), 0.);
        let tiny = parameter(0., f64::from_bits(1), f64::from_bits(1), false);
        assert_eq!(normalized(&tiny), 1.);
    }
    #[test]
    fn stepped_values_remain_in_integer_bounds() {
        let mode = parameter(0.2, 4.7, 1., true);
        assert_eq!(parameter_value(&mode, 0.), 1.);
        assert_eq!(parameter_value(&mode, 1.), 4.);
        assert_eq!(parameter_value(&mode, 0.5), 2.);
    }
    #[test]
    fn failed_deferred_restore_preserves_saved_rack() {
        let config = RackConfig {
            effects: vec![cadence_effects::EffectConfig {
                path: PathBuf::from("/nonexistent/cadence-test-effect.clap"),
                plugin_id: "test.effect".into(),
                bypass: false,
                state: vec![1, 2, 3],
            }],
        };
        let (controller, factory) = cadence_effects::EffectRack::new(Default::default());
        let pane = EffectPane::with_saved_config(controller, config.clone());
        let mut processor = factory().unwrap();
        pane.controller.restore(config.clone()).unwrap();
        assert!(pane.poll().is_none());
        processor.service().unwrap();
        assert!(pane.poll().is_none());
        assert!(pane.controller.snapshot().error.is_some());
        assert_eq!(*pane.saved.borrow(), config);
        if let Some(path) = std::env::var_os("CADENCE_TEST_CLAP") {
            pane.controller.discover(PathBuf::from(path)).unwrap();
            processor.service().unwrap();
            assert!(pane.controller.snapshot().error.is_none());
            assert!(pane.poll().is_none());
            assert_eq!(*pane.saved.borrow(), config);
        }
    }
    #[test]
    fn failed_initial_restore_does_not_replace_saved_rack() {
        let config = RackConfig {
            effects: vec![cadence_effects::EffectConfig {
                path: PathBuf::from("/nonexistent/cadence-test-effect.clap"),
                plugin_id: "test.effect".into(),
                bypass: false,
                state: vec![],
            }],
        };
        let (controller, factory) = cadence_effects::EffectRack::new(config);
        let pane = EffectPane::new(controller);
        let _processor = factory().unwrap();
        assert!(pane.poll().is_none());
        assert!(pane.controller.snapshot().error.is_some());
    }
}

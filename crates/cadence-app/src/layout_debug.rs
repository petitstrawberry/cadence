//! Opt-in inspection of the reservations delivered to the page content.
//! Enable with CADENCE_DEBUG_SAFE_AREA=1. Green marks the safe rectangle,
//! amber the remaining viewport, and blue the viewport boundary. Ordinary
//! page padding is separate from the inherited safe-area reservation.
use crate::ui::AnyView;
use scarlet_ui::{
    element::{ElementRenderObject, RenderElement, UpdateResult},
    prelude::*,
    renderer::PaintContext,
};
use std::any::Any;

/// Wrap the content boundary before ScrollView/SafeArea consumes its insets.
/// No wrapper is inserted during normal rendering.
pub fn inspect_safe_area(content: AnyView) -> AnyView {
    if matches!(
        std::env::var("CADENCE_DEBUG_SAFE_AREA").as_deref(),
        Ok("1" | "true")
    ) {
        AnyView::new(SafeAreaOverlay(content))
    } else {
        content
    }
}

#[derive(Clone)]
struct SafeAreaOverlay(AnyView);

impl View for SafeAreaOverlay {
    fn create_element(&self) -> Box<dyn Element> {
        Box::new(RenderElement::with_view_children_and_updater(
            self.clone(),
            |_| SafeAreaOverlayRenderObject::default(),
            |_, _| UpdateResult::NoChange,
            |view| vec![Box::new(view.0.clone())],
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
struct SafeAreaOverlayRenderObject {
    size: Size,
    insets: EdgeInsets,
}

impl ElementRenderObject for SafeAreaOverlayRenderObject {
    fn set_safe_area_insets(&mut self, insets: EdgeInsets) {
        self.insets = insets;
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

    fn hit_test(&self, point: Point) -> bool {
        Rect::new(Point::ZERO, self.size).contains(point)
    }

    fn paint_overlay(&self, ctx: &mut PaintContext, origin: Point) -> bool {
        let edge = |value: f32| if value.is_finite() { value.max(0.) } else { 0. };
        let left = edge(self.insets.left).min(self.size.width.max(0.));
        let top = edge(self.insets.top).min(self.size.height.max(0.));
        let safe = Rect::from_xywh(
            left,
            top,
            (self.size.width - left - edge(self.insets.right)).max(0.),
            (self.size.height - top - edge(self.insets.bottom)).max(0.),
        );
        let unsafe_tint = Color::rgb(1., 0.55, 0.).with_opacity(0.12);
        for rect in [
            Rect::from_xywh(0., 0., self.size.width, safe.top()),
            Rect::from_xywh(
                0.,
                safe.bottom(),
                self.size.width,
                self.size.height - safe.bottom(),
            ),
            Rect::from_xywh(0., safe.top(), safe.left(), safe.size.height),
            Rect::from_xywh(
                safe.right(),
                safe.top(),
                self.size.width - safe.right(),
                safe.size.height,
            ),
        ] {
            if rect.size.width > 0. && rect.size.height > 0. {
                ctx.fill_rect(
                    Rect::new(
                        Point::new(origin.x + rect.left(), origin.y + rect.top()),
                        rect.size,
                    ),
                    unsafe_tint,
                );
            }
        }
        if safe.size.width > 0. && safe.size.height > 0. {
            let rect = Rect::new(
                Point::new(origin.x + safe.left(), origin.y + safe.top()),
                safe.size,
            );
            ctx.fill_rect(rect, Color::rgb(0.05, 0.75, 0.35).with_opacity(0.14));
            ctx.stroke_rounded_rect(
                rect.inset(EdgeInsets::all(1.)),
                0.,
                2.,
                Color::rgb(0., 0.65, 0.28),
            );
        }
        if self.size.width > 2. && self.size.height > 2. {
            ctx.stroke_rounded_rect(
                Rect::new(origin, self.size).inset(EdgeInsets::all(1.)),
                0.,
                2.,
                Color::rgb(0., 0.4, 1.),
            );
        }
        true
    }

    fn render(&mut self) {}
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

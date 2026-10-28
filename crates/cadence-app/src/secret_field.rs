//! A password editor whose UI document contains only masking characters.
//!
//! Raw input is intercepted before it reaches TextField. It is kept in a
//! zeroizing allocation, never in State, a render object, or IME surrounding
//! text. The platform owns its incoming input-event buffers as with any editor.

use crate::ui::AnyView;
use scarlet_ui::{
    clipboard::{ClipboardHandle, TextClipboard},
    element::{
        Element, ElementId, ElementRenderObject, LayoutConstraints, TextInputElementState,
        UpdateResult,
    },
    event::{Event, KeyCode, KeyEvent, KeyModifiers, Phase},
    pipeline::MountContext,
    prelude::*,
};
use std::{
    any::Any,
    cell::{Cell, RefCell},
    rc::Rc,
};
use zeroize::Zeroizing;

const MAX_PASSWORD_BYTES: usize = 4096;
const MASK: char = '•';

#[derive(Clone)]
pub(crate) struct SecretInput {
    value: Rc<RefCell<Zeroizing<String>>>,
    masked: State<String>,
    clipboard: Rc<RefCell<Option<TextClipboard>>>,
    pending_paste: Rc<RefCell<Option<PendingPaste>>>,
    generation: Rc<Cell<u64>>,
    selection: Rc<Cell<Option<(usize, usize)>>>,
    pending_caret: Rc<Cell<Option<usize>>>,
}

struct PendingPaste {
    handle: ClipboardHandle<Option<String>>,
    generation: u64,
    selection: (usize, usize),
}

impl SecretInput {
    pub(crate) fn new() -> Self {
        Self {
            value: Rc::new(RefCell::new(Zeroizing::new(String::new()))),
            masked: crate::state(String::new()),
            clipboard: Rc::new(RefCell::new(None)),
            pending_paste: Rc::new(RefCell::new(None)),
            generation: Rc::new(Cell::new(0)),
            selection: Rc::new(Cell::new(None)),
            pending_caret: Rc::new(Cell::new(None)),
        }
    }

    /// Transfers ownership without putting the passphrase in ordinary UI state.
    pub(crate) fn take(&self) -> Zeroizing<String> {
        self.invalidate_paste();
        let value = std::mem::replace(&mut *self.value.borrow_mut(), Zeroizing::new(String::new()));
        if !self.masked.get().is_empty() {
            self.masked.set(String::new());
        }
        value
    }

    pub(crate) fn clear(&self) {
        if self.value.borrow().is_empty() && self.masked.get().is_empty() {
            self.invalidate_paste();
            return;
        }
        drop(self.take());
    }

    /// Attach the service for this application's live window; never reads it.
    pub(crate) fn set_clipboard(&self, clipboard: Option<TextClipboard>) {
        self.invalidate_paste();
        *self.clipboard.borrow_mut() = clipboard;
    }

    /// Poll on every idle, including after leaving the certificate page, so
    /// stale clipboard results are consumed and erased without being applied.
    pub(crate) fn poll(&self) {
        let ready = self.pending_paste.borrow().as_ref().and_then(|pending| {
            pending
                .handle
                .try_take()
                .map(|result| (pending.generation, pending.selection, result))
        });
        let Some((generation, selection, result)) = ready else {
            return;
        };
        self.pending_paste.borrow_mut().take();
        let Ok(Some(text)) = result else {
            return;
        };
        let text = Zeroizing::new(text);
        if generation == self.generation.get() && self.selection.get() == Some(selection) {
            let caret = self.replace(selection.0, selection.1, &text);
            self.pending_caret.set(Some(caret));
        }
    }

    fn invalidate_paste(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        self.pending_caret.set(None);
    }

    fn track_selection(&self, selection: Option<(usize, usize)>) {
        if self.selection.get() != selection {
            self.invalidate_paste();
            self.selection.set(selection);
        }
    }

    fn request_paste(&self, selection: (usize, usize)) {
        self.track_selection(Some(selection));
        if self.pending_paste.borrow().is_some() {
            return;
        }
        if let Some(clipboard) = self.clipboard.borrow().as_ref() {
            *self.pending_paste.borrow_mut() = Some(PendingPaste {
                handle: clipboard.read_text(),
                generation: self.generation.get(),
                selection,
            });
        }
    }

    pub(crate) fn view(&self, width: f32) -> AnyView {
        AnyView::new(
            SecretField {
                input: self.clone(),
            }
            .frame(width, 36.),
        )
    }

    fn replace(&self, start: usize, end: usize, inserted: &str) -> usize {
        self.invalidate_paste();
        let old = self.value.borrow();
        let len = old.chars().count();
        let start = start.min(len);
        let end = end.max(start).min(len);
        let byte_at = |index| {
            old.char_indices()
                .nth(index)
                .map_or(old.len(), |(byte, _)| byte)
        };
        let start_byte = byte_at(start);
        let end_byte = byte_at(end);
        // Reject controls and line separators without copying the raw input
        // into an unprotected temporary String.
        let allowed = |c: &char| !c.is_control() && *c != '\u{2028}' && *c != '\u{2029}';
        let added_bytes: usize = inserted.chars().filter(allowed).map(char::len_utf8).sum();
        if old.len() - (end_byte - start_byte) + added_bytes > MAX_PASSWORD_BYTES {
            return start;
        }
        let mut next = Zeroizing::new(String::with_capacity(MAX_PASSWORD_BYTES));
        next.push_str(&old[..start_byte]);
        next.extend(inserted.chars().filter(allowed));
        let caret = start + inserted.chars().filter(allowed).count();
        next.push_str(&old[end_byte..]);
        let mask = MASK.to_string().repeat(next.chars().count());
        drop(old);
        *self.value.borrow_mut() = next; // Erases the previous allocation.
        self.masked.set(mask);
        caret
    }
}

#[derive(Clone)]
struct SecretField {
    input: SecretInput,
}

impl SecretField {
    fn masked_field(&self) -> TextField {
        TextField::new(self.input.masked.clone())
            .placeholder("Certificate password")
            .font_size(13.)
            .padding(8.)
            .blur_on_submit(true)
    }
}

impl View for SecretField {
    fn create_element(&self) -> Box<dyn Element> {
        Box::new(SecretElement {
            inner: self.masked_field().create_element(),
            field: self.clone(),
            modifiers: KeyModifiers::empty(),
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Retains TextField's native focus, selection and painting, but its only
/// document is the mask. The wrapper owns the same element id so ordinary
/// subscriptions and text-input routing continue to target this interceptor.
struct SecretElement {
    inner: Box<dyn Element>,
    field: SecretField,
    modifiers: KeyModifiers,
}

impl SecretElement {
    fn selection(&self) -> Option<(usize, usize)> {
        let state = self.inner.text_input_state()?;
        let a = state.anchor_byte as usize / MASK.len_utf8();
        let c = state.cursor_byte as usize / MASK.len_utf8();
        Some((a.min(c), a.max(c)))
    }

    fn edit(&mut self, start: usize, end: usize, inserted: &str) {
        let caret = self.field.input.replace(start, end, inserted);
        self.inner.update(&self.field.masked_field());
        self.move_caret(caret);
    }

    fn move_caret(&mut self, caret: usize) {
        // TextField does not expose a public selection setter. Move within the
        // masking document using its normal keyboard path (never raw text).
        self.inner.handle_event(
            &Event::Keyboard(KeyEvent::Pressed {
                keycode: KeyCode::Home,
                modifiers: KeyModifiers::empty(),
            }),
            Phase::Target,
        );
        for _ in 0..caret {
            self.inner.handle_event(
                &Event::Keyboard(KeyEvent::Pressed {
                    keycode: KeyCode::Right,
                    modifiers: KeyModifiers::empty(),
                }),
                Phase::Target,
            );
        }
        self.field.input.track_selection(self.selection());
    }

    fn apply_pending_caret(&mut self) {
        if let Some(caret) = self.field.input.pending_caret.take() {
            self.inner.update(&self.field.masked_field());
            self.move_caret(caret);
        }
    }
}

impl Element for SecretElement {
    fn id(&self) -> ElementId {
        self.inner.id()
    }
    fn type_name(&self) -> &str {
        "SecretInput"
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
    fn children(&self) -> &[Box<dyn Element>] {
        self.inner.children()
    }
    fn children_mut(&mut self) -> &mut [Box<dyn Element>] {
        self.inner.children_mut()
    }
    fn update(&mut self, new_view: &dyn View) -> UpdateResult {
        let Some(field) = new_view.as_any().downcast_ref::<SecretField>() else {
            return UpdateResult::Replaced;
        };
        self.field = field.clone();
        let result = self.inner.update(&self.field.masked_field());
        self.apply_pending_caret();
        result
    }
    fn rebuild(&mut self) -> UpdateResult {
        let result = self.inner.rebuild();
        self.apply_pending_caret();
        result
    }
    fn mount(&mut self, ctx: &MountContext) {
        self.inner.mount(ctx);
    }
    fn unmount(&mut self) {
        self.field.input.clear();
        self.field.input.track_selection(None);
        self.inner.unmount();
    }
    fn layout(&mut self, constraints: LayoutConstraints) -> Size {
        self.inner.layout(constraints)
    }
    fn last_layout_constraints(&self) -> Option<LayoutConstraints> {
        self.inner.last_layout_constraints()
    }
    fn set_last_layout_constraints(&mut self, value: LayoutConstraints) {
        self.inner.set_last_layout_constraints(value);
    }
    fn position(&self) -> Point {
        self.inner.position()
    }
    fn set_position(&mut self, value: Point) {
        self.inner.set_position(value);
    }
    fn bounds(&self) -> Rect {
        self.inner.bounds()
    }
    fn hit_test(&self, point: Point) -> bool {
        self.inner.hit_test(point)
    }
    fn render_object(&self) -> Option<&dyn ElementRenderObject> {
        self.inner.render_object()
    }
    fn render_object_mut(&mut self) -> Option<&mut dyn ElementRenderObject> {
        self.inner.render_object_mut()
    }
    fn text_input_state(&self) -> Option<TextInputElementState> {
        self.inner.text_input_state()
    }
    fn wants_keyboard_focus(&self) -> bool {
        self.inner.wants_keyboard_focus()
    }
    fn accepts_keyboard_focus(&self) -> bool {
        true
    }
    fn clear_buffers(&mut self) {
        self.inner.clear_buffers();
    }

    fn handle_event(&mut self, event: &Event, phase: Phase) -> bool {
        self.apply_pending_caret();
        self.field.input.track_selection(self.selection());
        if let Event::ModifiersChanged(modifiers) = event {
            self.modifiers = *modifiers;
        }
        if phase == Phase::Target
            && let Some((start, end)) = self.selection()
        {
            match event {
                Event::Keyboard(KeyEvent::Char { c }) => {
                    if !self.modifiers.primary() && !c.is_control() {
                        let mut bytes = [0u8; 4];
                        self.edit(start, end, c.encode_utf8(&mut bytes));
                        zeroize::Zeroize::zeroize(&mut bytes);
                    }
                    return true;
                }
                Event::Keyboard(KeyEvent::Pressed { keycode, modifiers }) => {
                    self.modifiers = *modifiers;
                    match keycode {
                        KeyCode::Backspace => {
                            self.edit(
                                if start == end {
                                    start.saturating_sub(1)
                                } else {
                                    start
                                },
                                end,
                                "",
                            );
                            return true;
                        }
                        KeyCode::Delete => {
                            self.edit(
                                start,
                                if start == end {
                                    end.saturating_add(1)
                                } else {
                                    end
                                },
                                "",
                            );
                            return true;
                        }
                        KeyCode::Escape => {
                            self.field.input.clear();
                            self.inner.update(&self.field.masked_field());
                        }
                        // Never let the underlying editor fetch clipboard text
                        // into its masking State. Paste is an explicit bounded
                        // asynchronous request owned by the protected model.
                        KeyCode::Char('v' | 'V') if modifiers.primary() => {
                            self.field.input.request_paste((start, end));
                            return true;
                        }
                        KeyCode::Insert if modifiers.shift => {
                            self.field.input.request_paste((start, end));
                            return true;
                        }
                        KeyCode::Char('c' | 'C' | 'x' | 'X') if modifiers.primary() => {
                            return true;
                        }
                        _ => {}
                    }
                }
                Event::Keyboard(KeyEvent::Released { modifiers, .. }) => {
                    self.modifiers = *modifiers
                }
                Event::TextInputCommit { text, .. } => {
                    self.edit(start, end, text);
                    return true;
                }
                // Composition is never displayed or exposed as surrounding
                // text. The final IME commit still accepts Unicode passwords.
                Event::TextInputPreedit { text, .. } => {
                    if !text.is_empty() {
                        self.field.input.invalidate_paste();
                    }
                    return true;
                }
                Event::TextInputDone { .. } => return true,
                Event::TextInputDeleteSurroundingText {
                    before_bytes,
                    after_bytes,
                    ..
                } => {
                    let before = (*before_bytes as usize).div_ceil(MASK.len_utf8());
                    let after = (*after_bytes as usize).div_ceil(MASK.len_utf8());
                    self.edit(start.saturating_sub(before), end.saturating_add(after), "");
                    return true;
                }
                _ => {}
            }
        }
        let handled = self.inner.handle_event(event, phase);
        self.field.input.track_selection(self.selection());
        // Keep application shortcuts (e.g. playback on Space) out of the field.
        handled || (phase == Phase::Target && matches!(event, Event::Keyboard(_)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scarlet_ui::clipboard::{ClipboardError, ClipboardProvider};
    use scarlet_ui::event::FocusEvent;
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    };

    struct FixtureClipboard {
        reads: Arc<AtomicUsize>,
        gate: Option<Arc<Barrier>>,
    }
    impl ClipboardProvider for FixtureClipboard {
        fn read_text(&self) -> std::result::Result<Option<String>, ClipboardError> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            if let Some(gate) = &self.gate {
                gate.wait();
            }
            Ok(Some("fixture-秘密".into()))
        }
        fn write_text(&self, _: &str) -> std::result::Result<(), ClipboardError> {
            panic!("Secret input must never write the clipboard")
        }
    }

    fn finish_paste(input: &SecretInput, field: &mut dyn Element) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while input.pending_paste.borrow().is_some() {
            input.poll();
            assert!(
                std::time::Instant::now() < deadline,
                "fixture clipboard did not finish"
            );
            std::thread::yield_now();
        }
        field.rebuild();
    }

    fn primary_modifiers() -> KeyModifiers {
        KeyModifiers {
            super_key: true,
            control: true,
            ..KeyModifiers::empty()
        }
    }

    fn focused(input: &SecretInput) -> Box<dyn Element> {
        let mut field = SecretField {
            input: input.clone(),
        }
        .create_element();
        field.layout(LayoutConstraints::tight(280., 36.));
        field.handle_event(&Event::Focus(FocusEvent::Gained), Phase::Target);
        field
    }

    #[test]
    fn native_input_and_ime_expose_only_masks_and_take_clears_state() {
        let input = SecretInput::new();
        let mut field = focused(&input);
        field.handle_event(
            &Event::TextInputCommit {
                context_id: 1,
                serial: 1,
                text: "fixture-秘密".into(),
            },
            Phase::Target,
        );
        let exposed = field.text_input_state().unwrap();
        assert_eq!(exposed.surrounding_text, "•".repeat(10));
        assert!(!format!("{exposed:?}").contains("fixture"));
        assert_eq!(input.take().as_str(), "fixture-秘密");
        assert!(input.masked.get().is_empty());
        assert!(input.take().is_empty());
    }

    #[test]
    fn edits_respect_unicode_selection_and_escape_and_unmount_clear() {
        let input = SecretInput::new();
        let mut field = focused(&input);
        for c in "abc界".chars() {
            field.handle_event(&Event::Keyboard(KeyEvent::Char { c }), Phase::Target);
        }
        field.handle_event(
            &Event::Keyboard(KeyEvent::Pressed {
                keycode: KeyCode::Backspace,
                modifiers: KeyModifiers::empty(),
            }),
            Phase::Target,
        );
        assert_eq!(input.value.borrow().as_str(), "abc");
        field.handle_event(
            &Event::Keyboard(KeyEvent::Pressed {
                keycode: KeyCode::Home,
                modifiers: KeyModifiers::empty(),
            }),
            Phase::Target,
        );
        field.handle_event(&Event::Keyboard(KeyEvent::Char { c: 'é' }), Phase::Target);
        assert_eq!(input.value.borrow().as_str(), "éabc");
        field.handle_event(
            &Event::Keyboard(KeyEvent::Pressed {
                keycode: KeyCode::Escape,
                modifiers: KeyModifiers::empty(),
            }),
            Phase::Target,
        );
        assert!(input.take().is_empty());
        input.replace(0, 0, "synthetic");
        field.unmount();
        assert!(input.take().is_empty());
    }

    #[test]
    fn bounded_input_drops_controls_and_copy_never_reveals_value() {
        let input = SecretInput::new();
        assert_eq!(input.replace(0, 0, "a\nb\rc\t"), 3);
        assert_eq!(input.value.borrow().as_str(), "abc");
        input.replace(3, 3, &"x".repeat(MAX_PASSWORD_BYTES));
        assert_eq!(input.value.borrow().as_str(), "abc");
        let mut field = focused(&input);
        let primary = KeyModifiers {
            super_key: true,
            control: true,
            ..KeyModifiers::empty()
        };
        assert!(field.handle_event(
            &Event::Keyboard(KeyEvent::Pressed {
                keycode: KeyCode::Char('c'),
                modifiers: primary,
            }),
            Phase::Target
        ));
        assert_eq!(field.text_input_state().unwrap().surrounding_text, "•••");
        for (keycode, modifiers) in [
            (KeyCode::Char('v'), primary),
            (
                KeyCode::Insert,
                KeyModifiers {
                    shift: true,
                    ..KeyModifiers::empty()
                },
            ),
        ] {
            assert!(field.handle_event(
                &Event::Keyboard(KeyEvent::Pressed { keycode, modifiers }),
                Phase::Target
            ));
            assert_eq!(field.text_input_state().unwrap().surrounding_text, "•••");
            assert_eq!(input.value.borrow().as_str(), "abc");
        }
    }

    #[test]
    fn clearing_an_empty_password_does_not_notify_or_redraw() {
        use scarlet_ui::state::Listenable;
        let input = SecretInput::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        input.masked.subscribe_any(Arc::new(move || {
            observed.fetch_add(1, Ordering::Relaxed);
        }));
        for _ in 0..20 {
            input.clear();
        }
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        input.replace(0, 0, "synthetic");
        input.clear();
        let after_clear = calls.load(Ordering::Relaxed);
        assert!(after_clear > 0);
        for _ in 0..20 {
            input.clear();
        }
        assert_eq!(calls.load(Ordering::Relaxed), after_clear);
    }

    #[test]
    fn paste_reads_only_on_explicit_shortcut_and_never_exposes_clipboard_text() {
        for (keycode, modifiers) in [
            (KeyCode::Char('v'), primary_modifiers()),
            (
                KeyCode::Insert,
                KeyModifiers {
                    shift: true,
                    ..KeyModifiers::empty()
                },
            ),
        ] {
            let input = SecretInput::new();
            let reads = Arc::new(AtomicUsize::new(0));
            input.set_clipboard(Some(TextClipboard::new(FixtureClipboard {
                reads: reads.clone(),
                gate: None,
            })));
            let mut field = focused(&input);
            input.poll();
            assert_eq!(reads.load(Ordering::Relaxed), 0);
            field.handle_event(
                &Event::Keyboard(KeyEvent::Pressed { keycode, modifiers }),
                Phase::Target,
            );
            finish_paste(&input, field.as_mut());
            assert_eq!(reads.load(Ordering::Relaxed), 1);
            assert_eq!(input.value.borrow().as_str(), "fixture-秘密");
            assert_eq!(
                field.text_input_state().unwrap().surrounding_text,
                "•".repeat(10)
            );
            assert_eq!(input.masked.get(), "•".repeat(10));
            assert_eq!(field.text_input_state().unwrap().cursor_byte, 30);
        }
    }

    #[test]
    fn late_paste_is_drained_after_clear_typing_selection_or_navigation() {
        for interruption in ["clear", "type", "selection", "unmount"] {
            let input = SecretInput::new();
            input.replace(0, 0, "ab");
            let gate = Arc::new(Barrier::new(2));
            input.set_clipboard(Some(TextClipboard::new(FixtureClipboard {
                reads: Arc::new(AtomicUsize::new(0)),
                gate: Some(gate.clone()),
            })));
            let mut field = focused(&input);
            field.handle_event(
                &Event::Keyboard(KeyEvent::Pressed {
                    keycode: KeyCode::End,
                    modifiers: KeyModifiers::empty(),
                }),
                Phase::Target,
            );
            field.handle_event(
                &Event::Keyboard(KeyEvent::Pressed {
                    keycode: KeyCode::Char('v'),
                    modifiers: primary_modifiers(),
                }),
                Phase::Target,
            );
            match interruption {
                "clear" => input.clear(),
                "type" => {
                    field.handle_event(
                        &Event::ModifiersChanged(KeyModifiers::empty()),
                        Phase::Target,
                    );
                    field.handle_event(&Event::Keyboard(KeyEvent::Char { c: 'z' }), Phase::Target);
                }
                "selection" => {
                    field.handle_event(
                        &Event::Keyboard(KeyEvent::Pressed {
                            keycode: KeyCode::Home,
                            modifiers: KeyModifiers::empty(),
                        }),
                        Phase::Target,
                    );
                }
                "unmount" => field.unmount(),
                _ => unreachable!(),
            }
            gate.wait();
            finish_paste(&input, field.as_mut());
            assert_eq!(
                input.value.borrow().as_str(),
                match interruption {
                    "type" => "abz",
                    "selection" => "ab",
                    _ => "",
                }
            );
            assert!(!input.masked.get().contains("fixture"));
            assert!(input.pending_paste.borrow().is_none());
        }
    }
}

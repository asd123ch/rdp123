//! The custom `NSView` that renders the remote framebuffer and forwards input.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSCursor, NSEvent, NSEventType, NSMenuItem, NSMenuItemValidation, NSPasteboard,
    NSPasteboardTypeString, NSView,
};
use objc2_foundation::{NSObjectProtocol, NSPoint, NSRect};

use rdp123_core::{InputEvent, PointerButton, SessionCommand, SessionHandle, SharedFramebuffer};

use crate::ui;

#[derive(Default)]
pub struct RdpViewIvars {
    handle: RefCell<Option<SessionHandle>>,
    framebuffer: RefCell<Option<Arc<SharedFramebuffer>>>,
    input_routing: RefCell<InputRoutingState>,
    /// The server-provided pointer shape; `None` shows the native arrow.
    remote_cursor: RefCell<Option<Retained<NSCursor>>>,
    /// Opt-in bridge for external macOS speech-to-text tools that insert by
    /// invoking the focused application's standard Paste action.
    external_stt_paste_enabled: Cell<bool>,
    /// Recycled presentation buffers (see `ui::upload_framebuffer`).
    present_pool: ui::PresentPool,
    /// Recycled IOSurfaces for zero-copy presentation.
    surface_pool: ui::SurfacePool,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "RDP123RdpView"]
    #[ivars = RdpViewIvars]
    pub struct RdpView;

    unsafe impl NSObjectProtocol for RdpView {}

    unsafe impl NSMenuItemValidation for RdpView {
        #[unsafe(method(validateMenuItem:))]
        fn validate_menu_item(&self, item: &NSMenuItem) -> bool {
            let action = item.action();
            if action == Some(sel!(paste:)) {
                if self.ivars().external_stt_paste_enabled.get() {
                    self.external_stt_pasteboard_text().is_some()
                } else {
                    self.command_modifier_pressed()
                }
            } else {
                self.command_modifier_pressed()
                    && matches!(
                        action,
                        Some(action)
                            if action == sel!(undo:)
                                || action == sel!(redo:)
                                || action == sel!(cut:)
                                || action == sel!(copy:)
                                || action == sel!(selectAll:)
                    )
            }
        }
    }

    impl RdpView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(becomeFirstResponder))]
        fn become_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            self.forward_key_down(event);
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, event: &NSEvent) {
            let keycode = event.keyCode();
            let events = self
                .ivars()
                .input_routing
                .borrow_mut()
                .key_up(keycode, self.ivars().external_stt_paste_enabled.get());
            self.send(events);
        }

        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, event: &NSEvent) {
            let keycode = event.keyCode();
            let Some((device_mask, family_mask)) = modifier_masks(keycode) else {
                return;
            };
            let flags = event.modifierFlags().0;
            let mut routing = self.ivars().input_routing.borrow_mut();
            let down = if flags & DEVICE_MODIFIER_MASKS != 0 {
                flags & device_mask != 0
            } else if flags & family_mask == 0 {
                false
            } else {
                !routing.is_pressed(keycode)
            };
            let events = routing.modifier_changed(
                keycode,
                down,
                self.ivars().external_stt_paste_enabled.get(),
            );
            drop(routing);
            self.send(events);
        }

        #[unsafe(method(paste:))]
        fn paste(&self, _sender: Option<&AnyObject>) {
            if !self.ivars().external_stt_paste_enabled.get() {
                self.forward_menu_shortcut();
                return;
            }
            if self.external_stt_pasteboard_text().is_none() {
                return;
            }
            let (events, submit) = self
                .ivars()
                .input_routing
                .borrow_mut()
                .paste_invoked(true);
            self.send(events);
            if submit {
                self.submit_external_stt_paste();
            }
        }

        #[unsafe(method(undo:))]
        fn undo(&self, _sender: Option<&AnyObject>) {
            self.forward_menu_shortcut();
        }

        #[unsafe(method(redo:))]
        fn redo(&self, _sender: Option<&AnyObject>) {
            self.forward_menu_shortcut();
        }

        #[unsafe(method(cut:))]
        fn cut(&self, _sender: Option<&AnyObject>) {
            self.forward_menu_shortcut();
        }

        #[unsafe(method(copy:))]
        fn copy(&self, _sender: Option<&AnyObject>) {
            self.forward_menu_shortcut();
        }

        #[unsafe(method(selectAll:))]
        fn select_all(&self, _sender: Option<&AnyObject>) {
            self.forward_menu_shortcut();
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            self.mouse_button(event, PointerButton::Left, true);
        }
        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            self.mouse_button(event, PointerButton::Left, false);
        }
        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            self.mouse_button(event, PointerButton::Right, true);
        }
        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, event: &NSEvent) {
            self.mouse_button(event, PointerButton::Right, false);
        }
        #[unsafe(method(otherMouseDown:))]
        fn other_mouse_down(&self, event: &NSEvent) {
            if event.buttonNumber() == 2 {
                self.mouse_button(event, PointerButton::Middle, true);
            }
        }
        #[unsafe(method(otherMouseUp:))]
        fn other_mouse_up(&self, event: &NSEvent) {
            if event.buttonNumber() == 2 {
                self.mouse_button(event, PointerButton::Middle, false);
            }
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            self.mouse_move(event);
        }
        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            self.mouse_move(event);
        }
        #[unsafe(method(rightMouseDragged:))]
        fn right_mouse_dragged(&self, event: &NSEvent) {
            self.mouse_move(event);
        }
        #[unsafe(method(otherMouseDragged:))]
        fn other_mouse_dragged(&self, event: &NSEvent) {
            self.mouse_move(event);
        }

        #[unsafe(method(resetCursorRects))]
        fn reset_cursor_rects(&self) {
            let cursor = self.ivars().remote_cursor.borrow();
            let cursor = cursor.clone().unwrap_or_else(NSCursor::arrowCursor);
            self.addCursorRect_cursor(self.bounds(), &cursor);
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            if !self.accepts_pointer_event(event) {
                return;
            }
            let precise = event.hasPreciseScrollingDeltas();
            let factor = if precise { 4.0 } else { 120.0 };
            let dy = event.scrollingDeltaY();
            let dx = event.scrollingDeltaX();
            let mut events = Vec::new();
            let vy = (dy * factor) as i16;
            if vy != 0 {
                events.push(InputEvent::Wheel { delta: vy, horizontal: false });
            }
            let vx = (dx * factor) as i16;
            if vx != 0 {
                events.push(InputEvent::Wheel { delta: vx, horizontal: true });
            }
            if !events.is_empty() {
                self.send_pointer_input(events);
            }
        }

    }
);

impl RdpView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(RdpViewIvars::default());
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// Attach the session once it has been spawned.
    pub fn set_session(&self, handle: SessionHandle) {
        *self.ivars().framebuffer.borrow_mut() = Some(handle.framebuffer());
        *self.ivars().handle.borrow_mut() = Some(handle);
    }

    pub fn set_external_stt_paste_enabled(&self, enabled: bool) {
        self.ivars().external_stt_paste_enabled.set(enabled);
    }

    pub fn set_mac_shortcuts_enabled(&self, enabled: bool) {
        if self.ivars().input_routing.borrow().mac_shortcuts_enabled != enabled {
            self.release_all_keys();
            self.ivars()
                .input_routing
                .borrow_mut()
                .mac_shortcuts_enabled = enabled;
        }
    }

    pub fn forward_key_down(&self, event: &NSEvent) {
        let characters = event
            .charactersIgnoringModifiers()
            .map(|text| text.to_string());
        let (events, external_paste) = self
            .ivars()
            .input_routing
            .borrow_mut()
            .key_down_with_characters(
                event.keyCode(),
                characters.as_deref(),
                self.ivars().external_stt_paste_enabled.get(),
            );
        self.send(events);
        if external_paste {
            self.submit_external_stt_paste();
        }
    }

    pub fn forward_key_pulse(&self, event: &NSEvent) {
        let characters = event
            .charactersIgnoringModifiers()
            .map(|text| text.to_string());
        let (events, external_paste) = self
            .ivars()
            .input_routing
            .borrow_mut()
            .key_pulse_with_characters(
                event.keyCode(),
                characters.as_deref(),
                self.ivars().external_stt_paste_enabled.get(),
            );
        self.send(events);
        if external_paste {
            self.submit_external_stt_paste();
        }
    }

    fn command_modifier_pressed(&self) -> bool {
        self.ivars()
            .input_routing
            .borrow()
            .pressed_modifiers
            .iter()
            .any(|keycode| is_command_key(*keycode))
    }

    fn forward_menu_shortcut(&self) {
        if self.command_modifier_pressed() {
            let event = NSApplication::sharedApplication(self.mtm()).currentEvent();
            if menu_shortcut_keycode(event.as_deref()).is_some() {
                self.forward_key_pulse(event.as_ref().unwrap());
            }
        }
    }

    fn external_stt_pasteboard_text(&self) -> Option<String> {
        let pasteboard = NSPasteboard::generalPasteboard();
        let text = unsafe { pasteboard.stringForType(NSPasteboardTypeString) };
        let text = text.as_ref().map(|value| value.to_string());
        external_stt_paste_text(
            self.ivars().external_stt_paste_enabled.get(),
            text.as_deref(),
        )
    }

    fn submit_external_stt_paste(&self) {
        let Some(text) = self.external_stt_pasteboard_text() else {
            return;
        };
        if let Some(handle) = self.ivars().handle.borrow().as_ref() {
            handle.command(SessionCommand::PasteLocalClipboard(text));
        }
    }

    /// Apply a new server pointer shape (straight-alpha RGBA, remote pixels).
    pub fn set_pointer_bitmap(
        &self,
        rgba: Vec<u8>,
        width: u16,
        height: u16,
        hotspot_x: u16,
        hotspot_y: u16,
    ) {
        // Remote pixels -> view points, so the cursor matches the scale the
        // desktop is displayed at.
        let point_scale = match self.ivars().framebuffer.borrow().as_ref() {
            Some(fb) => {
                let (fb_w, _) = fb.dimensions();
                let bounds = self.bounds();
                if fb_w > 0 && bounds.size.width > 0.0 {
                    bounds.size.width / f64::from(fb_w)
                } else {
                    1.0
                }
            }
            None => 1.0,
        };
        let cursor = ui::make_remote_cursor(rgba, width, height, hotspot_x, hotspot_y, point_scale);
        self.apply_cursor(cursor);
    }

    /// Revert to the native arrow pointer.
    pub fn set_pointer_default(&self) {
        self.apply_cursor(None);
    }

    /// Hide the pointer over the remote desktop (transparent cursor, so it
    /// reappears as soon as it leaves the view).
    pub fn set_pointer_hidden(&self) {
        self.apply_cursor(ui::make_hidden_cursor());
    }

    fn apply_cursor(&self, cursor: Option<Retained<NSCursor>>) {
        *self.ivars().remote_cursor.borrow_mut() = cursor;
        if let Some(window) = self.window() {
            window.invalidateCursorRectsForView(self);
        }
    }

    /// Repaint from the current framebuffer contents. Prefers the zero-copy
    /// IOSurface path; falls back to a pooled CGImage.
    pub fn refresh(&self) {
        let fb = self.ivars().framebuffer.borrow().clone();
        if let Some(fb) = fb {
            if let Some(layer) = self.layer() {
                if !ui::upload_framebuffer_iosurface(&layer, &fb, &self.ivars().surface_pool) {
                    ui::upload_framebuffer(&layer, &fb, &self.ivars().present_pool);
                }
            }
        }
    }

    fn send(&self, events: Vec<InputEvent>) {
        if events.is_empty() {
            return;
        }
        if let Some(handle) = self.ivars().handle.borrow().as_ref() {
            handle.command(SessionCommand::Input(events));
        }
    }

    fn send_pointer_input(&self, events: Vec<InputEvent>) {
        let routed = self
            .ivars()
            .input_routing
            .borrow_mut()
            .pointer_input(events);
        self.send(routed);
    }

    /// Release every held key on the remote when we lose focus, so modifiers
    /// (Hyper key, ⌘Tab, Mission Control) never get stuck on the host.
    pub fn release_all_keys(&self) {
        self.ivars().input_routing.borrow_mut().clear();
        if let Some(handle) = self.ivars().handle.borrow().as_ref() {
            handle.command(SessionCommand::ReleaseAllKeys);
        }
    }

    fn mouse_button(&self, event: &NSEvent, button: PointerButton, down: bool) {
        if let Some((x, y)) = self.remote_point(event) {
            self.send_pointer_input(vec![InputEvent::MouseButton { button, down, x, y }]);
        }
    }

    fn mouse_move(&self, event: &NSEvent) {
        if let Some((x, y)) = self.remote_point(event) {
            self.send_pointer_input(vec![InputEvent::MouseMove { x, y }]);
        }
    }

    /// Map an event's window-space location to remote framebuffer pixels.
    fn remote_point(&self, event: &NSEvent) -> Option<(u16, u16)> {
        if !self.accepts_pointer_event(event) {
            return None;
        }
        let (fb_w, fb_h) = self.ivars().framebuffer.borrow().as_ref()?.dimensions();
        if fb_w == 0 || fb_h == 0 {
            return None;
        }
        let loc: NSPoint = event.locationInWindow();
        let local = self.convertPoint_fromView(loc, None);
        let bounds = self.bounds();
        if bounds.size.width <= 0.0 || bounds.size.height <= 0.0 {
            return None;
        }
        let nx = (local.x / bounds.size.width).clamp(0.0, 1.0);
        // Flip Y: AppKit origin is bottom-left, RDP is top-left.
        let ny = 1.0 - (local.y / bounds.size.height).clamp(0.0, 1.0);
        let x = ((nx * f64::from(fb_w)) as u16).min(fb_w - 1);
        let y = ((ny * f64::from(fb_h)) as u16).min(fb_h - 1);
        Some((x, y))
    }

    fn accepts_pointer_event(&self, event: &NSEvent) -> bool {
        let Some(window) = self.window() else {
            return false;
        };
        let local = self.convertPoint_fromView(event.locationInWindow(), None);
        pointer_event_allowed(
            event.r#type(),
            NSApplication::sharedApplication(self.mtm()).isActive(),
            window.isKeyWindow(),
            event.windowNumber() == window.windowNumber(),
            self.mouse_inRect(local, self.visibleRect()),
        )
    }
}

fn pointer_event_allowed(
    event_type: NSEventType,
    app_active: bool,
    key_window: bool,
    same_window: bool,
    inside_view: bool,
) -> bool {
    // AppKit keeps a drag and its matching release with the original view,
    // even outside its bounds. Losing focus releases held input separately.
    let captured = matches!(
        event_type,
        NSEventType::LeftMouseDragged
            | NSEventType::RightMouseDragged
            | NSEventType::OtherMouseDragged
            | NSEventType::LeftMouseUp
            | NSEventType::RightMouseUp
            | NSEventType::OtherMouseUp
    );
    app_active && key_window && same_window && (inside_view || captured)
}

const DEVICE_MODIFIER_MASKS: usize = 0x0000_20ff;

fn menu_shortcut_keycode(event: Option<&NSEvent>) -> Option<u16> {
    let event = event?;
    // Menu equivalents match characters, but Windows needs the original physical
    // key. keyCode must not be queried for mouse or accessibility menu actions.
    (event.r#type() == NSEventType::KeyDown).then(|| event.keyCode())
}

#[derive(Default)]
struct InputRoutingState {
    mac_shortcuts_enabled: bool,
    pressed_modifiers: HashSet<u16>,
    deferred_command_modifiers: HashSet<u16>,
    suppressed_command_modifiers: HashSet<u16>,
    suppressed_key_ups: HashSet<u16>,
    external_paste_active: bool,
}

impl InputRoutingState {
    fn key_down_with_characters(
        &mut self,
        keycode: u16,
        characters: Option<&str>,
        external_stt_paste_enabled: bool,
    ) -> (Vec<InputEvent>, bool) {
        if !self.translate_mac_shortcut(keycode, characters, external_stt_paste_enabled) {
            return self.key_down(keycode, external_stt_paste_enabled);
        }

        let mut events = self.suppress_command_modifiers();
        // Command key-up delivery is unreliable in AppKit. Keep the translated
        // chord balanced within one dispatch, including when the key repeats.
        events.extend([
            InputEvent::Key {
                keycode: 0x3b,
                down: true,
            },
            InputEvent::Key {
                keycode,
                down: true,
            },
            InputEvent::Key {
                keycode,
                down: false,
            },
            InputEvent::Key {
                keycode: 0x3b,
                down: false,
            },
        ]);
        self.suppressed_key_ups.insert(keycode);
        (events, false)
    }

    fn translate_mac_shortcut(
        &self,
        keycode: u16,
        characters: Option<&str>,
        external_stt_paste_enabled: bool,
    ) -> bool {
        self.mac_shortcuts_enabled
            && !(external_stt_paste_enabled && keycode == V_KEYCODE)
            && self
                .pressed_modifiers
                .iter()
                .any(|key| is_command_key(*key))
            && !self
                .pressed_modifiers
                .iter()
                .any(|key| matches!(key, 0x3b | 0x3e | 0x3a | 0x3d))
            && is_mac_shortcut(keycode, characters)
    }

    fn key_pulse_with_characters(
        &mut self,
        keycode: u16,
        characters: Option<&str>,
        external_stt_paste_enabled: bool,
    ) -> (Vec<InputEvent>, bool) {
        if self.translate_mac_shortcut(keycode, characters, external_stt_paste_enabled) {
            self.key_down_with_characters(keycode, characters, external_stt_paste_enabled)
        } else {
            self.key_pulse(keycode, external_stt_paste_enabled)
        }
    }

    fn is_pressed(&self, keycode: u16) -> bool {
        self.pressed_modifiers.contains(&keycode)
    }

    fn modifier_changed(
        &mut self,
        keycode: u16,
        down: bool,
        external_stt_paste_enabled: bool,
    ) -> Vec<InputEvent> {
        let changed = if down {
            self.pressed_modifiers.insert(keycode)
        } else {
            self.pressed_modifiers.remove(&keycode)
        };
        if !changed {
            return Vec::new();
        }
        if is_command_key(keycode) {
            if down && (external_stt_paste_enabled || self.mac_shortcuts_enabled) {
                self.deferred_command_modifiers.insert(keycode);
                return Vec::new();
            }
            if !down {
                if self.suppressed_command_modifiers.remove(&keycode) {
                    if self.suppressed_command_modifiers.is_empty() {
                        self.external_paste_active = false;
                    }
                    return Vec::new();
                }
                if self.deferred_command_modifiers.remove(&keycode) {
                    return vec![
                        InputEvent::Key {
                            keycode,
                            down: true,
                        },
                        InputEvent::Key {
                            keycode,
                            down: false,
                        },
                    ];
                }
            }
        }
        let mut events =
            if self.mac_shortcuts_enabled && down && matches!(keycode, 0x3b | 0x3e | 0x3a | 0x3d) {
                self.flush_deferred_command_modifiers()
            } else {
                Vec::new()
            };
        events.push(InputEvent::Key { keycode, down });
        events
    }

    fn key_down(
        &mut self,
        keycode: u16,
        external_stt_paste_enabled: bool,
    ) -> (Vec<InputEvent>, bool) {
        if is_command_key(keycode) {
            return (
                self.modifier_changed(keycode, true, external_stt_paste_enabled),
                false,
            );
        }
        if external_stt_paste_enabled
            && keycode == V_KEYCODE
            && self
                .pressed_modifiers
                .iter()
                .any(|key| is_command_key(*key))
        {
            return self.paste_invoked(true);
        }
        self.suppressed_key_ups.remove(&keycode);
        let mut events = self.flush_deferred_command_modifiers();
        events.push(InputEvent::Key {
            keycode,
            down: true,
        });
        (events, false)
    }

    fn key_up(&mut self, keycode: u16, external_stt_paste_enabled: bool) -> Vec<InputEvent> {
        if is_command_key(keycode) {
            return self.modifier_changed(keycode, false, external_stt_paste_enabled);
        }
        if self.suppressed_key_ups.remove(&keycode) {
            if keycode == V_KEYCODE {
                self.external_paste_active = false;
            }
            return Vec::new();
        }
        vec![InputEvent::Key {
            keycode,
            down: false,
        }]
    }

    fn key_pulse(
        &mut self,
        keycode: u16,
        external_stt_paste_enabled: bool,
    ) -> (Vec<InputEvent>, bool) {
        let (mut events, external_paste) = self.key_down(keycode, external_stt_paste_enabled);
        events.extend(self.key_up(keycode, external_stt_paste_enabled));
        (events, external_paste)
    }

    fn paste_invoked(&mut self, external_stt_paste_enabled: bool) -> (Vec<InputEvent>, bool) {
        if !external_stt_paste_enabled {
            return (Vec::new(), false);
        }
        let command_keys: Vec<_> = self
            .pressed_modifiers
            .iter()
            .copied()
            .filter(|key| is_command_key(*key))
            .collect();
        if !command_keys.is_empty() {
            self.suppressed_key_ups.insert(V_KEYCODE);
        }
        let submit = !self.external_paste_active;
        if !command_keys.is_empty() {
            self.external_paste_active = true;
        }

        (self.suppress_command_modifiers(), submit)
    }

    fn suppress_command_modifiers(&mut self) -> Vec<InputEvent> {
        let mut command_keys: Vec<_> = self
            .pressed_modifiers
            .iter()
            .copied()
            .filter(|key| is_command_key(*key))
            .collect();
        command_keys.sort_unstable();
        let mut events = Vec::new();
        for keycode in command_keys {
            if self.deferred_command_modifiers.remove(&keycode) {
                self.suppressed_command_modifiers.insert(keycode);
            } else if !self.suppressed_command_modifiers.contains(&keycode) {
                // The setting may have been enabled while Command was already
                // held. Release that forwarded modifier immediately.
                self.suppressed_command_modifiers.insert(keycode);
                events.push(InputEvent::Key {
                    keycode,
                    down: false,
                });
            }
        }
        events
    }

    fn flush_deferred_command_modifiers(&mut self) -> Vec<InputEvent> {
        let mut keycodes: Vec<_> = self.deferred_command_modifiers.drain().collect();
        if self.mac_shortcuts_enabled {
            keycodes.extend(self.suppressed_command_modifiers.drain());
        }
        keycodes.sort_unstable();
        keycodes
            .into_iter()
            .map(|keycode| InputEvent::Key {
                keycode,
                down: true,
            })
            .collect()
    }

    fn pointer_input(&mut self, mut events: Vec<InputEvent>) -> Vec<InputEvent> {
        // Moving the pointer must not commit a pending Mac shortcut to Win.
        // Clicks and scrolling still use the ordinary held modifier mapping.
        if self.mac_shortcuts_enabled
            && events
                .iter()
                .all(|event| matches!(event, InputEvent::MouseMove { .. }))
        {
            return events;
        }
        let mut routed = self.flush_deferred_command_modifiers();
        routed.append(&mut events);
        routed
    }

    fn clear(&mut self) {
        self.pressed_modifiers.clear();
        self.deferred_command_modifiers.clear();
        self.suppressed_command_modifiers.clear();
        self.suppressed_key_ups.clear();
        self.external_paste_active = false;
    }
}

const LEFT_COMMAND_KEYCODE: u16 = 0x37;
const RIGHT_COMMAND_KEYCODE: u16 = 0x36;
const V_KEYCODE: u16 = 0x09;

fn is_command_key(keycode: u16) -> bool {
    matches!(keycode, LEFT_COMMAND_KEYCODE | RIGHT_COMMAND_KEYCODE)
}

fn is_mac_shortcut(keycode: u16, characters: Option<&str>) -> bool {
    if let Some(text) = characters {
        let mut chars = text.chars();
        if let Some(character) = chars.next() {
            if chars.next().is_some() {
                return false;
            }
            if character.is_ascii() {
                return matches!(
                    character.to_ascii_lowercase(),
                    'c' | 'v' | 'x' | 'a' | 'z' | 'f' | 'w'
                );
            }
        }
    }
    // Non-Latin layouts use the conventional physical shortcut positions.
    matches!(keycode, 0x08 | 0x09 | 0x07 | 0x00 | 0x06 | 0x03 | 0x0d)
}

fn modifier_masks(keycode: u16) -> Option<(usize, usize)> {
    Some(match keycode {
        0x3b => (0x0000_0001, 0x0004_0000), // left control
        0x38 => (0x0000_0002, 0x0002_0000), // left shift
        0x3c => (0x0000_0004, 0x0002_0000), // right shift
        0x37 => (0x0000_0008, 0x0010_0000), // left command
        0x36 => (0x0000_0010, 0x0010_0000), // right command
        0x3a => (0x0000_0020, 0x0008_0000), // left option
        0x3d => (0x0000_0040, 0x0008_0000), // right option
        0x39 => (0x0000_0080, 0x0001_0000), // caps lock
        0x3e => (0x0000_2000, 0x0004_0000), // right control
        _ => return None,
    })
}

fn external_stt_paste_text(enabled: bool, text: Option<&str>) -> Option<String> {
    enabled
        .then_some(text)
        .flatten()
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::{
        external_stt_paste_text, menu_shortcut_keycode, modifier_masks, pointer_event_allowed,
        InputRoutingState,
    };
    use objc2::rc::Retained;
    use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType};
    use objc2_foundation::{NSPoint, NSString};
    use rdp123_core::InputEvent;

    #[test]
    fn pointer_events_stay_in_the_active_session() {
        for kind in [
            NSEventType::MouseMoved,
            NSEventType::LeftMouseDown,
            NSEventType::RightMouseDown,
            NSEventType::OtherMouseDown,
            NSEventType::ScrollWheel,
            NSEventType::LeftMouseDragged,
            NSEventType::LeftMouseUp,
        ] {
            assert!(pointer_event_allowed(kind, true, true, true, true));
            assert!(
                !pointer_event_allowed(kind, false, true, true, true),
                "inactive app: {kind:?}"
            );
            assert!(
                !pointer_event_allowed(kind, true, false, true, true),
                "inactive window: {kind:?}"
            );
            assert!(
                !pointer_event_allowed(kind, true, true, false, true),
                "other event window: {kind:?}"
            );
        }
        for kind in [
            NSEventType::MouseMoved,
            NSEventType::LeftMouseDown,
            NSEventType::ScrollWheel,
        ] {
            assert!(
                !pointer_event_allowed(kind, true, true, true, false),
                "outside desktop: {kind:?}"
            );
        }
    }

    #[test]
    fn captured_pointer_drag_and_release_can_leave_the_active_view() {
        for kind in [
            NSEventType::LeftMouseDragged,
            NSEventType::RightMouseDragged,
            NSEventType::OtherMouseDragged,
            NSEventType::LeftMouseUp,
            NSEventType::RightMouseUp,
            NSEventType::OtherMouseUp,
        ] {
            assert!(pointer_event_allowed(kind, true, true, true, false));
            assert!(!pointer_event_allowed(kind, false, true, true, false));
            assert!(!pointer_event_allowed(kind, true, false, true, false));
        }
    }

    fn key_events(events: &[InputEvent]) -> Vec<(u16, bool)> {
        events
            .iter()
            .map(|event| match event {
                InputEvent::Key { keycode, down } => (*keycode, *down),
                _ => panic!("unexpected non-key event: {event:?}"),
            })
            .collect()
    }

    #[test]
    fn mac_copy_shortcut_sends_balanced_control_without_windows_key() {
        let mut routing = InputRoutingState {
            mac_shortcuts_enabled: true,
            ..Default::default()
        };
        let mut events = routing.modifier_changed(0x37, true, false);
        let (shortcut, paste) = routing.key_down_with_characters(0x08, Some("c"), false);
        events.extend(shortcut);
        events.extend(routing.key_up(0x08, false));
        events.extend(routing.modifier_changed(0x37, false, false));

        assert!(!paste);
        assert_eq!(
            key_events(&events),
            [(0x3b, true), (0x08, true), (0x08, false), (0x3b, false)]
        );
    }

    #[test]
    fn mac_shortcuts_preserve_physical_keys_layout_shift_and_repeats() {
        for (keycode, characters) in [
            (0x08, "c"),
            (0x09, "v"),
            (0x07, "x"),
            (0x00, "a"),
            (0x06, "z"),
            (0x03, "f"),
            (0x0d, "w"),
            (0x10, "z"), // QWERTZ
            (0x06, "w"), // AZERTY
            (0x08, "с"), // Cyrillic, physical fallback
        ] {
            for command in [0x36, 0x37] {
                for shift in [None, Some(0x38), Some(0x3c)] {
                    let mut routing = InputRoutingState {
                        mac_shortcuts_enabled: true,
                        ..Default::default()
                    };
                    if let Some(shift) = shift {
                        assert_eq!(
                            key_events(&routing.modifier_changed(shift, true, false)),
                            [(shift, true)]
                        );
                    }
                    assert!(routing.modifier_changed(command, true, false).is_empty());
                    for _ in 0..3 {
                        let (events, paste) =
                            routing.key_pulse_with_characters(keycode, Some(characters), false);
                        assert!(!paste);
                        assert_eq!(
                            key_events(&events),
                            [
                                (0x3b, true),
                                (keycode, true),
                                (keycode, false),
                                (0x3b, false)
                            ]
                        );
                    }
                    assert!(routing.key_up(keycode, false).is_empty());
                    assert!(routing.modifier_changed(command, false, false).is_empty());
                    if let Some(shift) = shift {
                        assert_eq!(
                            key_events(&routing.modifier_changed(shift, false, false)),
                            [(shift, false)]
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn mac_shortcut_detection_does_not_remap_other_latin_keys() {
        assert!(!super::is_mac_shortcut(0x00, Some("q"))); // AZERTY Q, not A
        assert!(!super::is_mac_shortcut(0x06, Some("y"))); // QWERTZ Y, not Z
        assert!(!super::is_mac_shortcut(0x08, Some("cc")));
        assert!(!super::is_mac_shortcut(0x30, Some("\t")));
        assert!(super::is_mac_shortcut(0x08, Some("C")));
        assert!(super::is_mac_shortcut(0x08, None));
    }

    #[test]
    fn mac_shortcuts_are_opt_in_and_require_command_without_control_or_option() {
        for enabled in [false, true] {
            for extra in [None, Some(0x3b), Some(0x3e), Some(0x3a), Some(0x3d)] {
                let mut routing = InputRoutingState {
                    mac_shortcuts_enabled: enabled,
                    ..Default::default()
                };
                let (plain, _) = routing.key_pulse_with_characters(0x08, Some("c"), false);
                assert_eq!(key_events(&plain), [(0x08, true), (0x08, false)]);
                let mut events = routing.modifier_changed(0x37, true, false);
                if let Some(extra) = extra {
                    events.extend(routing.modifier_changed(extra, true, false));
                }
                events.extend(routing.key_pulse_with_characters(0x08, Some("c"), false).0);
                events.extend(routing.modifier_changed(0x37, false, false));
                if let Some(extra) = extra {
                    events.extend(routing.modifier_changed(extra, false, false));
                }
                let events = key_events(&events);
                if enabled && extra.is_none() {
                    assert_eq!(
                        events,
                        [(0x3b, true), (0x08, true), (0x08, false), (0x3b, false)]
                    );
                } else {
                    assert_eq!(
                        events
                            .iter()
                            .filter(|(key, _)| *key == 0x37)
                            .copied()
                            .collect::<Vec<_>>(),
                        [(0x37, true), (0x37, false)]
                    );
                    assert_eq!(
                        events
                            .iter()
                            .filter(|(key, _)| *key == 0x08)
                            .copied()
                            .collect::<Vec<_>>(),
                        [(0x08, true), (0x08, false)]
                    );
                    assert_eq!(
                        events.iter().filter(|(key, _)| *key == 0x3b).count(),
                        if extra == Some(0x3b) { 2 } else { 0 }
                    );
                }
            }
        }
    }

    #[test]
    fn mac_shortcuts_switch_between_windows_and_control_during_one_command_hold() {
        let mut routing = InputRoutingState {
            mac_shortcuts_enabled: true,
            ..Default::default()
        };
        assert!(routing.modifier_changed(0x37, true, false).is_empty());
        assert_eq!(
            key_events(&routing.key_pulse_with_characters(0x0e, Some("e"), false).0),
            [(0x37, true), (0x0e, true), (0x0e, false)]
        );
        assert_eq!(
            key_events(&routing.key_pulse_with_characters(0x08, Some("c"), false).0),
            [
                (0x37, false),
                (0x3b, true),
                (0x08, true),
                (0x08, false),
                (0x3b, false)
            ]
        );
        assert_eq!(
            key_events(&routing.key_pulse_with_characters(0x0f, Some("r"), false).0),
            [(0x37, true), (0x0f, true), (0x0f, false)]
        );
        assert_eq!(
            key_events(&routing.modifier_changed(0x37, false, false)),
            [(0x37, false)]
        );
    }

    #[test]
    fn mac_shortcuts_do_not_swallow_key_up_when_option_is_added() {
        let mut routing = InputRoutingState {
            mac_shortcuts_enabled: true,
            ..Default::default()
        };
        routing.modifier_changed(0x37, true, false);
        routing.key_pulse_with_characters(0x08, Some("c"), false);
        assert_eq!(
            key_events(&routing.modifier_changed(0x3a, true, false)),
            [(0x37, true), (0x3a, true)]
        );
        assert_eq!(
            key_events(&routing.key_pulse_with_characters(0x08, Some("c"), false).0),
            [(0x08, true), (0x08, false)]
        );
        assert_eq!(
            key_events(&routing.modifier_changed(0x37, false, false)),
            [(0x37, false)]
        );
        assert_eq!(
            key_events(&routing.modifier_changed(0x3a, false, false)),
            [(0x3a, false)]
        );
    }

    #[test]
    fn mac_shortcuts_suppress_both_command_keys_but_preserve_a_lone_tap() {
        let mut routing = InputRoutingState {
            mac_shortcuts_enabled: true,
            ..Default::default()
        };
        routing.modifier_changed(0x36, true, false);
        routing.modifier_changed(0x37, true, false);
        routing.key_pulse_with_characters(0x08, Some("c"), false);
        assert!(routing.modifier_changed(0x36, false, false).is_empty());
        assert!(routing.modifier_changed(0x37, false, false).is_empty());
        routing.modifier_changed(0x37, true, false);
        assert_eq!(
            key_events(&routing.modifier_changed(0x37, false, false)),
            [(0x37, true), (0x37, false)]
        );
    }

    #[test]
    fn mac_shortcuts_preserve_stt_paste_priority_and_focus_reset() {
        let mut routing = InputRoutingState {
            mac_shortcuts_enabled: true,
            ..Default::default()
        };
        routing.modifier_changed(0x37, true, true);
        routing.key_pulse_with_characters(0x08, Some("c"), true);
        let (events, paste) = routing.key_pulse_with_characters(0x09, Some("v"), true);
        assert!(paste);
        assert!(events.is_empty());
        assert!(routing.modifier_changed(0x37, false, true).is_empty());

        routing.modifier_changed(0x37, true, false);
        routing.key_pulse_with_characters(0x08, Some("c"), false);
        routing.clear();
        assert!(routing.mac_shortcuts_enabled);
        assert!(routing.modifier_changed(0x37, false, false).is_empty());
        assert_eq!(
            key_events(&routing.key_pulse_with_characters(0x08, Some("c"), false).0),
            [(0x08, true), (0x08, false)]
        );
    }

    #[test]
    fn mac_shortcut_control_mapping_is_unaffected_by_command_option_swap() {
        let mut routing = InputRoutingState {
            mac_shortcuts_enabled: true,
            ..Default::default()
        };
        routing.modifier_changed(0x37, true, false);
        let events = routing.key_pulse_with_characters(0x08, Some("c"), false).0;
        for swap in [false, true] {
            let scancodes: Vec<_> = key_events(&events)
                .into_iter()
                .map(|(key, down)| {
                    let code = rdp123_core::keymap::mac_keycode_to_scancode(key, swap).unwrap();
                    (code.code, code.extended, down)
                })
                .collect();
            assert_eq!(
                scancodes,
                [
                    (0x1d, false, true),
                    (0x2e, false, true),
                    (0x2e, false, false),
                    (0x1d, false, false)
                ]
            );
        }
    }

    #[test]
    fn mac_shortcuts_survive_pointer_movement_but_clicks_restore_command() {
        let mut routing = InputRoutingState {
            mac_shortcuts_enabled: true,
            ..Default::default()
        };
        routing.modifier_changed(0x37, true, false);
        assert!(matches!(
            routing
                .pointer_input(vec![InputEvent::MouseMove { x: 1, y: 2 }])
                .as_slice(),
            [InputEvent::MouseMove { x: 1, y: 2 }]
        ));
        let (events, _) = routing.key_pulse_with_characters(0x08, Some("c"), false);
        assert_eq!(
            key_events(&events),
            [(0x3b, true), (0x08, true), (0x08, false), (0x3b, false)]
        );
        assert!(matches!(
            routing
                .pointer_input(vec![InputEvent::MouseMove { x: 3, y: 4 }])
                .as_slice(),
            [InputEvent::MouseMove { x: 3, y: 4 }]
        ));
        assert!(matches!(
            routing
                .pointer_input(vec![InputEvent::MouseButton {
                    button: rdp123_core::PointerButton::Left,
                    down: true,
                    x: 3,
                    y: 4,
                }])
                .as_slice(),
            [
                InputEvent::Key {
                    keycode: 0x37,
                    down: true
                },
                InputEvent::MouseButton { .. }
            ]
        ));
        assert_eq!(
            key_events(&routing.modifier_changed(0x37, false, false)),
            [(0x37, false)]
        );
    }

    #[test]
    fn pointer_routing_preserves_stt_only_behavior_and_command_scrolling() {
        let mut routing = InputRoutingState::default();
        routing.modifier_changed(0x37, true, true);
        assert!(matches!(
            routing
                .pointer_input(vec![InputEvent::MouseMove { x: 1, y: 2 }])
                .as_slice(),
            [
                InputEvent::Key {
                    keycode: 0x37,
                    down: true
                },
                InputEvent::MouseMove { .. }
            ]
        ));

        let mut routing = InputRoutingState {
            mac_shortcuts_enabled: true,
            ..Default::default()
        };
        routing.modifier_changed(0x37, true, false);
        assert!(matches!(
            routing
                .pointer_input(vec![InputEvent::Wheel {
                    delta: 120,
                    horizontal: false
                }])
                .as_slice(),
            [
                InputEvent::Key {
                    keycode: 0x37,
                    down: true
                },
                InputEvent::Wheel { .. }
            ]
        ));
    }

    fn command_key_event(characters: &str, keycode: u16) -> Retained<NSEvent> {
        let characters = NSString::from_str(characters);
        NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
            NSEventType::KeyDown,
            NSPoint::new(0.0, 0.0),
            NSEventModifierFlags::Command,
            0.0,
            0,
            None,
            &characters,
            &characters,
            false,
            keycode,
        ).expect("create keyboard event")
    }

    #[test]
    fn menu_shortcut_preserves_swiss_german_physical_key() {
        let event = command_key_event("z", 0x10);
        let mut routing = InputRoutingState::default();
        routing.modifier_changed(0x37, true, true);

        let keycode = menu_shortcut_keycode(Some(&event)).expect("keyboard shortcut");
        let (events, external_paste) = routing.key_pulse(keycode, true);

        assert!(!external_paste);
        assert!(
            matches!(
                events.as_slice(),
                [
                    InputEvent::Key {
                        keycode: 0x37,
                        down: true
                    },
                    InputEvent::Key {
                        keycode: 0x10,
                        down: true
                    },
                    InputEvent::Key {
                        keycode: 0x10,
                        down: false
                    },
                ]
            ),
            "menu shortcut changed the physical key: {events:?}"
        );
    }

    #[test]
    fn menu_shortcut_ignores_non_keyboard_actions() {
        let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            NSEventType::LeftMouseUp,
            NSPoint::new(0.0, 0.0),
            NSEventModifierFlags::Command,
            0.0,
            0,
            None,
            0,
            1,
            0.0,
        ).expect("create mouse event");

        assert_eq!(menu_shortcut_keycode(Some(&event)), None);
        assert_eq!(menu_shortcut_keycode(None), None);
    }

    #[test]
    fn menu_shortcut_keeps_stt_paste_out_of_remote_keyboard_input() {
        let event = command_key_event("v", 0x09);
        let mut routing = InputRoutingState::default();
        assert!(routing.modifier_changed(0x37, true, true).is_empty());

        let keycode = menu_shortcut_keycode(Some(&event)).expect("keyboard shortcut");
        let (events, external_paste) = routing.key_pulse(keycode, true);

        assert!(external_paste);
        assert!(events.is_empty());
        assert!(routing.modifier_changed(0x37, false, true).is_empty());
    }

    #[test]
    fn modifier_keycodes_use_distinct_device_masks() {
        assert_ne!(
            modifier_masks(0x38).unwrap().0,
            modifier_masks(0x3c).unwrap().0
        );
        assert_ne!(
            modifier_masks(0x37).unwrap().0,
            modifier_masks(0x36).unwrap().0
        );
        assert_eq!(modifier_masks(0x00), None);
    }

    #[test]
    fn external_stt_paste_requires_the_global_setting_and_text() {
        assert_eq!(external_stt_paste_text(false, Some("dictated text")), None);
        assert_eq!(external_stt_paste_text(true, None), None);
        assert_eq!(external_stt_paste_text(true, Some("")), None);
        assert_eq!(
            external_stt_paste_text(true, Some("dictated text")),
            Some("dictated text".to_string())
        );
    }

    #[test]
    fn external_stt_paste_shortcut_never_reaches_windows() {
        let mut routing = InputRoutingState::default();
        let mut remote_events = routing.modifier_changed(0x37, true, true);
        let (paste_events, submit) = routing.paste_invoked(true);
        remote_events.extend(paste_events);
        remote_events.extend(routing.key_up(0x09, true));
        remote_events.extend(routing.modifier_changed(0x37, false, true));

        assert!(submit);
        assert!(
            remote_events.is_empty(),
            "the macOS Command+V shortcut leaked into the remote session"
        );
    }

    #[test]
    fn stt_runtime_regression_synthetic_command_key_events_never_reach_windows() {
        let mut routing = InputRoutingState::default();
        let (mut remote_events, _) = routing.key_down(0x37, true);
        let (paste_events, submit) = routing.paste_invoked(true);
        remote_events.extend(paste_events);
        remote_events.extend(routing.key_up(0x09, true));
        remote_events.extend(routing.key_up(0x37, true));

        assert!(submit);
        assert!(
            remote_events.is_empty(),
            "synthetic Command down/up leaked into the remote session"
        );
    }

    #[test]
    fn ordinary_command_shortcuts_still_reach_windows() {
        let mut routing = InputRoutingState::default();
        assert!(routing.modifier_changed(0x37, true, true).is_empty());

        let (events, external_paste) = routing.key_down(0x08, true);

        assert!(!external_paste);
        assert!(matches!(
            events.as_slice(),
            [
                InputEvent::Key {
                    keycode: 0x37,
                    down: true
                },
                InputEvent::Key {
                    keycode: 0x08,
                    down: true
                }
            ]
        ));
        assert!(matches!(
            routing.modifier_changed(0x37, false, true).as_slice(),
            [InputEvent::Key {
                keycode: 0x37,
                down: false
            }]
        ));
    }

    #[test]
    fn appkit_command_shortcuts_include_the_missing_key_release() {
        let mut routing = InputRoutingState::default();
        assert!(routing.modifier_changed(0x37, true, true).is_empty());

        let (events, external_paste) = routing.key_pulse(0x0f, true);

        assert!(!external_paste);
        assert!(matches!(
            events.as_slice(),
            [
                InputEvent::Key {
                    keycode: 0x37,
                    down: true
                },
                InputEvent::Key {
                    keycode: 0x0f,
                    down: true
                },
                InputEvent::Key {
                    keycode: 0x0f,
                    down: false
                }
            ]
        ));
        assert!(matches!(
            routing.modifier_changed(0x37, false, true).as_slice(),
            [InputEvent::Key {
                keycode: 0x37,
                down: false
            }]
        ));
    }

    #[test]
    fn command_input_is_unchanged_when_external_stt_paste_is_disabled() {
        let mut routing = InputRoutingState::default();

        assert!(matches!(
            routing.modifier_changed(0x37, true, false).as_slice(),
            [InputEvent::Key {
                keycode: 0x37,
                down: true
            }]
        ));
        assert!(matches!(
            routing.modifier_changed(0x37, false, false).as_slice(),
            [InputEvent::Key {
                keycode: 0x37,
                down: false
            }]
        ));
    }

    #[test]
    fn command_tap_is_preserved_when_external_stt_paste_is_enabled() {
        let mut routing = InputRoutingState::default();
        assert!(routing.modifier_changed(0x37, true, true).is_empty());

        assert!(matches!(
            routing.modifier_changed(0x37, false, true).as_slice(),
            [
                InputEvent::Key {
                    keycode: 0x37,
                    down: true
                },
                InputEvent::Key {
                    keycode: 0x37,
                    down: false
                }
            ]
        ));
    }

    #[test]
    fn duplicate_key_and_menu_paste_dispatch_submits_only_once() {
        let mut routing = InputRoutingState::default();
        routing.modifier_changed(0x37, true, true);

        let (_, key_dispatch_submits) = routing.key_down(0x09, true);
        let (_, menu_dispatch_submits) = routing.paste_invoked(true);

        assert!(key_dispatch_submits);
        assert!(!menu_dispatch_submits);
    }

    #[test]
    fn command_release_finishes_paste_when_appkit_consumes_v_key_up() {
        let mut routing = InputRoutingState::default();
        routing.modifier_changed(0x37, true, true);
        let (_, first_submits) = routing.paste_invoked(true);
        routing.modifier_changed(0x37, false, true);

        routing.modifier_changed(0x37, true, true);
        let (_, second_submits) = routing.paste_invoked(true);

        assert!(first_submits);
        assert!(second_submits);
    }
}

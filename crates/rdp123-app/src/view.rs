//! The custom `NSView` that renders the remote framebuffer and forwards input.

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSCursor, NSEvent, NSView};
use objc2_foundation::{NSPoint, NSRect};

use rdp123_core::{InputEvent, PointerButton, SessionCommand, SessionHandle, SharedFramebuffer};

use crate::ui;

#[derive(Default)]
pub struct RdpViewIvars {
    handle: RefCell<Option<SessionHandle>>,
    framebuffer: RefCell<Option<Arc<SharedFramebuffer>>>,
    /// Physical modifier keys currently held (by macOS key code), so
    /// `flagsChanged:` can be turned into discrete down/up events.
    pressed_modifiers: RefCell<HashSet<u16>>,
    /// The server-provided pointer shape; `None` shows the native arrow.
    remote_cursor: RefCell<Option<Retained<NSCursor>>>,
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
            self.send(vec![InputEvent::Key { keycode: event.keyCode(), down: true }]);
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, event: &NSEvent) {
            self.send(vec![InputEvent::Key { keycode: event.keyCode(), down: false }]);
        }

        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, event: &NSEvent) {
            let keycode = event.keyCode();
            let Some((device_mask, family_mask)) = modifier_masks(keycode) else {
                return;
            };
            let flags = event.modifierFlags().0;
            let mut pressed = self.ivars().pressed_modifiers.borrow_mut();
            let down = if flags & DEVICE_MODIFIER_MASKS != 0 {
                flags & device_mask != 0
            } else if flags & family_mask == 0 {
                false
            } else {
                !pressed.contains(&keycode)
            };
            let changed = if down {
                pressed.insert(keycode)
            } else {
                pressed.remove(&keycode)
            };
            drop(pressed);
            if changed {
                self.send(vec![InputEvent::Key { keycode, down }]);
            }
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
                self.send(events);
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
        if let Some(handle) = self.ivars().handle.borrow().as_ref() {
            handle.command(SessionCommand::Input(events));
        }
    }

    /// Release every held key on the remote when we lose focus, so modifiers
    /// (Hyper key, ⌘Tab, Mission Control) never get stuck on the host.
    pub fn release_all_keys(&self) {
        self.ivars().pressed_modifiers.borrow_mut().clear();
        if let Some(handle) = self.ivars().handle.borrow().as_ref() {
            handle.command(SessionCommand::ReleaseAllKeys);
        }
    }

    fn mouse_button(&self, event: &NSEvent, button: PointerButton, down: bool) {
        if let Some((x, y)) = self.remote_point(event) {
            self.send(vec![InputEvent::MouseButton { button, down, x, y }]);
        }
    }

    fn mouse_move(&self, event: &NSEvent) {
        if let Some((x, y)) = self.remote_point(event) {
            self.send(vec![InputEvent::MouseMove { x, y }]);
        }
    }

    /// Map an event's window-space location to remote framebuffer pixels.
    fn remote_point(&self, event: &NSEvent) -> Option<(u16, u16)> {
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
}

const DEVICE_MODIFIER_MASKS: usize = 0x0000_20ff;

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

#[cfg(test)]
mod tests {
    use super::modifier_masks;

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
}

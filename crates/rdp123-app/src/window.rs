//! A session window: the borderless-ish remote view plus resize and clipboard
//! plumbing. `WindowController` is the window's delegate and the clipboard
//! timer's target.

use std::cell::{Cell, RefCell};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{
    define_class, msg_send, sel, AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly,
};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBackingStoreType, NSPasteboard, NSPasteboardTypeString,
    NSProgressIndicator, NSProgressIndicatorStyle, NSScreen, NSTextField, NSTrackingArea,
    NSTrackingAreaOptions, NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{NSNotification, NSObjectProtocol, NSString, NSTimer};
use objc2_quartz_core::kCAFilterLinear;

use rdp123_core::{
    ClipboardMode, RdpOptions, ResolutionMode, ScalingLevel, SessionCommand, SessionHandle,
};

use crate::delegate;
use crate::ui;
use crate::view::RdpView;

const CLIPBOARD_POLL_SECONDS: f64 = 0.5;
const DEFAULT_CONTENT_W: f64 = 1280.0;
const DEFAULT_CONTENT_H: f64 = 800.0;

#[derive(Default)]
pub struct WindowControllerIvars {
    window: RefCell<Option<Retained<NSWindow>>>,
    view: RefCell<Option<Retained<RdpView>>>,
    handle: RefCell<Option<SessionHandle>>,
    timer: RefCell<Option<Retained<NSTimer>>>,
    status_label: RefCell<Option<Retained<NSTextField>>>,
    status_spinner: RefCell<Option<Retained<NSProgressIndicator>>>,
    connection_id: RefCell<String>,
    title: RefCell<String>,
    window_id: Cell<u64>,
    scaling: Cell<ScalingLevel>,
    resolution_mode: Cell<ResolutionMode>,
    fixed_resolution: Cell<Option<(u16, u16)>>,
    remember_size: Cell<bool>,
    clipboard_mode: Cell<ClipboardMode>,
    last_change_count: Cell<isize>,
}

define_class!(
    #[unsafe(super(objc2_foundation::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RDP123WindowController"]
    #[ivars = WindowControllerIvars]
    pub struct WindowController;

    unsafe impl NSObjectProtocol for WindowController {}

    unsafe impl NSWindowDelegate for WindowController {
        #[unsafe(method(windowDidResize:))]
        fn window_did_resize(&self, _notification: &NSNotification) {
            // Live-drag steps are ignored; the end-of-resize handler covers them.
            // This still fires for full-screen and programmatic changes.
            let in_live = self
                .ivars()
                .view
                .borrow()
                .as_ref()
                .map(|v| v.inLiveResize())
                .unwrap_or(false);
            if !in_live {
                self.send_resize();
            }
        }

        #[unsafe(method(windowDidEndLiveResize:))]
        fn window_did_end_live_resize(&self, _notification: &NSNotification) {
            self.send_resize();
        }

        #[unsafe(method(windowDidResignKey:))]
        fn window_did_resign_key(&self, _notification: &NSNotification) {
            if let Some(view) = self.ivars().view.borrow().as_ref() {
                view.release_all_keys();
            }
        }

        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            if let Some(timer) = self.ivars().timer.borrow_mut().take() {
                timer.invalidate();
            }
            if let Some(view) = self.ivars().view.borrow().as_ref() {
                view.release_all_keys();
            }
            if let Some(handle) = self.ivars().handle.borrow().as_ref() {
                handle.command(SessionCommand::Shutdown);
            }
            self.save_size_if_enabled();
            // Deferred: dropping the last controller retain inside its own
            // delegate callback would deallocate `self` mid-call.
            let window_id = self.ivars().window_id.get();
            DispatchQueue::main().exec_async(move || delegate::remove_window(window_id));
        }
    }

    impl WindowController {
        #[unsafe(method(pollClipboard:))]
        fn poll_clipboard(&self, _timer: &NSTimer) {
            self.poll_clipboard_impl();
        }
    }
);

impl WindowController {
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(WindowControllerIvars::default());
        unsafe { msg_send![super(this), init] }
    }

    /// Build and show the window. Call `initial_size` afterwards to learn the
    /// framebuffer size to request, then `attach_session`.
    pub fn setup(
        &self,
        mtm: MainThreadMarker,
        window_id: u64,
        connection_id: &str,
        title: &str,
        opts: &RdpOptions,
    ) {
        self.ivars().window_id.set(window_id);
        self.ivars().scaling.set(opts.scaling);
        self.ivars().resolution_mode.set(opts.resolution_mode);
        self.ivars().fixed_resolution.set(opts.resolution);
        self.ivars().remember_size.set(opts.remember_size);
        self.ivars().clipboard_mode.set(opts.clipboard);
        *self.ivars().connection_id.borrow_mut() = connection_id.to_string();
        *self.ivars().title.borrow_mut() = title.to_string();

        // Initial window size: fixed resolution (clamped), the remembered last
        // size (fit-to-window), or a default. A full-screen start sizes to the
        // screen up front, because the session connects before the async
        // full-screen transition finishes.
        let (content_width, content_height) = match (opts.resolution_mode, opts.resolution) {
            (ResolutionMode::Fixed, Some((w, h))) => {
                (f64::from(w).min(2400.0), f64::from(h).min(1400.0))
            }
            _ if opts.fullscreen => match NSScreen::mainScreen(mtm) {
                Some(screen) => {
                    let size = screen.visibleFrame().size;
                    (size.width, size.height)
                }
                None => (DEFAULT_CONTENT_W, DEFAULT_CONTENT_H),
            },
            _ => match opts.last_window_size.filter(|_| opts.remember_size) {
                Some((w, h)) => (
                    f64::from(w).clamp(480.0, 4000.0),
                    f64::from(h).clamp(360.0, 3000.0),
                ),
                None => (DEFAULT_CONTENT_W, DEFAULT_CONTENT_H),
            },
        };

        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;
        let frame = ui::rect(content_width, content_height);
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                frame,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        window.setTitle(&NSString::from_str(&format!("{title} — connecting…")));
        unsafe { window.setReleasedWhenClosed(false) };
        window.setAcceptsMouseMovedEvents(true);

        let view = RdpView::new(mtm, ui::rect(content_width, content_height));
        view.setWantsLayer(true);
        if let Some(layer) = view.layer() {
            layer.setContentsScale(window.backingScaleFactor());
            // Linear (not nearest) so a scaled framebuffer isn't blocky/pixelated.
            unsafe { layer.setMagnificationFilter(kCAFilterLinear) };
        }

        let tracking = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                view.bounds(),
                NSTrackingAreaOptions::MouseEnteredAndExited
                    | NSTrackingAreaOptions::MouseMoved
                    | NSTrackingAreaOptions::ActiveInKeyWindow
                    | NSTrackingAreaOptions::InVisibleRect,
                Some(&view),
                None,
            )
        };
        view.addTrackingArea(&tracking);

        let spinner = NSProgressIndicator::initWithFrame(
            NSProgressIndicator::alloc(mtm),
            ui::rect(32.0, 32.0),
        );
        spinner.setStyle(NSProgressIndicatorStyle::Spinning);
        spinner.setIndeterminate(true);
        spinner.setDisplayedWhenStopped(false);
        spinner.setFrameOrigin(objc2_foundation::NSPoint::new(
            (content_width - 32.0) / 2.0,
            content_height / 2.0 + 8.0,
        ));
        spinner.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewMinXMargin
                | NSAutoresizingMaskOptions::ViewMaxXMargin
                | NSAutoresizingMaskOptions::ViewMinYMargin
                | NSAutoresizingMaskOptions::ViewMaxYMargin,
        );
        unsafe { spinner.startAnimation(None) };
        view.addSubview(&spinner);

        let status_label = NSTextField::labelWithString(&NSString::from_str("Connecting…"), mtm);
        status_label.setAlignment(objc2_app_kit::NSTextAlignment::Center);
        status_label.setFrame(objc2_core_foundation::CGRect::new(
            objc2_core_foundation::CGPoint::new(20.0, content_height / 2.0 - 28.0),
            objc2_core_foundation::CGSize::new(content_width - 40.0, 24.0),
        ));
        status_label.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewMinYMargin
                | NSAutoresizingMaskOptions::ViewMaxYMargin,
        );
        view.addSubview(&status_label);

        window.setContentView(Some(&view));
        window.setDelegate(Some(ProtocolObject::from_ref(self)));
        window.center();
        window.makeKeyAndOrderFront(None);
        window.makeFirstResponder(Some(&view));
        if opts.fullscreen {
            window.toggleFullScreen(None);
        }

        *self.ivars().window.borrow_mut() = Some(window);
        *self.ivars().view.borrow_mut() = Some(view);
        *self.ivars().status_label.borrow_mut() = Some(status_label);
        *self.ivars().status_spinner.borrow_mut() = Some(spinner);
    }

    /// Whether the remote resolution should follow the window (fit-to-window).
    pub fn dynamic_resolution(&self) -> bool {
        self.ivars().resolution_mode.get() == ResolutionMode::FitToWindow
    }

    /// The framebuffer size (and optional scale) to request for this window.
    pub fn initial_size(&self) -> (u16, u16, Option<u32>) {
        self.current_size()
    }

    pub fn attach_session(&self, handle: SessionHandle) {
        if let Some(view) = self.ivars().view.borrow().as_ref() {
            view.set_session(handle.clone());
        }
        *self.ivars().handle.borrow_mut() = Some(handle);
        if self.ivars().clipboard_mode.get().allow_local_to_remote() {
            self.start_clipboard_timer();
        }
    }

    /// Raise the session after the app's activation policy has switched from
    /// menu-bar accessory to a regular foreground application.
    pub fn bring_to_front(&self) {
        let window = self.ivars().window.borrow();
        let Some(window) = window.as_ref() else {
            return;
        };
        window.makeKeyAndOrderFront(None);
        if let Some(view) = self.ivars().view.borrow().as_ref() {
            window.makeFirstResponder(Some(view));
        }
    }

    pub fn connection_id(&self) -> String {
        self.ivars().connection_id.borrow().clone()
    }

    /// Persist the current window size so the next connect reopens at this size.
    /// Only for fit-to-window sessions that aren't full-screen.
    fn save_size_if_enabled(&self) {
        if !self.ivars().remember_size.get()
            || self.ivars().resolution_mode.get() != ResolutionMode::FitToWindow
        {
            return;
        }
        let window = self.ivars().window.borrow();
        let Some(window) = window.as_ref() else {
            return;
        };
        if window.styleMask().contains(NSWindowStyleMask::FullScreen) {
            return;
        }
        let view = self.ivars().view.borrow();
        let Some(view) = view.as_ref() else { return };
        let size = view.bounds().size;
        let (w, h) = (size.width as u16, size.height as u16);
        if w >= 480 && h >= 360 {
            delegate::save_window_size(&self.ivars().connection_id.borrow(), w, h);
        }
    }

    pub fn refresh(&self) {
        if let Some(view) = self.ivars().view.borrow().as_ref() {
            view.refresh();
        }
    }

    pub fn set_pointer_bitmap(
        &self,
        rgba: Vec<u8>,
        width: u16,
        height: u16,
        hotspot_x: u16,
        hotspot_y: u16,
    ) {
        if let Some(view) = self.ivars().view.borrow().as_ref() {
            view.set_pointer_bitmap(rgba, width, height, hotspot_x, hotspot_y);
        }
    }

    pub fn set_pointer_default(&self) {
        if let Some(view) = self.ivars().view.borrow().as_ref() {
            view.set_pointer_default();
        }
    }

    pub fn set_pointer_hidden(&self) {
        if let Some(view) = self.ivars().view.borrow().as_ref() {
            view.set_pointer_hidden();
        }
    }

    /// Reflect reconnect state in the window title.
    pub fn set_reconnecting(&self, on: bool) {
        if let Some(window) = self.ivars().window.borrow().as_ref() {
            let base = self.ivars().title.borrow().clone();
            let title = if on {
                format!("{base} — reconnecting…")
            } else {
                base
            };
            window.setTitle(&NSString::from_str(&title));
        }
        if let Some(label) = self.ivars().status_label.borrow().as_ref() {
            label.setStringValue(&NSString::from_str("Reconnecting…"));
            label.setHidden(!on);
        }
        if let Some(spinner) = self.ivars().status_spinner.borrow().as_ref() {
            spinner.setHidden(!on);
            if on {
                unsafe { spinner.startAnimation(None) };
            } else {
                unsafe { spinner.stopAnimation(None) };
            }
        }
    }

    pub fn close(&self) {
        if let Some(window) = self.ivars().window.borrow().as_ref() {
            window.close();
        }
    }

    /// Write remote clipboard text locally without triggering our own poll.
    pub fn set_clipboard(&self, text: &str) {
        if !self.ivars().clipboard_mode.get().allow_remote_to_local() {
            return;
        }
        let pasteboard = NSPasteboard::generalPasteboard();
        pasteboard.clearContents();
        let ok = unsafe {
            pasteboard.setString_forType(&NSString::from_str(text), NSPasteboardTypeString)
        };
        let _ = ok;
        self.ivars().last_change_count.set(pasteboard.changeCount());
    }

    /// The remote framebuffer size (and optional server scale %) to request.
    fn current_size(&self) -> (u16, u16, Option<u32>) {
        let scaling = self.ivars().scaling.get();
        // Fixed resolution: request exactly the chosen size; the view scales it.
        if self.ivars().resolution_mode.get() == ResolutionMode::Fixed {
            let (w, h) = self
                .ivars()
                .fixed_resolution
                .get()
                .unwrap_or((DEFAULT_CONTENT_W as u16, DEFAULT_CONTENT_H as u16));
            return (w & !1, h, scaling.percent().or(Some(100)));
        }

        // Fit to window: size follows the view.
        let view = self.ivars().view.borrow();
        let Some(view) = view.as_ref() else {
            return (0, 0, None);
        };
        let bounds = view.bounds();
        let retina = self
            .ivars()
            .window
            .borrow()
            .as_ref()
            .map(|w| w.backingScaleFactor() >= 1.5)
            .unwrap_or(false);

        match scaling.percent() {
            // Explicit DPI: request physical pixels at that scale.
            Some(pct) => {
                let backing = view.convertSizeToBacking(bounds.size);
                (even(backing.width), backing.height as u16, Some(pct))
            }
            // Auto: Retina => physical pixels at 200%, otherwise points at 100%.
            None if retina => {
                let backing = view.convertSizeToBacking(bounds.size);
                (even(backing.width), backing.height as u16, Some(200))
            }
            None => (
                even(bounds.size.width),
                bounds.size.height as u16,
                Some(100),
            ),
        }
    }

    fn send_resize(&self) {
        let (width, height, scale) = self.current_size();
        if width == 0 || height == 0 {
            return;
        }
        if let Some(handle) = self.ivars().handle.borrow().as_ref() {
            handle.command(SessionCommand::Resize {
                width,
                height,
                scale,
            });
        }
    }

    fn start_clipboard_timer(&self) {
        let pasteboard = NSPasteboard::generalPasteboard();
        self.ivars().last_change_count.set(pasteboard.changeCount());
        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                CLIPBOARD_POLL_SECONDS,
                self as &AnyObject,
                sel!(pollClipboard:),
                None,
                true,
            )
        };
        *self.ivars().timer.borrow_mut() = Some(timer);
    }

    fn poll_clipboard_impl(&self) {
        let is_key = self
            .ivars()
            .window
            .borrow()
            .as_ref()
            .map(|w| w.isKeyWindow())
            .unwrap_or(false);
        if !is_key {
            return;
        }
        let pasteboard = NSPasteboard::generalPasteboard();
        let count = pasteboard.changeCount();
        if count == self.ivars().last_change_count.get() {
            return;
        }
        self.ivars().last_change_count.set(count);
        if let Some(text) = unsafe { pasteboard.stringForType(NSPasteboardTypeString) } {
            let text = text.to_string();
            if !text.is_empty() {
                if let Some(handle) = self.ivars().handle.borrow().as_ref() {
                    handle.command(SessionCommand::LocalClipboard(text));
                }
            }
        }
    }
}

fn even(v: f64) -> u16 {
    let n = v as u16;
    n & !1
}

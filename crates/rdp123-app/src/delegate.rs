//! The application delegate: menu-bar item, connection menu, connect flow, and
//! the bridge that delivers session events back onto the main thread.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dispatch2::{DispatchQueue, DispatchTime};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly, Message};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSMenu, NSMenuDelegate,
    NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{ns_string, NSNotification, NSObject, NSObjectProtocol, NSString};

use rdp123_core::{
    secrets, spawn_session, terminal, Connection, ConnectionKind, PasswordPolicy, ProfileStore,
    SessionConfig, SessionEvent,
};

use crate::settings::SettingsController;
use crate::ui;
use crate::window::WindowController;

thread_local! {
    static DELEGATE: RefCell<Option<Retained<AppDelegate>>> = const { RefCell::new(None) };
}

/// Deliver a session event to its window. Called on the main thread.
pub fn deliver(window_id: u64, event: SessionEvent) {
    let delegate = DELEGATE.with(|d| d.borrow().clone());
    if let Some(delegate) = delegate {
        delegate.handle_session_event(window_id, event);
    }
}

/// Forget a closed window. Called from `windowWillClose:` on the main thread.
pub fn remove_window(window_id: u64) {
    DELEGATE.with(|d| {
        if let Some(delegate) = d.borrow().as_ref() {
            delegate.ivars().borrow_mut().windows.remove(&window_id);
            delegate.update_activation_policy();
        }
    });
}

/// Persist a session window's size for its connection. Called on close.
pub fn save_window_size(connection_id: &str, width: u16, height: u16) {
    DELEGATE.with(|d| {
        if let Some(delegate) = d.borrow().as_ref() {
            let store = delegate.ivars().borrow().store.clone();
            if let Err(e) = store.set_last_window_size(connection_id, (width, height)) {
                tracing::warn!("could not save window size: {e}");
            }
        }
    });
}

pub struct AppState {
    store: ProfileStore,
    /// Snapshot backing the current menu; menu tags index into this.
    connections: Vec<Connection>,
    status_item: Option<Retained<NSStatusItem>>,
    settings: Option<Retained<SettingsController>>,
    windows: HashMap<u64, Retained<WindowController>>,
    next_id: u64,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RDP123AppDelegate"]
    #[ivars = RefCell<AppState>]
    pub struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            let mtm = self.mtm();
            DELEGATE.with(|d| *d.borrow_mut() = Some(self.retain()));

            let menu = NSMenu::new(mtm);
            menu.setAutoenablesItems(false);
            let delegate: &ProtocolObject<dyn NSMenuDelegate> = ProtocolObject::from_ref(self);
            menu.setDelegate(Some(delegate));

            let status = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
            if let Some(button) = status.button(mtm) {
                match ui::menu_bar_icon() {
                    Some(icon) => button.setImage(Some(&icon)),
                    None => button.setTitle(&NSString::from_str("RDP")),
                }
            }
            status.setMenu(Some(&menu));
            self.rebuild_menu(&menu, mtm);
            self.ivars().borrow_mut().status_item = Some(status);
        }
    }

    unsafe impl NSMenuDelegate for AppDelegate {
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            let mtm = self.mtm();
            self.rebuild_menu(menu, mtm);
        }
    }

    impl AppDelegate {
        #[unsafe(method(openConnection:))]
        fn open_connection(&self, sender: Option<&AnyObject>) {
            let Some(sender) = sender else { return };
            let tag: isize = unsafe { msg_send![sender, tag] };
            if tag >= 0 {
                self.open_index(tag as usize);
            }
        }

        #[unsafe(method(openSettings:))]
        fn open_settings(&self, _sender: Option<&AnyObject>) {
            self.show_settings();
        }

        /// Quit via our own selector: macOS attaches an automatic icon to
        /// menu items wired to the well-known `terminate:` action, which
        /// breaks the menu's left alignment.
        #[unsafe(method(quitApp:))]
        fn quit_app(&self, _sender: Option<&AnyObject>) {
            NSApplication::sharedApplication(self.mtm()).terminate(None);
        }
    }
);

impl AppDelegate {
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let store = ProfileStore::open_default().expect("resolve config directory");
        let state = AppState {
            store,
            connections: Vec::new(),
            status_item: None,
            settings: None,
            windows: HashMap::new(),
            next_id: 1,
        };
        let this = Self::alloc(mtm).set_ivars(RefCell::new(state));
        unsafe { msg_send![super(this), init] }
    }

    fn rebuild_menu(&self, menu: &NSMenu, mtm: MainThreadMarker) {
        menu.removeAllItems();

        let store = self.ivars().borrow().store.clone();
        let (connections, load_failed) = match store.load() {
            Ok(connections) => (connections, false),
            Err(error) => {
                tracing::error!("could not load connections: {error:#}");
                let item = unsafe {
                    NSMenuItem::initWithTitle_action_keyEquivalent(
                        NSMenuItem::alloc(mtm),
                        &NSString::from_str("Connections file is invalid — open Settings"),
                        None,
                        ns_string!(""),
                    )
                };
                item.setEnabled(false);
                menu.addItem(&item);
                (Vec::new(), true)
            }
        };

        if connections.is_empty() && !load_failed {
            let empty = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str("No connections — edit to add"),
                    None,
                    ns_string!(""),
                )
            };
            empty.setEnabled(false);
            menu.addItem(&empty);
        }

        for (index, connection) in connections.iter().enumerate() {
            let label = match connection.kind {
                ConnectionKind::Rdp => connection.name.clone(),
                ConnectionKind::Ssh => format!("{} (SSH)", connection.name),
            };
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str(&label),
                    Some(sel!(openConnection:)),
                    ns_string!(""),
                )
            };
            item.setTag(index as isize);
            let target: &AnyObject = self;
            unsafe { item.setTarget(Some(target)) };
            menu.addItem(&item);
        }

        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let settings = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Settings…"),
                Some(sel!(openSettings:)),
                ns_string!(","),
            )
        };
        let target: &AnyObject = self;
        unsafe { settings.setTarget(Some(target)) };
        menu.addItem(&settings);

        let quit = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Quit RDP123"),
                Some(sel!(quitApp:)),
                ns_string!("q"),
            )
        };
        let target: &AnyObject = self;
        unsafe { quit.setTarget(Some(target)) };
        menu.addItem(&quit);

        self.ivars().borrow_mut().connections = connections;
    }

    fn open_index(&self, index: usize) {
        let connection = self.ivars().borrow().connections.get(index).cloned();
        let Some(connection) = connection else { return };
        match connection.kind {
            ConnectionKind::Rdp => self.open_rdp(connection),
            ConnectionKind::Ssh => self.open_ssh(&connection),
        }
    }

    fn open_ssh(&self, connection: &Connection) {
        let store = self.ivars().borrow().store.clone();
        let settings = match store.load_document() {
            Ok(document) => document.settings,
            Err(error) => {
                ui::show_error(self.mtm(), "Could not load settings", &format!("{error:#}"));
                return;
            }
        };
        if let Err(e) = terminal::launch_ssh(&settings, connection) {
            ui::show_error(self.mtm(), "Could not start SSH session", &e.to_string());
        }
    }

    /// Obtain the password: use the saved Keychain entry silently when present,
    /// otherwise prompt. The prompt offers a "remember" checkbox that saves the
    /// password and stops asking on future connects.
    fn resolve_password(&self, mtm: MainThreadMarker, connection: &Connection) -> Option<String> {
        let remember = connection.rdp.password_policy == PasswordPolicy::Remember;
        if remember {
            match secrets::load_password(&connection.id) {
                Ok(Some(p)) => {
                    // Re-create the item so it is owned by the current app
                    // identity. Items written by an earlier build otherwise
                    // carry a stale access list and prompt on every read.
                    if let Err(e) = secrets::store_password(&connection.id, &p) {
                        tracing::warn!("could not refresh keychain item: {e:#}");
                    }
                    return Some(p);
                }
                Ok(None) => {}
                Err(e) => ui::show_error(mtm, "Keychain error", &format!("{e:#}")),
            }
        }

        // Default the checkbox to on for "remember" connections.
        let (password, save) = ui::prompt_password(mtm, &connection.name, remember)?;
        if save {
            if let Err(e) = secrets::store_password(&connection.id, &password) {
                ui::show_error(mtm, "Could not save password", &format!("{e:#}"));
            } else if !remember {
                // The user chose to remember: persist the switch so we stop asking.
                let store = self.ivars().borrow().store.clone();
                if let Err(e) = store.set_password_policy(&connection.id, PasswordPolicy::Remember)
                {
                    tracing::warn!("could not update password policy: {e}");
                }
            }
        }
        Some(password)
    }

    fn open_rdp(&self, connection: Connection) {
        let mtm = self.mtm();

        let password = match self.resolve_password(mtm, &connection) {
            Some(p) => p,
            None => return,
        };

        let window_id = {
            let mut state = self.ivars().borrow_mut();
            let id = state.next_id;
            state.next_id += 1;
            id
        };

        let controller = WindowController::new(mtm);
        controller.setup(
            mtm,
            window_id,
            &connection.id,
            &connection.name,
            &connection.rdp,
        );

        let (width, height, scale) = controller.initial_size();
        let opts = &connection.rdp;
        let swap_cmd_alt = {
            let store = self.ivars().borrow().store.clone();
            store
                .load_document()
                .map(|document| document.settings.swap_cmd_alt)
                .unwrap_or(false)
        };
        let config = SessionConfig {
            host: connection.host.clone(),
            port: connection.port,
            username: connection.username.clone(),
            password,
            domain: connection.domain.clone(),
            width,
            height,
            scale,
            expected_fingerprint: connection.cert_fingerprint.clone(),
            color_depth: opts.color_quality.bits(),
            compression: opts.compression,
            clipboard: opts.clipboard,
            audio: opts.audio,
            graphics: opts.graphics,
            dynamic_resolution: controller.dynamic_resolution(),
            reconnect: opts.reconnect,
            reconnect_per_minute: opts.reconnect_per_minute,
            swap_cmd_alt,
            wake_mac: opts.wake_mac.clone(),
        };

        let frame_pending = Arc::new(AtomicBool::new(false));
        let frame_dirty = Arc::new(AtomicBool::new(false));
        let event_cb: Box<dyn Fn(SessionEvent) + Send> = Box::new(move |event| {
            if matches!(
                event,
                SessionEvent::FrameUpdated { .. } | SessionEvent::Resized { .. }
            ) {
                if frame_pending
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    // Coalesced into the open window; the trailing repaint
                    // will show these pixels.
                    frame_dirty.store(true, Ordering::Release);
                    return;
                }
                let frame_pending = frame_pending.clone();
                let frame_dirty = frame_dirty.clone();
                // Leading paint: repaint immediately so a lone update (typing,
                // cursor feedback) carries no added latency.
                DispatchQueue::main().exec_async(move || deliver(window_id, event));
                // Trailing paint: only when updates were coalesced while the
                // window was open (up to ~120 fps under a sustained stream —
                // ProMotion-friendly — no redundant repaint for a lone update).
                let when = DispatchTime::try_from(Duration::from_millis(8))
                    .expect("8 ms fits in dispatch time");
                let _ = DispatchQueue::main().after(when, move || {
                    frame_pending.store(false, Ordering::Release);
                    if frame_dirty.swap(false, Ordering::AcqRel) {
                        deliver(
                            window_id,
                            SessionEvent::FrameUpdated {
                                x: 0,
                                y: 0,
                                width: 0,
                                height: 0,
                            },
                        );
                    }
                });
            } else {
                DispatchQueue::main().exec_async(move || deliver(window_id, event));
            }
        });
        let handle = spawn_session(config, event_cb);
        controller.attach_session(handle);

        self.ivars()
            .borrow_mut()
            .windows
            .insert(window_id, controller.clone());
        // A session window should appear in the Dock and ⌘-Tab so it can be
        // raised again; the app drops back to menu-bar-only when none are open.
        self.update_activation_policy();
        let app = NSApplication::sharedApplication(mtm);
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
        controller.bring_to_front();
    }

    /// Show a Dock icon / ⌘-Tab entry while any session window is open, and
    /// revert to an accessory (menu-bar-only) app when the last one closes.
    fn update_activation_policy(&self) {
        let mtm = self.mtm();
        let has_windows = !self.ivars().borrow().windows.is_empty();
        let policy = if has_windows {
            NSApplicationActivationPolicy::Regular
        } else {
            NSApplicationActivationPolicy::Accessory
        };
        NSApplication::sharedApplication(mtm).setActivationPolicy(policy);
    }

    fn show_settings(&self) {
        let mtm = self.mtm();
        let existing = self.ivars().borrow().settings.clone();
        let controller = match existing {
            Some(c) => c,
            None => {
                let store = self.ivars().borrow().store.clone();
                let c = SettingsController::new(mtm, store);
                self.ivars().borrow_mut().settings = Some(c.clone());
                c
            }
        };
        controller.show(mtm);
        NSApplication::sharedApplication(mtm).activate();
    }

    fn handle_session_event(&self, window_id: u64, event: SessionEvent) {
        let mtm = self.mtm();
        let controller = self.ivars().borrow().windows.get(&window_id).cloned();
        let Some(controller) = controller else { return };

        match event {
            SessionEvent::Connected { .. } => {
                controller.set_reconnecting(false);
                controller.refresh();
            }
            SessionEvent::FrameUpdated { .. } | SessionEvent::Resized { .. } => {
                controller.refresh()
            }
            SessionEvent::PointerBitmap {
                width,
                height,
                hotspot_x,
                hotspot_y,
                rgba,
            } => controller.set_pointer_bitmap(rgba, width, height, hotspot_x, hotspot_y),
            SessionEvent::PointerDefault => controller.set_pointer_default(),
            SessionEvent::PointerHidden => controller.set_pointer_hidden(),
            SessionEvent::Reconnecting => controller.set_reconnecting(true),
            SessionEvent::ClipboardText(text) => controller.set_clipboard(&text),
            SessionEvent::ClipboardFiles(items) => controller.offer_remote_files(items),
            SessionEvent::CertificateApproval {
                fingerprint,
                is_change,
                reply,
            } => {
                let ok = ui::prompt_certificate(mtm, &fingerprint, is_change);
                let _ = reply.send(ok);
            }
            SessionEvent::CertTrusted { fingerprint } => {
                let store = self.ivars().borrow().store.clone();
                if let Err(e) = store.set_fingerprint(&controller.connection_id(), &fingerprint) {
                    ui::show_error(mtm, "Could not save server key", &format!("{e:#}"));
                }
            }
            SessionEvent::Disconnected { reason } => {
                // Close first so the dead window doesn't linger behind the
                // dialog; a normal remote logoff needs no dialog at all.
                controller.close();
                if reason != rdp123_core::REMOTE_ENDED {
                    ui::show_error(mtm, "Disconnected", &reason);
                }
            }
            SessionEvent::Error(message) => {
                controller.close();
                ui::show_error(mtm, "Connection failed", &message);
            }
        }
    }
}

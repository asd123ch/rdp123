//! The Settings window: manage connections and global settings.
//!
//! Layout: a "Connections | Global | About" segmented control switches panes. The
//! Connection pane has the connection list plus a grouped editor; the Global
//! pane has the shared SSH-terminal setting (entered once, used by every SSH
//! connection).
//!
//! Save model: editing a connection stages changes in the form and marks it
//! dirty; nothing is written until **Save** (or the confirm-on-switch dialog).
//! **Revert** re-loads from disk. Global settings are simple and auto-save.
//! Programmatic form population is guarded by `loading` so it never writes back
//! or marks dirty; `updating` guards a programmatic re-selection from recursing.

#![allow(clippy::too_many_arguments)]

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly, Message};
use objc2_app_kit::{
    NSApplication, NSAutoresizingMaskOptions, NSBackingStoreType, NSBorderType, NSBox, NSBoxType,
    NSButton, NSCellImagePosition, NSColor, NSControlStateValueOff, NSControlStateValueOn,
    NSControlTextEditingDelegate, NSFont, NSImage, NSImageView, NSPasteboard,
    NSPasteboardTypeString, NSPopUpButton, NSScrollView, NSSecureTextField,
    NSSegmentSwitchTracking, NSSegmentedControl, NSTableColumn, NSTableView, NSTableViewDataSource,
    NSTableViewDelegate, NSTextAlignment, NSTextField, NSTextFieldDelegate,
    NSUserInterfaceItemIdentification, NSView, NSWindow, NSWindowDelegate, NSWindowStyleMask,
    NSWorkspace,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{
    NSArray, NSIndexSet, NSInteger, NSNotification, NSObject, NSObjectProtocol, NSString, NSURL,
};

use rdp123_core::{
    secrets, AudioMode, AuthenticationMode, ClipboardMode, ColorQuality, Connection,
    ConnectionKind, Document, GraphicsMode, PasswordPolicy, ProfileStore, ResolutionMode,
    ScalingLevel, TerminalKind,
};

use crate::ui::{self, UnsavedChoice};

// Popup item orders (index <-> enum).
const COLOR: [ColorQuality; 2] = [ColorQuality::High32, ColorQuality::Medium16];
const CLIP: [ClipboardMode; 4] = [
    ClipboardMode::Bidirectional,
    ClipboardMode::Disabled,
    ClipboardMode::LocalToRemote,
    ClipboardMode::RemoteToLocal,
];
const SCALING: [ScalingLevel; 5] = [
    ScalingLevel::Auto,
    ScalingLevel::Percent100,
    ScalingLevel::Percent140,
    ScalingLevel::Percent180,
    ScalingLevel::Percent200,
];
const AUDIO: [AudioMode; 3] = [
    AudioMode::ThisComputer,
    AudioMode::Never,
    AudioMode::RemoteComputer,
];
const GRAPHICS: [GraphicsMode; 2] = [GraphicsMode::Egfx, GraphicsMode::Classic];
const RESMODE: [ResolutionMode; 2] = [ResolutionMode::FitToWindow, ResolutionMode::Fixed];
const PWPOLICY: [PasswordPolicy; 2] = [PasswordPolicy::Remember, PasswordPolicy::AlwaysAsk];
const AUTHENTICATION: [AuthenticationMode; 2] =
    [AuthenticationMode::Password, AuthenticationMode::EntraWeb];

// Window / layout geometry.
const W: f64 = 800.0;
const H: f64 = 760.0;
const CH: f64 = 716.0; // container height (below the segmented control)
const SEG_H: f64 = 24.0;
const FORM_H: f64 = 1040.0;
const EDIT_TOP: f64 = FORM_H - 32.0;
const FORM_X: f64 = 16.0;
const LABEL_W: f64 = 144.0;
const FIELD_X: f64 = 172.0;
const FIELD_W: f64 = 350.0;
const ROW_H: f64 = 22.0;
const PITCH: f64 = 32.0;
const CHECKBOX_PITCH: f64 = 28.0;
const HDR_PITCH: f64 = 34.0;
const ABOUT_LIST_HEIGHT: f64 = 344.0;

fn about_library_row_y(row: usize) -> f64 {
    ABOUT_LIST_HEIGHT - 24.0 - row as f64 * 24.0
}

struct AboutLibrary<'a> {
    name: String,
    version: &'a str,
    crates: Vec<&'a str>,
}

fn about_libraries(metadata: &str) -> Vec<AboutLibrary<'_>> {
    let mut libraries = Vec::new();
    let mut frameworks: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for entry in metadata.split(';').filter(|entry| !entry.is_empty()) {
        let (name, version) = entry.split_once(' ').unwrap_or((entry, ""));
        if name.starts_with("objc2-") {
            frameworks.entry(version).or_default().push(name);
        } else {
            libraries.push(AboutLibrary {
                name: name.into(),
                version,
                crates: vec![name],
            });
        }
    }
    for (version, crates) in frameworks {
        libraries.push(AboutLibrary {
            name: if crates.len() > 1 {
                format!("objc2 frameworks ({})", crates.len())
            } else {
                crates[0].into()
            },
            version,
            crates,
        });
    }
    libraries.sort_by(|a, b| a.name.cmp(&b.name).then(a.version.cmp(b.version)));
    libraries
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
}

fn index_of<T: PartialEq>(items: &[T], value: &T) -> isize {
    items.iter().position(|v| v == value).unwrap_or(0) as isize
}

type Field = RefCell<Option<Retained<NSTextField>>>;
type Secure = RefCell<Option<Retained<NSSecureTextField>>>;
type Popup = RefCell<Option<Retained<NSPopUpButton>>>;
type Check = RefCell<Option<Retained<NSButton>>>;

#[derive(Default)]
pub struct SettingsIvars {
    store: RefCell<Option<ProfileStore>>,
    document: RefCell<Document>,
    selected: Cell<isize>,
    loading: Cell<bool>,
    /// Guards a programmatic table re-selection from recursing into the handler.
    updating: Cell<bool>,
    /// The selected connection has unsaved edits in the form.
    dirty: Cell<bool>,
    built: Cell<bool>,
    window: RefCell<Option<Retained<NSWindow>>>,
    table: RefCell<Option<Retained<NSTableView>>>,
    segmented: RefCell<Option<Retained<NSSegmentedControl>>>,
    conn_pane: RefCell<Option<Retained<NSScrollView>>>,
    connection_form: RefCell<Option<Retained<NSView>>>,
    global_pane: RefCell<Option<Retained<NSScrollView>>>,
    about_pane: RefCell<Option<Retained<NSScrollView>>>,
    save_button: Check,
    revert_button: Check,
    remove_button: Check,
    duplicate_button: Check,
    /// Shown in the editor area when no connection is selected.
    empty_label: RefCell<Option<Retained<NSTextField>>>,
    /// Editor controls shared by RDP and SSH (hidden when nothing is selected).
    common_group: RefCell<Vec<Retained<NSView>>>,

    // Common (RDP + SSH)
    name: Field,
    kind: Popup,
    host: Field,
    port: Field,
    user: Field,

    // RDP
    authentication: Popup,
    password: Secure,
    pw_policy: Popup,
    domain: Field,
    color: Popup,
    clipboard: Popup,
    scaling: Popup,
    res_mode: Popup,
    res_w: Field,
    res_h: Field,
    rate: Field,
    wake_mac: Field,
    compression: Check,
    fullscreen: Check,
    remember_size: Check,
    audio: Popup,
    graphics: Popup,
    reconnect: Check,
    keep_alive: Check,

    // Global
    terminal: Popup,
    custom: Field,
    swap_cmd_alt: Check,
    mac_shortcuts: Check,
    external_stt_paste: Check,
    /// Launch-at-login. Not persisted in the document: the checkbox mirrors
    /// the system's `SMAppService` status.
    launch_at_login: Check,

    rdp_group: RefCell<Vec<Retained<NSView>>>,
    password_auth_group: RefCell<Vec<Retained<NSView>>>,
    entra_auth_group: RefCell<Vec<Retained<NSView>>>,
    ssh_group: RefCell<Vec<Retained<NSView>>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RDP123SettingsController"]
    #[ivars = SettingsIvars]
    pub struct SettingsController;

    unsafe impl NSObjectProtocol for SettingsController {}

    unsafe impl NSTableViewDataSource for SettingsController {
        #[unsafe(method(numberOfRowsInTableView:))]
        fn number_of_rows(&self, _t: &NSTableView) -> NSInteger {
            self.ivars().document.borrow().connections.len() as NSInteger
        }

        #[unsafe(method_id(tableView:objectValueForTableColumn:row:))]
        fn object_value(
            &self,
            _t: &NSTableView,
            _c: &NSTableColumn,
            row: NSInteger,
        ) -> Option<Retained<AnyObject>> {
            self.ivars().document.borrow().connections.get(row as usize).map(|c| {
                let label = match c.kind {
                    ConnectionKind::Rdp => c.name.clone(),
                    ConnectionKind::Ssh => format!("{} (SSH)", c.name),
                };
                let s = NSString::from_str(&label);
                let any: &AnyObject = &s;
                any.retain()
            })
        }
    }

    unsafe impl NSControlTextEditingDelegate for SettingsController {
        #[unsafe(method(controlTextDidEndEditing:))]
        fn text_end(&self, _n: &NSNotification) {
            // Global text saves when editing ends. Connection text is already
            // tracked by controlTextDidChange; marking it here would report a
            // false edit whenever focus moves to another connection.
            if self.pane() == 1 {
                self.save_global();
            }
        }

        // Mark dirty on every keystroke, not only on end-editing, so closing
        // the window mid-edit still triggers the save prompt.
        #[unsafe(method(controlTextDidChange:))]
        fn text_changed(&self, _n: &NSNotification) {
            if self.pane() == 0 {
                self.mark_dirty();
            }
        }
    }

    unsafe impl NSTextFieldDelegate for SettingsController {}

    unsafe impl NSTableViewDelegate for SettingsController {
        #[unsafe(method(tableViewSelectionDidChange:))]
        fn selection_changed(&self, _n: &NSNotification) {
            self.handle_selection_change();
        }
    }

    unsafe impl NSWindowDelegate for SettingsController {
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _n: &NSObject) -> bool {
            self.confirm_discard_ok()
        }
    }

    impl SettingsController {
        #[unsafe(method(addConnection:))]
        fn add(&self, _s: Option<&AnyObject>) {
            if !self.confirm_discard_ok() {
                return;
            }
            let mut document = self.ivars().document.borrow().clone();
            let mut connection = Connection::new("New Connection", ConnectionKind::Rdp);
            connection.host = "localhost".to_string();
            document.connections.push(connection);
            let new_index = document.connections.len() as isize - 1;
            if !self.save_document(&mut document) {
                return;
            }
            *self.ivars().document.borrow_mut() = document;
            self.ivars().selected.set(-1);
            self.reload_table();
            self.select_row(new_index);
            // Put the cursor straight into the name so typing replaces the stub.
            if let (Some(w), Some(name)) = (
                self.ivars().window.borrow().as_ref(),
                self.ivars().name.borrow().as_ref(),
            ) {
                w.makeFirstResponder(Some(name));
            }
        }

        #[unsafe(method(duplicateConnection:))]
        fn duplicate(&self, _s: Option<&AnyObject>) {
            if !self.confirm_discard_ok() {
                return;
            }
            let row = self.ivars().selected.get();
            let mut document = self.ivars().document.borrow().clone();
            let Some(original) = usize::try_from(row).ok()
                .and_then(|row| document.connections.get(row)).cloned() else {
                return;
            };
            let Some(index) = document.duplicate_connection(&original.id) else {
                return;
            };
            let copy_id = document.connections[index].id.clone();
            let copied_password = if original.kind == ConnectionKind::Rdp
                && original.rdp.authentication == AuthenticationMode::Password
                && original.rdp.password_policy == PasswordPolicy::Remember
            {
                match secrets::copy_password(&original.id, &copy_id) {
                    Ok(copied) => copied,
                    Err(error) => {
                        ui::show_error(self.mtm(), "Could not duplicate connection", &format!("{error:#}"));
                        return;
                    }
                }
            } else {
                false
            };
            if !self.save_document(&mut document) {
                if copied_password {
                    if let Err(error) = secrets::delete_password(&copy_id) {
                        ui::show_error(self.mtm(), "Could not remove copied password", &format!("{error:#}"));
                    }
                }
                return;
            }
            *self.ivars().document.borrow_mut() = document;
            self.ivars().selected.set(-1);
            self.reload_table();
            self.select_row(index as isize);
            if let (Some(window), Some(name)) = (
                self.ivars().window.borrow().as_ref(),
                self.ivars().name.borrow().as_ref(),
            ) {
                window.makeFirstResponder(Some(name));
            }
        }

        #[unsafe(method(removeConnection:))]
        fn remove(&self, _s: Option<&AnyObject>) {
            if !self.confirm_discard_ok() {
                return;
            }
            let row = self.ivars().selected.get();
            if row < 0 {
                return;
            }
            let Some(connection) = self
                .ivars()
                .document
                .borrow()
                .connections
                .get(row as usize)
                .cloned()
            else {
                return;
            };
            if !ui::confirm_delete(self.mtm(), &connection.name) {
                return;
            }
            let mut document = self.ivars().document.borrow().clone();
            document.connections.remove(row as usize);
            if !self.save_document(&mut document) {
                return;
            }
            *self.ivars().document.borrow_mut() = document;
            if let Err(error) = secrets::delete_password(&connection.id) {
                ui::show_error(self.mtm(), "Could not remove saved password", &format!("{error:#}"));
            }
            self.ivars().selected.set(-1);
            self.ivars().dirty.set(false);
            self.reload_table();
            let len = self.ivars().document.borrow().connections.len() as isize;
            self.select_row(if len == 0 { -1 } else { row.min(len - 1) });
        }

        #[unsafe(method(saveConnection:))]
        fn save(&self, _s: Option<&AnyObject>) {
            if self.commit_connection() {
                self.ivars().dirty.set(false);
                self.update_dirty_ui();
                self.reload_table();
            }
        }

        #[unsafe(method(revertConnection:))]
        fn revert(&self, _s: Option<&AnyObject>) {
            let row = self.ivars().selected.get();
            self.populate(row);
        }

        #[unsafe(method(markDirty:))]
        fn mark_dirty_action(&self, _s: Option<&AnyObject>) {
            self.mark_dirty();
        }

        #[unsafe(method(resModeChanged:))]
        fn res_mode_changed(&self, _s: Option<&AnyObject>) {
            self.mark_dirty();
            self.update_fixed_enabled();
        }

        #[unsafe(method(authenticationChanged:))]
        fn authentication_changed(&self, _s: Option<&AnyObject>) {
            self.mark_dirty();
            self.update_visibility();
        }

        #[unsafe(method(typeChanged:))]
        fn type_changed(&self, _s: Option<&AnyObject>) {
            self.mark_dirty();
            self.sync_default_port();
            self.update_visibility();
        }

        #[unsafe(method(paneChanged:))]
        fn pane_changed(&self, _s: Option<&AnyObject>) {
            self.update_visibility();
        }

        #[unsafe(method(globalChanged:))]
        fn global_changed(&self, _s: Option<&AnyObject>) {
            self.save_global();
        }

        #[unsafe(method(loginItemChanged:))]
        fn login_item_changed(&self, _s: Option<&AnyObject>) {
            let enable = self.check_on(&self.ivars().launch_at_login);
            if let Err(error) = crate::login_item::set_enabled(enable) {
                // Revert to the actual system state and surface the error.
                self.set_check(&self.ivars().launch_at_login, crate::login_item::is_enabled());
                ui::show_error(self.mtm(), "Could not update the login item", &error);
            }
        }

        #[unsafe(method(openLink:))]
        fn open_link(&self, sender: Option<&AnyObject>) {
            let Some(sender) = sender else { return };
            let url: Option<Retained<NSString>> = unsafe { msg_send![sender, identifier] };
            if let Some(url) = url.and_then(|url| NSURL::URLWithString(&url)) {
                NSWorkspace::sharedWorkspace().openURL(&url);
            }
        }

        #[unsafe(method(copyVersionInfo:))]
        fn copy_version_info(&self, _s: Option<&AnyObject>) {
            let info = format!(
                "RDP123 {} ({}), built {}",
                env!("CARGO_PKG_VERSION"),
                env!("RDP123_GIT"),
                env!("RDP123_BUILD_TIME"),
            );
            let pasteboard = NSPasteboard::generalPasteboard();
            pasteboard.clearContents();
            unsafe {
                pasteboard.setString_forType(&NSString::from_str(&info), NSPasteboardTypeString);
            }
        }
    }
);

impl SettingsController {
    pub fn new(mtm: MainThreadMarker, store: ProfileStore) -> Retained<Self> {
        let ivars = SettingsIvars::default();
        *ivars.store.borrow_mut() = Some(store);
        ivars.selected.set(-1);
        let this = Self::alloc(mtm).set_ivars(ivars);
        unsafe { msg_send![super(this), init] }
    }

    pub fn show(&self, mtm: MainThreadMarker) {
        if !self.ivars().built.get() {
            self.build(mtm);
            self.ivars().built.set(true);
        }
        // Already open: just bring it forward — a reload would silently discard
        // unsaved edits.
        if let Some(w) = self.ivars().window.borrow().as_ref() {
            if w.isVisible() {
                w.makeKeyAndOrderFront(None);
                return;
            }
        }
        if let Some(store) = self.ivars().store.borrow().as_ref() {
            match store.load_document() {
                Ok(document) => *self.ivars().document.borrow_mut() = document,
                Err(error) => {
                    ui::show_error(
                        mtm,
                        "Could not load connections",
                        &format!("{error:#}\n\nFix or restore:\n{}", store.path().display()),
                    );
                    return;
                }
            }
        }
        self.ivars().selected.set(-1);
        self.ivars().dirty.set(false);
        if let Some(seg) = self.ivars().segmented.borrow().as_ref() {
            seg.setSelectedSegment(0);
        }
        self.reload_table();
        let first = if self.ivars().document.borrow().connections.is_empty() {
            -1
        } else {
            0
        };
        self.select_row(first);
        self.update_visibility();
        if let Some(w) = self.ivars().window.borrow().as_ref() {
            w.center();
            w.makeKeyAndOrderFront(None);
        }
    }

    fn build(&self, mtm: MainThreadMarker) {
        // Fixed size: the panes are laid out with frame math for exactly this
        // content size, so resizing would only reveal dead space.
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable;
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect(0.0, 0.0, W, H),
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        window.setTitle(&NSString::from_str("RDP123 Settings"));
        unsafe { window.setReleasedWhenClosed(false) };
        window.setDelegate(Some(ProtocolObject::from_ref(self)));
        let content = window.contentView().expect("content view");

        // ---- pane switch ----
        let labels = NSArray::from_retained_slice(&[
            NSString::from_str("Connections"),
            NSString::from_str("Global"),
            NSString::from_str("About"),
        ]);
        let seg = unsafe {
            NSSegmentedControl::segmentedControlWithLabels_trackingMode_target_action(
                &labels,
                NSSegmentSwitchTracking::SelectOne,
                Some(self.any()),
                Some(sel!(paneChanged:)),
                mtm,
            )
        };
        seg.setFrame(rect((W - 360.0) / 2.0, H - 36.0, 360.0, SEG_H));
        seg.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewMinXMargin
                | NSAutoresizingMaskOptions::ViewMaxXMargin
                | NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        seg.setSelectedSegment(0);
        content.addSubview(&seg);
        *self.ivars().segmented.borrow_mut() = Some(seg);

        // ---- panes ----
        let conn_scroll =
            NSScrollView::initWithFrame(NSScrollView::alloc(mtm), rect(0.0, 0.0, W, CH));
        conn_scroll.setBorderType(NSBorderType::NoBorder);
        conn_scroll.setHasVerticalScroller(false);
        conn_scroll.setHasHorizontalScroller(false);
        conn_scroll.setAutohidesScrollers(true);
        conn_scroll.setDrawsBackground(false);
        conn_scroll.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        let global_scroll =
            NSScrollView::initWithFrame(NSScrollView::alloc(mtm), rect(0.0, 0.0, W, CH));
        global_scroll.setBorderType(NSBorderType::NoBorder);
        global_scroll.setHasVerticalScroller(true);
        global_scroll.setHasHorizontalScroller(false);
        global_scroll.setAutohidesScrollers(true);
        global_scroll.setDrawsBackground(false);
        global_scroll.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );

        let about_scroll =
            NSScrollView::initWithFrame(NSScrollView::alloc(mtm), rect(0.0, 0.0, W, CH));
        about_scroll.setBorderType(NSBorderType::NoBorder);
        about_scroll.setHasVerticalScroller(false);
        about_scroll.setAutohidesScrollers(true);
        about_scroll.setDrawsBackground(false);
        about_scroll.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );

        let conn = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, W, CH));
        let global = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, W, CH));
        let about = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, W, CH));
        self.build_connection_pane(mtm, &conn);
        self.build_global_pane(mtm, &global);
        self.build_about_pane(mtm, &about);
        conn_scroll.setDocumentView(Some(&conn));
        global_scroll.setDocumentView(Some(&global));
        about_scroll.setDocumentView(Some(&about));
        content.addSubview(&conn_scroll);
        content.addSubview(&global_scroll);
        content.addSubview(&about_scroll);

        *self.ivars().conn_pane.borrow_mut() = Some(conn_scroll);
        *self.ivars().global_pane.borrow_mut() = Some(global_scroll);
        *self.ivars().about_pane.borrow_mut() = Some(about_scroll);
        *self.ivars().window.borrow_mut() = Some(window);
    }

    /// A centered label helper for the About pane.
    fn centered(
        &self,
        mtm: MainThreadMarker,
        parent: &NSView,
        y: f64,
        h: f64,
        text: &str,
    ) -> Retained<NSTextField> {
        let l = self.label(mtm, parent, rect(60.0, y, W - 120.0, h), text);
        l.setAlignment(NSTextAlignment::Center);
        l
    }

    /// Standard macOS "About" layout: icon, name, version, then the libraries.
    fn build_about_pane(&self, mtm: MainThreadMarker, parent: &NSView) {
        // App icon, centered.
        if let Some(icon) = NSApplication::sharedApplication(mtm).applicationIconImage() {
            let view = NSImageView::imageViewWithImage(&icon, mtm);
            view.setFrame(rect((W - 48.0) / 2.0, 648.0, 48.0, 48.0));
            parent.addSubview(&view);
        }

        // Name + version identity block.
        let title = self.centered(mtm, parent, 610.0, 28.0, "RDP123");
        title.setFont(Some(&NSFont::boldSystemFontOfSize(24.0)));

        let website = self.url_button(
            mtm,
            parent,
            rect(W / 2.0 - 100.0, 584.0, 200.0, 22.0),
            "by asd123.ai",
            "https://asd123.ai",
        );
        website.setAlignment(NSTextAlignment::Center);
        website.setFont(Some(&NSFont::systemFontOfSize(12.0)));

        let version = self.centered(
            mtm,
            parent,
            558.0,
            18.0,
            &format!(
                "Version {} ({})",
                env!("CARGO_PKG_VERSION"),
                env!("RDP123_GIT")
            ),
        );
        self.muted(&version);
        version.setSelectable(true);

        let built = self.centered(
            mtm,
            parent,
            538.0,
            16.0,
            &format!("Built {}", env!("RDP123_BUILD_TIME")),
        );
        built.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        built.setTextColor(Some(&NSColor::tertiaryLabelColor()));
        built.setSelectable(true);

        self.button_ret(
            mtm,
            parent,
            rect(W / 2.0 - 166.0, 498.0, 150.0, 28.0),
            "Copy Version Info",
            sel!(copyVersionInfo:),
        );
        let github = self.url_button(
            mtm,
            parent,
            rect(W / 2.0 - 4.0, 498.0, 170.0, 28.0),
            "Source on GitHub",
            "https://github.com/asd123ch/rdp123",
        );
        github.setBordered(true);
        github.setContentTintColor(None);
        github.setAlignment(NSTextAlignment::Center);
        github.setFont(Some(&NSFont::systemFontOfSize(13.0)));
        if let Some(icon) = NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str("chevron.left.forwardslash.chevron.right"),
            None,
        ) {
            github.setImage(Some(&icon));
            github.setImagePosition(NSCellImagePosition::ImageLeading);
        }

        // Separator line.
        let separator = NSBox::initWithFrame(NSBox::alloc(mtm), rect(120.0, 480.0, W - 240.0, 1.0));
        separator.setBoxType(NSBoxType::Separator);
        parent.addSubview(&separator);

        let header = self.centered(mtm, parent, 450.0, 18.0, "Open Source Libraries");
        header.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
        let note = self.centered(
            mtm,
            parent,
            428.0,
            15.0,
            "Direct dependencies. Click for details; hover over a group to see its libraries.",
        );
        note.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        self.muted(&note);

        let libs = about_libraries(env!("RDP123_LIBS"));
        let rows = libs.len().div_ceil(3);
        let list = NSView::initWithFrame(
            NSView::alloc(mtm),
            rect(28.0, 64.0, W - 56.0, ABOUT_LIST_HEIGHT),
        );
        for (i, library) in libs.iter().enumerate() {
            let col_x = (i / rows) as f64 * ((W - 56.0) / 3.0);
            let y = about_library_row_y(i % rows);
            self.link_button(mtm, &list, rect(col_x, y, 160.0, 20.0), library);
            let ver = self.label(
                mtm,
                &list,
                rect(col_x + 164.0, y, 50.0, 20.0),
                library.version,
            );
            ver.setFont(Some(&NSFont::systemFontOfSize(11.0)));
            self.muted(&ver);
        }
        parent.addSubview(&list);

        // License line pinned to the bottom.
        let license = self.centered(mtm, parent, 28.0, 15.0, "Open source under GNU AGPL v3.0");
        license.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        license.setTextColor(Some(&NSColor::tertiaryLabelColor()));
    }

    fn build_connection_pane(&self, mtm: MainThreadMarker, parent: &NSView) {
        // ---- connection list ----
        let scroll = NSScrollView::initWithFrame(
            NSScrollView::alloc(mtm),
            rect(16.0, 56.0, 190.0, CH - 64.0),
        );
        scroll.setHasVerticalScroller(true);
        scroll.setBorderType(NSBorderType::BezelBorder);
        let table =
            NSTableView::initWithFrame(NSTableView::alloc(mtm), rect(0.0, 0.0, 188.0, CH - 66.0));
        let column = NSTableColumn::initWithIdentifier(
            NSTableColumn::alloc(mtm),
            &NSString::from_str("name"),
        );
        column.setWidth(184.0);
        column.setEditable(false);
        unsafe {
            table.addTableColumn(&column);
            table.setHeaderView(None);
            table.setRowHeight(20.0);
            table.setUsesAlternatingRowBackgroundColors(true);
            table.setDataSource(Some(ProtocolObject::from_ref(self)));
            table.setDelegate(Some(ProtocolObject::from_ref(self)));
        }
        scroll.setDocumentView(Some(&table));
        parent.addSubview(&scroll);
        *self.ivars().table.borrow_mut() = Some(table);

        // Small square +/− directly under the list (macOS source-list idiom).
        let _ = self.button_ret(
            mtm,
            parent,
            rect(16.0, 16.0, 36.0, 26.0),
            "+",
            sel!(addConnection:),
        );
        let remove = self.button_ret(
            mtm,
            parent,
            rect(56.0, 16.0, 36.0, 26.0),
            "−",
            sel!(removeConnection:),
        );
        remove.setEnabled(false);
        *self.ivars().remove_button.borrow_mut() = Some(remove);

        let duplicate = self.button_ret(
            mtm,
            parent,
            rect(96.0, 16.0, 110.0, 26.0),
            "Duplicate",
            sel!(duplicateConnection:),
        );
        duplicate.setEnabled(false);
        *self.ivars().duplicate_button.borrow_mut() = Some(duplicate);

        let save = self.button_ret(
            mtm,
            parent,
            rect(W - 124.0, 16.0, 108.0, 30.0),
            "Save",
            sel!(saveConnection:),
        );
        // Return key triggers Save (standard default-button behavior).
        save.setKeyEquivalent(&NSString::from_str("\r"));
        let revert = self.button_ret(
            mtm,
            parent,
            rect(W - 240.0, 16.0, 108.0, 30.0),
            "Revert",
            sel!(revertConnection:),
        );
        save.setEnabled(false);
        revert.setEnabled(false);
        *self.ivars().save_button.borrow_mut() = Some(save);
        *self.ivars().revert_button.borrow_mut() = Some(revert);

        let dirty = sel!(markDirty:);

        // Shown when the list is empty / nothing is selected.
        let empty = self.label(
            mtm,
            parent,
            rect(238.0, CH / 2.0, W - 254.0, ROW_H),
            "No connection selected — click + to add one.",
        );
        empty.setAlignment(NSTextAlignment::Center);
        self.muted(&empty);
        *self.ivars().empty_label.borrow_mut() = Some(empty);

        let form_scroll = NSScrollView::initWithFrame(
            NSScrollView::alloc(mtm),
            rect(222.0, 56.0, W - 238.0, CH - 64.0),
        );
        form_scroll.setBorderType(NSBorderType::NoBorder);
        form_scroll.setHasVerticalScroller(true);
        form_scroll.setAutohidesScrollers(true);
        form_scroll.setDrawsBackground(false);
        let form = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, W - 238.0, FORM_H));
        form_scroll.setDocumentView(Some(&form));
        parent.addSubview(&form_scroll);
        form.scrollPoint(CGPoint::new(0.0, FORM_H - (CH - 64.0)));
        *self.ivars().connection_form.borrow_mut() = Some(form.clone());
        let parent = &*form;

        // ---- common fields (hidden while nothing is selected) ----
        let mut common = Vec::new();
        let mut y = EDIT_TOP;
        self.header_g(mtm, parent, &mut y, "Connection", &mut common);
        let name = self.text_row_g(mtm, parent, &mut y, "Name:", &mut common);
        name.setPlaceholderString(Some(&NSString::from_str("Office PC")));
        *self.ivars().name.borrow_mut() = Some(name);
        *self.ivars().kind.borrow_mut() = Some(self.popup_row_g(
            mtm,
            parent,
            &mut y,
            "Type:",
            &["RDP", "SSH"],
            sel!(typeChanged:),
            &mut common,
        ));
        let host = self.text_row_g(mtm, parent, &mut y, "Host:", &mut common);
        host.setPlaceholderString(Some(&NSString::from_str("hostname or IP address")));
        *self.ivars().host.borrow_mut() = Some(host);
        let port = self.text_row_g(mtm, parent, &mut y, "Port:", &mut common);
        port.setPlaceholderString(Some(&NSString::from_str("3389")));
        *self.ivars().port.borrow_mut() = Some(port);

        // RDP and SSH groups share the region below the common fields.
        let group_top = y;

        // ---- RDP group ----
        let mut rdp = Vec::new();
        let mut yr = group_top;

        self.header_g(mtm, parent, &mut yr, "Authentication", &mut rdp);
        let authentication = self.popup_row_g(
            mtm,
            parent,
            &mut yr,
            "Method:",
            &["Password (NLA)", "Microsoft Entra web"],
            sel!(authenticationChanged:),
            &mut rdp,
        );
        *self.ivars().authentication.borrow_mut() = Some(authentication);
        // The username applies to RDP and SSH alike, and both layouts have a
        // row at this position (under "Authentication" and under "SSH"), so
        // the field lives in the common group at shared coordinates.
        let user = self.text_row_g(mtm, parent, &mut yr, "Username:", &mut common);
        user.setPlaceholderString(Some(&NSString::from_str("user or user@company.com")));
        *self.ivars().user.borrow_mut() = Some(user);
        let password_start = rdp.len();
        let password_top = yr;
        *self.ivars().domain.borrow_mut() =
            Some(self.text_row_g(mtm, parent, &mut yr, "Domain:", &mut rdp));
        let pw = self.secure_row_g(mtm, parent, &mut yr, "Password:", &mut rdp);
        pw.setPlaceholderString(Some(&NSString::from_str("(unchanged)")));
        *self.ivars().password.borrow_mut() = Some(pw);
        *self.ivars().pw_policy.borrow_mut() = Some(self.popup_row_g(
            mtm,
            parent,
            &mut yr,
            "Password handling:",
            &["Remember (Keychain)", "Always ask"],
            dirty,
            &mut rdp,
        ));
        *self.ivars().password_auth_group.borrow_mut() = rdp[password_start..].to_vec();

        let entra_line_1 = self.label(
            mtm,
            parent,
            rect(FIELD_X, password_top, FIELD_W, ROW_H),
            "Sign in with your Microsoft account.",
        );
        self.muted(&entra_line_1);
        rdp.push(unsafe { Retained::cast_unchecked(entra_line_1.clone()) });
        let entra_line_2 = self.label(
            mtm,
            parent,
            rect(FIELD_X, password_top - PITCH, FIELD_W, ROW_H),
            "Use a hostname, not an IP address.",
        );
        self.muted(&entra_line_2);
        rdp.push(unsafe { Retained::cast_unchecked(entra_line_2.clone()) });
        let entra_line_3 = self.label(
            mtm,
            parent,
            rect(FIELD_X, password_top - 2.0 * PITCH, FIELD_W, ROW_H),
            "No RDP password or domain is stored or sent.",
        );
        self.muted(&entra_line_3);
        rdp.push(unsafe { Retained::cast_unchecked(entra_line_3.clone()) });
        *self.ivars().entra_auth_group.borrow_mut() = vec![
            unsafe { Retained::cast_unchecked(entra_line_1) },
            unsafe { Retained::cast_unchecked(entra_line_2) },
            unsafe { Retained::cast_unchecked(entra_line_3) },
        ];

        self.header_g(mtm, parent, &mut yr, "Display", &mut rdp);
        *self.ivars().res_mode.borrow_mut() = Some(self.popup_row_g(
            mtm,
            parent,
            &mut yr,
            "Resolution:",
            &["Fit to window", "Fixed"],
            sel!(resModeChanged:),
            &mut rdp,
        ));
        {
            let label = self.row_label(mtm, parent, yr, "Fixed size:");
            rdp.push(unsafe { Retained::cast_unchecked(label) });
            let w = self.plain_text(mtm, parent, rect(FIELD_X, yr, 70.0, ROW_H));
            w.setPlaceholderString(Some(&NSString::from_str("1920")));
            let x = self.label(mtm, parent, rect(FIELD_X + 76.0, yr, 16.0, ROW_H), "×");
            let h = self.plain_text(mtm, parent, rect(FIELD_X + 96.0, yr, 70.0, ROW_H));
            h.setPlaceholderString(Some(&NSString::from_str("1080")));
            rdp.push(unsafe { Retained::cast_unchecked(w.clone()) });
            rdp.push(unsafe { Retained::cast_unchecked(x) });
            rdp.push(unsafe { Retained::cast_unchecked(h.clone()) });
            *self.ivars().res_w.borrow_mut() = Some(w);
            *self.ivars().res_h.borrow_mut() = Some(h);
            yr -= PITCH;
        }
        *self.ivars().scaling.borrow_mut() = Some(self.popup_row_g(
            mtm,
            parent,
            &mut yr,
            "Scaling:",
            &["Auto", "100%", "140%", "180%", "200%"],
            dirty,
            &mut rdp,
        ));
        *self.ivars().color.borrow_mut() = Some(self.popup_row_g(
            mtm,
            parent,
            &mut yr,
            "Color quality:",
            &["High (32-bit)", "Medium (16-bit)"],
            dirty,
            &mut rdp,
        ));
        *self.ivars().graphics.borrow_mut() = Some(self.popup_row_g(
            mtm,
            parent,
            &mut yr,
            "Graphics:",
            &["RDP 8.0 (Experimental)", "RDP 6.1 (Classic bitmaps)"],
            dirty,
            &mut rdp,
        ));
        // This controls the outer FastPath/XCRUSH transport layer in either
        // graphics mode. EGFX additionally manages ZGFX and codec compression.
        let comp = self.checkbox_fit(
            mtm,
            parent,
            FIELD_X,
            yr,
            "Transport compression (recommended)",
            dirty,
        );
        rdp.push(unsafe { Retained::cast_unchecked(comp.clone()) });
        *self.ivars().compression.borrow_mut() = Some(comp);
        yr -= CHECKBOX_PITCH;
        let fs = self.checkbox_fit(mtm, parent, FIELD_X, yr, "Start in full screen", dirty);
        rdp.push(unsafe { Retained::cast_unchecked(fs.clone()) });
        *self.ivars().fullscreen.borrow_mut() = Some(fs);
        yr -= CHECKBOX_PITCH;
        let rs = self.checkbox_fit(mtm, parent, FIELD_X, yr, "Remember window size", dirty);
        rdp.push(unsafe { Retained::cast_unchecked(rs.clone()) });
        *self.ivars().remember_size.borrow_mut() = Some(rs);
        yr -= CHECKBOX_PITCH;

        self.header_g(mtm, parent, &mut yr, "Clipboard & sound", &mut rdp);
        *self.ivars().clipboard.borrow_mut() = Some(self.popup_row_g(
            mtm,
            parent,
            &mut yr,
            "Clipboard:",
            &[
                "Bidirectional",
                "Disabled",
                "Local → Remote",
                "Remote → Local",
            ],
            dirty,
            &mut rdp,
        ));
        *self.ivars().audio.borrow_mut() = Some(self.popup_row_g(
            mtm,
            parent,
            &mut yr,
            "Play sound:",
            &["On this computer", "Never", "On the remote computer"],
            dirty,
            &mut rdp,
        ));
        self.header_g(mtm, parent, &mut yr, "Session", &mut rdp);
        let re = self.checkbox_fit(mtm, parent, FIELD_X, yr, "Reconnect automatically", dirty);
        rdp.push(unsafe { Retained::cast_unchecked(re.clone()) });
        *self.ivars().reconnect.borrow_mut() = Some(re);
        yr -= CHECKBOX_PITCH;
        {
            let label = self.row_label(mtm, parent, yr, "Retries per minute:");
            rdp.push(unsafe { Retained::cast_unchecked(label) });
            let rate = self.plain_text(mtm, parent, rect(FIELD_X, yr, 60.0, ROW_H));
            rdp.push(unsafe { Retained::cast_unchecked(rate.clone()) });
            *self.ivars().rate.borrow_mut() = Some(rate);
            yr -= PITCH;
        }
        let ka = self.checkbox_fit(mtm, parent, FIELD_X, yr, "Keep session awake", dirty);
        ka.setToolTip(Some(&NSString::from_str(
            "While idle, taps an invisible key so the remote session is not \
             disconnected or locked. Also keeps the host from auto-locking.",
        )));
        rdp.push(unsafe { Retained::cast_unchecked(ka.clone()) });
        *self.ivars().keep_alive.borrow_mut() = Some(ka);
        yr -= CHECKBOX_PITCH;
        let wake = self.text_row_g(mtm, parent, &mut yr, "Wake on LAN (MAC):", &mut rdp);
        wake.setPlaceholderString(Some(&NSString::from_str("AA:BB:CC:DD:EE:FF (optional)")));
        *self.ivars().wake_mac.borrow_mut() = Some(wake);

        // ---- SSH group ----
        let mut ssh = Vec::new();
        let mut ys = group_top - PITCH;
        self.header_g(mtm, parent, &mut ys, "SSH", &mut ssh);
        // The shared Username row (common group) occupies this first slot.
        ys -= PITCH;
        let l1 = self.label(
            mtm,
            parent,
            rect(FORM_X, ys, FIELD_W + LABEL_W, ROW_H),
            "Opens in the terminal chosen under the “Global” tab.",
        );
        self.muted(&l1);
        ssh.push(unsafe { Retained::cast_unchecked(l1) });
        ys -= PITCH;
        let l2 = self.label(
            mtm,
            parent,
            rect(FORM_X, ys, FIELD_W + LABEL_W, ROW_H),
            "Authenticates with your SSH keys — no password is handled here.",
        );
        self.muted(&l2);
        ssh.push(unsafe { Retained::cast_unchecked(l2) });

        *self.ivars().common_group.borrow_mut() = common;
        *self.ivars().rdp_group.borrow_mut() = rdp;
        *self.ivars().ssh_group.borrow_mut() = ssh;
    }

    fn build_global_pane(&self, mtm: MainThreadMarker, parent: &NSView) {
        let gx = 44.0;
        let gfield = 216.0;
        let field_width = W - gfield - gx;
        self.global_heading(mtm, parent, 686.0, "SSH connections");
        let terminal_label = self.label(mtm, parent, rect(gx, 650.0, 156.0, ROW_H), "Terminal:");
        terminal_label.setAlignment(NSTextAlignment::Right);
        let terms: Vec<&str> = TerminalKind::ALL.iter().map(|k| k.display_name()).collect();
        let term = self.popup(
            mtm,
            parent,
            rect(gfield, 650.0, field_width, ROW_H + 2.0),
            &terms,
            sel!(globalChanged:),
        );
        term.setToolTip(Some(&NSString::from_str("Used by all SSH connections.")));
        *self.ivars().terminal.borrow_mut() = Some(term);
        let custom_label = self.label(
            mtm,
            parent,
            rect(gx, 614.0, 156.0, ROW_H),
            "Custom command:",
        );
        custom_label.setAlignment(NSTextAlignment::Right);
        let custom = self.plain_text(mtm, parent, rect(gfield, 614.0, field_width, ROW_H));
        custom.setPlaceholderString(Some(&NSString::from_str("e.g. wezterm start -- {ssh}")));
        *self.ivars().custom.borrow_mut() = Some(custom);
        self.help_text(mtm, parent, rect(gfield, 570.0, field_width, 34.0),
            "Custom terminal only. Use {ssh} for the full SSH command,\nor {host}, {port} and {user} for individual values.");

        self.global_heading(mtm, parent, 530.0, "RDP keyboard");
        let swap = self.checkbox_fit(
            mtm,
            parent,
            gfield,
            498.0,
            "Swap Command (⌘) and Option (⌥)",
            sel!(globalChanged:),
        );
        *self.ivars().swap_cmd_alt.borrow_mut() = Some(swap);
        self.help_text(mtm, parent, rect(gfield + 20.0, 460.0, field_width - 20.0, 32.0),
            "⌘ sends Alt; ⌥ sends the Windows key.\nPlaces Alt next to the space bar, as on a PC keyboard.");
        let mac_shortcuts = self.checkbox_fit(
            mtm,
            parent,
            gfield,
            416.0,
            "Use ⌘ instead of Ctrl for common shortcuts",
            sel!(globalChanged:),
        );
        *self.ivars().mac_shortcuts.borrow_mut() = Some(mac_shortcuts);
        self.help_text(mtm, parent, rect(gfield + 20.0, 352.0, field_width - 20.0, 54.0),
            "Use ⌘C / ⌘V to copy and paste remotely, like on your Mac.\nAlso translates ⌘X, ⌘A, ⌘Z, ⌘F and ⌘W, including Shift.\nOther shortcuts and Hyper keep their normal mapping.");

        self.global_heading(mtm, parent, 306.0, "Speech to text");
        let external_stt_paste = self.checkbox_fit(
            mtm,
            parent,
            gfield,
            274.0,
            "Allow pasting from speech-to-text apps",
            sel!(globalChanged:),
        );
        *self.ivars().external_stt_paste.borrow_mut() = Some(external_stt_paste);
        self.help_text(mtm, parent, rect(gfield + 20.0, 232.0, field_width - 20.0, 34.0),
            "Transfers dictated text from the Mac clipboard into the active remote app.\nRequires clipboard access from Mac to remote.");

        self.global_heading(mtm, parent, 186.0, "Startup");
        let login = self.checkbox_fit(
            mtm,
            parent,
            gfield,
            154.0,
            "Open RDP123 at login",
            sel!(loginItemChanged:),
        );
        if !crate::login_item::is_supported() {
            login.setEnabled(false);
            self.help_text(
                mtm,
                parent,
                rect(gfield + 20.0, 126.0, field_width - 20.0, ROW_H),
                "Requires macOS 13 or newer.",
            );
        }
        *self.ivars().launch_at_login.borrow_mut() = Some(login);
    }

    // ---------- small control builders ----------

    fn label(
        &self,
        mtm: MainThreadMarker,
        parent: &NSView,
        f: CGRect,
        text: &str,
    ) -> Retained<NSTextField> {
        let l = NSTextField::labelWithString(&NSString::from_str(text), mtm);
        l.setFrame(f);
        parent.addSubview(&l);
        l
    }

    fn global_heading(&self, mtm: MainThreadMarker, parent: &NSView, y: f64, text: &str) {
        let l = self.label(mtm, parent, rect(44.0, y, W - 88.0, 20.0), text);
        l.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
        let line = NSBox::initWithFrame(NSBox::alloc(mtm), rect(44.0, y - 6.0, W - 88.0, 1.0));
        line.setBoxType(NSBoxType::Separator);
        parent.addSubview(&line);
    }

    fn help_text(&self, mtm: MainThreadMarker, parent: &NSView, frame: CGRect, text: &str) {
        let label = NSTextField::wrappingLabelWithString(&NSString::from_str(text), mtm);
        label.setFrame(frame);
        label.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        self.muted(&label);
        parent.addSubview(&label);
    }

    /// A right-aligned label for the form's label column (macOS convention).
    fn row_label(
        &self,
        mtm: MainThreadMarker,
        parent: &NSView,
        y: f64,
        text: &str,
    ) -> Retained<NSTextField> {
        let l = self.label(mtm, parent, rect(FORM_X, y, LABEL_W, ROW_H), text);
        l.setAlignment(NSTextAlignment::Right);
        l
    }

    /// Colour a label as secondary/muted text.
    fn muted(&self, label: &NSTextField) {
        label.setTextColor(Some(&NSColor::secondaryLabelColor()));
    }

    fn link_button(
        &self,
        mtm: MainThreadMarker,
        parent: &NSView,
        f: CGRect,
        library: &AboutLibrary<'_>,
    ) {
        let url = if library.crates.len() > 1 {
            "https://github.com/madsmtm/objc2/tree/main/framework-crates".into()
        } else {
            format!("https://crates.io/crates/{}", library.crates[0])
        };
        let b = self.url_button(mtm, parent, f, &library.name, &url);
        b.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        b.setToolTip(Some(&NSString::from_str(&library.crates.join("\n"))));
    }

    fn url_button(
        &self,
        mtm: MainThreadMarker,
        parent: &NSView,
        frame: CGRect,
        title: &str,
        url: &str,
    ) -> Retained<NSButton> {
        let button = self.button_ret(mtm, parent, frame, title, sel!(openLink:));
        button.setBordered(false);
        button.setAlignment(NSTextAlignment::Left);
        button.setContentTintColor(Some(&NSColor::linkColor()));
        button.setIdentifier(Some(&NSString::from_str(url)));
        button.setToolTip(Some(&NSString::from_str(url)));
        button
    }

    fn header_g(
        &self,
        mtm: MainThreadMarker,
        parent: &NSView,
        y: &mut f64,
        text: &str,
        group: &mut Vec<Retained<NSView>>,
    ) {
        *y -= 12.0;
        let l = self.label(mtm, parent, rect(FORM_X, *y, 460.0, 20.0), text);
        l.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
        group.push(unsafe { Retained::cast_unchecked(l) });
        *y -= HDR_PITCH;
    }

    fn plain_text(
        &self,
        mtm: MainThreadMarker,
        parent: &NSView,
        f: CGRect,
    ) -> Retained<NSTextField> {
        let t = NSTextField::initWithFrame(NSTextField::alloc(mtm), f);
        unsafe { t.setDelegate(Some(ProtocolObject::from_ref(self))) };
        parent.addSubview(&t);
        t
    }

    fn button_ret(
        &self,
        mtm: MainThreadMarker,
        parent: &NSView,
        f: CGRect,
        title: &str,
        action: Sel,
    ) -> Retained<NSButton> {
        let b = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str(title),
                Some(self.any()),
                Some(action),
                mtm,
            )
        };
        b.setFrame(f);
        parent.addSubview(&b);
        b
    }

    /// A checkbox sized to its title so the label never truncates.
    fn checkbox_fit(
        &self,
        mtm: MainThreadMarker,
        parent: &NSView,
        x: f64,
        y: f64,
        title: &str,
        action: Sel,
    ) -> Retained<NSButton> {
        let b = unsafe {
            NSButton::checkboxWithTitle_target_action(
                &NSString::from_str(title),
                Some(self.any()),
                Some(action),
                mtm,
            )
        };
        b.setFrame(rect(x, y, 240.0, ROW_H));
        b.sizeToFit();
        parent.addSubview(&b);
        b
    }

    fn text_row_g(
        &self,
        mtm: MainThreadMarker,
        parent: &NSView,
        y: &mut f64,
        label: &str,
        group: &mut Vec<Retained<NSView>>,
    ) -> Retained<NSTextField> {
        let l = self.row_label(mtm, parent, *y, label);
        let t = self.plain_text(mtm, parent, rect(FIELD_X, *y, FIELD_W, ROW_H));
        group.push(unsafe { Retained::cast_unchecked(l) });
        group.push(unsafe { Retained::cast_unchecked(t.clone()) });
        *y -= PITCH;
        t
    }

    fn secure_row_g(
        &self,
        mtm: MainThreadMarker,
        parent: &NSView,
        y: &mut f64,
        label: &str,
        group: &mut Vec<Retained<NSView>>,
    ) -> Retained<NSSecureTextField> {
        let l = self.row_label(mtm, parent, *y, label);
        let t = NSSecureTextField::initWithFrame(
            NSSecureTextField::alloc(mtm),
            rect(FIELD_X, *y, FIELD_W, ROW_H),
        );
        unsafe { t.setDelegate(Some(ProtocolObject::from_ref(self))) };
        parent.addSubview(&t);
        group.push(unsafe { Retained::cast_unchecked(l) });
        group.push(unsafe { Retained::cast_unchecked(t.clone()) });
        *y -= PITCH;
        t
    }

    fn popup_row_g(
        &self,
        mtm: MainThreadMarker,
        parent: &NSView,
        y: &mut f64,
        label: &str,
        items: &[&str],
        action: Sel,
        group: &mut Vec<Retained<NSView>>,
    ) -> Retained<NSPopUpButton> {
        let l = self.row_label(mtm, parent, *y, label);
        let p = self.popup(
            mtm,
            parent,
            rect(FIELD_X, *y, FIELD_W, ROW_H + 2.0),
            items,
            action,
        );
        group.push(unsafe { Retained::cast_unchecked(l) });
        group.push(unsafe { Retained::cast_unchecked(p.clone()) });
        *y -= PITCH;
        p
    }

    fn popup(
        &self,
        mtm: MainThreadMarker,
        parent: &NSView,
        f: CGRect,
        items: &[&str],
        action: Sel,
    ) -> Retained<NSPopUpButton> {
        let p = NSPopUpButton::initWithFrame_pullsDown(NSPopUpButton::alloc(mtm), f, false);
        for it in items {
            p.addItemWithTitle(&NSString::from_str(it));
        }
        unsafe {
            p.setTarget(Some(self.any()));
            p.setAction(Some(action));
        }
        parent.addSubview(&p);
        p
    }

    fn any(&self) -> &AnyObject {
        self
    }

    // ---------- data flow ----------

    fn pane(&self) -> isize {
        self.ivars()
            .segmented
            .borrow()
            .as_ref()
            .map(|s| s.selectedSegment())
            .unwrap_or(0)
    }

    fn save_document(&self, document: &mut Document) -> bool {
        let Some(store) = self.ivars().store.borrow().as_ref().cloned() else {
            ui::show_error(
                self.mtm(),
                "Could not save settings",
                "The profile store is unavailable.",
            );
            return false;
        };
        match store.save_settings_document(&self.ivars().document.borrow(), document) {
            Ok(merged) => {
                let original = self.ivars().document.borrow();
                if let Some(before) = original
                    .connections
                    .get(self.ivars().selected.get() as usize)
                {
                    if self.popup_index(&self.ivars().pw_policy)
                        == index_of(&PWPOLICY, &before.rdp.password_policy)
                    {
                        if let Some(saved) = merged.connections.iter().find(|c| c.id == before.id) {
                            self.set_popup(
                                &self.ivars().pw_policy,
                                index_of(&PWPOLICY, &saved.rdp.password_policy),
                            );
                        }
                    }
                }
                *document = merged;
                true
            }
            Err(error) => {
                ui::show_error(self.mtm(), "Could not save settings", &format!("{error:#}"));
                false
            }
        }
    }

    fn reload_table(&self) {
        if let Some(t) = self.ivars().table.borrow().as_ref() {
            t.reloadData();
        }
    }

    fn select_row(&self, row: isize) {
        let len = self.ivars().document.borrow().connections.len() as isize;
        if row >= 0 && row < len {
            if let Some(t) = self.ivars().table.borrow().as_ref() {
                self.ivars().updating.set(true);
                let set = NSIndexSet::indexSetWithIndex(row as usize);
                t.selectRowIndexes_byExtendingSelection(&set, false);
                self.ivars().updating.set(false);
            }
            self.populate(row);
        } else {
            if let Some(t) = self.ivars().table.borrow().as_ref() {
                unsafe { t.deselectAll(None) };
            }
            self.populate(-1);
        }
    }

    fn handle_selection_change(&self) {
        if self.ivars().updating.get() {
            return;
        }
        let target = self
            .ivars()
            .table
            .borrow()
            .as_ref()
            .map(|t| t.selectedRow())
            .unwrap_or(-1);
        let prev = self.ivars().selected.get();
        if target == prev {
            return;
        }
        if self.ivars().dirty.get() && prev >= 0 {
            let name = self
                .ivars()
                .document
                .borrow()
                .connections
                .get(prev as usize)
                .map(|c| c.name.clone())
                .unwrap_or_default();
            match ui::confirm_unsaved(self.mtm(), &name) {
                UnsavedChoice::Save => {
                    if !self.commit_connection() {
                        self.restore_selection(prev);
                        return;
                    }
                }
                UnsavedChoice::Discard => {}
                UnsavedChoice::Cancel => {
                    self.restore_selection(prev);
                    return;
                }
            }
        }
        self.populate(target);
    }

    /// Returns true if it is OK to proceed (discard/save handled), false to abort.
    fn confirm_discard_ok(&self) -> bool {
        if !self.ivars().dirty.get() || self.ivars().selected.get() < 0 {
            return true;
        }
        let name = self
            .ivars()
            .document
            .borrow()
            .connections
            .get(self.ivars().selected.get() as usize)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        match ui::confirm_unsaved(self.mtm(), &name) {
            UnsavedChoice::Save => {
                if self.commit_connection() {
                    self.ivars().dirty.set(false);
                    self.update_dirty_ui();
                    true
                } else {
                    false
                }
            }
            UnsavedChoice::Discard => {
                self.ivars().dirty.set(false);
                self.update_dirty_ui();
                true
            }
            UnsavedChoice::Cancel => false,
        }
    }

    fn mark_dirty(&self) {
        // Nothing selected means nothing to stage — don't arm a Save that would
        // silently no-op.
        if self.ivars().loading.get() || self.pane() != 0 || self.ivars().selected.get() < 0 {
            return;
        }
        self.ivars().dirty.set(true);
        self.update_dirty_ui();
    }

    fn update_dirty_ui(&self) {
        let dirty = self.ivars().dirty.get();
        if let Some(b) = self.ivars().save_button.borrow().as_ref() {
            b.setEnabled(dirty);
        }
        if let Some(b) = self.ivars().revert_button.borrow().as_ref() {
            b.setEnabled(dirty);
        }
    }

    fn update_visibility(&self) {
        let pane = self.pane();
        if let Some(c) = self.ivars().conn_pane.borrow().as_ref() {
            c.setHidden(pane != 0);
        }
        if let Some(g) = self.ivars().global_pane.borrow().as_ref() {
            g.setHidden(pane != 1);
        }
        if let Some(a) = self.ivars().about_pane.borrow().as_ref() {
            a.setHidden(pane != 2);
        }
        let has_selection = self.ivars().selected.get() >= 0;
        let is_ssh = self
            .ivars()
            .kind
            .borrow()
            .as_ref()
            .map(|k| k.indexOfSelectedItem() == 1)
            .unwrap_or(false);
        for v in self.ivars().common_group.borrow().iter() {
            v.setHidden(!has_selection);
        }
        for v in self.ivars().rdp_group.borrow().iter() {
            v.setHidden(!has_selection || is_ssh);
        }
        for v in self.ivars().ssh_group.borrow().iter() {
            v.setHidden(!has_selection || !is_ssh);
        }
        if let Some(e) = self.ivars().empty_label.borrow().as_ref() {
            e.setHidden(has_selection);
        }
        if let Some(b) = self.ivars().remove_button.borrow().as_ref() {
            b.setEnabled(has_selection);
        }
        if let Some(b) = self.ivars().duplicate_button.borrow().as_ref() {
            b.setEnabled(has_selection);
        }
        let entra = self.popup_index(&self.ivars().authentication) == 1;
        for v in self.ivars().password_auth_group.borrow().iter() {
            v.setHidden(!has_selection || is_ssh || entra);
        }
        for v in self.ivars().entra_auth_group.borrow().iter() {
            v.setHidden(!has_selection || is_ssh || !entra);
        }
    }

    fn update_fixed_enabled(&self) {
        let fixed = self.popup_index(&self.ivars().res_mode) == 1;
        if let Some(f) = self.ivars().res_w.borrow().as_ref() {
            f.setEnabled(fixed);
        }
        if let Some(f) = self.ivars().res_h.borrow().as_ref() {
            f.setEnabled(fixed);
        }
    }

    fn sync_default_port(&self) {
        let is_ssh = self.popup_index(&self.ivars().kind) == 1;
        let port = self.read_field(&self.ivars().port);
        if let Ok(p) = port.trim().parse::<u16>() {
            if is_ssh && p == 3389 {
                self.set_field(&self.ivars().port, "22");
            } else if !is_ssh && p == 22 {
                self.set_field(&self.ivars().port, "3389");
            }
        }
    }

    fn set_field(&self, field: &Field, value: &str) {
        if let Some(f) = field.borrow().as_ref() {
            f.setStringValue(&NSString::from_str(value));
        }
    }

    fn read_field(&self, field: &Field) -> String {
        field
            .borrow()
            .as_ref()
            .map(|f| f.stringValue().to_string())
            .unwrap_or_default()
    }

    fn set_popup(&self, popup: &Popup, index: isize) {
        if let Some(p) = popup.borrow().as_ref() {
            p.selectItemAtIndex(index);
        }
    }

    fn popup_index(&self, popup: &Popup) -> isize {
        popup
            .borrow()
            .as_ref()
            .map(|p| p.indexOfSelectedItem())
            .unwrap_or(0)
    }

    fn set_check(&self, check: &Check, on: bool) {
        if let Some(c) = check.borrow().as_ref() {
            c.setState(if on {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
        }
    }

    fn check_on(&self, check: &Check) -> bool {
        check
            .borrow()
            .as_ref()
            .map(|c| c.state() == NSControlStateValueOn)
            .unwrap_or(false)
    }

    fn read_secure(&self, field: &Secure) -> String {
        field
            .borrow()
            .as_ref()
            .map(|f| f.stringValue().to_string())
            .unwrap_or_default()
    }

    fn set_secure(&self, field: &Secure, value: &str) {
        if let Some(f) = field.borrow().as_ref() {
            f.setStringValue(&NSString::from_str(value));
        }
    }

    fn populate(&self, row: isize) {
        self.ivars().loading.set(true);
        let iv = self.ivars();
        if iv.selected.get() != row {
            if let Some(form) = iv.connection_form.borrow().as_ref() {
                form.scrollPoint(CGPoint::new(0.0, FORM_H - (CH - 64.0)));
            }
        }

        // Global settings.
        let (term_idx, custom) = {
            let doc = iv.document.borrow();
            let idx = TerminalKind::ALL
                .iter()
                .position(|k| *k == doc.settings.terminal)
                .unwrap_or(0);
            (
                idx as isize,
                doc.settings.custom_terminal.clone().unwrap_or_default(),
            )
        };
        self.set_popup(&iv.terminal, term_idx);
        self.set_field(&iv.custom, &custom);
        if let Some(field) = iv.custom.borrow().as_ref() {
            field.setEnabled(iv.document.borrow().settings.terminal == TerminalKind::Custom);
        }
        let swap_cmd_alt = iv.document.borrow().settings.swap_cmd_alt;
        self.set_check(&iv.swap_cmd_alt, swap_cmd_alt);
        let mac_shortcuts = iv.document.borrow().settings.mac_shortcuts;
        self.set_check(&iv.mac_shortcuts, mac_shortcuts);
        let external_stt_paste = iv.document.borrow().settings.external_stt_paste;
        self.set_check(&iv.external_stt_paste, external_stt_paste);
        // Reflect the system's login-item state, not a stored flag.
        self.set_check(&iv.launch_at_login, crate::login_item::is_enabled());

        let conn = if row >= 0 {
            iv.document.borrow().connections.get(row as usize).cloned()
        } else {
            None
        };

        if let Some(c) = conn {
            self.set_field(&iv.name, &c.name);
            self.set_field(&iv.host, &c.host);
            self.set_field(&iv.port, &c.port.to_string());
            self.set_field(&iv.user, &c.username);
            self.set_popup(
                &iv.kind,
                match c.kind {
                    ConnectionKind::Rdp => 0,
                    ConnectionKind::Ssh => 1,
                },
            );
            self.set_secure(&iv.password, "");
            self.set_popup(
                &iv.authentication,
                index_of(&AUTHENTICATION, &c.rdp.authentication),
            );
            self.set_popup(&iv.pw_policy, index_of(&PWPOLICY, &c.rdp.password_policy));
            self.set_field(&iv.domain, c.domain.as_deref().unwrap_or(""));
            self.set_popup(&iv.color, index_of(&COLOR, &c.rdp.color_quality));
            self.set_popup(&iv.clipboard, index_of(&CLIP, &c.rdp.clipboard));
            self.set_popup(&iv.scaling, index_of(&SCALING, &c.rdp.scaling));
            self.set_popup(&iv.res_mode, index_of(&RESMODE, &c.rdp.resolution_mode));
            let (rw, rh) = c.rdp.resolution.unwrap_or((1920, 1080));
            self.set_field(&iv.res_w, &rw.to_string());
            self.set_field(&iv.res_h, &rh.to_string());
            self.set_field(&iv.rate, &c.rdp.reconnect_per_minute.to_string());
            self.set_field(&iv.wake_mac, c.rdp.wake_mac.as_deref().unwrap_or(""));
            self.set_check(&iv.compression, c.rdp.compression);
            self.set_check(&iv.fullscreen, c.rdp.fullscreen);
            self.set_check(&iv.remember_size, c.rdp.remember_size);
            self.set_popup(&iv.audio, index_of(&AUDIO, &c.rdp.audio));
            self.set_popup(&iv.graphics, index_of(&GRAPHICS, &c.rdp.graphics));
            self.set_check(&iv.reconnect, c.rdp.reconnect);
            self.set_check(&iv.keep_alive, c.rdp.keep_alive);
        } else {
            for f in [&iv.name, &iv.host, &iv.port, &iv.user, &iv.domain] {
                self.set_field(f, "");
            }
            self.set_secure(&iv.password, "");
        }

        iv.selected.set(row);
        iv.dirty.set(false);
        iv.loading.set(false);
        self.update_dirty_ui();
        self.update_visibility();
        self.update_fixed_enabled();
    }

    /// Write the form into the selected connection and persist (called on Save).
    fn commit_connection(&self) -> bool {
        let row = self.ivars().selected.get();
        if row < 0 {
            return false;
        }
        let iv = self.ivars();
        let name = self.read_field(&iv.name).trim().to_string();
        let host = self.read_field(&iv.host).trim().to_string();
        let port = match parse_number::<u16>("Port", &self.read_field(&iv.port)) {
            Ok(value) if value > 0 => value,
            Ok(_) => {
                ui::show_error(
                    self.mtm(),
                    "Invalid connection",
                    "Port must be between 1 and 65535.",
                );
                return false;
            }
            Err(message) => {
                ui::show_error(self.mtm(), "Invalid connection", &message);
                return false;
            }
        };
        let user = self.read_field(&iv.user).trim().to_string();
        let domain = self.read_field(&iv.domain).trim().to_string();
        let res_w = self.read_field(&iv.res_w);
        let res_h = self.read_field(&iv.res_h);
        let rate =
            match parse_number::<u32>("Maximum attempts per minute", &self.read_field(&iv.rate)) {
                Ok(value) => value,
                Err(message) => {
                    ui::show_error(self.mtm(), "Invalid connection", &message);
                    return false;
                }
            };
        let wake_mac = {
            let raw = self.read_field(&iv.wake_mac).trim().to_string();
            if raw.is_empty() {
                None
            } else if rdp123_core::wol::parse_mac(&raw).is_some() {
                Some(raw)
            } else {
                ui::show_error(
                    self.mtm(),
                    "Invalid connection",
                    "The Wake-on-LAN MAC address must look like AA:BB:CC:DD:EE:FF.",
                );
                return false;
            }
        };
        let kind = if self.popup_index(&iv.kind) == 1 {
            ConnectionKind::Ssh
        } else {
            ConnectionKind::Rdp
        };
        let color = COLOR[self.popup_index(&iv.color).clamp(0, 1) as usize];
        let clip = CLIP[self.popup_index(&iv.clipboard).clamp(0, 3) as usize];
        let scaling = SCALING[self.popup_index(&iv.scaling).clamp(0, 4) as usize];
        let res_mode = RESMODE[self.popup_index(&iv.res_mode).clamp(0, 1) as usize];
        let pw_policy = PWPOLICY[self.popup_index(&iv.pw_policy).clamp(0, 1) as usize];
        let authentication =
            AUTHENTICATION[self.popup_index(&iv.authentication).clamp(0, 1) as usize];
        let compression = self.check_on(&iv.compression);
        let fullscreen = self.check_on(&iv.fullscreen);
        let remember_size = self.check_on(&iv.remember_size);
        let audio = AUDIO[self.popup_index(&iv.audio).clamp(0, 2) as usize];
        let graphics = GRAPHICS[self.popup_index(&iv.graphics).clamp(0, 1) as usize];
        let reconnect = self.check_on(&iv.reconnect);
        let keep_alive = self.check_on(&iv.keep_alive);
        let password = self.read_secure(&iv.password);

        let mut document = iv.document.borrow().clone();
        let Some(connection) = document.connections.get_mut(row as usize) else {
            ui::show_error(
                self.mtm(),
                "Could not save connection",
                "The selected connection no longer exists.",
            );
            return false;
        };
        connection.name = name;
        connection.kind = kind;
        connection.host = host;
        connection.port = port;
        connection.username = user;
        connection.domain = if authentication == AuthenticationMode::EntraWeb || domain.is_empty() {
            None
        } else {
            Some(domain)
        };
        connection.rdp.color_quality = color;
        connection.rdp.authentication = authentication;
        connection.rdp.clipboard = clip;
        connection.rdp.scaling = scaling;
        connection.rdp.resolution_mode = res_mode;
        connection.rdp.resolution = if res_mode == ResolutionMode::Fixed {
            let width = match parse_number::<u16>("Fixed width", &res_w) {
                Ok(value) => value,
                Err(message) => {
                    ui::show_error(self.mtm(), "Invalid connection", &message);
                    return false;
                }
            };
            let height = match parse_number::<u16>("Fixed height", &res_h) {
                Ok(value) => value,
                Err(message) => {
                    ui::show_error(self.mtm(), "Invalid connection", &message);
                    return false;
                }
            };
            Some((width, height))
        } else {
            None
        };
        connection.rdp.reconnect_per_minute = rate;
        connection.rdp.wake_mac = wake_mac;
        connection.rdp.compression = compression;
        connection.rdp.fullscreen = fullscreen;
        connection.rdp.remember_size = remember_size;
        connection.rdp.audio = audio;
        connection.rdp.graphics = graphics;
        connection.rdp.reconnect = reconnect;
        connection.rdp.keep_alive = keep_alive;
        connection.rdp.password_policy = pw_policy;
        if let Err(error) = connection.validate() {
            ui::show_error(self.mtm(), "Invalid connection", &format!("{error:#}"));
            return false;
        }
        let connection_id = connection.id.clone();

        if !self.save_document(&mut document) {
            return false;
        }
        let saved = &document.connections[row as usize];
        let remember_password = saved.rdp.authentication == AuthenticationMode::Password
            && saved.rdp.password_policy == PasswordPolicy::Remember;
        *iv.document.borrow_mut() = document;
        let password_result = if !remember_password {
            secrets::delete_password(&connection_id)
        } else if !password.is_empty() {
            secrets::store_password(&connection_id, &password)
        } else {
            Ok(())
        };
        if let Err(error) = password_result {
            ui::show_error(
                self.mtm(),
                "Could not update saved password",
                &format!("{error:#}"),
            );
            return false;
        }
        self.set_secure(&iv.password, "");
        true
    }

    /// Global settings are simple and auto-save on change.
    fn save_global(&self) {
        if self.ivars().loading.get() {
            return;
        }
        let mut document = self.ivars().document.borrow().clone();
        let ti = self.popup_index(&self.ivars().terminal).max(0) as usize;
        document.settings.terminal = TerminalKind::ALL.get(ti).copied().unwrap_or_default();
        let custom = self.read_field(&self.ivars().custom);
        document.settings.custom_terminal = if custom.trim().is_empty() {
            None
        } else {
            Some(custom)
        };
        document.settings.swap_cmd_alt = self.check_on(&self.ivars().swap_cmd_alt);
        document.settings.mac_shortcuts = self.check_on(&self.ivars().mac_shortcuts);
        document.settings.external_stt_paste = self.check_on(&self.ivars().external_stt_paste);
        if self.save_document(&mut document) {
            if let Some(field) = self.ivars().custom.borrow().as_ref() {
                field.setEnabled(document.settings.terminal == TerminalKind::Custom);
            }
            crate::delegate::set_external_stt_paste_enabled(document.settings.external_stt_paste);
            crate::delegate::set_mac_shortcuts_enabled(document.settings.mac_shortcuts);
            *self.ivars().document.borrow_mut() = document;
        }
    }

    fn restore_selection(&self, row: isize) {
        if row < 0 {
            return;
        }
        if let Some(table) = self.ivars().table.borrow().as_ref() {
            self.ivars().updating.set(true);
            let set = NSIndexSet::indexSetWithIndex(row as usize);
            table.selectRowIndexes_byExtendingSelection(&set, false);
            self.ivars().updating.set(false);
        }
    }
}

fn parse_number<T>(label: &str, value: &str) -> Result<T, String>
where
    T: std::str::FromStr,
{
    value
        .trim()
        .parse()
        .map_err(|_| format!("{label} must be a valid number."))
}

#[cfg(test)]
mod about_tests {
    use super::about_libraries;

    #[test]
    fn thirteen_library_rows_fit_without_scrolling() {
        for row in 0..13 {
            let y = super::about_library_row_y(row);
            assert!(
                y >= 0.0 && y + 20.0 <= super::ABOUT_LIST_HEIGHT,
                "library row {row} is outside the About pane"
            );
        }
    }
    #[test]
    fn groups_frameworks_without_losing_names_or_versions() {
        let libraries =
            about_libraries("objc2 0.6.5;objc2-app-kit 0.3.2;objc2-foundation 0.3.2;tokio 1.54.1");
        assert_eq!(libraries.len(), 3);
        assert_eq!(libraries[0].name, "objc2");
        assert_eq!(libraries[0].version, "0.6.5");
        assert_eq!(libraries[1].name, "objc2 frameworks (2)");
        assert_eq!(libraries[1].version, "0.3.2");
        assert_eq!(libraries[1].crates, ["objc2-app-kit", "objc2-foundation"]);
        assert_eq!(libraries[2].name, "tokio");
    }

    #[test]
    fn keeps_frameworks_with_different_versions_separate() {
        let libraries = about_libraries("objc2-app-kit 0.3.2;objc2-foundation 0.4.0");
        assert_eq!(libraries.len(), 2);
        assert_eq!(libraries[0].name, "objc2-app-kit");
        assert_eq!(libraries[0].version, "0.3.2");
        assert_eq!(libraries[1].name, "objc2-foundation");
        assert_eq!(libraries[1].version, "0.4.0");
        assert!(about_libraries("").is_empty());
    }
}

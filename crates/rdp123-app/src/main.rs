//! RDP123 — a minimal menu-bar RDP client for macOS.

mod delegate;
mod file_pasteboard;
mod login_item;
mod settings;
mod ui;
mod view;
mod web_auth;
mod window;

use objc2::runtime::ProtocolObject;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate};

use delegate::AppDelegate;

fn main() {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some(file_pasteboard::FLAG) {
        file_pasteboard::serve(args);
        return;
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let mtm = MainThreadMarker::new().expect("main() must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    // Menu-bar resident: no Dock icon, no main menu bar.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let delegate = AppDelegate::new(mtm);
    let proto: &ProtocolObject<dyn NSApplicationDelegate> = ProtocolObject::from_ref(&*delegate);
    app.setDelegate(Some(proto));

    app.run();
    drop(delegate);
}

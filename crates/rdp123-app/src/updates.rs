//! Daily and on-demand update checks against GitHub Releases.

use std::cell::{Cell, RefCell};
use std::time::Duration;

use dispatch2::{DispatchQueue, DispatchTime};
use objc2_app_kit::NSWorkspace;
use objc2_foundation::{NSString, NSURL};

use rdp123_core::update::{newer_release, Release};
use rdp123_core::ProfileStore;

const FIRST_CHECK_DELAY: Duration = Duration::from_secs(10);
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// The result of one update check.
pub enum Outcome {
    UpToDate,
    Available(Release),
    Failed(String),
}

type Callback = Box<dyn FnOnce(&Outcome)>;

thread_local! {
    static AVAILABLE: RefCell<Option<Release>> = const { RefCell::new(None) };
    static CALLBACKS: RefCell<Vec<Callback>> = const { RefCell::new(Vec::new()) };
    static CHECKING: Cell<bool> = const { Cell::new(false) };
}

/// The newer release found by the last successful check, if any.
pub fn available() -> Option<Release> {
    AVAILABLE.with(|a| a.borrow().clone())
}

/// Schedule the automatic checks. Each one honours the current setting, so
/// turning it off in Settings takes effect without a restart.
pub fn start(store: ProfileStore) {
    schedule(store, FIRST_CHECK_DELAY);
}

fn schedule(store: ProfileStore, delay: Duration) {
    let Ok(when) = DispatchTime::try_from(delay) else {
        return;
    };
    let _ = DispatchQueue::main().after(when, move || {
        if automatic_checks_enabled(&store) {
            check(None);
        }
        schedule(store, CHECK_INTERVAL);
    });
}

pub fn automatic_checks_enabled(store: &ProfileStore) -> bool {
    store
        .load_document()
        .map(|document| document.settings.check_for_updates)
        .unwrap_or(true)
}

/// Check now and report the outcome to `callback` on the main thread. A check
/// already in flight is shared instead of starting a second request.
pub fn check(callback: Option<Callback>) {
    if let Some(callback) = callback {
        CALLBACKS.with(|c| c.borrow_mut().push(callback));
    }
    if CHECKING.with(|c| c.replace(true)) {
        return;
    }
    std::thread::spawn(move || {
        let outcome = match newer_release(env!("CARGO_PKG_VERSION")) {
            Ok(Some(release)) => Outcome::Available(release),
            Ok(None) => Outcome::UpToDate,
            Err(error) => Outcome::Failed(format!("{error:#}")),
        };
        DispatchQueue::main().exec_async(move || finish(outcome));
    });
}

fn finish(outcome: Outcome) {
    CHECKING.with(|c| c.set(false));
    match &outcome {
        Outcome::Available(release) => {
            tracing::info!("update available: RDP123 {}", release.version);
            AVAILABLE.with(|a| *a.borrow_mut() = Some(release.clone()));
        }
        Outcome::UpToDate => AVAILABLE.with(|a| *a.borrow_mut() = None),
        Outcome::Failed(error) => tracing::warn!("update check failed: {error}"),
    }
    for callback in CALLBACKS.with(|c| std::mem::take(&mut *c.borrow_mut())) {
        callback(&outcome);
    }
}

pub fn open_download_page(release: &Release) {
    if let Some(url) = NSURL::URLWithString(&NSString::from_str(&release.url)) {
        NSWorkspace::sharedWorkspace().openURL(&url);
    }
}

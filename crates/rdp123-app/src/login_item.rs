//! Launch-at-login via `SMAppService` (macOS 13+).
//!
//! The system is the source of truth: the Settings checkbox reflects
//! `SMAppService.status` and toggling registers/unregisters directly, so the
//! state never drifts from what System Settings → Login Items shows. Nothing
//! is stored in the profile document.

use objc2::runtime::AnyClass;
use objc2_service_management::{SMAppService, SMAppServiceStatus};

/// `SMAppService` exists on macOS 13 and later; on older systems the
/// checkbox is disabled.
pub fn is_supported() -> bool {
    AnyClass::get(c"SMAppService").is_some()
}

/// Whether the app is currently registered to launch at login.
pub fn is_enabled() -> bool {
    if !is_supported() {
        return false;
    }
    unsafe { SMAppService::mainAppService().status() == SMAppServiceStatus::Enabled }
}

/// Register or unregister the app as a login item.
pub fn set_enabled(enable: bool) -> Result<(), String> {
    if !is_supported() {
        return Err("Launch at login requires macOS 13 or newer.".to_string());
    }
    let service = unsafe { SMAppService::mainAppService() };
    let result = if enable {
        unsafe { service.registerAndReturnError() }
    } else {
        unsafe { service.unregisterAndReturnError() }
    };
    result.map_err(|error| error.localizedDescription().to_string())
}

//! Password storage backed by the macOS Keychain.
//!
//! Secrets are keyed by `(SERVICE, account)` where `account` is a connection's
//! stable id. `set_generic_password` is create-or-update in one call.

use anyhow::{anyhow, Result};
use security_framework::passwords::{
    delete_generic_password, get_generic_password, set_generic_password,
};
use zeroize::{Zeroize, Zeroizing};

const SERVICE: &str = "ch.asd123.rdp123";
/// Service name used by builds before the bundle-identifier change; entries
/// found under it are migrated on first read.
const LEGACY_SERVICE: &str = "ch.rdp123.app";

/// `errSecItemNotFound` from `<Security/SecBase.h>`: no matching Keychain item.
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

/// Store or overwrite the password for `account`.
///
/// Update in place so a failed write cannot delete the previous password.
pub fn store_password(account: &str, password: &str) -> Result<()> {
    set_generic_password(SERVICE, account, password.as_bytes())?;
    Ok(())
}

/// Load the password for `account`, or `None` if there is no entry.
///
/// Entries stored by older builds under [`LEGACY_SERVICE`] are migrated to the
/// current service name on first read.
pub fn load_password(account: &str) -> Result<Option<String>> {
    if let Some(password) = load_from_service(SERVICE, account)? {
        return Ok(Some(password));
    }
    let Some(password) = load_from_service(LEGACY_SERVICE, account)? else {
        return Ok(None);
    };
    set_generic_password(SERVICE, account, password.as_bytes())?;
    let _ = delete_generic_password(LEGACY_SERVICE, account);
    Ok(Some(password))
}

/// Copy a saved password to an independent Keychain entry, if one exists.
pub fn copy_password(source: &str, destination: &str) -> Result<bool> {
    let Some(password) = load_password(source)?.map(Zeroizing::new) else {
        return Ok(false);
    };
    store_password(destination, &password)?;
    Ok(true)
}

fn load_from_service(service: &str, account: &str) -> Result<Option<String>> {
    match get_generic_password(service, account) {
        // `from_utf8` moves the bytes into the String (no stray copy); a lossy
        // conversion would silently corrupt the password into a baffling
        // "logon failed", so report it instead.
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(password) => Ok(Some(password)),
            Err(e) => {
                let mut bytes = e.into_bytes();
                bytes.zeroize();
                Err(anyhow!(
                    "the stored password is not valid text; delete it in Settings and save it again"
                ))
            }
        },
        Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Delete the password for `account`, including an entry an older build left
/// under [`LEGACY_SERVICE`] that [`load_password`] would otherwise bring back.
/// Missing entries are treated as success.
pub fn delete_password(account: &str) -> Result<()> {
    for service in [SERVICE, LEGACY_SERVICE] {
        match delete_generic_password(service, account) {
            Ok(()) => {}
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

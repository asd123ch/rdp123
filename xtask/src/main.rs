//! Build automation for RDP123.
//!
//! `cargo xtask bundle` builds the release binary, assembles `RDP123.app`, and
//! code-signs it. Signing identity comes from `RDP123_SIGN_IDENTITY` (defaults
//! to ad-hoc `-`). See the README for why a stable self-signed identity is
//! preferable to ad-hoc for repeated Keychain access.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

const APP_NAME: &str = "RDP123";
const BIN_NAME: &str = "rdp123";
const BUNDLE_ID: &str = "ch.asd123.rdp123";
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> Result<()> {
    let cmd = env::args().nth(1).unwrap_or_default();
    match cmd.as_str() {
        "bundle" => bundle().map(|_| ()),
        "install" => install(),
        other => {
            eprintln!("unknown xtask command: {other:?}");
            eprintln!("usage: cargo xtask [bundle|install]");
            std::process::exit(2);
        }
    }
}

const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";

fn lsregister(args: &[&str]) {
    let _ = Command::new(LSREGISTER).args(args).status();
}

/// Build, then install into /Applications as the single copy. Removes the build
/// copy from `dist/` and Launch Services so the app doesn't appear twice in
/// Spotlight/Launchpad.
fn install() -> Result<()> {
    let app = bundle()?;
    let dest = Path::new("/Applications").join(format!("{APP_NAME}.app"));

    // Quit a running instance so the copy can replace it.
    let _ = Command::new("pkill")
        .args(["-f", "RDP123.app/Contents/MacOS"])
        .status();

    if dest.exists() {
        fs::remove_dir_all(&dest).ok();
    }
    // `cp -R` would MERGE into a leftover bundle, producing a mixed-version app
    // with a broken signature — refuse instead.
    if dest.exists() {
        bail!(
            "could not remove the old {} — close the app and remove it manually",
            dest.display()
        );
    }
    run(Command::new("cp").arg("-R").arg(&app).arg("/Applications/"))
        .context("copying app into /Applications")?;

    // Keep exactly one registered copy.
    lsregister(&["-u", &app.to_string_lossy()]);
    fs::remove_dir_all(&app).ok();
    lsregister(&["-f", &dest.to_string_lossy()]);

    println!("Installed: {}", dest.display());
    println!("Launch:  open {}", dest.display());
    Ok(())
}

fn workspace_root() -> PathBuf {
    // xtask/ -> workspace root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask has a parent directory")
        .to_path_buf()
}

fn bundle() -> Result<PathBuf> {
    let root = workspace_root();

    // Make the app's build script rerun so the About tab's build time and commit
    // reflect this build.
    let _ = Command::new("touch")
        .arg(root.join("crates/rdp123-app/build.rs"))
        .status();

    run(Command::new(env!("CARGO")).current_dir(&root).args([
        "build",
        "--release",
        "-p",
        "rdp123-app",
        "--locked",
    ]))
    .context("cargo build failed")?;

    let bin = root.join("target/release").join(BIN_NAME);
    if !bin.exists() {
        bail!("release binary not found at {}", bin.display());
    }

    let app = root.join("dist").join(format!("{APP_NAME}.app"));
    if app.exists() {
        fs::remove_dir_all(&app).ok();
    }
    let macos = app.join("Contents/MacOS");
    let resources = app.join("Contents/Resources");
    fs::create_dir_all(&macos)?;
    fs::create_dir_all(&resources)?;

    fs::copy(&bin, macos.join(BIN_NAME)).context("copy binary into bundle")?;
    fs::write(app.join("Contents/Info.plist"), info_plist())?;
    fs::write(app.join("Contents/PkgInfo"), "APPL????")?;

    // Optional icon: assets/AppIcon.icns -> Resources/AppIcon.icns
    let icon = root.join("assets/AppIcon.icns");
    if icon.exists() {
        fs::copy(&icon, resources.join("AppIcon.icns"))?;
    }

    fs::copy(root.join("LICENSE"), resources.join("LICENSE"))
        .context("copy LICENSE into bundle")?;

    codesign(&app)?;

    // Don't let the build copy show up as a second app in Spotlight/Launchpad.
    lsregister(&["-u", &app.to_string_lossy()]);

    println!("\nBundled: {}", app.display());
    println!("Install: cargo xtask install   (into /Applications, single copy)");
    Ok(app)
}

/// The stable self-signed identity created by `scripts/make-signing-identity.sh`.
const STABLE_IDENTITY: &str = "RDP123 Local";

fn signing_keychain() -> Option<(PathBuf, String)> {
    let home = env::var_os("HOME")?;
    let keychain = Path::new(&home)
        .join("Library/Keychains")
        .join("rdp123-signing.keychain-db");
    let password_file = Path::new(&home)
        .join("Library/Application Support/RDP123")
        .join("signing-keychain-password");
    if !keychain.exists() {
        return None;
    }
    let password = fs::read_to_string(password_file).ok()?;
    let password = password.trim().to_string();
    (!password.is_empty()).then_some((keychain, password))
}

fn codesign(app: &Path) -> Result<()> {
    let app_path = app.to_string_lossy().into_owned();

    // 1. Explicit override wins (user-supplied identity in the default keychains).
    if let Ok(id) = env::var("RDP123_SIGN_IDENTITY") {
        if !id.trim().is_empty() {
            run(Command::new("codesign").args([
                "--force",
                "--deep",
                "--sign",
                &id,
                "--timestamp=none",
                &app_path,
            ]))
            .context("codesign failed")?;
            println!("Signed with identity: {id}");
            return Ok(());
        }
    }

    // 2. Dedicated signing keychain from make-signing-identity.sh.
    if let Some((kc, password)) = signing_keychain() {
        let kc = kc.to_string_lossy().into_owned();
        // Unlock, then (re)authorize Apple signing tools to use the private key.
        // The random keychain password is stored in a user-only file by the
        // setup script, keeping it out of the repository and process defaults.
        let _ = Command::new("security")
            .args(["unlock-keychain", "-p", &password, &kc])
            .status();
        let _ = Command::new("security")
            .args([
                "set-key-partition-list",
                "-S",
                "apple-tool:,apple:",
                "-s",
                "-k",
                &password,
                &kc,
            ])
            .output();
        let present = Command::new("security")
            .args(["find-identity", "-p", "codesigning", &kc])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(STABLE_IDENTITY))
            .unwrap_or(false);
        // Pin a designated requirement so the code identity (and thus the Local
        // Network grant / Keychain ACL) stays stable across rebuilds.
        let requirement = format!(
            "=designated => identifier \"{BUNDLE_ID}\" and certificate leaf[subject.CN] = \"{STABLE_IDENTITY}\""
        );
        let signed = present
            && Command::new("codesign")
                .args([
                    "--force",
                    "--sign",
                    STABLE_IDENTITY,
                    "--keychain",
                    &kc,
                    "--timestamp=none",
                    "--requirements",
                    &requirement,
                    &app_path,
                ])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
        if signed {
            println!("Signed with identity: {STABLE_IDENTITY} (dedicated keychain)");
            return Ok(());
        }
    }

    // 3. Ad-hoc fallback: enough to launch locally, mandatory on Apple Silicon.
    run(Command::new("codesign").args([
        "--force",
        "--deep",
        "--sign",
        "-",
        "--timestamp=none",
        &app_path,
    ]))
    .context("codesign failed")?;
    println!("Signed with identity: - (ad-hoc)");
    eprintln!(
        "note: ad-hoc signed. macOS Keychain 'Always Allow' and the Local Network\n\
         permission will NOT persist across rebuilds, so you may be re-prompted and\n\
         LAN connections may fail on the first try after each build.\n\
         Run scripts/make-signing-identity.sh once for stable, prompt-free signing."
    );
    Ok(())
}

/// A monotonic build number so Launch Services can tell rebuilds of the same
/// marketing version apart: the git commit count, falling back to the version.
fn build_number() -> String {
    Command::new("git")
        .args(["rev-list", "--count", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| VERSION.to_string())
}

fn info_plist() -> String {
    let build = build_number();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>{APP_NAME}</string>
    <key>CFBundleDisplayName</key><string>{APP_NAME}</string>
    <key>CFBundleIdentifier</key><string>{BUNDLE_ID}</string>
    <key>CFBundleExecutable</key><string>{BIN_NAME}</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>{VERSION}</string>
    <key>CFBundleVersion</key><string>{build}</string>
    <key>CFBundleIconFile</key><string>AppIcon</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>LSUIElement</key><true/>
    <key>NSLocalNetworkUsageDescription</key><string>RDP123 connects to remote desktop hosts on your local network by their IP address or hostname.</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSPrincipalClass</key><string>NSApplication</string>
    <key>NSHumanReadableCopyright</key><string>Copyright (c) 2026 asd123.ai. AGPL-3.0-only.</string>
</dict>
</plist>
"#
    )
}

fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("failed to spawn {cmd:?}"))?;
    if !status.success() {
        bail!("command {cmd:?} exited with {status}");
    }
    Ok(())
}

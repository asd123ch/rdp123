//! Build automation for RDP123.
//!
//! `cargo xtask bundle` builds the release binary, assembles `RDP123.app`, and
//! code-signs it. Signing identity comes from `RDP123_SIGN_IDENTITY` (defaults
//! to ad-hoc `-`). See the README for why a stable self-signed identity is
//! preferable to ad-hoc for repeated Keychain access.

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
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
    let libraries = runtime_libraries(&root)?;
    let home = PathBuf::from(env::var_os("HOME").context("HOME is not set")?);
    if !home.is_absolute() {
        bail!("HOME must be an absolute path for build-path remapping");
    }
    let rustflags = env::var_os("CARGO_ENCODED_RUSTFLAGS").unwrap_or_else(|| {
        env::var("RUSTFLAGS")
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join("\x1f")
            .into()
    });

    // Make the app's build script rerun so the About tab's build time and commit
    // reflect this build.
    let _ = Command::new("touch")
        .arg(root.join("crates/rdp123-app/build.rs"))
        .status();

    run(Command::new(env!("CARGO"))
        .current_dir(&root)
        .args(["build", "--release", "-p", "rdp123-app", "--locked"])
        .env(
            "CARGO_ENCODED_RUSTFLAGS",
            remap_build_paths(rustflags, &root, &home),
        )
        .env("RDP123_LIBS", libraries))
    .context("cargo build failed")?;

    let bin = root.join("target/release").join(BIN_NAME);
    if !bin.exists() {
        bail!("release binary not found at {}", bin.display());
    }
    let binary = fs::read(&bin).context("checking release binary for private build paths")?;
    for path in [&root, &home] {
        let prefix = path.as_os_str().as_encoded_bytes();
        if binary.windows(prefix.len()).any(|window| window == prefix) {
            bail!("release binary still contains a private build path; refusing to bundle it");
        }
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

fn remap_build_paths(mut flags: OsString, root: &Path, home: &Path) -> OsString {
    // Cargo's encoded flags keep paths with spaces intact. The more specific
    // workspace mapping comes last because rustc uses the last matching prefix.
    for (from, to) in [(home, "/build/home"), (root, "/build/rdp123")] {
        if !flags.is_empty() {
            flags.push("\x1f");
        }
        flags.push("--remap-path-prefix=");
        flags.push(from);
        flags.push("=");
        flags.push(to);
    }
    flags
}

fn runtime_libraries(root: &Path) -> Result<String> {
    let output = Command::new(env!("CARGO"))
        .current_dir(root)
        .args([
            "tree",
            "--locked",
            "--edges",
            "normal",
            "--prefix",
            "depth",
            "--depth",
            "1",
            "--no-dedupe",
            "--format",
            "{p}",
            "-p",
            "rdp123-app",
            "-p",
            "rdp123-core",
        ])
        .output()
        .context("running cargo tree for About metadata")?;
    if !output.status.success() {
        bail!(
            "cargo tree failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let tree = String::from_utf8(output.stdout).context("cargo tree output was not UTF-8")?;
    Ok(parse_runtime_libraries(&tree))
}

fn parse_runtime_libraries(tree: &str) -> String {
    let mut libraries = BTreeMap::new();
    for line in tree.lines() {
        let Some(package) = line.strip_prefix('1') else {
            continue;
        };
        let mut fields = package.split_whitespace();
        let (Some(name), Some(version)) = (fields.next(), fields.next()) else {
            continue;
        };
        if name == "rdp123-core" {
            continue;
        }
        if let Some(version) = version.strip_prefix('v') {
            libraries.insert(name, version);
        }
    }
    libraries
        .into_iter()
        .map(|(name, version)| format!("{name} {version}"))
        .collect::<Vec<_>>()
        .join(";")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_path_remapping_preserves_existing_flags_and_spaces() {
        let flags = remap_build_paths(
            OsString::from("-C\x1ftarget-cpu=apple-m1"),
            Path::new("/Users/build user/Work/RDP123"),
            Path::new("/Users/build user"),
        );
        assert_eq!(
            flags,
            "-C\x1ftarget-cpu=apple-m1\x1f\
             --remap-path-prefix=/Users/build user=/build/home\x1f\
             --remap-path-prefix=/Users/build user/Work/RDP123=/build/rdp123"
        );
    }

    #[test]
    fn build_path_remapping_starts_without_an_empty_flag() {
        let flags = remap_build_paths(
            OsString::new(),
            Path::new("/workspace/RDP123"),
            Path::new("/home/builder"),
        );
        assert_eq!(
            flags,
            "--remap-path-prefix=/home/builder=/build/home\x1f\
             --remap-path-prefix=/workspace/RDP123=/build/rdp123"
        );
    }

    #[test]
    fn runtime_libraries_keep_direct_external_packages_once() {
        let tree = "\
0rdp123-app v0.6.10 (/workspace/crates/rdp123-app)\n\
1objc2 v0.6.4\n\
1rdp123-core v0.6.10 (/workspace/crates/rdp123-core)\n\
1tokio v1.53.1\n\
\n\
0rdp123-core v0.6.10 (/workspace/crates/rdp123-core)\n\
1anyhow v1.0.104\n\
1tokio v1.53.1\n";

        assert_eq!(
            parse_runtime_libraries(tree),
            "anyhow 1.0.104;objc2 0.6.4;tokio 1.53.1"
        );
    }
}

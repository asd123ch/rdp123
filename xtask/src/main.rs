//! Build automation for RDP123.
//!
//! `cargo xtask bundle` builds the release binary, assembles `RDP123.app`, and
//! code-signs it. Signing identity comes from `RDP123_SIGN_IDENTITY` (defaults
//! to ad-hoc `-`). See the README for why a stable self-signed identity is
//! preferable to ad-hoc for repeated Keychain access.
//!
//! `cargo xtask release` builds the Apple Silicon app, signs it with a Developer ID,
//! notarizes the app and its DMG, and writes the matching Homebrew cask. It
//! never uploads anything.

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
        "release" => release(env::args().skip(2).any(|arg| arg == "--skip-notarization")),
        other => {
            eprintln!("unknown xtask command: {other:?}");
            eprintln!("usage: cargo xtask [bundle|install|release [--skip-notarization]]");
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
    let stage_root = dest
        .parent()
        .unwrap()
        .join(format!(".rdp123-install-{}", std::process::id()));
    fs::create_dir(&stage_root).context("creating installation staging directory")?;
    let staged = stage_root.join(format!("{APP_NAME}.app"));
    let prepared = run(Command::new("cp").arg("-R").arg(&app).arg(&staged))
        .context("staging the new app")
        .and_then(|()| {
            run(Command::new("codesign")
                .args(["--verify", "--deep", "--strict"])
                .arg(&staged))
        });
    if let Err(error) = prepared {
        let _ = fs::remove_dir_all(&stage_root);
        return Err(error.context("the installed app was left unchanged"));
    }

    // Quit a running instance so the copy can replace it.
    let _ = Command::new("pkill")
        .args(["-f", "RDP123.app/Contents/MacOS"])
        .status();

    replace_staged_app(&staged, &dest, &stage_root.join("previous.app"))?;
    fs::remove_dir_all(&stage_root).context("removing the installation staging directory")?;

    // Keep exactly one registered copy.
    lsregister(&["-u", &app.to_string_lossy()]);
    fs::remove_dir_all(&app).ok();
    lsregister(&["-f", &dest.to_string_lossy()]);

    println!("Installed: {}", dest.display());
    println!("Launch:  open {}", dest.display());
    Ok(())
}

fn replace_staged_app(staged: &Path, dest: &Path, backup: &Path) -> Result<()> {
    let had_previous = dest.exists();
    if had_previous {
        fs::rename(dest, backup).context("backing up the installed app")?;
    }
    if let Err(error) = fs::rename(staged, dest) {
        if had_previous {
            fs::rename(backup, dest).with_context(|| {
                format!(
                    "installation failed ({error}); restore the previous app from {}",
                    backup.display()
                )
            })?;
        }
        return Err(error).context("installing the staged app; the previous app was preserved");
    }
    Ok(())
}

fn workspace_root() -> PathBuf {
    // xtask/ -> workspace root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask has a parent directory")
        .to_path_buf()
}

struct BuildEnv {
    root: PathBuf,
    rustflags: OsString,
    libraries: String,
}

impl BuildEnv {
    fn new() -> Result<Self> {
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

        // Make the app's build script rerun so the About tab's build time and
        // commit reflect this build.
        let _ = Command::new("touch")
            .arg(root.join("crates/rdp123-app/build.rs"))
            .status();

        let rustflags = remap_build_paths(rustflags, &root, &home);
        Ok(Self {
            root,
            rustflags,
            libraries,
        })
    }

    /// Build the app binary, for the host or for one target triple.
    fn build(&self, target: Option<&str>) -> Result<PathBuf> {
        let mut cargo = Command::new(env!("CARGO"));
        cargo
            .current_dir(&self.root)
            .args(["build", "--release", "-p", "rdp123-app", "--locked"])
            .env("CARGO_ENCODED_RUSTFLAGS", &self.rustflags)
            .env("RDP123_LIBS", &self.libraries);
        let mut out = self.root.join("target");
        if let Some(target) = target {
            cargo.args(["--target", target]);
            out.push(target);
        }
        run(&mut cargo).context("cargo build failed")?;
        let bin = out.join("release").join(BIN_NAME);
        if !bin.exists() {
            bail!("release binary not found at {}", bin.display());
        }
        Ok(bin)
    }

    fn check_private_paths(&self, bin: &Path) -> Result<()> {
        let home = PathBuf::from(env::var_os("HOME").context("HOME is not set")?);
        let binary = fs::read(bin).context("checking release binary for private build paths")?;
        for path in [&self.root, &home] {
            let prefix = path.as_os_str().as_encoded_bytes();
            if binary.windows(prefix.len()).any(|window| window == prefix) {
                bail!("release binary still contains a private build path; refusing to bundle it");
            }
        }
        Ok(())
    }

    /// Assemble `dist/RDP123.app` around an already built binary (unsigned).
    fn assemble(&self, bin: &Path) -> Result<PathBuf> {
        self.check_private_paths(bin)?;
        let app = self.root.join("dist").join(format!("{APP_NAME}.app"));
        if app.exists() {
            fs::remove_dir_all(&app).ok();
        }
        let macos = app.join("Contents/MacOS");
        let resources = app.join("Contents/Resources");
        fs::create_dir_all(&macos)?;
        fs::create_dir_all(&resources)?;

        fs::copy(bin, macos.join(BIN_NAME)).context("copy binary into bundle")?;
        fs::write(app.join("Contents/Info.plist"), info_plist())?;
        fs::write(app.join("Contents/PkgInfo"), "APPL????")?;

        let icon = self.root.join("assets/AppIcon.icns");
        if icon.exists() {
            fs::copy(&icon, resources.join("AppIcon.icns"))?;
        }

        fs::copy(self.root.join("LICENSE"), resources.join("LICENSE"))
            .context("copy LICENSE into bundle")?;
        Ok(app)
    }
}

fn bundle() -> Result<PathBuf> {
    let build = BuildEnv::new()?;
    let bin = build.build(None)?;
    let app = build.assemble(&bin)?;
    codesign(&app)?;

    // Don't let the build copy show up as a second app in Spotlight/Launchpad.
    lsregister(&["-u", &app.to_string_lossy()]);

    println!("\nBundled: {}", app.display());
    println!("Install: cargo xtask install   (into /Applications, single copy)");
    Ok(app)
}

const RELEASE_TARGET: &str = "aarch64-apple-darwin";

/// Credentials for `notarytool`, read from the file named by
/// `RDP123_NOTARIZATION_ENV` (`APPLE_API_KEY`, `APPLE_API_ISSUER`,
/// `APPLE_API_KEY_PATH`).
struct Notary {
    key_id: String,
    issuer: String,
    key_path: PathBuf,
}

impl Notary {
    fn from_env() -> Result<Self> {
        let file = env::var_os("RDP123_NOTARIZATION_ENV").context(
            "set RDP123_NOTARIZATION_ENV to the notarization credentials file \
             (or pass --skip-notarization for a local test build)",
        )?;
        let text = fs::read_to_string(&file)
            .with_context(|| format!("reading {}", Path::new(&file).display()))?;
        let home = env::var("HOME").context("HOME is not set")?;
        let values = parse_env_file(&text, &home);
        let get = |key: &str| {
            values
                .get(key)
                .filter(|value| !value.is_empty())
                .cloned()
                .with_context(|| format!("{key} is missing from the notarization file"))
        };
        let notary = Self {
            key_id: get("APPLE_API_KEY")?,
            issuer: get("APPLE_API_ISSUER")?,
            key_path: PathBuf::from(get("APPLE_API_KEY_PATH")?),
        };
        if !notary.key_path.is_file() {
            bail!("APPLE_API_KEY_PATH points to a missing file");
        }
        Ok(notary)
    }

    /// Upload `upload` for notarization, then staple the ticket to `staple`.
    fn submit(&self, upload: &Path, staple: &Path) -> Result<()> {
        println!("Notarizing {} …", staple.display());
        // Report failures without the command line: it names the API key.
        let status = Command::new("xcrun")
            .args(["notarytool", "submit"])
            .arg(upload)
            .arg("--key")
            .arg(&self.key_path)
            .args(["--key-id", &self.key_id, "--issuer", &self.issuer, "--wait"])
            .status()
            .context("starting notarytool")?;
        if !status.success() {
            bail!("notarization failed ({status}); see the notarytool output above");
        }
        run(Command::new("xcrun")
            .args(["stapler", "staple"])
            .arg(staple))
        .context("stapling the notarization ticket failed")?;
        run(Command::new("xcrun")
            .args(["stapler", "validate"])
            .arg(staple))
    }
}

/// Parse `KEY=value` lines as written for a POSIX shell, expanding `$HOME`.
fn parse_env_file(text: &str, home: &str) -> BTreeMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let line = line.strip_prefix("export ").unwrap_or(line);
            let (key, value) = line.split_once('=')?;
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(value);
            let value = match value.strip_prefix("~/") {
                Some(rest) => format!("{home}/{rest}"),
                None => value.replace("${HOME}", home).replace("$HOME", home),
            };
            Some((key.trim().to_string(), value))
        })
        .collect()
}

fn developer_id_identity() -> Result<String> {
    if let Ok(identity) = env::var("RDP123_RELEASE_IDENTITY") {
        if !identity.trim().is_empty() {
            return Ok(identity);
        }
    }
    let output = Command::new("security")
        .args(["find-identity", "-v", "-p", "codesigning"])
        .output()
        .context("listing code-signing identities")?;
    let listing = String::from_utf8_lossy(&output.stdout);
    let identities: Vec<&str> = listing
        .lines()
        .filter_map(|line| line.split('"').nth(1))
        .filter(|name| name.starts_with("Developer ID Application:"))
        .collect();
    match identities.as_slice() {
        [identity] => Ok((*identity).to_string()),
        [] => bail!("no Developer ID Application identity found in the keychain"),
        _ => bail!("several Developer ID identities found; set RDP123_RELEASE_IDENTITY"),
    }
}

fn release(skip_notarization: bool) -> Result<()> {
    let root = workspace_root();
    let notary = if skip_notarization {
        None
    } else {
        let status = Command::new("git")
            .current_dir(&root)
            .args(["status", "--porcelain"])
            .output()
            .context("checking the working tree")?;
        if !status.stdout.is_empty() {
            bail!("the working tree has uncommitted changes; commit them before a release");
        }
        Some(Notary::from_env()?)
    };
    let identity = developer_id_identity()?;

    let build = BuildEnv::new()?;
    let bin = build.build(Some(RELEASE_TARGET))?;
    let app = build.assemble(&bin)?;
    run(Command::new("codesign")
        .args(["--force", "--options", "runtime", "--timestamp", "--sign"])
        .arg(&identity)
        .arg(&app))
    .context("signing the app with the Developer ID")?;
    run(Command::new("codesign")
        .args(["--verify", "--deep", "--strict", "--verbose=2"])
        .arg(&app))?;
    lsregister(&["-u", &app.to_string_lossy()]);

    let work = root.join("target/release-work");
    if work.exists() {
        fs::remove_dir_all(&work).context("clearing the release work directory")?;
    }
    fs::create_dir_all(&work)?;

    if let Some(notary) = &notary {
        let zip = work.join(format!("{APP_NAME}.zip"));
        run(Command::new("ditto")
            .args(["-c", "-k", "--keepParent"])
            .arg(&app)
            .arg(&zip))?;
        notary.submit(&zip, &app)?;
        run(Command::new("spctl")
            .args(["--assess", "--type", "execute", "--verbose=2"])
            .arg(&app))
        .context("Gatekeeper rejected the app")?;
    }

    let stage = work.join("dmg");
    fs::create_dir_all(&stage)?;
    run(Command::new("ditto")
        .arg(&app)
        .arg(stage.join(format!("{APP_NAME}.app"))))?;
    std::os::unix::fs::symlink("/Applications", stage.join("Applications"))
        .context("adding the Applications link to the DMG")?;
    let dmg = root.join("dist").join(format!("{APP_NAME}-{VERSION}.dmg"));
    if dmg.exists() {
        fs::remove_file(&dmg)?;
    }
    run(Command::new("hdiutil")
        .args(["create", "-quiet", "-volname", APP_NAME, "-srcfolder"])
        .arg(&stage)
        .args(["-format", "UDZO"])
        .arg(&dmg))
    .context("creating the DMG")?;
    run(Command::new("codesign")
        .args(["--force", "--timestamp", "--sign"])
        .arg(&identity)
        .arg(&dmg))
    .context("signing the DMG")?;
    run(Command::new("hdiutil").args(["verify", "-quiet"]).arg(&dmg))?;

    if let Some(notary) = &notary {
        notary.submit(&dmg, &dmg)?;
        run(Command::new("spctl")
            .args([
                "--assess",
                "--type",
                "open",
                "--context",
                "context:primary-signature",
                "--verbose=2",
            ])
            .arg(&dmg))
        .context("Gatekeeper rejected the DMG")?;
    }
    fs::remove_dir_all(&work).ok();

    let sha256 = sha256_of(&dmg)?;
    println!("\nDMG:     {}", dmg.display());
    println!("SHA-256: {sha256}");
    if notary.is_some() {
        let template = fs::read_to_string(root.join("packaging/rdp123.rb.in"))
            .context("reading packaging/rdp123.rb.in")?;
        let cask = root.join("dist/rdp123.rb");
        fs::write(&cask, render_cask(&template, VERSION, &sha256))?;
        println!("Cask:    {}", cask.display());
    } else {
        println!("Not notarized (--skip-notarization): for local testing only, no cask written.");
    }
    Ok(())
}

fn sha256_of(file: &Path) -> Result<String> {
    let output = Command::new("shasum")
        .args(["-a", "256"])
        .arg(file)
        .output()
        .context("running shasum")?;
    if !output.status.success() {
        bail!("shasum failed for {}", file.display());
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .map(str::to_string)
        .context("shasum printed no checksum")
}

fn render_cask(template: &str, version: &str, sha256: &str) -> String {
    template
        .replace("@VERSION@", version)
        .replace("@SHA256@", sha256)
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
    fn failed_replacement_restores_the_installed_app() {
        let root = env::temp_dir().join(format!("rdp123-install-test-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let dest = root.join("installed.app");
        fs::create_dir(&dest).unwrap();
        fs::write(dest.join("version"), "previous").unwrap();
        assert!(
            replace_staged_app(&root.join("missing.app"), &dest, &root.join("backup.app")).is_err()
        );
        assert_eq!(
            fs::read_to_string(dest.join("version")).unwrap(),
            "previous"
        );
        let staged = root.join("staged.app");
        fs::create_dir(&staged).unwrap();
        fs::write(staged.join("version"), "new").unwrap();
        replace_staged_app(&staged, &dest, &root.join("backup.app")).unwrap();
        assert_eq!(fs::read_to_string(dest.join("version")).unwrap(), "new");
        assert_eq!(
            fs::read_to_string(root.join("backup.app/version")).unwrap(),
            "previous"
        );
        fs::remove_dir_all(root).unwrap();
    }

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
    fn notarization_file_accepts_shell_syntax() {
        let values = parse_env_file(
            "# comment\nexport APPLE_API_KEY=\"ABC123\"\nAPPLE_API_ISSUER='issuer'\n\
             APPLE_API_KEY_PATH=$HOME/keys/AuthKey.p8\nOTHER=~/x\n",
            "/Users/me",
        );
        assert_eq!(values["APPLE_API_KEY"], "ABC123");
        assert_eq!(values["APPLE_API_ISSUER"], "issuer");
        assert_eq!(values["APPLE_API_KEY_PATH"], "/Users/me/keys/AuthKey.p8");
        assert_eq!(values["OTHER"], "/Users/me/x");
    }

    #[test]
    fn cask_template_receives_version_and_checksum() {
        assert_eq!(
            render_cask("version \"@VERSION@\"\nsha256 \"@SHA256@\"", "1.2.3", "abc"),
            "version \"1.2.3\"\nsha256 \"abc\""
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

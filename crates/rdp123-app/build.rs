//! Capture build metadata for the Settings → About tab: build time, git commit,
//! and the versions of the main libraries (read from Cargo.lock). Emitted as
//! compile-time env vars consumed via `env!(...)`.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

fn main() {
    let build_time =
        run(&["date", "-u", "+%Y-%m-%d %H:%M UTC"]).unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=RDP123_BUILD_TIME={build_time}");

    let git = run(&["git", "rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "nogit".into());
    println!("cargo:rustc-env=RDP123_GIT={git}");

    println!(
        "cargo:rustc-env=RDP123_LIBS={}",
        read_libs().unwrap_or_default()
    );

    // Rerun when the lockfile or the checked-out commit changes; xtask also
    // touches this file on every bundle so the timestamp stays fresh.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../Cargo.lock");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
}

fn run(args: &[&str]) -> Option<String> {
    let out = Command::new(args[0]).args(&args[1..]).output().ok()?;
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Extract `name version` for a curated set of crates from Cargo.lock, joined by
/// `;` (env vars can't hold newlines; the UI splits on `;`).
fn read_libs() -> Option<String> {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let text = std::fs::read_to_string(Path::new(&manifest).join("../../Cargo.lock")).ok()?;

    let mut versions: BTreeMap<String, String> = BTreeMap::new();
    let mut name: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if line == "[[package]]" {
            name = None;
        } else if let Some(v) = line.strip_prefix("name = ") {
            name = Some(v.trim_matches('"').to_string());
        } else if let Some(v) = line.strip_prefix("version = ") {
            if let Some(n) = name.take() {
                versions.insert(n, v.trim_matches('"').to_string());
            }
        }
    }

    let wanted = [
        "ironrdp",
        "tokio",
        "rustls",
        "objc2",
        "objc2-app-kit",
        "objc2-foundation",
        "security-framework",
        "serde",
        "serde_json",
        "anyhow",
        "uuid",
        "sha2",
        "zeroize",
        "directories",
        "tracing",
    ];
    let mut parts = Vec::new();
    for w in wanted {
        if let Some(v) = versions.get(w) {
            parts.push(format!("{w} {v}"));
        }
    }
    Some(parts.join(";"))
}

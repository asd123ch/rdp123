use std::process::Command;

fn main() {
    let build_time =
        run(&["date", "-u", "+%Y-%m-%d %H:%M UTC"]).unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=RDP123_BUILD_TIME={build_time}");

    let git = run(&["git", "rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "nogit".into());
    println!("cargo:rustc-env=RDP123_GIT={git}");

    let libraries = std::env::var("RDP123_LIBS").unwrap_or_default();
    println!("cargo:rustc-env=RDP123_LIBS={libraries}");

    // xtask touches this file on every bundle so the timestamp stays fresh.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
    println!("cargo:rerun-if-env-changed=RDP123_LIBS");
}

fn run(args: &[&str]) -> Option<String> {
    let out = Command::new(args[0]).args(&args[1..]).output().ok()?;
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

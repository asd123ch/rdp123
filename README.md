<p align="center">
  <img src="assets/logo.png" alt="RDP123" width="120">
</p>

<h1 align="center">RDP123</h1>

<p align="center">
  A minimal, fast, and responsive <strong>RDP client for macOS</strong>, built in Rust.
</p>

<p align="center">
  <img alt="Version 0.5.0" src="https://img.shields.io/badge/version-0.5.0-2f81f7">
  <a href="LICENSE"><img alt="GNU GPL v3.0" src="https://img.shields.io/badge/license-GPLv3-3da639"></a>
  <img alt="macOS 11 or newer" src="https://img.shields.io/badge/macOS-11%2B-black">
  <img alt="Rust 1.89 or newer" src="https://img.shields.io/badge/Rust-1.89%2B-b7410e">
  <a href="#privacy"><img alt="Privacy: no telemetry" src="https://img.shields.io/badge/privacy-no_telemetry-6f42c1"></a>
</p>

<p align="center">
  <a href="#features">Features</a> ·
  <a href="#build--install">Build</a> ·
  <a href="#usage">Usage</a> ·
  <a href="#security">Security</a> ·
  <a href="#privacy">Privacy</a> ·
  <a href="#credits">Credits</a>
</p>

RDP123 lives in the menu bar and opens a clean remote window: nothing but the
native title bar and your Windows desktop. It's built to be **simple to set up,
responsive, and pleasant to use**. Pick a saved connection and you're in.

<p align="center">
  <img src="assets/screenshot-session.png" alt="A remote Windows 11 desktop in a plain macOS window" width="720">
</p>

## Highlights

**Native through and through.** No Electron, no web view, no bundled browser:
a real AppKit app written in Rust (`objc2`), a few megabytes on disk, near-zero
idle footprint. It sits in the menu bar until you need it.

**Small on purpose.** The installed app is about 8 MB. For comparison,
Microsoft's Windows App installs at over 200 MB and Remote Desktop Manager at
close to a gigabyte (sizes as installed, mid-2026). Fewer features, sure, but
the ones you actually use every day, in a fraction of the footprint.

<p align="center">
  <img src="assets/size-comparison.svg" alt="Installed size: RDP123 8 MB, Microsoft Windows App 228 MB, Remote Desktop Manager (Devolutions) 993 MB" width="640">
</p>

**A latency-obsessed render path.** Most remote-desktop lag is homemade:
fixed frame timers, full-frame copies, texture uploads. RDP123 removes all
three:

- The first update after quiet **paints immediately**, so typing and cursor
  feedback carry no artificial delay. Only bursts are coalesced (8 ms window,
  so sustained streams still reach 120 fps on ProMotion displays).
- **Dirty-region tracking** end to end: a keystroke syncs kilobytes, not the
  whole screen. RDP traffic is mostly small rectangles, so this is the common
  case, not the exception.
- **Zero-copy presentation**: frames live in pooled IOSurfaces that
  CoreAnimation binds directly as GPU textures, the same technique Chromium
  and mpv use. No per-frame image objects, no full-frame uploads, no per-frame
  allocations at steady state.

**The real RDP 8 graphics pipeline, done properly.** Beyond H.264, RDP 8
(EGFX) mixes several codecs per frame: RemoteFX Progressive for images,
ClearCodec for text and UI, cache-driven compositing for everything that
repeats. RDP123 implements the full client-side suite (Progressive, ClearCodec
including the NSCodec subcodec, Planar, AVC420), each decoder verified
line-by-line against the reference implementation, fixing several upstream
library bugs along the way. It's selectable per connection and marked
experimental until it is pixel-perfect; the battle-tested Classic mode is the
default.

**Cursor shapes without cursor lag.** The server's pointer shapes (resize
arrows, I-beam, hand) are decoded and applied to the *native* macOS cursor.
You get the correct shape with zero compositing latency, instead of the laggy
server-drawn cursor most clients show.

**Secure by design, not by checkbox.** Passwords live only in the macOS
Keychain. Server keys are pinned on first use, *before* any credentials are
sent. No telemetry, no accounts, no listening ports. And since you build and
sign the app yourself, you know exactly what runs.

## Features

- **Lives in the menu bar** — click the menu-bar item, pick a connection, connect.
  A session window shows in the Dock and ⌘-Tab while open so you can raise it
  again, and the app quietly returns to the menu bar when you're done.
- **Clean remote window** — a standard macOS title bar and your desktop edge to
  edge. Text is rendered with font smoothing and the native pointer is used, so
  it stays crisp and responsive.
- **Two graphics pipelines** — the proven Classic (RDP 6.1) mode by default,
  and the RDP 8 Graphics Pipeline (H.264/AVC420, RemoteFX Progressive,
  ClearCodec, with automatic ZGFX compression) selectable per connection while
  it finishes maturing.
- **Remote cursor shapes, native pointer** — resize arrows, I-beam and hand
  follow the remote UI, applied to the macOS cursor with zero lag.
- **Resizes with you** — the remote resolution follows the window (including
  Retina/HiDPI), or you can pin a fixed size. The window can reopen at the size
  you left it.
- **Shared clipboard** — copy and paste text in both directions.
- **Remote sound** — audio from the Windows host plays on your Mac. Per
  connection: *On this computer* (default), *Never*, or *On the remote
  computer*.
- **Keyboard that just works** — physical-scancode mapping for non-US layouts
  (e.g. Swiss German); modifiers never get "stuck" when switching apps.
- **Secure by default** — passwords are kept in the macOS **Keychain**, and each
  server's key is pinned the first time you trust it.
- **Simple Settings** — add and edit connections in a native window with a clear
  **Save** button (no config files). Choose colour depth, clipboard mode,
  scaling, fit-to-window or fixed resolution, full screen, compression, and
  auto-reconnect. An **About** tab shows the exact version you're running.
- **SSH shortcuts** — keep SSH hosts alongside RDP and open them in your terminal
  of choice; authentication uses your own SSH keys.

## Requirements

- macOS 11 or newer (Apple Silicon or Intel).
- **Xcode Command Line Tools** — for linking against the system frameworks:
  ```sh
  xcode-select --install
  ```
- **Rust 1.89 or newer** — install the current stable toolchain via
  [rustup](https://rustup.rs):
  ```sh
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  ```
- The target Windows host must accept RDP with **NLA** (Network Level
  Authentication), the default on modern Windows.

## Build & install

There is no prebuilt download of RDP123, and that is on purpose: shipping a
ready-made macOS app that opens without warnings requires a paid Apple
Developer account for notarization, which this project does not have. Instead,
you build the app on your own Mac. That takes three commands and a couple of
minutes, works without any Apple account, and has a nice side effect: a
locally built app never gets the quarantine flag, so Gatekeeper simply stays
out of your way. You also know exactly what you are running, because it came
from the source in front of you.

```sh
git clone https://github.com/asd123ch/rdp123.git
cd RDP123
scripts/make-signing-identity.sh   # one-time: create a stable signing identity
cargo xtask install                # builds, signs, and installs into /Applications
open /Applications/RDP123.app
```

`cargo xtask install` builds a release, code-signs the app, and copies it into
`/Applications` as the single copy (removing the build copy so it doesn't show up
twice in Spotlight/Launchpad). Use `cargo xtask bundle` if you only want to build
`dist/RDP123.app` without installing. It uses the stable **`RDP123 Local`**
identity if present (auto-detected), otherwise falls
back to **ad-hoc** (signature `-`). The generated bundle includes the GPLv3
license text under `Contents/Resources`.

### Strongly recommended: run the signing script once

Do **not** skip `scripts/make-signing-identity.sh`. An ad-hoc signature changes
on every rebuild, and macOS ties two things to a *stable* code identity:

- the Keychain **"Always Allow"** decision for your saved passwords, and
- the **Local Network** privacy permission needed to reach LAN hosts.

With ad-hoc signing, both are forgotten after each rebuild: you get
re-prompted for the Keychain forever, and **LAN connections can fail on the
first try** even though the host is reachable.

The script creates a self-signed `RDP123 Local` code-signing certificate in a
**dedicated keychain** (`~/Library/Keychains/rdp123-signing.keychain-db`). Its
random password is stored in a user-only file under
`~/Library/Application Support/RDP123`, and the non-extractable private key is
restricted to Apple's signing tools. That is deliberate: signing **never touches
your login keychain and never shows a password prompt**. `cargo xtask bundle`
unlocks that keychain and signs with it automatically. It runs fully
non-interactively; no GUI, no Apple Developer account, no cost.

To remove it later:

```bash
security delete-keychain ~/Library/Keychains/rdp123-signing.keychain-db
rm -f "$HOME/Library/Application Support/RDP123/signing-keychain-password"
```

To override with your own identity instead:
`RDP123_SIGN_IDENTITY="My Identity" cargo xtask bundle`. The app is still not
notarised (that would need a paid Developer ID), but for a locally built app
that is unnecessary.

### First connection: the Local Network prompt

The first time you connect to a LAN host, macOS asks whether to let RDP123 *"find
devices on your local network."* Click **Allow**; this permission is required to
open the TCP connection to the RDP host. RDP123 retries temporary TCP reachability
failures for a few seconds, so it goes through right after you approve. It never
retries authentication, certificate, or protocol failures automatically, avoiding
accidental domain-account lockouts. (If you built ad-hoc, this prompt returns
after every rebuild, another reason to run the signing script.)

### Saved passwords and the Keychain prompt

With a stable-signed build, saving a password prompts the Keychain once: click
**Always Allow** and it sticks. If that dialog ever asks for a password and
**rejects your login password**, your *login keychain* password is out of sync
with your account password (a macOS-level issue, not RDP123): fix it in *Keychain
Access* → right-click the **login** keychain → *Change Password for Keychain
"login"…*. To avoid the Keychain entirely, set a connection's **Password
handling** to **Always ask**. RDP123 then prompts for the password each time and
stores nothing.

## Usage

1. Launch the app; an **RDP** item appears in the menu bar.
2. First run has no connections. Open the menu → **Settings…**. The window has
   two tabs:
   - **Connection** — the connection list plus a grouped editor (Connection,
     Authentication, Display, Clipboard/reconnect). Add with **+**, edit
     the fields, then press **Save** (or **Revert** to undo). Switching away with
     unsaved edits asks first, so nothing is lost silently.
   - **Global** — app-wide options: the **SSH terminal** (entered once and
     used by every SSH connection), the ⌘/⌥ swap for sessions, and launching
     RDP123 automatically at login.
3. Click a connection in the menu:
   - **RDP** → asks for the password the first time (offers to save it to the
     Keychain), verifies the server key, then opens the remote window.
   - **SSH** → opens the chosen terminal running `ssh user@host`.

<p align="center">
  <img src="assets/screenshot-settings.png" alt="The Settings window with the connection editor" width="560">
</p>

### Where things are stored

You manage everything from the **Settings** window; you never need to edit a
file. For reference:

- **Connections** (names, hosts, options) live in a plain JSON file at
  `~/Library/Application Support/ch.asd123.RDP123/connections.json`.
- **Passwords** are stored only in the macOS **Keychain** (service
  `ch.asd123.rdp123`), never on disk.
- Each **server key** is pinned automatically the first time you trust it, so a
  later change is flagged.

### SSH terminal

SSH entries just launch a terminal with an `ssh` command; authentication uses
your SSH keys / `~/.ssh/config`. Pick the terminal in **Settings → Global**:

- **Terminal.app** / **iTerm2** — driven with AppleScript (`osascript`).
- **Kaku** / **WezTerm** — `<cli> start -- ssh …`; the binary is found on the
  `PATH`, in Homebrew locations, or inside the app bundle automatically.
- **Ghostty** / **Alacritty** — launched via `open -na <App> --args -e ssh …`.
- **Custom** — your own command, where `{ssh}` is replaced by the `ssh …`
  invocation (also `{host}`, `{port}`, `{user}`), run via `/bin/sh -c`.

### Keyboard remapping and Karabiner

RDP123 passes keys straight through as physical scan codes, so Windows
shortcuts (Ctrl+C, Home/End, F5, …) just work inside a session without any
remapping tool. If you use [Karabiner-Elements](https://karabiner-elements.pqrs.org)
for PC-style shortcuts in the rest of macOS, add `^ch\.asd123\.rdp123` to your
rules' exclusion lists so sessions keep receiving raw keys. The author uses
[this Karabiner configuration](https://github.com/patrickdobler/karabiner-config)
for the most common shortcuts and key remaps; it is set up exactly that way.

For PC muscle memory *inside* sessions there is a built-in option instead:
**Settings → Global → Swap ⌘ and ⌥** makes ⌘ act as Alt and ⌥ as the Windows
key, matching the PC key positions around the space bar. Off by default.

## Security

- **Passwords**: stored only in the macOS Keychain, never on disk or in logs.
- **Transport**: TLS 1.2+ (rustls) with mandatory NLA (CredSSP/NTLM).
- **Server identity (trust-on-first-use)**: RDP hosts almost always present a
  self-signed certificate, so there is no CA to check. On the first connection
  RDP123 shows the server's **public-key SHA-256 fingerprint** and asks you to
  trust it, *before* any credentials are sent. The fingerprint is pinned in the
  profile; on later connections a mismatch is flagged loudly. Pinning the public
  key (not the whole certificate) means routine certificate renewals with the
  same key don't nag you.
- **Reduced attack surface**: no listening ports and no drive, printer,
  microphone or smart-card redirection. Clipboard and remote audio can be
  disabled per connection.

## Privacy

RDP123 is deliberately local and privacy-first:

- **No telemetry, analytics, crash reporting, advertising, tracking, accounts
  or automatic update checks.**
- **No data is sent to the developer or to third parties.** The application
  only opens connections that you explicitly request: RDP traffic goes to the
  configured Windows host, and SSH shortcuts invoke your local SSH client for
  the configured SSH host.
- Connection profiles stay in your local application-support directory.
  Passwords stay in the macOS Keychain and are never written to profile files
  or application logs.
- Clipboard contents are transferred only when clipboard redirection is enabled
  for that connection. Remote audio is received only when audio playback is
  enabled.
- RDP123 does not include a background cloud service, web account, hosted
  control plane or hidden network listener.

There are intentionally no prebuilt, notarised binaries in this repository.
As with any open-source remote-access software, review the source and its
dependencies before use and build it yourself. The local signing helper exists
only to give the self-built app a stable macOS identity; it does not contact
Apple or require an Apple Developer account.

## Architecture

Two crates:

- **`rdp123-core`** — UI-independent: profiles, Keychain, the RDP session engine
  (IronRDP connect + active loop, input, resize, clipboard), the shared
  framebuffer, and the macOS→RDP scan-code table. Unit-tested without AppKit.
- **`rdp123-app`** — the native AppKit front-end via `objc2`: the menu-bar item,
  the window/view, rendering (IOSurface/CoreAnimation), and input handling.

Each session runs on its own thread with a single-threaded Tokio runtime. It
receives commands (input, resize, clipboard, shutdown) over a channel and emits
events (frame updated, clipboard text, certificate prompt, disconnect) back to
the main thread via GCD. The session never touches AppKit; the UI never speaks
RDP.

## Known limitations

- Developed and tested against current Windows 10/11 hosts on a direct
  TLS+NLA connection. Setups the author does not use (Windows Server
  editions, RDS farms, gateways) may still have rough edges.
- RDP 8 graphics (experimental) can still show occasional compositing
  artifacts; Classic graphics is the default and fully reliable.
- Clipboard is text only (no images/files).
- Audio is playback only (remote → local) and uses uncompressed PCM; microphone
  redirection is not implemented.
- Multi-monitor and console/admin sessions are not implemented and therefore
  are not exposed in Settings.

## Contributing

Issues and focused pull requests are welcome. Before submitting a change, run
the checks below and avoid attaching real credentials, hostnames, captured RDP
traffic or screenshots containing private remote-desktop content. Changes to
the vendored IronRDP crates should remain narrowly scoped and documented so
they can be upstreamed or rebased later.

## Development checks

The CI workflow runs formatting, unit tests, Clippy with warnings denied,
an Intel compile check, and a dependency audit. Run the same core checks
locally:

```sh
cargo fmt --all -- --check
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo check --workspace --target x86_64-apple-darwin --locked
cargo audit --ignore RUSTSEC-2023-0071 -D warnings
```

`RUSTSEC-2023-0071` is currently an unavoidable transitive advisory in the
latest IronRDP/SSPI dependency graph. It concerns RSA private-key operations;
RDP123 performs password-based NTLM client authentication and does not load or
operate an RSA private key. The exception is explicit so a future upstream
upgrade can remove it rather than hiding new advisories.

## Credits

RDP123 stands on the work of many open-source projects. The main building
blocks are:

- [IronRDP](https://github.com/Devolutions/IronRDP) by Devolutions and its
  contributors — RDP protocol, channels and graphics pipeline.
- [rustls](https://github.com/rustls/rustls) and
  [AWS-LC](https://github.com/aws/aws-lc-rs) — TLS and cryptography.
- [Tokio](https://github.com/tokio-rs/tokio) — asynchronous networking and
  session runtime.
- [objc2](https://github.com/madsmtm/objc2) — native AppKit and Apple-framework
  bindings.
- [OpenH264](https://github.com/cisco/openh264) and
  [openh264-rs](https://github.com/ralfbiedert/openh264-rs) — AVC420 decoding.
- [CPAL](https://github.com/RustAudio/cpal) — remote audio playback.
- The RustCrypto, serde, tracing and wider Rust ecosystem projects used
  throughout the application.

Copyright for third-party components remains with their respective authors.
The complete locked dependency inventory, including versions and license
expressions, is recorded in `Cargo.lock` and each crate's metadata. The
modified vendored IronRDP components retain their original MIT/Apache-2.0
license files in their respective `vendor/` directories.

## License

Copyright (c) 2026 asd123.ai

RDP123 is free software licensed under the
[GNU General Public License, version 3](LICENSE). You may use, study, modify and
redistribute it under those terms. Third-party components remain available
under their own licenses (see [Credits](#credits)).

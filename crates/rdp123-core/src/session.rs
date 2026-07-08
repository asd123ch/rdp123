//! The RDP session engine.
//!
//! A session runs on its own OS thread with a single-threaded Tokio runtime.
//! It drives the IronRDP connect sequence, then an active loop that decodes
//! server graphics into the shared framebuffer and forwards input, resize and
//! clipboard traffic. It talks to the UI through a command channel (in) and an
//! event callback (out); it never touches AppKit.

#![allow(clippy::too_many_arguments)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::oneshot;
use zeroize::Zeroize;

use ironrdp::cliprdr::backend::CliprdrBackend;
use ironrdp::cliprdr::pdu::{
    ClipboardFileAttributes, ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags,
    FileContentsFlags, FileContentsRequest, FileContentsResponse, FileDescriptor,
    FormatDataRequest, FormatDataResponse, LockDataId, OwnedFormatDataResponse,
};
use ironrdp::cliprdr::{Client, CliprdrClient, CliprdrSvcMessages};
use ironrdp::connector::connection_activation::{
    ConnectionActivationSequence, ConnectionActivationState,
};
use ironrdp::connector::sspi::generator::NetworkRequest;
use ironrdp::connector::{
    BitmapConfig, ClientConnector, Config, ConnectionResult, ConnectorError, ConnectorErrorExt,
    ConnectorResult, Credentials, DesktopSize, ServerName,
};
use ironrdp::core::WriteBuf;
use ironrdp::displaycontrol::client::DisplayControlClient;
use ironrdp::displaycontrol::pdu::MonitorLayoutEntry;
use ironrdp::dvc::DrdynvcClient;
use ironrdp::graphics::image_processing::PixelFormat;
use ironrdp::input::{Database, MouseButton, MousePosition, Operation, Scancode, WheelRotations};
use ironrdp::pdu::gcc::KeyboardType;
use ironrdp::pdu::geometry::Rectangle as _;
use ironrdp::pdu::rdp::capability_sets::{client_codecs_capabilities, MajorPlatformType};
use ironrdp::pdu::rdp::client_info::{CompressionType, PerformanceFlags, TimezoneInfo};
use ironrdp::pdu::PduResult;
use ironrdp::rdpdr::{NoopRdpdrBackend, Rdpdr};
use ironrdp::rdpsnd::client::Rdpsnd;
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStage, ActiveStageOutput};
use ironrdp_tokio::{
    connect_begin, connect_finalize, mark_as_upgraded, single_sequence_step_read,
    split_tokio_framed, FramedWrite as _, NetworkClient, TokioFramed,
};

use crate::framebuffer::SharedFramebuffer;
use crate::gfx::GfxEvent;
use crate::keymap;
use crate::profile::{AudioMode, ClipboardMode, GraphicsMode};

/// The pixel format shared by the decoded image, the framebuffer and CoreGraphics.
const PIXEL_FORMAT: PixelFormat = PixelFormat::BgrX32;

/// How long to let the writer flush a graceful shutdown before giving up.
const SHUTDOWN_FLUSH: std::time::Duration = std::time::Duration::from_millis(500);

/// `Disconnected` reason for a normal server-side session end (logoff). The UI
/// treats this as expected and shows no error dialog.
pub const REMOTE_ENDED: &str = "the remote session ended";

type TlsStream = tokio_rustls::client::TlsStream<TcpStream>;
type SessionFramed = TokioFramed<TlsStream>;
type SessionReader = TokioFramed<tokio::io::ReadHalf<TlsStream>>;
type SessionWriter = TokioFramed<tokio::io::WriteHalf<TlsStream>>;
type OutSender = Sender<Vec<u8>>;
type EventCb = Box<dyn Fn(SessionEvent) + Send>;
const COMMAND_QUEUE_CAPACITY: usize = 256;
const CLIPBOARD_QUEUE_CAPACITY: usize = 32;
const OUTPUT_QUEUE_CAPACITY: usize = 64;
const MAX_CLIPBOARD_TEXT_BYTES: usize = 16 * 1024 * 1024;
/// Cap on the number of files offered to the remote clipboard in one copy
/// (folders are walked recursively; a runaway selection is truncated).
const MAX_CLIPBOARD_FILES: usize = 4096;
/// Cap on a single FileContents chunk we are willing to serve.
const MAX_FILE_CHUNK_BYTES: u32 = 16 * 1024 * 1024;
/// Idle keep-alive: how long a session must be free of real user input before
/// an invisible F15 tap is injected. 30 s sits well under the one-minute
/// minimum of a Windows idle-session policy, so the keep-alive always wins the
/// race against the timeout (and the remote lock screen).
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(30);
/// Set-1 scancode for F15 (not extended). No mainstream application reacts to
/// it, so the injected tap resets the remote idle timer with no visible effect.
const KEEP_ALIVE_SCANCODE: u8 = 0x66;

/// What the local (macOS) clipboard currently offers to the remote session.
#[derive(Debug, Default)]
enum LocalClip {
    #[default]
    Empty,
    Text(String),
    Files(Vec<LocalClipFile>),
}

/// One local file (or directory) offered to the remote clipboard.
#[derive(Debug, Clone)]
struct LocalClipFile {
    /// Absolute path on disk.
    path: std::path::PathBuf,
    /// Wire name relative to the copied selection, `\`-separated.
    wire_name: String,
    size: u64,
    is_dir: bool,
}

type LocalClipState = Arc<Mutex<LocalClip>>;

/// Walk the copied selection into the flat, relative-path list the Windows
/// clipboard expects. Unreadable entries and non-UTF-8 names are skipped.
fn collect_clipboard_files(roots: &[std::path::PathBuf]) -> Vec<LocalClipFile> {
    fn visit(path: &std::path::Path, wire_name: String, out: &mut Vec<LocalClipFile>) {
        if out.len() >= MAX_CLIPBOARD_FILES {
            return;
        }
        let Ok(metadata) = std::fs::metadata(path) else {
            tracing::warn!("clipboard: skipping unreadable {}", path.display());
            return;
        };
        if metadata.is_dir() {
            out.push(LocalClipFile {
                path: path.to_path_buf(),
                wire_name: wire_name.clone(),
                size: 0,
                is_dir: true,
            });
            let Ok(entries) = std::fs::read_dir(path) else {
                return;
            };
            for entry in entries.flatten() {
                let child = entry.path();
                let Some(name) = child.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                visit(&child, format!("{wire_name}\\{name}"), out);
            }
        } else if metadata.is_file() {
            out.push(LocalClipFile {
                path: path.to_path_buf(),
                wire_name,
                size: metadata.len(),
                is_dir: false,
            });
        }
    }

    let mut out = Vec::new();
    for root in roots {
        let Some(name) = root.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        visit(root, name.to_string(), &mut out);
    }
    if out.len() >= MAX_CLIPBOARD_FILES {
        tracing::warn!("clipboard: selection truncated at {MAX_CLIPBOARD_FILES} files");
    }
    out
}

/// Build the CLIPRDR descriptors for the offered files (wire name is split
/// into `relative_path` + `name` as the encoder expects).
fn to_file_descriptors(files: &[LocalClipFile]) -> Vec<FileDescriptor> {
    files
        .iter()
        .map(|file| {
            let (relative_path, name) = match file.wire_name.rsplit_once('\\') {
                Some((path, name)) => (Some(path), name),
                None => (None, file.wire_name.as_str()),
            };
            let mut descriptor = FileDescriptor::new(name);
            if let Some(path) = relative_path {
                descriptor = descriptor.with_relative_path(path);
            }
            if file.is_dir {
                descriptor = descriptor.with_attributes(ClipboardFileAttributes::DIRECTORY);
            } else {
                descriptor = descriptor
                    .with_attributes(ClipboardFileAttributes::NORMAL)
                    .with_file_size(file.size);
            }
            descriptor
        })
        .collect()
}

/// Chunk size for pulling remote files (one outstanding request at a time).
const FETCH_CHUNK_BYTES: u32 = 1024 * 1024;

/// A file entry on the remote clipboard (from `FileGroupDescriptorW`).
#[derive(Debug, Clone)]
struct RemoteFileEntry {
    /// `\`-separated name relative to the copied selection.
    wire_name: String,
    size: Option<u64>,
    is_dir: bool,
}

/// A top-level entry on the remote clipboard, offered to Finder as a file
/// promise. Fetch it with [`SessionCommand::FetchRemoteClipItem`].
#[derive(Debug, Clone)]
pub struct RemoteClipItem {
    pub name: String,
    pub is_dir: bool,
}

#[derive(Debug)]
struct PlannedEntry {
    /// Index into the remote file list (CLIPRDR `lindex`).
    index: i32,
    dest: std::path::PathBuf,
    size: Option<u64>,
    is_dir: bool,
}

#[derive(Debug)]
struct CurrentFetchFile {
    file: std::fs::File,
    index: i32,
    /// Total size; not yet known while a SIZE request is outstanding.
    size: u64,
    needs_size: bool,
    offset: u64,
}

/// One Finder paste (a top-level item plus its descendants).
#[derive(Debug)]
struct FetchJob {
    queue: std::collections::VecDeque<PlannedEntry>,
    current: Option<CurrentFetchFile>,
    done: std::sync::mpsc::SyncSender<Result<(), String>>,
}

/// State of the remote clipboard's file offer and the fetch pipeline.
#[derive(Debug, Default)]
struct RemoteClipboard {
    files: Vec<RemoteFileEntry>,
    data_id: Option<u32>,
    jobs: std::collections::VecDeque<FetchJob>,
    next_stream_id: u32,
    /// Outstanding request: (stream id, was a SIZE request).
    outstanding: Option<(u32, bool)>,
}

/// Plan the entries for one pasted top-level item. Wire names containing
/// `..` components are rejected (a hostile server must not escape `dest`).
fn plan_fetch_job(
    remote: &RemoteClipboard,
    name: &str,
    dest: &std::path::Path,
    done: std::sync::mpsc::SyncSender<Result<(), String>>,
) -> Option<FetchJob> {
    let prefix = format!("{name}\\");
    let mut queue = std::collections::VecDeque::new();
    let mut found_root = false;
    for (index, entry) in remote.files.iter().enumerate() {
        let relative: Option<std::path::PathBuf> = if entry.wire_name == name {
            found_root = true;
            Some(std::path::PathBuf::new())
        } else {
            entry
                .wire_name
                .strip_prefix(&prefix)
                .map(|sub| sub.split('\\').collect::<std::path::PathBuf>())
        };
        let Some(relative) = relative else { continue };
        if relative
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
            && !relative.as_os_str().is_empty()
        {
            tracing::warn!(
                "clipboard: skipping suspicious remote path {:?}",
                entry.wire_name
            );
            continue;
        }
        let dest_path = if relative.as_os_str().is_empty() {
            dest.to_path_buf()
        } else {
            dest.join(relative)
        };
        queue.push_back(PlannedEntry {
            index: i32::try_from(index).unwrap_or(i32::MAX),
            dest: dest_path,
            size: entry.size,
            is_dir: entry.is_dir,
        });
    }
    if !found_root {
        let _ = done.send(Err(format!(
            "'{name}' is no longer on the remote clipboard"
        )));
        return None;
    }
    Some(FetchJob {
        queue,
        current: None,
        done,
    })
}

/// Drive the fetch pipeline until it needs the next FileContents response
/// (or all jobs are drained). Creates directories and files locally and
/// issues at most one outstanding request.
async fn advance_remote_fetch(
    remote: &mut RemoteClipboard,
    active_stage: &mut ActiveStage,
    out_tx: &OutSender,
) -> Result<()> {
    loop {
        if remote.outstanding.is_some() {
            return Ok(());
        }
        let data_id = remote.data_id;
        let Some(job) = remote.jobs.front_mut() else {
            return Ok(());
        };

        if let Some(current) = &job.current {
            let (flags, requested, was_size) = if current.needs_size {
                (FileContentsFlags::SIZE, 8, true)
            } else {
                let remaining = current.size.saturating_sub(current.offset);
                let chunk = u32::try_from(remaining.min(u64::from(FETCH_CHUNK_BYTES)))
                    .unwrap_or(FETCH_CHUNK_BYTES);
                (FileContentsFlags::RANGE, chunk, false)
            };
            let stream_id = remote.next_stream_id;
            remote.next_stream_id = remote.next_stream_id.wrapping_add(1);
            let request = FileContentsRequest {
                stream_id,
                index: current.index,
                flags,
                position: current.offset,
                requested_size: requested,
                data_id,
            };
            remote.outstanding = Some((stream_id, was_size));
            return send_cliprdr(active_stage, out_tx, |c| c.request_file_contents(request)).await;
        }

        match job.queue.pop_front() {
            Some(entry) if entry.is_dir => {
                if let Err(e) = std::fs::create_dir_all(&entry.dest) {
                    fail_front_job(remote, format!("creating {}: {e}", entry.dest.display()));
                }
            }
            Some(entry) => {
                if let Some(parent) = entry.dest.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::File::create(&entry.dest) {
                    Ok(file) => {
                        if entry.size == Some(0) {
                            continue; // empty file, nothing to fetch
                        }
                        job.current = Some(CurrentFetchFile {
                            file,
                            index: entry.index,
                            size: entry.size.unwrap_or(0),
                            needs_size: entry.size.is_none(),
                            offset: 0,
                        });
                    }
                    Err(e) => {
                        fail_front_job(remote, format!("creating {}: {e}", entry.dest.display()));
                    }
                }
            }
            None => {
                if let Some(job) = remote.jobs.pop_front() {
                    let _ = job.done.send(Ok(()));
                }
            }
        }
    }
}

fn fail_front_job(remote: &mut RemoteClipboard, reason: String) {
    if let Some(job) = remote.jobs.pop_front() {
        tracing::warn!("clipboard: file fetch failed: {reason}");
        let _ = job.done.send(Err(reason));
    }
}

/// Apply one FileContents response to the fetch pipeline.
async fn handle_remote_file_contents(
    remote: &mut RemoteClipboard,
    stream_id: u32,
    data: Option<Vec<u8>>,
    active_stage: &mut ActiveStage,
    out_tx: &OutSender,
) -> Result<()> {
    use std::io::Write as _;

    let Some((expected_id, was_size)) = remote.outstanding else {
        return Ok(()); // stale response after a failed/cancelled job
    };
    if stream_id != expected_id {
        return Ok(());
    }
    remote.outstanding = None;

    let outcome: Result<(), String> = (|| {
        let bytes = data.ok_or("the remote refused the transfer")?;
        let job = remote.jobs.front_mut().ok_or("no active transfer")?;
        let current = job.current.as_mut().ok_or("no file in progress")?;
        if was_size {
            let size: [u8; 8] = bytes
                .get(..8)
                .and_then(|b| b.try_into().ok())
                .ok_or("malformed size response")?;
            current.size = u64::from_le_bytes(size);
            current.needs_size = false;
            if current.size == 0 {
                job.current = None;
            }
        } else {
            if bytes.is_empty() {
                return Err("transfer ended early".to_string());
            }
            current
                .file
                .write_all(&bytes)
                .map_err(|e| format!("writing file: {e}"))?;
            current.offset += bytes.len() as u64;
            if current.offset >= current.size {
                job.current = None;
            }
        }
        Ok(())
    })();

    if let Err(reason) = outcome {
        fail_front_job(remote, reason);
    }
    advance_remote_fetch(remote, active_stage, out_tx).await
}

/// A logical mouse button.
#[derive(Debug, Clone, Copy)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
}

/// UI-originated input, already translated into remote pixel coordinates.
#[derive(Debug, Clone)]
pub enum InputEvent {
    Key {
        keycode: u16,
        down: bool,
    },
    MouseMove {
        x: u16,
        y: u16,
    },
    MouseButton {
        button: PointerButton,
        down: bool,
        x: u16,
        y: u16,
    },
    Wheel {
        delta: i16,
        horizontal: bool,
    },
}

/// Commands the UI sends into a running session.
#[derive(Debug)]
pub enum SessionCommand {
    Input(Vec<InputEvent>),
    Resize {
        width: u16,
        height: u16,
        scale: Option<u32>,
    },
    LocalClipboard(String),
    /// The user copied files in Finder; offer them to the remote clipboard.
    LocalClipboardFiles(Vec<std::path::PathBuf>),
    /// Finder redeemed a file promise: pull `name` (and its descendants) from
    /// the remote clipboard to `dest`, then report on `done`.
    FetchRemoteClipItem {
        name: String,
        dest: std::path::PathBuf,
        done: std::sync::mpsc::SyncSender<Result<(), String>>,
    },
    ReleaseAllKeys,
    Shutdown,
}

/// Events the session emits back to the UI. Delivered on the session thread —
/// the UI callback is responsible for hopping to the main thread.
pub enum SessionEvent {
    Connected {
        width: u16,
        height: u16,
    },
    FrameUpdated {
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    },
    Resized {
        width: u16,
        height: u16,
    },
    /// A new pointer shape from the server, decoded to straight-alpha RGBA.
    /// Coordinates are in remote pixels.
    PointerBitmap {
        width: u16,
        height: u16,
        hotspot_x: u16,
        hotspot_y: u16,
        rgba: Vec<u8>,
    },
    /// The server asks for the default (arrow) pointer.
    PointerDefault,
    /// The server hides the pointer (e.g. touch input or full-screen video).
    PointerHidden,
    ClipboardText(String),
    /// The remote clipboard offers files; the app should put file promises on
    /// the pasteboard and fetch on paste via `FetchRemoteClipItem`.
    ClipboardFiles(Vec<RemoteClipItem>),
    /// Ask the user to trust a server key fingerprint. `is_change` is true when
    /// a *different* key was previously pinned. Reply true to proceed.
    CertificateApproval {
        fingerprint: String,
        is_change: bool,
        reply: oneshot::Sender<bool>,
    },
    /// The user accepted `fingerprint`; the app should persist it.
    CertTrusted {
        fingerprint: String,
    },
    /// The connection dropped and a reconnect attempt is starting.
    Reconnecting,
    Disconnected {
        reason: String,
    },
    Error(String),
}

/// Everything needed to open a connection. The password is held only for the
/// duration of the connect; callers should source it from the Keychain.
pub struct SessionConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub domain: Option<String>,
    pub width: u16,
    pub height: u16,
    pub scale: Option<u32>,
    pub expected_fingerprint: Option<String>,
    // --- RDP options ---
    pub color_depth: u32,
    /// FastPath transport compression (RDP 6.1 XCRUSH). EGFX/ZGFX and graphics
    /// codec compression remain protocol-managed independently.
    pub compression: bool,
    pub clipboard: ClipboardMode,
    /// Where remote audio plays (local playback, discarded, or left remote).
    pub audio: AudioMode,
    /// Graphics pipeline: EGFX (H.264/RemoteFX Progressive) or legacy bitmaps.
    pub graphics: GraphicsMode,
    /// When false, the remote stays at a fixed resolution (window resizes just scale it).
    pub dynamic_resolution: bool,
    pub reconnect: bool,
    pub reconnect_per_minute: u32,
    /// Global setting: ⌘ sends Alt and ⌥ sends the Windows key.
    pub swap_cmd_alt: bool,
    /// Wake-on-LAN MAC address; a magic packet is broadcast before connecting.
    pub wake_mac: Option<String>,
    /// Keep the remote session awake: while the user is idle, tap an invisible
    /// F15 every [`KEEP_ALIVE_INTERVAL`] so idle-disconnect policies and the
    /// remote lock screen never trigger.
    pub keep_alive: bool,
}

impl Drop for SessionConfig {
    fn drop(&mut self) {
        self.password.zeroize();
    }
}

/// Handle to a running session held by the UI.
#[derive(Clone)]
pub struct SessionHandle {
    command_tx: Sender<SessionCommand>,
    framebuffer: Arc<SharedFramebuffer>,
    pending: Arc<PendingCommands>,
}

#[derive(Debug, Default)]
struct PendingValue<T> {
    value: Option<T>,
    queued: bool,
}

#[derive(Debug, Default)]
struct PendingCommands {
    mouse_move: Mutex<PendingValue<(u16, u16)>>,
    resize: Mutex<PendingValue<(u16, u16, Option<u32>)>>,
    release_all_keys: AtomicBool,
    shutdown: AtomicBool,
}

impl SessionHandle {
    /// Send a command; ignored if the session has already ended.
    pub fn command(&self, cmd: SessionCommand) {
        if let SessionCommand::Input(events) = &cmd {
            if let [InputEvent::MouseMove { x, y }] = events.as_slice() {
                self.queue_mouse_move(*x, *y);
                return;
            }
        }
        match cmd {
            SessionCommand::Resize {
                width,
                height,
                scale,
            } => self.queue_resize(width, height, scale),
            SessionCommand::ReleaseAllKeys => {
                self.pending.release_all_keys.store(true, Ordering::Release);
                let _ = self.command_tx.try_send(SessionCommand::ReleaseAllKeys);
            }
            SessionCommand::Shutdown => {
                self.pending.shutdown.store(true, Ordering::Release);
                let _ = self.command_tx.try_send(SessionCommand::Shutdown);
            }
            command => {
                if let Err(error) = self.command_tx.try_send(command) {
                    tracing::warn!("session command queue is full or closed: {error}");
                }
            }
        }
    }

    pub fn framebuffer(&self) -> Arc<SharedFramebuffer> {
        self.framebuffer.clone()
    }

    fn queue_mouse_move(&self, x: u16, y: u16) {
        let should_signal = {
            let mut pending = self.pending.mouse_move.lock().unwrap();
            pending.value = Some((x, y));
            if pending.queued {
                false
            } else {
                pending.queued = true;
                true
            }
        };
        if should_signal
            && self
                .command_tx
                .try_send(SessionCommand::Input(vec![InputEvent::MouseMove { x, y }]))
                .is_err()
        {
            self.pending.mouse_move.lock().unwrap().queued = false;
        }
    }

    fn queue_resize(&self, width: u16, height: u16, scale: Option<u32>) {
        let should_signal = {
            let mut pending = self.pending.resize.lock().unwrap();
            pending.value = Some((width, height, scale));
            if pending.queued {
                false
            } else {
                pending.queued = true;
                true
            }
        };
        if should_signal
            && self
                .command_tx
                .try_send(SessionCommand::Resize {
                    width,
                    height,
                    scale,
                })
                .is_err()
        {
            self.pending.resize.lock().unwrap().queued = false;
        }
    }
}

/// Start a session on a dedicated thread and return a handle immediately.
pub fn spawn(config: SessionConfig, event_cb: EventCb) -> SessionHandle {
    let framebuffer = SharedFramebuffer::new();
    let (command_tx, command_rx) = channel(COMMAND_QUEUE_CAPACITY);
    let fb = framebuffer.clone();
    let pending = Arc::new(PendingCommands::default());
    let thread_pending = pending.clone();

    std::thread::Builder::new()
        .name("rdp-session".to_string())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    event_cb(SessionEvent::Error(format!("runtime: {e}")));
                    return;
                }
            };
            rt.block_on(run(config, fb, command_rx, thread_pending, event_cb));
        })
        .expect("spawn session thread");

    SessionHandle {
        command_tx,
        framebuffer,
        pending,
    }
}

/// Signals produced by the clipboard backend, drained by the session loop.
enum ClipSignal {
    InitiateCopy(Vec<ClipboardFormat>),
    /// Announce a file copy (FileGroupDescriptorW format list).
    InitiateFileCopy(Vec<FileDescriptor>),
    /// Serve one FileContents chunk or size to the remote.
    SubmitFileContents(FileContentsResponse<'static>),
    /// The remote clipboard's file list arrived (parsed FileGroupDescriptorW).
    RemoteFileList {
        files: Vec<RemoteFileEntry>,
        data_id: Option<u32>,
    },
    /// One FileContents response for our fetch pipeline (`None` = error).
    RemoteFileContents {
        stream_id: u32,
        data: Option<Vec<u8>>,
    },
    InitiatePaste(ClipboardFormatId),
    SubmitData(OwnedFormatDataResponse),
    RemoteText(String),
}

/// Give up after this many consecutive failed reconnect attempts.
const MAX_RECONNECT_FAILURES: u32 = 20;
/// Attempts for the very first connect. A few retries let a just-granted macOS
/// Local Network permission (or a transient blip) succeed instead of failing.
const MAX_INITIAL_FAILURES: u32 = 4;
/// Initial attempts when Wake-on-LAN is configured: the host may need to boot
/// or resume, which takes far longer than a permission blip.
const MAX_INITIAL_FAILURES_WOL: u32 = 20;
/// Delay between initial-connect retries.
const INITIAL_RETRY_DELAY: Duration = Duration::from_secs(2);
/// Bound the TCP connect so a Local-Network-blocked LAN connect fails fast with
/// a clear message rather than hanging on the long OS default.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(12);

/// Why a single session attempt ended.
enum SessionEnd {
    /// User closed the window (or dropped the handle) — do not reconnect.
    UserQuit,
    /// Connection dropped — reconnect if enabled.
    Disconnected(String),
}

/// What the reconnect loop should do after one attempt.
enum Outcome {
    Stop,
    /// (reason, is_error): emit Error vs Disconnected, then stop.
    Fail(String, bool),
    /// Wait `delay`, then try again. `announce` emits a `Reconnecting` event.
    Retry {
        delay: Duration,
        announce: bool,
    },
}

enum ConnectFailure {
    Retryable(anyhow::Error),
    Fatal(anyhow::Error),
}

impl ConnectFailure {
    fn retryable(error: impl Into<anyhow::Error>) -> Self {
        Self::Retryable(error.into())
    }

    fn fatal(error: impl Into<anyhow::Error>) -> Self {
        Self::Fatal(error.into())
    }

    fn into_parts(self) -> (anyhow::Error, bool) {
        match self {
            Self::Retryable(error) => (error, true),
            Self::Fatal(error) => (error, false),
        }
    }
}

/// Delay between reconnect attempts, derived from the per-minute cap.
fn reconnect_delay(config: &SessionConfig) -> Duration {
    let secs = (60 / u64::from(config.reconnect_per_minute.max(1))).max(1);
    Duration::from_secs(secs)
}

async fn run(
    mut config: SessionConfig,
    framebuffer: Arc<SharedFramebuffer>,
    mut command_rx: Receiver<SessionCommand>,
    pending: Arc<PendingCommands>,
    event_cb: EventCb,
) {
    ensure_crypto_provider();
    let local_clip: LocalClipState = Arc::new(Mutex::new(LocalClip::Empty));
    // One audio player for the whole session (reconnects reuse it). Missing
    // audio is never fatal — the session simply runs silent.
    let audio = if config.audio == AudioMode::ThisComputer {
        match crate::audio::AudioPlayer::start() {
            Ok(player) => Some(player),
            Err(e) => {
                tracing::warn!("audio playback unavailable: {e:#}");
                None
            }
        }
    } else {
        None
    };
    // Keys trusted during this session so retries never re-prompt for the cert.
    let mut session_trusted: Option<String> = config.expected_fingerprint.clone();
    let mut connected_once = false;
    let mut failures: u32 = 0;
    // Wake-on-LAN: parsed once; a packet goes out before every initial attempt
    // (a machine that is already awake simply ignores it).
    let wake_mac = config.wake_mac.as_deref().and_then(crate::wol::parse_mac);
    let max_initial_failures = if wake_mac.is_some() {
        MAX_INITIAL_FAILURES_WOL
    } else {
        MAX_INITIAL_FAILURES
    };

    loop {
        if !connected_once {
            if let Some(mac) = wake_mac {
                match crate::wol::send_magic_packet(mac) {
                    Ok(()) => {
                        if failures == 0 {
                            tracing::info!("wol: magic packet sent");
                        }
                    }
                    Err(e) => tracing::warn!("wol: sending the magic packet failed: {e}"),
                }
            }
        }
        let (clip_tx, clip_rx) = channel::<ClipSignal>(CLIPBOARD_QUEUE_CAPACITY);
        let (gfx_tx, gfx_rx) = tokio::sync::mpsc::unbounded_channel::<GfxEvent>();
        let outcome = match connect(
            &config,
            &mut session_trusted,
            local_clip.clone(),
            clip_tx,
            gfx_tx,
            audio.as_ref(),
            &framebuffer,
            &event_cb,
        )
        .await
        {
            Ok((connection_result, framed)) => {
                connected_once = true;
                failures = 0;
                if audio.is_some() {
                    // Joined channel = the host accepted audio redirection; a
                    // missing join means it is disabled server-side (GPO or a
                    // stopped Windows Audio service), not a client problem.
                    match connection_result
                        .static_channels
                        .get_channel_id_by_type::<Rdpsnd>()
                    {
                        Some(id) => tracing::info!("rdpsnd: channel joined (id {id})"),
                        None => tracing::warn!(
                            "rdpsnd: the server did not join the audio channel — audio \
                             redirection is disabled on the host"
                        ),
                    }
                }
                if !config.reconnect {
                    config.password.zeroize();
                }
                match run_session(
                    connection_result,
                    framed,
                    &config,
                    &framebuffer,
                    &mut command_rx,
                    &pending,
                    clip_rx,
                    gfx_rx,
                    &local_clip,
                    &event_cb,
                )
                .await
                {
                    SessionEnd::UserQuit => Outcome::Stop,
                    SessionEnd::Disconnected(reason) => {
                        if config.reconnect {
                            Outcome::Retry {
                                delay: reconnect_delay(&config),
                                announce: true,
                            }
                        } else {
                            Outcome::Fail(reason, false)
                        }
                    }
                }
            }
            Err(failure) => {
                let (error, retryable) = failure.into_parts();
                // `{error:#}` keeps the underlying io/TLS/auth cause.
                let reason = format!("{error:#}");
                if !connected_once && retryable {
                    // Only TCP reachability failures are retried. Authentication,
                    // certificate and protocol failures must never be repeated,
                    // because repeated bad credentials can lock a domain account.
                    failures += 1;
                    if failures >= max_initial_failures {
                        Outcome::Fail(reason, true)
                    } else {
                        Outcome::Retry {
                            delay: INITIAL_RETRY_DELAY,
                            announce: false,
                        }
                    }
                } else if !connected_once || !config.reconnect || !retryable {
                    Outcome::Fail(reason, false)
                } else {
                    failures += 1;
                    if failures >= MAX_RECONNECT_FAILURES {
                        Outcome::Fail(reason, false)
                    } else {
                        Outcome::Retry {
                            delay: reconnect_delay(&config),
                            announce: true,
                        }
                    }
                }
            }
        };

        match outcome {
            Outcome::Stop => break,
            Outcome::Fail(reason, is_error) => {
                if is_error {
                    event_cb(SessionEvent::Error(reason));
                } else {
                    event_cb(SessionEvent::Disconnected { reason });
                }
                break;
            }
            Outcome::Retry { delay, announce } => {
                if drain_should_stop(&mut command_rx, &pending, &mut config) {
                    break;
                }
                if announce {
                    event_cb(SessionEvent::Reconnecting);
                }
                if wait_for_retry(delay, &mut command_rx, &pending, &mut config).await {
                    break;
                }
            }
        }
    }
}

/// A resize observed while disconnected becomes the size of the next connect,
/// so the session comes back matching the current window.
fn absorb_offline_command(command: Option<SessionCommand>, config: &mut SessionConfig) {
    if let Some(SessionCommand::Resize {
        width,
        height,
        scale,
    }) = command
    {
        config.width = width;
        config.height = height;
        config.scale = scale;
    }
}

async fn wait_for_retry(
    delay: Duration,
    command_rx: &mut Receiver<SessionCommand>,
    pending: &PendingCommands,
    config: &mut SessionConfig,
) -> bool {
    let deadline = tokio::time::Instant::now() + delay;
    loop {
        if pending.shutdown.swap(false, Ordering::AcqRel) {
            return true;
        }
        pending.release_all_keys.store(false, Ordering::Release);
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return false,
            command = command_rx.recv() => match command {
                Some(SessionCommand::Shutdown) | None => return true,
                Some(command) => {
                    absorb_offline_command(resolve_pending_command(command, pending), config);
                }
            }
        }
    }
}

/// Drain any pending commands between reconnects. Returns true if the session
/// should stop (the window was closed, so a Shutdown is queued or all senders
/// are gone).
fn drain_should_stop(
    command_rx: &mut Receiver<SessionCommand>,
    pending: &PendingCommands,
    config: &mut SessionConfig,
) -> bool {
    use tokio::sync::mpsc::error::TryRecvError;
    if pending.shutdown.swap(false, Ordering::AcqRel) {
        return true;
    }
    loop {
        match command_rx.try_recv() {
            Ok(SessionCommand::Shutdown) => return true,
            Ok(command) => {
                absorb_offline_command(resolve_pending_command(command, pending), config);
            }
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => return true,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_session(
    connection_result: ConnectionResult,
    framed: SessionFramed,
    config: &SessionConfig,
    framebuffer: &SharedFramebuffer,
    command_rx: &mut Receiver<SessionCommand>,
    pending: &PendingCommands,
    mut clip_rx: Receiver<ClipSignal>,
    mut gfx_rx: tokio::sync::mpsc::UnboundedReceiver<GfxEvent>,
    local_clip: &LocalClipState,
    event_cb: &EventCb,
) -> SessionEnd {
    let width = connection_result.desktop_size.width;
    let height = connection_result.desktop_size.height;
    if width > crate::profile::MAX_REMOTE_DIMENSION || height > crate::profile::MAX_REMOTE_DIMENSION
    {
        return SessionEnd::Disconnected(format!(
            "server negotiated an unreasonable desktop size {width}x{height}"
        ));
    }
    framebuffer.resize(width, height);
    let mut image = DecodedImage::new(PIXEL_FORMAT, width, height);
    let mut active_stage = ActiveStage::new(connection_result);
    let mut input_db = Database::new();
    event_cb(SessionEvent::Connected { width, height });

    // Split the stream so a write blocked on TCP backpressure can never stall
    // reads: reads run here, writes drain on a dedicated task.
    let (mut reader, writer) = split_tokio_framed(framed);
    let (out_tx, out_rx) = channel::<Vec<u8>>(OUTPUT_QUEUE_CAPACITY);
    let mut writer_task = tokio::spawn(writer_loop(writer, out_rx));
    // With clipboard disabled the sender is dropped at connect; stop polling the
    // closed channel or the select loop would spin at 100% CPU. Same for gfx.
    let mut clip_open = true;
    let mut gfx_open = true;
    let mut remote_clip = RemoteClipboard::default();
    // Idle keep-alive bookkeeping: `last_input` is pushed forward by every real
    // user command, so the timer arm below only fires after a full idle
    // interval; `keys_down` suppresses the tap while a key is held so the
    // injected F15 can never merge with a modifier the user is pressing.
    let mut last_input = tokio::time::Instant::now();
    let mut keys_down: u32 = 0;

    let end = loop {
        if pending.shutdown.swap(false, Ordering::AcqRel) {
            let _ = handle_command(
                SessionCommand::Shutdown,
                config,
                &mut reader,
                &out_tx,
                &mut active_stage,
                &mut image,
                &mut input_db,
                framebuffer,
                local_clip,
                &mut remote_clip,
                event_cb,
            )
            .await;
            break SessionEnd::UserQuit;
        }
        if pending.release_all_keys.swap(false, Ordering::AcqRel) {
            // This path (not the command channel) usually wins the race, so
            // clear the held-key counter here too or the keep-alive could stay
            // suppressed after focus is lost while a key was held.
            keys_down = 0;
            let fp_events = input_db.release_all();
            if !fp_events.is_empty() {
                match active_stage.process_fastpath_input(&mut image, &fp_events) {
                    Ok(outputs) => match drain_outputs(
                        outputs,
                        &mut reader,
                        &out_tx,
                        &mut active_stage,
                        &mut image,
                        framebuffer,
                        event_cb,
                    )
                    .await
                    {
                        Ok(true) => {
                            break SessionEnd::Disconnected(REMOTE_ENDED.to_string());
                        }
                        Ok(false) => {}
                        Err(error) => break SessionEnd::Disconnected(format!("{error:#}")),
                    },
                    Err(error) => break SessionEnd::Disconnected(format!("{error:#}")),
                }
            }
        }
        tokio::select! {
            cmd = command_rx.recv() => {
                let Some(cmd) = cmd else { break SessionEnd::UserQuit };
                let Some(cmd) = resolve_pending_command(cmd, pending) else {
                    continue;
                };
                if matches!(cmd, SessionCommand::Input(_)) {
                    last_input = tokio::time::Instant::now();
                }
                keys_down = update_keys_down(&cmd, keys_down);
                match handle_command(
                    cmd, config, &mut reader, &out_tx, &mut active_stage, &mut image,
                    &mut input_db, framebuffer, local_clip, &mut remote_clip, event_cb,
                ).await {
                    Ok(true) => break SessionEnd::UserQuit,
                    Ok(false) => {}
                    Err(e) => break SessionEnd::Disconnected(format!("{e:#}")),
                }
            }

            sig = clip_rx.recv(), if clip_open => {
                match sig {
                    Some(sig) => {
                        if let Err(e) = handle_clip_signal(sig, &out_tx, &mut active_stage, &mut remote_clip, event_cb).await {
                            tracing::warn!("clipboard: {e}");
                        }
                    }
                    None => clip_open = false,
                }
            }

            ev = gfx_rx.recv(), if gfx_open => {
                match ev {
                    // The compositor already wrote the pixels; just repaint.
                    Some(GfxEvent::Updated) => event_cb(SessionEvent::FrameUpdated {
                        x: 0, y: 0, width: 0, height: 0,
                    }),
                    Some(GfxEvent::Resized { width, height }) => {
                        event_cb(SessionEvent::Resized { width, height });
                    }
                    None => gfx_open = false,
                }
            }

            // Idle keep-alive. Disabled unless enabled and no key is held; the
            // sleep target moves with `last_input`, so any real input reschedules
            // it and the tap only fires after a full idle interval.
            _ = tokio::time::sleep_until(last_input + KEEP_ALIVE_INTERVAL),
                if config.keep_alive && keys_down == 0 =>
            {
                match send_keepalive_tap(
                    &mut reader, &out_tx, &mut active_stage, &mut image,
                    &mut input_db, framebuffer, event_cb,
                ).await {
                    Ok(true) => break SessionEnd::Disconnected(REMOTE_ENDED.to_string()),
                    Ok(false) => {}
                    Err(e) => break SessionEnd::Disconnected(format!("{e:#}")),
                }
                last_input = tokio::time::Instant::now();
            }

            pdu = reader.read_pdu() => {
                match pdu {
                    Ok((action, payload)) => match active_stage.process(&mut image, action, &payload) {
                        Ok(outputs) => match drain_outputs(outputs, &mut reader, &out_tx, &mut active_stage, &mut image, framebuffer, event_cb).await {
                            Ok(true) => break SessionEnd::Disconnected(REMOTE_ENDED.to_string()),
                            Ok(false) => {}
                            Err(e) => break SessionEnd::Disconnected(format!("{e:#}")),
                        },
                        Err(e) => break SessionEnd::Disconnected(format!("{e:#}")),
                    },
                    Err(e) => break SessionEnd::Disconnected(format!("{e:#}")),
                }
            }
        }
    };

    // Close the queue so the writer drains and exits. On a clean quit, wait for
    // the drain (bounded) so the graceful-shutdown PDU is flushed; then abort so
    // a task blocked on a wedged socket can never leak.
    drop(out_tx);
    if matches!(end, SessionEnd::UserQuit) {
        let _ = tokio::time::timeout(SHUTDOWN_FLUSH, &mut writer_task).await;
    }
    writer_task.abort();
    end
}

fn resolve_pending_command(
    command: SessionCommand,
    pending: &PendingCommands,
) -> Option<SessionCommand> {
    match command {
        SessionCommand::ReleaseAllKeys => pending
            .release_all_keys
            .swap(false, Ordering::AcqRel)
            .then_some(SessionCommand::ReleaseAllKeys),
        SessionCommand::Shutdown => pending
            .shutdown
            .swap(false, Ordering::AcqRel)
            .then_some(SessionCommand::Shutdown),
        SessionCommand::Input(events)
            if matches!(events.as_slice(), [InputEvent::MouseMove { .. }]) =>
        {
            let mut slot = pending.mouse_move.lock().unwrap();
            slot.queued = false;
            slot.value
                .take()
                .map(|(x, y)| SessionCommand::Input(vec![InputEvent::MouseMove { x, y }]))
        }
        SessionCommand::Resize { .. } => {
            let mut slot = pending.resize.lock().unwrap();
            slot.queued = false;
            slot.value
                .take()
                .map(|(width, height, scale)| SessionCommand::Resize {
                    width,
                    height,
                    scale,
                })
        }
        command => Some(command),
    }
}

/// Drain queued frames to the socket. Runs concurrently with the read loop so a
/// blocked write never stalls reads.
async fn writer_loop(mut writer: SessionWriter, mut out_rx: Receiver<Vec<u8>>) {
    while let Some(bytes) = out_rx.recv().await {
        if let Err(e) = writer.write_all(&bytes).await {
            tracing::debug!("write failed, stopping writer: {e}");
            break;
        }
    }
}

async fn connect(
    config: &SessionConfig,
    session_trusted: &mut Option<String>,
    local_clip: LocalClipState,
    clip_tx: Sender<ClipSignal>,
    gfx_tx: tokio::sync::mpsc::UnboundedSender<GfxEvent>,
    audio: Option<&crate::audio::AudioPlayer>,
    framebuffer: &Arc<SharedFramebuffer>,
    event_cb: &EventCb,
) -> std::result::Result<(ConnectionResult, SessionFramed), ConnectFailure> {
    let tcp = connect_tcp(&config.host, config.port).await?;
    tcp.set_nodelay(true).ok();
    let client_addr = tcp
        .local_addr()
        .context("resolving local address")
        .map_err(ConnectFailure::fatal)?;

    let display_control = DisplayControlClient::new(|_caps| Ok(Vec::new()));
    let mut drdynvc = DrdynvcClient::new().with_dynamic_channel(display_control);
    if let Some(player) = audio {
        // Windows 7+ prefers audio over the dynamic channel when the client
        // supports DVC; without this listener the session stays silent.
        drdynvc =
            drdynvc.with_dynamic_channel(crate::audio::RdpsndDvcChannel::new(player.handler()));
    }
    if config.graphics == GraphicsMode::Egfx {
        // H.264 decode failing to initialize is not fatal: the pipeline still
        // renders uncompressed updates, and the server prefers AVC only when
        // the (filtered) capabilities advertise it.
        let decoder = match ironrdp_egfx::decode::OpenH264Decoder::new() {
            Ok(d) => Some(Box::new(d) as Box<dyn ironrdp_egfx::decode::H264Decoder>),
            Err(e) => {
                tracing::warn!("egfx: H.264 decoder unavailable ({e}); using fallback caps");
                None
            }
        };
        let handler = crate::gfx::GfxHandler::new(framebuffer.clone(), gfx_tx);
        drdynvc = drdynvc.with_dynamic_channel(ironrdp_egfx::client::GraphicsPipelineClient::new(
            Box::new(handler),
            decoder,
        ));
    }

    let mut connector = ClientConnector::new(build_config(config, audio.is_some()), client_addr)
        .with_static_channel(drdynvc);

    if let Some(player) = audio {
        connector.attach_static_channel(Rdpsnd::new(Box::new(player.handler())));
        // mstsc always announces device redirection; some servers gate parts
        // of channel setup (audio among them) on its presence. No devices are
        // shared — the Noop backend only completes the core handshake.
        connector
            .attach_static_channel(Rdpdr::new(Box::new(NoopRdpdrBackend), "RDP123".to_owned()));
    }

    // Only register clipboard redirection when it is enabled.
    if config.clipboard.enabled() {
        let backend = MacClipboardBackend {
            tx: clip_tx,
            local_clip,
            tmp: std::env::temp_dir().to_string_lossy().into_owned(),
            mode: config.clipboard,
        };
        connector.attach_static_channel(CliprdrClient::new(Box::new(backend)));
    } else {
        drop((clip_tx, local_clip));
    }

    let mut framed = TokioFramed::new(tcp);

    // 1. Pre-TLS X.224 negotiation.
    let should_upgrade = connect_begin(&mut framed, &mut connector)
        .await
        .map_err(connector_err)
        .map_err(ConnectFailure::fatal)?;

    // 2. TLS handshake. CA/hostname verification is intentionally replaced by
    // TOFU below, but the server must still prove possession of the certificate
    // private key by signing the TLS handshake.
    let (initial_stream, leftover) = framed.into_inner();
    let (tls_stream, server_public_key) = crate::tls::upgrade(initial_stream, &config.host)
        .await
        .context("TLS handshake")
        .map_err(ConnectFailure::fatal)?;

    // 3. Trust-on-first-use gate — before any credentials are sent via CredSSP.
    let fingerprint = fingerprint_hex(&server_public_key);
    tofu_gate(session_trusted, &fingerprint, event_cb)
        .await
        .map_err(ConnectFailure::fatal)?;

    // 4. Finalize (MCS, licensing, capabilities, CredSSP/NLA).
    let upgraded = mark_as_upgraded(should_upgrade, &mut connector);
    let mut framed = TokioFramed::new_with_leftover(tls_stream, leftover);
    let mut network_client = NoNetworkClient;
    let result = connect_finalize(
        upgraded,
        connector,
        &mut framed,
        &mut network_client,
        ServerName::new(&config.host),
        server_public_key,
        None,
    )
    .await
    .map_err(connector_err)
    .map_err(ConnectFailure::fatal)?;

    Ok((result, framed))
}

/// Open the TCP socket with a bounded timeout. A LAN connect that macOS blocks
/// for Local Network privacy fails or hangs here, so the message points at the fix.
async fn connect_tcp(host: &str, port: u16) -> std::result::Result<TcpStream, ConnectFailure> {
    match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port))).await {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(e)) => Err(ConnectFailure::retryable(anyhow!(
            "could not reach {host}:{port} ({e}). If this host works in other RDP \
             clients, enable RDP123 under System Settings → Privacy & Security → \
             Local Network, then try again."
        ))),
        Err(_) => Err(ConnectFailure::retryable(anyhow!(
            "timed out reaching {host}:{port}. Check the host and port, and that RDP123 \
             is allowed under System Settings → Privacy & Security → Local Network."
        ))),
    }
}

async fn tofu_gate(
    session_trusted: &mut Option<String>,
    fingerprint: &str,
    event_cb: &EventCb,
) -> Result<()> {
    if session_trusted.as_deref() == Some(fingerprint) {
        return Ok(());
    }
    let is_change = session_trusted.is_some();
    let (reply, rx) = oneshot::channel();
    event_cb(SessionEvent::CertificateApproval {
        fingerprint: fingerprint.to_string(),
        is_change,
        reply,
    });
    if rx.await.unwrap_or(false) {
        event_cb(SessionEvent::CertTrusted {
            fingerprint: fingerprint.to_string(),
        });
        *session_trusted = Some(fingerprint.to_string());
        Ok(())
    } else {
        Err(anyhow!("server certificate not trusted"))
    }
}

async fn handle_command(
    cmd: SessionCommand,
    config: &SessionConfig,
    reader: &mut SessionReader,
    out_tx: &OutSender,
    active_stage: &mut ActiveStage,
    image: &mut DecodedImage,
    input_db: &mut Database,
    framebuffer: &SharedFramebuffer,
    local_clip: &LocalClipState,
    remote_clip: &mut RemoteClipboard,
    event_cb: &EventCb,
) -> Result<bool> {
    match cmd {
        SessionCommand::Input(events) => {
            let (operations, last_mouse) = translate_input(events, config.swap_cmd_alt);
            if let Some((x, y)) = last_mouse {
                active_stage.update_mouse_pos(x, y);
            }
            let fp_events = input_db.apply(operations);
            if !fp_events.is_empty() {
                let outputs = active_stage.process_fastpath_input(image, &fp_events)?;
                return drain_outputs(
                    outputs,
                    reader,
                    out_tx,
                    active_stage,
                    image,
                    framebuffer,
                    event_cb,
                )
                .await;
            }
        }
        SessionCommand::Resize {
            width,
            height,
            scale,
        } => {
            // Fixed-resolution sessions keep the remote size; the window just scales it.
            if !config.dynamic_resolution {
                return Ok(false);
            }
            let (w, h) =
                MonitorLayoutEntry::adjust_display_size(u32::from(width), u32::from(height));
            match active_stage.encode_resize(w, h, scale, None) {
                Some(Ok(frame)) => emit(out_tx, frame).await?,
                Some(Err(e)) => tracing::warn!("resize encode failed: {e}"),
                None => tracing::debug!("resize ignored: display control not ready yet"),
            }
        }
        SessionCommand::LocalClipboard(text) => {
            if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
                tracing::warn!(
                    "local clipboard text is too large to redirect ({} bytes)",
                    text.len()
                );
                return Ok(false);
            }
            *local_clip.lock().unwrap() = LocalClip::Text(text);
            let formats = vec![ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)];
            send_cliprdr(active_stage, out_tx, |c| c.initiate_copy(&formats)).await?;
        }
        SessionCommand::LocalClipboardFiles(paths) => {
            let files = collect_clipboard_files(&paths);
            if files.is_empty() {
                return Ok(false);
            }
            let descriptors = to_file_descriptors(&files);
            tracing::debug!("clipboard: offering {} file entries", files.len());
            *local_clip.lock().unwrap() = LocalClip::Files(files);
            send_cliprdr(active_stage, out_tx, |c| c.initiate_file_copy(descriptors)).await?;
        }
        SessionCommand::FetchRemoteClipItem { name, dest, done } => {
            if let Some(job) = plan_fetch_job(remote_clip, &name, &dest, done) {
                remote_clip.jobs.push_back(job);
                advance_remote_fetch(remote_clip, active_stage, out_tx).await?;
            }
        }
        SessionCommand::ReleaseAllKeys => {
            let fp_events = input_db.release_all();
            if !fp_events.is_empty() {
                let outputs = active_stage.process_fastpath_input(image, &fp_events)?;
                return drain_outputs(
                    outputs,
                    reader,
                    out_tx,
                    active_stage,
                    image,
                    framebuffer,
                    event_cb,
                )
                .await;
            }
        }
        SessionCommand::Shutdown => {
            if let Ok(outputs) = active_stage.graceful_shutdown() {
                for out in outputs {
                    if let ActiveStageOutput::ResponseFrame(frame) = out {
                        emit(out_tx, frame).await?;
                    }
                }
            }
            return Ok(true);
        }
    }
    Ok(false)
}

async fn handle_clip_signal(
    sig: ClipSignal,
    out_tx: &OutSender,
    active_stage: &mut ActiveStage,
    remote_clip: &mut RemoteClipboard,
    event_cb: &EventCb,
) -> Result<()> {
    match sig {
        ClipSignal::InitiateCopy(formats) => {
            send_cliprdr(active_stage, out_tx, |c| c.initiate_copy(&formats)).await
        }
        ClipSignal::InitiateFileCopy(descriptors) => {
            send_cliprdr(active_stage, out_tx, |c| c.initiate_file_copy(descriptors)).await
        }
        ClipSignal::SubmitFileContents(response) => {
            send_cliprdr(active_stage, out_tx, |c| c.submit_file_contents(response)).await
        }
        ClipSignal::InitiatePaste(format) => {
            send_cliprdr(active_stage, out_tx, |c| c.initiate_paste(format)).await
        }
        ClipSignal::SubmitData(response) => {
            send_cliprdr(active_stage, out_tx, |c| c.submit_format_data(response)).await
        }
        ClipSignal::RemoteText(text) => {
            event_cb(SessionEvent::ClipboardText(text));
            Ok(())
        }
        ClipSignal::RemoteFileList { files, data_id } => {
            remote_clip.files = files;
            remote_clip.data_id = data_id;
            let items: Vec<RemoteClipItem> = remote_clip
                .files
                .iter()
                .filter(|f| !f.wire_name.contains('\\'))
                .map(|f| RemoteClipItem {
                    name: f.wire_name.clone(),
                    is_dir: f.is_dir,
                })
                .collect();
            if !items.is_empty() {
                event_cb(SessionEvent::ClipboardFiles(items));
            }
            Ok(())
        }
        ClipSignal::RemoteFileContents { stream_id, data } => {
            handle_remote_file_contents(remote_clip, stream_id, data, active_stage, out_tx).await
        }
    }
}

/// Queue a frame for the writer task with bounded backpressure.
async fn emit(out_tx: &OutSender, bytes: Vec<u8>) -> Result<()> {
    out_tx
        .send(bytes)
        .await
        .map_err(|_| anyhow!("session writer stopped"))
}

/// Call a `CliprdrClient` method (if the channel exists), then encode and queue
/// the resulting messages.
async fn send_cliprdr<F>(active_stage: &mut ActiveStage, out_tx: &OutSender, make: F) -> Result<()>
where
    F: FnOnce(&mut CliprdrClient) -> PduResult<CliprdrSvcMessages<Client>>,
{
    let produced = active_stage
        .get_svc_processor_mut::<CliprdrClient>()
        .map(make);
    if let Some(result) = produced {
        let messages = result.map_err(|e| anyhow!("cliprdr: {e}"))?;
        let bytes = active_stage
            .process_svc_processor_messages(messages)
            .map_err(|e| anyhow!("cliprdr encode: {e}"))?;
        emit(out_tx, bytes).await?;
    }
    Ok(())
}

/// Apply every `ActiveStageOutput`. Returns `Ok(true)` when the session should end.
async fn drain_outputs(
    outputs: Vec<ActiveStageOutput>,
    reader: &mut SessionReader,
    out_tx: &OutSender,
    active_stage: &mut ActiveStage,
    image: &mut DecodedImage,
    framebuffer: &SharedFramebuffer,
    event_cb: &EventCb,
) -> Result<bool> {
    for output in outputs {
        match output {
            ActiveStageOutput::ResponseFrame(frame) => emit(out_tx, frame).await?,
            ActiveStageOutput::GraphicsUpdate(region) => {
                let width = region.width();
                let height = region.height();
                framebuffer.blit_rect(image.data(), region.left, region.top, width, height);
                event_cb(SessionEvent::FrameUpdated {
                    x: region.left,
                    y: region.top,
                    width,
                    height,
                });
            }
            ActiveStageOutput::DeactivateAll(cas) => {
                reactivate(
                    cas,
                    reader,
                    out_tx,
                    active_stage,
                    image,
                    framebuffer,
                    event_cb,
                )
                .await?;
            }
            ActiveStageOutput::Terminate(_reason) => return Ok(true),
            // Pointer shapes are mirrored onto the native macOS cursor so the
            // remote shape (resize arrows, I-beam, hand) shows without the
            // laggy server-composited cursor.
            ActiveStageOutput::PointerBitmap(pointer) => {
                event_cb(SessionEvent::PointerBitmap {
                    width: pointer.width,
                    height: pointer.height,
                    hotspot_x: pointer.hotspot_x,
                    hotspot_y: pointer.hotspot_y,
                    rgba: pointer.bitmap_data.clone(),
                });
            }
            ActiveStageOutput::PointerDefault => event_cb(SessionEvent::PointerDefault),
            ActiveStageOutput::PointerHidden => event_cb(SessionEvent::PointerHidden),
            // Server-initiated pointer warps are not applied to the local
            // mouse; the multitransport/autodetect paths are unused.
            ActiveStageOutput::PointerPosition { .. }
            | ActiveStageOutput::MultitransportRequest(_)
            | ActiveStageOutput::AutoDetect(_) => {}
        }
    }
    Ok(false)
}

/// Drive a Deactivation-Reactivation sequence (e.g. a server-side resolution
/// change) to completion, then re-size local state to the new desktop.
async fn reactivate(
    mut cas: Box<ConnectionActivationSequence>,
    reader: &mut SessionReader,
    out_tx: &OutSender,
    active_stage: &mut ActiveStage,
    image: &mut DecodedImage,
    framebuffer: &SharedFramebuffer,
    event_cb: &EventCb,
) -> Result<()> {
    let (size, share_id) = loop {
        if let ConnectionActivationState::Finalized {
            desktop_size,
            share_id,
            ..
        } = cas.connection_activation_state()
        {
            break (desktop_size, share_id);
        }
        // Read + step on the read half; queue any response for the writer task.
        let mut buf = WriteBuf::new();
        single_sequence_step_read(reader, &mut *cas, &mut buf)
            .await
            .map_err(connector_err)?;
        if !buf.filled().is_empty() {
            emit(out_tx, buf.filled().to_vec()).await?;
        }
    };

    // Never allocate for an absurd server-announced size (a hostile or buggy
    // server could otherwise request a multi-gigabyte framebuffer).
    if size.width > crate::profile::MAX_REMOTE_DIMENSION
        || size.height > crate::profile::MAX_REMOTE_DIMENSION
    {
        return Err(anyhow!(
            "server requested an unreasonable desktop size {}x{}",
            size.width,
            size.height
        ));
    }
    *image = DecodedImage::new(PIXEL_FORMAT, size.width, size.height);
    framebuffer.resize(size.width, size.height);
    active_stage.set_share_id(share_id);
    event_cb(SessionEvent::Resized {
        width: size.width,
        height: size.height,
    });
    Ok(())
}

/// Fold a command's key events into the held-key counter that gates the idle
/// keep-alive. Key-down bumps it, key-up drops it (saturating at zero), and a
/// full release resets it; everything else leaves the count untouched. While
/// the count is above zero the keep-alive is suppressed so an injected F15
/// cannot combine with a modifier the user is holding.
fn update_keys_down(cmd: &SessionCommand, keys_down: u32) -> u32 {
    match cmd {
        SessionCommand::Input(events) => {
            let mut n = i64::from(keys_down);
            for ev in events {
                if let InputEvent::Key { down, .. } = ev {
                    n += if *down { 1 } else { -1 };
                }
            }
            n.max(0) as u32
        }
        SessionCommand::ReleaseAllKeys => 0,
        _ => keys_down,
    }
}

/// Inject a single invisible F15 tap (down+up) straight into the FastPath input
/// stream, bypassing the mac→scancode keymap. Returns `Ok(true)` if flushing the
/// tap revealed that the remote had already ended. Used by the idle keep-alive.
#[allow(clippy::too_many_arguments)]
async fn send_keepalive_tap(
    reader: &mut SessionReader,
    out_tx: &OutSender,
    active_stage: &mut ActiveStage,
    image: &mut DecodedImage,
    input_db: &mut Database,
    framebuffer: &SharedFramebuffer,
    event_cb: &EventCb,
) -> Result<bool> {
    let scan = Scancode::from_u8(false, KEEP_ALIVE_SCANCODE);
    let fp_events = input_db.apply(vec![
        Operation::KeyPressed(scan),
        Operation::KeyReleased(scan),
    ]);
    if fp_events.is_empty() {
        return Ok(false);
    }
    let outputs = active_stage.process_fastpath_input(image, &fp_events)?;
    drain_outputs(
        outputs,
        reader,
        out_tx,
        active_stage,
        image,
        framebuffer,
        event_cb,
    )
    .await
}

/// Convert UI input into IronRDP operations, returning the last mouse position seen.
fn translate_input(
    events: Vec<InputEvent>,
    swap_cmd_alt: bool,
) -> (Vec<Operation>, Option<(u16, u16)>) {
    let mut ops = Vec::with_capacity(events.len() + 2);
    let mut last_mouse = None;
    for event in events {
        match event {
            InputEvent::Key { keycode, down } => {
                if let Some(sc) = keymap::mac_keycode_to_scancode(keycode, swap_cmd_alt) {
                    let scan = Scancode::from_u8(sc.extended, sc.code as u8);
                    ops.push(if down {
                        Operation::KeyPressed(scan)
                    } else {
                        Operation::KeyReleased(scan)
                    });
                }
            }
            InputEvent::MouseMove { x, y } => {
                last_mouse = Some((x, y));
                ops.push(Operation::MouseMove(MousePosition { x, y }));
            }
            InputEvent::MouseButton { button, down, x, y } => {
                last_mouse = Some((x, y));
                ops.push(Operation::MouseMove(MousePosition { x, y }));
                let b = match button {
                    PointerButton::Left => MouseButton::Left,
                    PointerButton::Right => MouseButton::Right,
                    PointerButton::Middle => MouseButton::Middle,
                };
                ops.push(if down {
                    Operation::MouseButtonPressed(b)
                } else {
                    Operation::MouseButtonReleased(b)
                });
            }
            InputEvent::Wheel { delta, horizontal } => {
                ops.push(Operation::WheelRotations(WheelRotations {
                    is_vertical: !horizontal,
                    rotation_units: delta,
                }));
            }
        }
    }
    (ops, last_mouse)
}

fn build_config(config: &SessionConfig, audio_active: bool) -> Config {
    let bitmap = Some(BitmapConfig {
        lossy_compression: true,
        color_depth: config.color_depth,
        codecs: client_codecs_capabilities(&[]).expect("default codecs"),
    });
    Config {
        desktop_size: DesktopSize {
            width: config.width,
            height: config.height,
        },
        desktop_scale_factor: config.scale.unwrap_or(100),
        enable_tls: false,
        enable_credssp: true,
        credentials: Credentials::UsernamePassword {
            username: config.username.clone(),
            password: config.password.clone(),
        },
        domain: config.domain.clone(),
        client_build: 0,
        client_name: "RDP123".to_string(),
        keyboard_type: KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_functional_keys_count: 12,
        keyboard_layout: 0,
        ime_file_name: String::new(),
        bitmap,
        dig_product_id: String::new(),
        client_dir: String::new(),
        alternate_shell: String::new(),
        work_dir: String::new(),
        platform: MajorPlatformType::MACINTOSH,
        hardware_id: None,
        request_data: None,
        autologon: false,
        // ThisComputer: announced only when the local output is actually ready
        // (false sets INFO_NOAUDIOPLAYBACK, telling the server to discard).
        // RemoteComputer: don't suppress, but no redirection channel either —
        // IronRDP does not expose INFO_REMOTECONSOLEAUDIO for true console audio.
        enable_audio_playback: match config.audio {
            AudioMode::ThisComputer => audio_active,
            AudioMode::RemoteComputer => true,
            AudioMode::Never => false,
        },
        // From our vendored connector patch: advertises the Graphics Pipeline.
        support_gfx: config.graphics == GraphicsMode::Egfx,
        // Ask the host to render text with ClearType/anti-aliasing; without this
        // fonts come across rough and un-smoothed.
        performance_flags: PerformanceFlags::ENABLE_FONT_SMOOTHING,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        compression_type: config.compression.then_some(CompressionType::Rdp61),
        // Do not composite the server pointer into the framebuffer: that produces
        // a laggy remote cursor that lingers. Pointer shapes are decoded to RGBA
        // and applied to the native macOS cursor instead (see drain_outputs).
        enable_server_pointer: true,
        pointer_software_rendering: false,
        multitransport_flags: None,
    }
}

fn fingerprint_hex(public_key: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(public_key);
    let digest = hasher.finalize();
    let mut out = String::from("sha256:");
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn connector_err(e: ConnectorError) -> anyhow::Error {
    anyhow!("{e}")
}

fn ensure_crypto_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

/// CredSSP with password auth (NTLM) needs no out-of-band network access; this
/// stub errors if the connector ever tries a Kerberos KDC request.
#[derive(Debug)]
struct NoNetworkClient;

impl NetworkClient for NoNetworkClient {
    fn send(
        &mut self,
        _request: &NetworkRequest,
    ) -> impl std::future::Future<Output = ConnectorResult<Vec<u8>>> {
        std::future::ready(Err(ConnectorError::general(
            "out-of-band network requests (Kerberos) are not supported",
        )))
    }
}

/// A clipboard backend bridging CLIPRDR to `NSPasteboard` (via the app):
/// Unicode text in both directions plus local files offered to the remote
/// (file streams, MS-RDPECLIP 3.1.1.4).
#[derive(Debug)]
struct MacClipboardBackend {
    tx: Sender<ClipSignal>,
    local_clip: LocalClipState,
    tmp: String,
    mode: ClipboardMode,
}

impl MacClipboardBackend {
    /// Serve one FileContents request from the offered files snapshot.
    fn file_contents_response(
        &self,
        request: &FileContentsRequest,
    ) -> FileContentsResponse<'static> {
        use std::io::{Read as _, Seek as _};

        let error = FileContentsResponse::new_error(request.stream_id);
        if !self.mode.allow_local_to_remote() {
            return error;
        }
        let clip = self.local_clip.lock().unwrap();
        let LocalClip::Files(files) = &*clip else {
            return error;
        };
        let Some(file) = usize::try_from(request.index)
            .ok()
            .and_then(|index| files.get(index))
        else {
            tracing::warn!(
                "clipboard: FileContents request for unknown index {}",
                request.index
            );
            return error;
        };

        if request.flags.contains(FileContentsFlags::SIZE) {
            return FileContentsResponse::new_size_response(request.stream_id, file.size);
        }
        if !request.flags.contains(FileContentsFlags::RANGE) || file.is_dir {
            return error;
        }

        let requested = request.requested_size.min(MAX_FILE_CHUNK_BYTES) as usize;
        let mut open = match std::fs::File::open(&file.path) {
            Ok(open) => open,
            Err(e) => {
                tracing::warn!("clipboard: cannot open {}: {e}", file.path.display());
                return error;
            }
        };
        if let Err(e) = open.seek(std::io::SeekFrom::Start(request.position)) {
            tracing::warn!("clipboard: seek in {} failed: {e}", file.path.display());
            return error;
        }
        let mut data = vec![0u8; requested];
        let mut filled = 0usize;
        while filled < requested {
            match open.read(&mut data[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) => {
                    tracing::warn!("clipboard: read from {} failed: {e}", file.path.display());
                    return error;
                }
            }
        }
        data.truncate(filled);
        FileContentsResponse::new_data_response(request.stream_id, data)
    }
}

impl ironrdp::core::AsAny for MacClipboardBackend {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl CliprdrBackend for MacClipboardBackend {
    fn temporary_directory(&self) -> &str {
        &self.tmp
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        // File streams for Finder -> Explorer copies; names stay relative.
        ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
            | ClipboardGeneralCapabilityFlags::FILECLIP_NO_FILE_PATHS
            | ClipboardGeneralCapabilityFlags::HUGE_FILE_SUPPORT_ENABLED
    }

    fn on_ready(&mut self) {}

    fn on_request_format_list(&mut self) {
        // Advertise our clipboard only if local -> remote is allowed.
        if !self.mode.allow_local_to_remote() {
            return;
        }
        match &*self.local_clip.lock().unwrap() {
            LocalClip::Files(files) => {
                let _ = self
                    .tx
                    .try_send(ClipSignal::InitiateFileCopy(to_file_descriptors(files)));
            }
            _ => {
                let formats = vec![ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)];
                let _ = self.tx.try_send(ClipSignal::InitiateCopy(formats));
            }
        }
    }

    fn on_process_negotiated_capabilities(&mut self, _caps: ClipboardGeneralCapabilityFlags) {}

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        // Fetch what the remote copied only if remote -> local is allowed.
        // A file copy wins over the file-name text that accompanies it.
        if !self.mode.allow_remote_to_local() {
            return;
        }
        let file_list = available_formats.iter().find(|f| {
            f.name()
                .is_some_and(|name| name.value() == "FileGroupDescriptorW")
        });
        if let Some(format) = file_list {
            let _ = self.tx.try_send(ClipSignal::InitiatePaste(format.id()));
        } else if available_formats
            .iter()
            .any(|f| f.id == ClipboardFormatId::CF_UNICODETEXT)
        {
            let _ = self
                .tx
                .try_send(ClipSignal::InitiatePaste(ClipboardFormatId::CF_UNICODETEXT));
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        let response = if self.mode.allow_local_to_remote()
            && request.format == ClipboardFormatId::CF_UNICODETEXT
        {
            let text = match &*self.local_clip.lock().unwrap() {
                LocalClip::Text(text) => text.clone(),
                _ => String::new(),
            };
            let crlf = normalize_clipboard_to_crlf(&text);
            FormatDataResponse::new_unicode_string(&crlf)
        } else {
            FormatDataResponse::new_error()
        };
        let _ = self.tx.try_send(ClipSignal::SubmitData(response));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        if response.is_error() || !self.mode.allow_remote_to_local() {
            return;
        }
        if let Ok(text) = response.to_unicode_string() {
            if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
                tracing::warn!(
                    "remote clipboard text is too large to accept ({} bytes)",
                    text.len()
                );
                return;
            }
            let _ = self
                .tx
                .try_send(ClipSignal::RemoteText(text.replace("\r\n", "\n")));
        }
    }

    fn on_file_contents_request(&mut self, request: FileContentsRequest) {
        let response = self.file_contents_response(&request);
        let _ = self.tx.try_send(ClipSignal::SubmitFileContents(response));
    }

    fn on_file_contents_response(&mut self, response: FileContentsResponse<'_>) {
        let data = if response.is_error() {
            None
        } else {
            Some(response.data().to_vec())
        };
        let _ = self.tx.try_send(ClipSignal::RemoteFileContents {
            stream_id: response.stream_id(),
            data,
        });
    }

    fn on_remote_file_list(&mut self, files: &[FileDescriptor], clip_data_id: Option<u32>) {
        if !self.mode.allow_remote_to_local() {
            return;
        }
        let entries: Vec<RemoteFileEntry> = files
            .iter()
            .map(|descriptor| RemoteFileEntry {
                wire_name: match descriptor.relative_path.as_deref() {
                    Some(path) if !path.is_empty() => format!("{path}\\{}", descriptor.name),
                    _ => descriptor.name.clone(),
                },
                size: descriptor.file_size,
                is_dir: descriptor
                    .attributes
                    .is_some_and(|a| a.contains(ClipboardFileAttributes::DIRECTORY)),
            })
            .collect();
        let _ = self.tx.try_send(ClipSignal::RemoteFileList {
            files: entries,
            data_id: clip_data_id,
        });
    }

    // Lock snapshots for outgoing copies are managed inside ironrdp-cliprdr.
    fn on_lock(&mut self, _data_id: LockDataId) {}
    fn on_unlock(&mut self, _data_id: LockDataId) {}
}

fn normalize_clipboard_to_crlf(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', "\r\n")
}

#[cfg(test)]
mod tests {
    use super::{
        build_config, normalize_clipboard_to_crlf, resolve_pending_command, update_keys_down,
        InputEvent, PendingCommands, SessionCommand, SessionConfig,
    };
    use crate::profile::{AudioMode, ClipboardMode, GraphicsMode};
    use ironrdp::pdu::rdp::client_info::CompressionType;
    use std::sync::atomic::Ordering;

    fn test_session_config(graphics: GraphicsMode, compression: bool) -> SessionConfig {
        SessionConfig {
            host: "example.test".to_string(),
            port: 3389,
            username: "user".to_string(),
            password: "password".to_string(),
            domain: None,
            width: 1280,
            height: 720,
            scale: Some(100),
            expected_fingerprint: None,
            color_depth: 32,
            compression,
            clipboard: ClipboardMode::Disabled,
            audio: AudioMode::Never,
            graphics,
            dynamic_resolution: false,
            reconnect: false,
            reconnect_per_minute: 0,
            swap_cmd_alt: false,
            wake_mac: None,
            keep_alive: false,
        }
    }

    fn key(down: bool) -> InputEvent {
        InputEvent::Key { keycode: 0, down }
    }

    #[test]
    fn clipboard_line_endings_are_normalized_without_doubling_crlf() {
        assert_eq!(
            normalize_clipboard_to_crlf("one\r\ntwo\nthree\rfour"),
            "one\r\ntwo\r\nthree\r\nfour"
        );
    }

    #[test]
    fn control_flags_do_not_consume_coalesced_mouse_markers() {
        let pending = PendingCommands::default();
        {
            let mut mouse = pending.mouse_move.lock().unwrap();
            mouse.value = Some((40, 50));
            mouse.queued = true;
        }
        pending.release_all_keys.store(true, Ordering::Release);

        let command = resolve_pending_command(
            SessionCommand::Input(vec![InputEvent::MouseMove { x: 1, y: 2 }]),
            &pending,
        );
        assert!(matches!(
            command,
            Some(SessionCommand::Input(events))
                if matches!(events.as_slice(), [InputEvent::MouseMove { x: 40, y: 50 }])
        ));
        assert!(pending.release_all_keys.load(Ordering::Acquire));
    }

    #[test]
    fn stale_control_markers_are_ignored() {
        let pending = PendingCommands::default();
        assert!(resolve_pending_command(SessionCommand::ReleaseAllKeys, &pending).is_none());
        assert!(resolve_pending_command(SessionCommand::Shutdown, &pending).is_none());
    }

    #[test]
    fn held_keys_are_counted_and_balanced() {
        // Two presses without releases keep the count up (keep-alive suppressed).
        let cmd = SessionCommand::Input(vec![key(true), key(true)]);
        assert_eq!(update_keys_down(&cmd, 0), 2);
        // The matching releases bring it back to zero.
        let cmd = SessionCommand::Input(vec![key(false), key(false)]);
        assert_eq!(update_keys_down(&cmd, 2), 0);
    }

    #[test]
    fn key_counter_never_underflows_and_full_release_zeroes_it() {
        // A stray release with nothing held saturates at zero, not u32::MAX.
        assert_eq!(
            update_keys_down(&SessionCommand::Input(vec![key(false)]), 0),
            0
        );
        // ReleaseAllKeys clears any stuck held-key state.
        assert_eq!(update_keys_down(&SessionCommand::ReleaseAllKeys, 3), 0);
    }

    #[test]
    fn non_key_input_leaves_the_held_counter_untouched() {
        let mouse = SessionCommand::Input(vec![InputEvent::MouseMove { x: 5, y: 5 }]);
        assert_eq!(update_keys_down(&mouse, 1), 1);
        assert_eq!(update_keys_down(&SessionCommand::Shutdown, 1), 1);
    }

    #[test]
    fn transport_compression_is_independent_of_graphics_mode() {
        for graphics in [GraphicsMode::Classic, GraphicsMode::Egfx] {
            let enabled = build_config(&test_session_config(graphics, true), false);
            assert_eq!(enabled.compression_type, Some(CompressionType::Rdp61));

            let disabled = build_config(&test_session_config(graphics, false), false);
            assert_eq!(disabled.compression_type, None);
        }
    }

    #[test]
    fn file_descriptors_split_wire_names_and_mark_directories() {
        use super::{to_file_descriptors, LocalClipFile};
        use ironrdp::cliprdr::pdu::ClipboardFileAttributes;

        let files = vec![
            LocalClipFile {
                path: "/tmp/report.pdf".into(),
                wire_name: "report.pdf".to_string(),
                size: 1234,
                is_dir: false,
            },
            LocalClipFile {
                path: "/tmp/project".into(),
                wire_name: "project".to_string(),
                size: 0,
                is_dir: true,
            },
            LocalClipFile {
                path: "/tmp/project/src/main.rs".into(),
                wire_name: "project\\src\\main.rs".to_string(),
                size: 7,
                is_dir: false,
            },
        ];
        let descriptors = to_file_descriptors(&files);

        assert_eq!(descriptors[0].name, "report.pdf");
        assert_eq!(descriptors[0].relative_path, None);
        assert_eq!(descriptors[0].file_size, Some(1234));

        assert_eq!(descriptors[1].name, "project");
        assert_eq!(
            descriptors[1].attributes,
            Some(ClipboardFileAttributes::DIRECTORY)
        );
        assert_eq!(descriptors[1].file_size, None);

        assert_eq!(descriptors[2].name, "main.rs");
        assert_eq!(
            descriptors[2].relative_path.as_deref(),
            Some("project\\src")
        );
    }

    #[test]
    fn collected_selection_walks_folders_with_relative_wire_names() {
        use super::collect_clipboard_files;

        let root = std::env::temp_dir().join(format!("rdp123-clip-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("folder/inner")).unwrap();
        std::fs::write(root.join("folder/a.txt"), b"aaa").unwrap();
        std::fs::write(root.join("folder/inner/b.txt"), b"bb").unwrap();

        let files = collect_clipboard_files(&[root.join("folder")]);
        let mut names: Vec<&str> = files.iter().map(|f| f.wire_name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "folder",
                "folder\\a.txt",
                "folder\\inner",
                "folder\\inner\\b.txt"
            ]
        );
        let a = files
            .iter()
            .find(|f| f.wire_name == "folder\\a.txt")
            .unwrap();
        assert_eq!(a.size, 3);
        assert!(!a.is_dir);

        let _ = std::fs::remove_dir_all(&root);
    }
}

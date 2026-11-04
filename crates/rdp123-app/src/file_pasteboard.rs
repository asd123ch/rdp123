//! Finder requests a file URL synchronously. A windowless copy of our signed
//! executable owns that pasteboard offer so slow downloads cannot block AppKit.
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};

use objc2::{
    define_class, msg_send,
    rc::{autoreleasepool, Retained},
    runtime::ProtocolObject,
    AnyThread, DefinedClass, MainThreadMarker,
};
use objc2_app_kit::{
    NSPasteboard, NSPasteboardContentsOptions, NSPasteboardItem, NSPasteboardItemDataProvider,
    NSPasteboardTypeFileURL, NSPasteboardWriting,
};
use objc2_foundation::{NSArray, NSDate, NSObject, NSObjectProtocol, NSRunLoop, NSString};
use rdp123_core::session::RemoteFileOffer;

pub const FLAG: &str = "--serve-file-offer";
pub const MARKER: &str = "ch.asd123.rdp123.remote-files";

pub struct Owner(Child);

impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn publish(offer: RemoteFileOffer, change: isize) -> std::io::Result<Owner> {
    let mut child = Owner(
        Command::new(std::env::current_exe()?)
            .args([FLAG, &change.to_string(), &offer.len().to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?,
    );
    let mut replies = child.0.stdin.take().unwrap();
    let requests = child.0.stdout.take().unwrap();
    std::thread::Builder::new()
        .name("file-pasteboard".into())
        .spawn(move || {
            for line in BufReader::new(requests).lines().map_while(Result::ok) {
                let Ok(index) = line.parse::<usize>() else {
                    break;
                };
                let url = offer.fetch(index).and_then(|path| {
                    if path.is_dir() {
                        url::Url::from_directory_path(path).ok()
                    } else {
                        url::Url::from_file_path(path).ok()
                    }
                });
                if writeln!(replies, "{}", url.as_ref().map_or("", |u| u.as_str())).is_err() {
                    break;
                }
            }
        })?;
    Ok(child)
}

struct Data {
    index: usize,
    replies: Arc<Mutex<mpsc::Receiver<String>>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = Data]
    struct Provider;
    unsafe impl NSObjectProtocol for Provider {}
    unsafe impl NSPasteboardItemDataProvider for Provider {
        #[unsafe(method(pasteboard:item:provideDataForType:))]
        fn provide(&self, _board: Option<&NSPasteboard>, item: &NSPasteboardItem, ty: &NSString) {
            let replies = self.ivars().replies.lock().unwrap();
            if writeln!(std::io::stdout().lock(), "{}", self.ivars().index).is_err() {
                return;
            }
            if let Ok(url) = replies.recv() {
                if !url.is_empty() {
                    item.setString_forType(&NSString::from_str(&url), ty);
                }
            }
        }
    }
);

pub fn serve(mut args: impl Iterator<Item = String>) -> Option<()> {
    let _mtm = MainThreadMarker::new()?;
    let expected: isize = args.next()?.parse().ok()?;
    let count: usize = args.next()?.parse().ok()?;
    if !(1..=100_000).contains(&count) {
        return None;
    }
    let board = match args.next() {
        Some(name) => NSPasteboard::pasteboardWithName(&NSString::from_str(&name)),
        None => NSPasteboard::generalPasteboard(),
    };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
        // The owner exited or closed its pipe. Do not retain stale clipboard offers.
        std::process::exit(0);
    });
    let replies = Arc::new(Mutex::new(rx));
    let types = NSArray::from_slice(&[unsafe { NSPasteboardTypeFileURL }]);
    let mut writers: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = Vec::new();
    let mut providers = Vec::new();
    for index in 0..count {
        let allocated = Provider::alloc().set_ivars(Data {
            index,
            replies: replies.clone(),
        });
        let provider: Retained<Provider> = unsafe { msg_send![super(allocated), init] };
        let item = NSPasteboardItem::new();
        if !item.setDataProvider_forTypes(ProtocolObject::from_ref(&*provider), &types) {
            return None;
        }
        item.setString_forType(&NSString::from_str("1"), &NSString::from_str(MARKER));
        providers.push(provider);
        writers.push(ProtocolObject::from_retained(item));
    }
    if board.changeCount() != expected {
        return None;
    }
    board.prepareForNewContentsWithOptions(NSPasteboardContentsOptions::CurrentHostOnly);
    if !board.writeObjects(&NSArray::from_retained_slice(&writers)) {
        return None;
    }
    let generation = board.changeCount();
    while board.changeCount() == generation {
        autoreleasepool(|_| {
            NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.2))
        });
    }
    Some(())
}

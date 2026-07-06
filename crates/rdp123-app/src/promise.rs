//! File promises for pasting remote (Windows) clipboard files into Finder.
//!
//! When the remote session copies files, one `NSFilePromiseProvider` per
//! top-level item goes onto the pasteboard. Finder redeems a promise by
//! calling `writePromiseToURL:` on a background operation queue; the delegate
//! asks the session to stream the item from the remote clipboard and blocks
//! that queue thread until the transfer finishes.

use std::sync::mpsc::sync_channel;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AnyThread, DefinedClass};
use objc2_app_kit::{NSFilePromiseProvider, NSFilePromiseProviderDelegate};
use objc2_foundation::{NSError, NSObject, NSObjectProtocol, NSOperationQueue, NSString, NSURL};

use rdp123_core::{RemoteClipItem, SessionCommand, SessionHandle};

/// Give a huge transfer up to an hour before the promise fails.
const FETCH_TIMEOUT: Duration = Duration::from_secs(3600);

pub struct PromiseDelegateIvars {
    handle: SessionHandle,
    item: RemoteClipItem,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "RDP123FilePromiseDelegate"]
    #[ivars = PromiseDelegateIvars]
    pub struct FilePromiseDelegate;

    unsafe impl NSObjectProtocol for FilePromiseDelegate {}

    unsafe impl NSFilePromiseProviderDelegate for FilePromiseDelegate {
        #[unsafe(method_id(filePromiseProvider:fileNameForType:))]
        fn file_name_for_type(
            &self,
            _provider: &NSFilePromiseProvider,
            _file_type: &NSString,
        ) -> Retained<NSString> {
            NSString::from_str(&self.ivars().item.name)
        }

        #[unsafe(method(filePromiseProvider:writePromiseToURL:completionHandler:))]
        fn write_promise_to_url(
            &self,
            _provider: &NSFilePromiseProvider,
            url: &NSURL,
            completion: &block2::DynBlock<dyn Fn(*mut NSError)>,
        ) {
            match self.fetch(url) {
                Ok(()) => completion.call((core::ptr::null_mut(),)),
                Err(message) => {
                    tracing::warn!(
                        "clipboard: paste of '{}' failed: {message}",
                        self.ivars().item.name
                    );
                    let error = unsafe {
                        NSError::errorWithDomain_code_userInfo(
                            &NSString::from_str("ch.asd123.rdp123"),
                            1,
                            None,
                        )
                    };
                    completion.call((Retained::as_ptr(&error).cast_mut(),));
                }
            }
        }

        // Without this the promise would be fulfilled on the MAIN queue,
        // deadlocking against the session events that must run there.
        #[unsafe(method_id(operationQueueForFilePromiseProvider:))]
        fn operation_queue(&self, _provider: &NSFilePromiseProvider) -> Retained<NSOperationQueue> {
            NSOperationQueue::new()
        }
    }
);

impl FilePromiseDelegate {
    fn new(handle: SessionHandle, item: RemoteClipItem) -> Retained<Self> {
        let this = Self::alloc().set_ivars(PromiseDelegateIvars { handle, item });
        unsafe { msg_send![super(this), init] }
    }

    /// Pull the promised item from the remote clipboard to `url`, blocking
    /// the (background) promise queue until the session reports completion.
    fn fetch(&self, url: &NSURL) -> Result<(), String> {
        let path = url
            .path()
            .ok_or_else(|| "the destination is not a file path".to_string())?
            .to_string();
        let (done_tx, done_rx) = sync_channel(1);
        self.ivars()
            .handle
            .command(SessionCommand::FetchRemoteClipItem {
                name: self.ivars().item.name.clone(),
                dest: std::path::PathBuf::from(path),
                done: done_tx,
            });
        match done_rx.recv_timeout(FETCH_TIMEOUT) {
            Ok(result) => result,
            Err(_) => Err("the transfer timed out or the session ended".to_string()),
        }
    }
}

/// Build a promise provider (plus its retained delegate — the provider only
/// holds it weakly) for one top-level remote clipboard item.
pub fn make_provider(
    handle: &SessionHandle,
    item: RemoteClipItem,
) -> (
    Retained<NSFilePromiseProvider>,
    Retained<FilePromiseDelegate>,
) {
    let file_type = if item.is_dir {
        "public.folder"
    } else {
        "public.data"
    };
    let delegate = FilePromiseDelegate::new(handle.clone(), item);
    let provider = NSFilePromiseProvider::initWithFileType_delegate(
        NSFilePromiseProvider::alloc(),
        &NSString::from_str(file_type),
        ProtocolObject::from_ref(&*delegate),
    );
    (provider, delegate)
}

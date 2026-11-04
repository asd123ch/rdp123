use objc2_app_kit::{NSPasteboard, NSPasteboardTypeFileURL};
use objc2_foundation::NSString;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn app_binary() -> std::path::PathBuf {
    std::env::var_os("RDP123_TEST_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_rdp123").into())
        .into()
}

#[test]
fn stale_file_offer_preserves_a_newer_local_copy() {
    let board = NSPasteboard::pasteboardWithUniqueName();
    let old = board.clearContents();
    board.clearContents();
    let ty = unsafe { objc2_app_kit::NSPasteboardTypeString };
    board.setString_forType(&NSString::from_str("newer local text"), ty);
    let mut child = Command::new(app_binary())
        .args([
            "--serve-file-offer",
            &old.to_string(),
            "1",
            &board.name().to_string(),
        ])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let _keep_pipe_open = child.stdin.take();
    assert!(child.wait().unwrap().success());
    assert_eq!(
        board.stringForType(ty).unwrap().to_string(),
        "newer local text"
    );
}

#[test]
fn named_pasteboard_serves_each_file_on_first_read_only() {
    let board = NSPasteboard::pasteboardWithUniqueName();
    let change = board.clearContents();
    let mut child = Command::new(app_binary())
        .args([
            "--serve-file-offer",
            &change.to_string(),
            "2",
            &board.name().to_string(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut replies = child.stdin.take().unwrap();
    let output = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    let responder = std::thread::spawn(move || {
        for line in BufReader::new(output).lines().map_while(Result::ok) {
            tx.send(line.clone()).unwrap();
            writeln!(replies, "file:///tmp/rdp123-private-test-{line}").unwrap();
        }
    });
    let marker = NSString::from_str("ch.asd123.rdp123.remote-files");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !board
        .types()
        .is_some_and(|types| types.containsObject(&marker))
    {
        assert!(Instant::now() < deadline, "no native file offer");
        std::thread::sleep(Duration::from_millis(20));
    }
    let items = board.pasteboardItems().unwrap();
    assert_eq!(items.len(), 2);
    assert!(
        rx.try_recv().is_err(),
        "inspecting types must not download any data"
    );
    for i in [1, 0] {
        let item = items.objectAtIndex(i);
        for _ in 0..2 {
            let value = unsafe { item.stringForType(NSPasteboardTypeFileURL) }.expect("file URL");
            assert_eq!(
                value.to_string(),
                format!("file:///tmp/rdp123-private-test-{i}")
            );
        }
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            i.to_string()
        );
        assert!(
            rx.try_recv().is_err(),
            "reading the same URL must not download it again"
        );
    }
    board.clearContents();
    let _ = child.kill();
    child.wait().unwrap();
    responder.join().unwrap();
}

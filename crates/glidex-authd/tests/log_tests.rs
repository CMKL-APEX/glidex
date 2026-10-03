//! The password never reaches the log. Its own test binary because it
//! installs a global tracing subscriber (server threads don't see a
//! thread-local one).

mod common;

use common::*;
use glidex_authd::proto::DEFAULT_SERVICE;
use std::io::Write;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn passwords_are_never_logged() {
    let capture = Capture::default();
    let writer = capture.clone();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .init();

    let h = start();
    let c = h.client();
    let wrong = "wrong-pw-7d1f3a";
    assert!(c.authenticate("alice", PASSWORD, DEFAULT_SERVICE).is_ok());
    let _ = c.authenticate("alice", wrong, DEFAULT_SERVICE);
    let _ = c.authenticate("mallory", wrong, DEFAULT_SERVICE);
    let _ = c.authenticate("nobody", wrong, DEFAULT_SERVICE);
    // A password typed into the user name field is not logged either.
    let _ = c.authenticate("Typed My Password", wrong, DEFAULT_SERVICE);
    for _ in 0..6 {
        let _ = c.authenticate("user1", wrong, DEFAULT_SERVICE);
    }

    let log = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(log.contains("alice"), "user names are logged: {log}");
    assert!(log.contains("success") && log.contains("denied") && log.contains("rate_limited"), "{log}");
    assert!(!log.contains(PASSWORD), "{log}");
    assert!(!log.contains(wrong), "{log}");
    assert!(!log.contains("Typed My Password"), "{log}");
}

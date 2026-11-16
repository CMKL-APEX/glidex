//! Stand-in programs for unit tests.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Write an executable script at `path` from a child process.
///
/// A file this process held open for writing can be inherited by a child
/// that another test thread forks at that moment, and running the script
/// then fails with `ETXTBSY` ("Text file busy") until that child execs.
/// Written by `sh`, the write descriptor never exists in this process.
pub fn write_script(path: &Path, body: &str) {
    let mut sh = Command::new("sh")
        .arg("-c")
        .arg("cat > \"$1\" && chmod 755 \"$1\"")
        .arg("sh")
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    sh.stdin.take().unwrap().write_all(body.as_bytes()).unwrap();
    let st = sh.wait().unwrap();
    assert!(st.success(), "writing {}: {st}", path.display());
}

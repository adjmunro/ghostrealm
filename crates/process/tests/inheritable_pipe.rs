#![cfg(unix)]
//! Alone in its own test binary: a child another test starts at the same
//! moment would inherit the pipe for a while.

use std::process::{Command, Stdio};

use ghostrealm_process::inherit_stdio_only;

/// Given a pipe that is still inheritable, as std's pipes are on macOS
/// between their creation and being marked close-on-exec;
/// when a long-lived child starts and the pipe's writer is closed;
/// then the reader sees the end of the pipe while the child runs.
#[test]
fn a_long_lived_child_holds_no_inheritable_pipe() {
    let mut fds = [0; 2];
    // SAFETY: `fds` holds the two descriptors `pipe` writes.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let [read, write] = fds;
    let mut cmd = Command::new("sleep");
    cmd.arg("30").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let mut child = inherit_stdio_only(&mut cmd).spawn().unwrap();
    // SAFETY: both are open descriptors this test owns.
    unsafe {
        libc::close(write);
        libc::fcntl(read, libc::F_SETFL, libc::O_NONBLOCK);
    }
    let mut byte = 0u8;
    // SAFETY: reads at most one byte into `byte`.
    let n = unsafe { libc::read(read, (&raw mut byte).cast(), 1) };
    let err = std::io::Error::last_os_error();
    let _ = child.kill();
    let _ = child.wait();
    // SAFETY: `read` is open and owned by this test.
    unsafe { libc::close(read) };
    assert_eq!(n, 0, "the child holds the pipe's writer: {err}");
}

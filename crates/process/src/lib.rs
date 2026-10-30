//! Starting a child process that outlives the moment it starts.

use std::process::Command;

/// Make the child `cmd` starts inherit no file descriptor but its stdin,
/// stdout and stderr: every descriptor above 2 is marked close-on-exec in the
/// child, before it execs.
///
/// Use it for every child that runs for long. On macOS std creates a pipe and
/// only then marks it close-on-exec (there is no `pipe2`), and other code
/// (sockets, `openpty`, libraries) can leave a descriptor inheritable too. A
/// child started while another thread holds such a descriptor keeps it open:
/// a short-lived `Command::output()` on that thread then waits for the end of
/// a pipe the long-lived child holds, until that child exits. Sanitising the
/// long-lived child covers stray descriptors from any source, which a lock
/// around the app's own spawns would not. It doesn't cover a short-lived child
/// that starts a long-lived one of its own: that child's spawn decides what
/// it passes on (git's `fsmonitor--daemon` keeps none).
///
/// Descriptors are marked, not closed: std reports a failed exec through a
/// close-on-exec pipe that must stay open until the exec. The `pre_exec` hook
/// makes std fork and exec instead of calling `posix_spawn`.
///
/// Only descriptors below the descriptor table size (at most 65536) are
/// marked.
pub fn inherit_stdio_only(cmd: &mut Command) -> &mut Command {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let end = fd_end();
        // SAFETY: the hook calls only `fcntl`, which is async-signal-safe,
        // and allocates nothing.
        unsafe {
            cmd.pre_exec(move || {
                for fd in 3..end {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
                        libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
                    }
                }
                Ok(())
            });
        }
    }
    cmd
}

/// One past the highest descriptor to mark: the descriptor table size (on
/// macOS the soft `RLIMIT_NOFILE` bounded by `kern.maxfilesperproc`), at most
/// 65536 so a raised limit doesn't stall the child.
#[cfg(unix)]
fn fd_end() -> libc::c_int {
    // SAFETY: no arguments; it only reads the process's limits.
    let size = unsafe { libc::getdtablesize() };
    const CAP: libc::c_int = 1 << 16;
    if size > 2 {
        size.min(CAP)
    } else {
        CAP
    }
}

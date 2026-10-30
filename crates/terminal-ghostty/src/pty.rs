//! The PTY and the child on it.
//!
//! Both ends are close-on-exec from the moment they open, so a child another
//! thread spawns meanwhile inherits neither. The child is spawned with std's
//! `Command` on the slave: its `pre_exec` makes only async-signal-safe calls
//! (it runs between fork and exec), and std's exec-error pipe stays open, so a
//! program that fails to exec is a spawn error.

use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result};

/// A program to run on a PTY, with its arguments, extra environment and
/// working directory; or the user's login shell.
#[derive(Clone, Debug)]
pub struct CommandBuilder {
    /// The program then its arguments; empty runs the login shell.
    argv: Vec<OsString>,
    /// Set on top of this process's environment.
    env: Vec<(OsString, OsString)>,
    /// Used when it is a directory; else the home directory.
    cwd: Option<PathBuf>,
}

impl CommandBuilder {
    /// Run `program`, looked up in `PATH` when it has no slash.
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self { argv: vec![program.as_ref().to_owned()], env: Vec::new(), cwd: None }
    }

    /// Run the user's shell as a login shell: `$SHELL` if it is executable,
    /// else the password database's shell if it is, else `/bin/sh`.
    pub fn new_default_prog() -> Self {
        Self { argv: Vec::new(), env: Vec::new(), cwd: None }
    }

    /// Append an argument. Panics on the login shell, which takes none.
    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        assert!(!self.argv.is_empty(), "the login shell takes no arguments");
        self.argv.push(arg.as_ref().to_owned());
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for arg in args {
            self.arg(arg);
        }
        self
    }

    /// Set `key` in the child's environment.
    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.env.push((key.as_ref().to_owned(), value.as_ref().to_owned()));
        self
    }

    /// Start in `dir` (the home directory if it isn't a directory).
    pub fn cwd(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.cwd = Some(dir.as_ref().to_owned());
        self
    }

    fn command(self) -> Command {
        self.command_with_shell(user_shell())
    }

    /// The command, with `shell` as the login shell and as `SHELL` (unless
    /// the caller set `SHELL`), so the child never sees a `$SHELL` that
    /// [`user_shell`] passed over.
    fn command_with_shell(self, shell: OsString) -> Command {
        let mut command = match self.argv.split_first() {
            None => {
                let mut c = Command::new(&shell);
                // A leading `-` in argv[0] makes a shell a login shell.
                let name = shell.as_bytes().rsplit(|b| *b == b'/').next().unwrap_or_default();
                c.arg0(OsString::from_vec([&b"-"[..], name].concat()));
                c
            }
            Some((program, args)) => {
                let mut c = Command::new(program);
                c.args(args);
                c
            }
        };
        command.current_dir(self.cwd.filter(|d| d.is_dir()).unwrap_or_else(home_dir));
        command.env("SHELL", &shell);
        command.envs(self.env);
        command
    }
}

/// The window size of a PTY, in cells and pixels.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WinSize {
    pub cols: u16,
    pub rows: u16,
    pub cell_w: u32,
    pub cell_h: u32,
}

impl WinSize {
    fn to_libc(self) -> libc::winsize {
        libc::winsize {
            ws_row: self.rows,
            ws_col: self.cols,
            ws_xpixel: (self.cols as u32 * self.cell_w) as u16,
            ws_ypixel: (self.rows as u32 * self.cell_h) as u16,
        }
    }
}

/// The master end of a PTY.
pub(crate) struct Pty {
    master: File,
}

impl Pty {
    /// Open a PTY of `size`: the master and its slave, both close-on-exec
    /// from the start (opened with `O_CLOEXEC`), so no child spawned
    /// meanwhile by another thread inherits either.
    pub(crate) fn open(size: WinSize) -> io::Result<(Pty, OwnedFd)> {
        let master = open_master()?;
        let fd = master.as_raw_fd();
        // SAFETY: grantpt and unlockpt only act on the master descriptor.
        if unsafe { libc::grantpt(fd) } != 0 || unsafe { libc::unlockpt(fd) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // std opens every file with O_CLOEXEC.
        let slave = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(slave_name(fd)?)?;
        let pty = Pty { master };
        pty.resize(size)?;
        Ok((pty, slave.into()))
    }

    pub(crate) fn resize(&self, size: WinSize) -> io::Result<()> {
        let ws = size.to_libc();
        // SAFETY: TIOCSWINSZ reads the winsize it is given.
        if unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// The PTY's foreground process group.
    pub(crate) fn foreground_group(&self) -> Option<i32> {
        // SAFETY: tcgetpgrp only reads the terminal's foreground group.
        let group = unsafe { libc::tcgetpgrp(self.master.as_raw_fd()) };
        (group > 0).then_some(group)
    }

    /// A reader of the child's output, and the handle that ends it.
    pub(crate) fn reader(&self) -> io::Result<(PtyReader, ReaderStop)> {
        // Close-on-exec: atomically on Linux (`SOCK_CLOEXEC`), just after
        // creation on macOS, where `inherit_stdio_only` keeps it from
        // long-lived children.
        let (stop_rx, stop_tx) = UnixStream::pair()?;
        #[cfg(all(target_vendor = "apple", not(any(target_os = "linux", target_os = "android", target_os = "freebsd", target_os = "dragonfly", target_os = "netbsd", target_os = "openbsd", target_os = "illumos", target_os = "solaris"))))]
        no_sigpipe(&stop_tx)?;
        Ok((PtyReader { master: self.master.try_clone()?, stop: stop_rx }, ReaderStop(stop_tx)))
    }

    pub(crate) fn writer(&self) -> io::Result<PtyWriter> {
        Ok(PtyWriter(self.master.try_clone()?))
    }
}

/// A new PTY master, close-on-exec: from `posix_openpt`, or from opening
/// `/dev/ptmx` where `posix_openpt` fails with `EINVAL` (POSIX leaves flags
/// other than `O_RDWR` and `O_NOCTTY` unspecified, so it may reject
/// `O_CLOEXEC`).
fn open_master() -> io::Result<File> {
    open_master_with(openpt_cloexec, open_ptmx)
}

/// `openpt`'s master, or `fallback`'s when `openpt` fails with `EINVAL`.
fn open_master_with(
    openpt: impl FnOnce() -> io::Result<File>,
    fallback: impl FnOnce() -> io::Result<File>,
) -> io::Result<File> {
    match openpt() {
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => fallback(),
        r => r,
    }
}

fn openpt_cloexec() -> io::Result<File> {
    // SAFETY: posix_openpt opens a descriptor and returns it or -1.
    let fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just opened and nothing else owns it.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// The master from `/dev/ptmx`; std opens every file with `O_CLOEXEC`.
fn open_ptmx() -> io::Result<File> {
    OpenOptions::new().read(true).write(true).custom_flags(libc::O_NOCTTY).open("/dev/ptmx")
}

/// The slave's path, from `ptsname_r` (`ptsname` shares a static buffer
/// between threads).
fn slave_name(master: libc::c_int) -> io::Result<PathBuf> {
    extern "C" {
        // In glibc, musl and macOS (10.13.4 on); the libc crate lacks it on macOS.
        fn ptsname_r(fd: libc::c_int, buf: *mut libc::c_char, buflen: libc::size_t) -> libc::c_int;
    }
    let mut buf = [0 as libc::c_char; 128];
    // SAFETY: ptsname_r writes at most `buf.len()` bytes, NUL included.
    let r = unsafe { ptsname_r(master, buf.as_mut_ptr(), buf.len()) };
    // glibc and musl return the error number; macOS returns -1 and sets errno.
    if r > 0 {
        return Err(io::Error::from_raw_os_error(r));
    }
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: on success the buffer holds a NUL-terminated name.
    let name = unsafe { CStr::from_ptr(buf.as_ptr()) };
    Ok(PathBuf::from(OsStr::from_bytes(name.to_bytes())))
}

/// Reads the child's output; EOF once the slave has closed or its
/// [`ReaderStop`] has dropped.
pub(crate) struct PtyReader {
    master: File,
    stop: UnixStream,
}

impl Read for PtyReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut fds = [
                libc::pollfd { fd: self.master.as_raw_fd(), events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: self.stop.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            ];
            // SAFETY: poll writes only the `revents` of the pollfds it is given.
            if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) } < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if fds[1].revents != 0 {
                return Ok(0);
            }
            if fds[0].revents != 0 {
                break;
            }
        }
        match self.master.read(buf) {
            // Linux reports a closed slave as EIO.
            Err(e) if e.raw_os_error() == Some(libc::EIO) => Ok(0),
            other => other,
        }
    }
}

/// Ends its [`PtyReader`] when dropped. The reader holds a copy of the master,
/// and the master's last close is what hangs up the child's terminal; blocked
/// in `read`, the reader would keep it open until the child next writes, which
/// a program that turned echo off may never do.
pub(crate) struct ReaderStop(UnixStream);

impl Drop for ReaderStop {
    fn drop(&mut self) {
        // A byte, not just the close: a copy of this end leaked to a child
        // would keep the close from showing. The reader has usually ended
        // already, so the send fails with EPIPE, without raising SIGPIPE
        // whatever its disposition (see [`STOP_SEND_FLAGS`]).
        // SAFETY: send reads only the one byte it is given.
        unsafe { libc::send(self.0.as_raw_fd(), [0u8].as_ptr().cast(), 1, STOP_SEND_FLAGS) };
    }
}

/// Flags for the stop byte's `send`: `MSG_NOSIGNAL` where the OS honours it;
/// on Apple, `SO_NOSIGPIPE` (set in `Pty::reader`) does that job instead.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd", target_os = "dragonfly", target_os = "netbsd", target_os = "openbsd", target_os = "illumos", target_os = "solaris"))]
const STOP_SEND_FLAGS: libc::c_int = libc::MSG_NOSIGNAL;
#[cfg(all(target_vendor = "apple", not(any(target_os = "linux", target_os = "android", target_os = "freebsd", target_os = "dragonfly", target_os = "netbsd", target_os = "openbsd", target_os = "illumos", target_os = "solaris"))))]
const STOP_SEND_FLAGS: libc::c_int = 0;
#[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android", target_os = "freebsd", target_os = "dragonfly", target_os = "netbsd", target_os = "openbsd", target_os = "illumos", target_os = "solaris")))]
compile_error!("no way to send the reader stop byte without raising SIGPIPE on this target");

/// Keep writes to `socket` from raising SIGPIPE.
#[cfg(all(target_vendor = "apple", not(any(target_os = "linux", target_os = "android", target_os = "freebsd", target_os = "dragonfly", target_os = "netbsd", target_os = "openbsd", target_os = "illumos", target_os = "solaris"))))]
fn no_sigpipe(socket: &UnixStream) -> io::Result<()> {
    let on: libc::c_int = 1;
    // SAFETY: setsockopt reads the int it is given.
    let r = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_NOSIGPIPE,
            (&on as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Writes the child's input. Dropping it sends a newline and the EOF
/// character, so a shell reading input ends as it would on Ctrl-D (saving its
/// history) rather than only by the hangup that follows.
pub(crate) struct PtyWriter(File);

impl Write for PtyWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl Drop for PtyWriter {
    fn drop(&mut self) {
        // SAFETY: tcgetattr writes only the zeroed termios it is given.
        let eof = unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            (libc::tcgetattr(self.0.as_raw_fd(), &mut t) == 0).then(|| t.c_cc[libc::VEOF])
        };
        // EOF counts only at the start of a line.
        if let Some(eof) = eof.filter(|c| *c != 0) {
            let _ = self.0.write_all(&[b'\n', eof]);
        }
    }
}

/// Spawn `cmd` on the PTY `slave` as a session leader with the PTY as its
/// controlling terminal, inheriting no descriptor but its stdio. The slave is
/// closed here once the child holds it, so the master sees EOF when the child
/// side closes.
pub(crate) fn spawn(slave: OwnedFd, cmd: CommandBuilder) -> Result<Child> {
    let mut command = cmd.command();
    let program = command.get_program().to_owned();
    command.stdin(Stdio::from(slave.try_clone()?));
    command.stdout(Stdio::from(slave.try_clone()?));
    command.stderr(Stdio::from(slave));
    // SAFETY: the hook calls only `signal`, `setsid` and `ioctl`, which are
    // async-signal-safe, and allocates nothing (an OS error holds a code).
    unsafe {
        command.pre_exec(|| {
            // Ignored dispositions survive exec; the shell starts with defaults.
            for signal in [
                libc::SIGCHLD,
                libc::SIGHUP,
                libc::SIGINT,
                libc::SIGQUIT,
                libc::SIGTERM,
                libc::SIGALRM,
            ] {
                libc::signal(signal, libc::SIG_DFL);
            }
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            // std has made the slave stdin by now. As controlling terminal it
            // sends the child SIGWINCH on resize and SIGHUP on close.
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    ghostrealm_process::inherit_stdio_only(&mut command);
    command.spawn().with_context(|| format!("spawn {program:?}"))
}

/// `$SHELL` if it is executable, else the password database's shell if it
/// is, else `/bin/sh`.
fn user_shell() -> OsString {
    resolve_shell(std::env::var_os("SHELL"))
}

/// `shell` if it is executable, else the password database's shell if it
/// is, else `/bin/sh`.
fn resolve_shell(shell: Option<OsString>) -> OsString {
    shell
        .filter(|s| executable(s))
        .or_else(|| passwd_field(|pw| pw.pw_shell).filter(|s| executable(s)))
        .unwrap_or_else(|| "/bin/sh".into())
}

/// `$HOME`, else the password database's home directory, else `/`.
fn home_dir() -> PathBuf {
    std::env::var_os("HOME").or_else(|| passwd_field(|pw| pw.pw_dir)).unwrap_or_else(|| "/".into()).into()
}

fn executable(path: &OsStr) -> bool {
    let Ok(path) = CString::new(path.as_bytes()) else {
        return false;
    };
    // SAFETY: access only reads the NUL-terminated path.
    unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 }
}

/// A field of the current user's password database entry.
fn passwd_field(field: impl Fn(&libc::passwd) -> *mut libc::c_char) -> Option<OsString> {
    let mut buf = vec![0 as libc::c_char; 1024];
    loop {
        // SAFETY: getpwuid_r fills `pw` with pointers into `buf`, which
        // outlives their use here, and sets `found` to `pw` or null.
        let (r, value) = unsafe {
            let mut pw: libc::passwd = std::mem::zeroed();
            let mut found: *mut libc::passwd = std::ptr::null_mut();
            let r = libc::getpwuid_r(libc::getuid(), &mut pw, buf.as_mut_ptr(), buf.len(), &mut found);
            let value = (r == 0 && !found.is_null())
                .then(|| field(&pw))
                .filter(|p| !p.is_null())
                .map(|p| OsStr::from_bytes(CStr::from_ptr(p).to_bytes()).to_owned());
            (r, value)
        };
        if r == libc::ERANGE && buf.len() < 1 << 16 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        return value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const SIZE: WinSize = WinSize { cols: 80, rows: 24, cell_w: 8, cell_h: 16 };

    fn cloexec(fd: libc::c_int) -> bool {
        // SAFETY: F_GETFD only reads the descriptor's flags.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        flags >= 0 && flags & libc::FD_CLOEXEC != 0
    }

    #[test]
    fn both_ends_open_close_on_exec() {
        let (pty, slave) = Pty::open(SIZE).unwrap();
        assert!(cloexec(pty.master.as_raw_fd()), "master");
        assert!(cloexec(slave.as_raw_fd()), "slave");
    }

    #[test]
    fn the_stop_pair_is_close_on_exec() {
        let (pty, _slave) = Pty::open(SIZE).unwrap();
        let (reader, stop) = pty.reader().unwrap();
        assert!(cloexec(reader.stop.as_raw_fd()), "reader end");
        assert!(cloexec(stop.0.as_raw_fd()), "stop end");
    }

    #[cfg(all(target_vendor = "apple", not(any(target_os = "linux", target_os = "android", target_os = "freebsd", target_os = "dragonfly", target_os = "netbsd", target_os = "openbsd", target_os = "illumos", target_os = "solaris"))))]
    #[test]
    fn the_stop_end_sets_no_sigpipe() {
        let (pty, _slave) = Pty::open(SIZE).unwrap();
        let (_reader, stop) = pty.reader().unwrap();
        let mut on: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: getsockopt writes at most `len` bytes into `on`.
        let r = unsafe {
            libc::getsockopt(
                stop.0.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                (&mut on as *mut libc::c_int).cast(),
                &mut len,
            )
        };
        assert_eq!(r, 0, "{}", io::Error::last_os_error());
        assert_ne!(on, 0);
    }

    /// Set, to the parent's pid, in the child process that runs
    /// [`stop_after_its_reader`].
    const SIGPIPE_CHILD: &str = "GHOSTREALM_TEST_SIGPIPE_CHILD";

    /// Run in a child process by
    /// [`a_stop_dropped_after_its_reader_raises_no_sigpipe`]; a no-op unless
    /// [`SIGPIPE_CHILD`] names this process's parent, since it sets SIGPIPE
    /// to `SIG_DFL` for the whole process.
    #[test]
    #[ignore = "run by a_stop_dropped_after_its_reader_raises_no_sigpipe"]
    fn stop_after_its_reader() {
        let parent = std::os::unix::process::parent_id().to_string();
        if std::env::var_os(SIGPIPE_CHILD).is_none_or(|v| v != *parent) {
            return;
        }
        // SAFETY: resets one signal's disposition in this child process.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
        let (pty, _slave) = Pty::open(SIZE).unwrap();
        let (reader, stop) = pty.reader().unwrap();
        drop(reader);
        drop(stop);
    }

    #[test]
    fn a_stop_dropped_after_its_reader_raises_no_sigpipe() {
        use std::os::unix::process::ExitStatusExt;
        let out = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "pty::tests::stop_after_its_reader", "--test-threads=1"])
            .env(SIGPIPE_CHILD, std::process::id().to_string())
            .output()
            .unwrap();
        assert_eq!(out.status.signal(), None, "killed by a signal");
        assert!(out.status.success(), "{out:?}");
        assert!(String::from_utf8_lossy(&out.stdout).contains("1 passed"), "{out:?}");
    }

    /// Read once from `reader` on another thread; its result arrives on the
    /// returned channel.
    fn read_on_a_thread(mut reader: PtyReader) -> std::sync::mpsc::Receiver<io::Result<Vec<u8>>> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            let _ = tx.send(reader.read(&mut buf).map(|n| buf[..n].to_vec()));
        });
        rx
    }

    /// Given a reader blocked on a PTY whose child side is open and silent,
    /// when its stop drops,
    /// then it reads EOF, so its copy of the master can close.
    #[test]
    fn a_blocked_reader_ends_when_its_stop_drops() {
        let (pty, _slave) = Pty::open(SIZE).unwrap();
        let (reader, stop) = pty.reader().unwrap();
        let rx = read_on_a_thread(reader);
        std::thread::sleep(Duration::from_millis(100));
        assert!(rx.try_recv().is_err(), "blocked while the PTY is silent");
        drop(stop);
        let read = rx.recv_timeout(Duration::from_secs(2)).expect("woken by the stop");
        assert_eq!(read.unwrap(), b"");
    }

    #[test]
    fn a_reader_reads_output_and_ends_when_the_slave_closes() {
        let (pty, slave) = Pty::open(SIZE).unwrap();
        let (reader, _stop) = pty.reader().unwrap();
        let mut slave = File::from(slave);
        slave.write_all(b"hi").unwrap();
        assert_eq!(read_on_a_thread(reader).recv_timeout(Duration::from_secs(2)).unwrap().unwrap(), b"hi");

        let (reader, _stop) = pty.reader().unwrap();
        drop(slave);
        let read = read_on_a_thread(reader).recv_timeout(Duration::from_secs(2)).expect("woken by the close");
        assert_eq!(read.unwrap(), b"");
    }

    #[test]
    fn the_ptmx_fallback_opens_a_close_on_exec_master() {
        let master = open_ptmx().unwrap();
        assert!(cloexec(master.as_raw_fd()));
        // SAFETY: grantpt and unlockpt only act on the master descriptor.
        assert_eq!(unsafe { libc::grantpt(master.as_raw_fd()) }, 0);
        assert_eq!(unsafe { libc::unlockpt(master.as_raw_fd()) }, 0);
        assert!(slave_name(master.as_raw_fd()).unwrap().starts_with("/dev"));
    }

    fn os_err(code: libc::c_int) -> io::Result<File> {
        Err(io::Error::from_raw_os_error(code))
    }

    #[test]
    fn open_master_falls_back_only_on_einval() {
        let fallback_called = std::cell::Cell::new(false);
        let fallback = || {
            fallback_called.set(true);
            File::open("/dev/null")
        };

        assert!(open_master_with(|| os_err(libc::EINVAL), fallback).is_ok());
        assert!(fallback_called.replace(false), "EINVAL opens /dev/ptmx");

        let err = open_master_with(|| os_err(libc::EMFILE), fallback).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EMFILE));
        assert!(!fallback_called.get(), "other errors are returned");

        assert!(open_master_with(|| File::open("/dev/null"), fallback).is_ok());
        assert!(!fallback_called.get(), "success keeps posix_openpt's master");
    }

    fn env_of(command: &Command, key: &str) -> Option<OsString> {
        command.get_envs().find(|(k, _)| *k == key).and_then(|(_, v)| v.map(OsStr::to_owned))
    }

    /// Given a `$SHELL` that isn't executable,
    /// when the login shell's command is built,
    /// then the child runs the fallback shell and sees it as `SHELL`.
    #[test]
    fn shell_is_the_resolved_shell_not_an_unusable_one() {
        let unusable = OsString::from("/nonexistent/shell");
        let shell = resolve_shell(Some(unusable.clone()));
        assert_ne!(shell, unusable);
        let command = CommandBuilder::new_default_prog().command_with_shell(shell.clone());
        assert_eq!(command.get_program(), shell);
        assert_eq!(env_of(&command, "SHELL"), Some(shell));
    }

    #[test]
    fn a_callers_shell_wins_over_the_resolved_one() {
        let mut builder = CommandBuilder::new("/usr/bin/env");
        builder.env("SHELL", "/bin/zsh");
        let command = builder.command_with_shell("/bin/sh".into());
        assert_eq!(env_of(&command, "SHELL").as_deref(), Some(OsStr::new("/bin/zsh")));
    }

    #[test]
    fn the_size_is_set_on_open() {
        let (pty, _slave) = Pty::open(SIZE).unwrap();
        // SAFETY: TIOCGWINSZ writes only the zeroed winsize it is given.
        let ws = unsafe {
            let mut ws: libc::winsize = std::mem::zeroed();
            assert_eq!(libc::ioctl(pty.master.as_raw_fd(), libc::TIOCGWINSZ as _, &mut ws), 0);
            ws
        };
        assert_eq!((ws.ws_col, ws.ws_row, ws.ws_xpixel, ws.ws_ypixel), (80, 24, 640, 384));
    }

    /// The number of the child's descriptors above stderr that are terminals.
    fn ttys_in_a_short_lived_child() -> usize {
        let script = r#"for f in /dev/fd/*; do n=${f##*/}; [ "$n" -gt 2 ] 2>/dev/null && [ -t "$n" ] && echo "$n"; done"#;
        let out = Command::new("/bin/sh").args(["-c", script]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).lines().count()
    }

    /// Given one thread opening PTYs and holding each a moment,
    /// when another runs short-lived children meanwhile,
    /// then no child holds a terminal it didn't already hold without them.
    #[test]
    fn a_concurrent_short_lived_child_inherits_no_pty() {
        let baseline = ttys_in_a_short_lived_child();
        let stop = Arc::new(AtomicBool::new(false));
        let opener = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let until = Instant::now() + Duration::from_secs(10);
                let mut opened = 0usize;
                while !stop.load(Ordering::Relaxed) && Instant::now() < until {
                    let held = Pty::open(SIZE).unwrap();
                    opened += 1;
                    std::thread::sleep(Duration::from_millis(1));
                    drop(held);
                }
                opened
            })
        };
        let until = Instant::now() + Duration::from_millis(1500);
        let mut runs = 0;
        let mut leaked = 0;
        while Instant::now() < until {
            leaked = leaked.max(ttys_in_a_short_lived_child());
            runs += 1;
        }
        stop.store(true, Ordering::Relaxed);
        let opened = opener.join().unwrap();
        assert!(runs > 10 && opened > 10, "too little overlap: {runs} children, {opened} PTYs");
        assert_eq!(leaked, baseline, "a child inherited a PTY descriptor");
    }
}

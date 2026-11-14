//! The agent channel in the window: a Unix socket per window, in a directory
//! only the person's user can enter. A connection's first line is
//! `{"op":"hello","token":"…"}` with a workspace's token
//! ([`crate::agent::TOKEN_ENV`]); every later line is a request, served for
//! that workspace's agent ([`Scope::Workspace`]) while the workspace is open,
//! with the reach its grant gives at that request ([`Grants`]).
//!
//! Each connection has a thread that reads a line, hands it to the UI thread
//! (which owns the app state) through a channel and the waker, and writes the
//! reply; a client that stalls blocks only its own thread. The UI thread
//! serves what is waiting ([`Server::drain`]) and replies to commands whose
//! file jobs finished ([`Server::settle`]); it does no socket I/O.
//!
//! Limits keep one workspace's agent from locking out the others: at most
//! [`MAX_CONNECTIONS`] connections at once, [`MAX_PER_WORKSPACE`] of them
//! past `hello` per workspace. The first line must arrive within
//! [`HELLO_TIMEOUT`] of connecting, else the connection is closed without a
//! reply; each later line within [`IDLE_TIMEOUT`] of the last reply, else the
//! connection is sent `{"ok":false,…,"closed":"idle"}` and closed. That line
//! is written only when no request was read since the last reply, so
//! [`super::client::Connection`] reopens and sends its request again.
//!
//! On macOS, binding first removes the sockets dead windows left in the
//! directory ([`sweep`]). With `XDG_RUNTIME_DIR` unset the directory also
//! holds the web host's `.bgra` frame files; the sweep matches only the
//! socket names [`Server::bind`] gives.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use ghostrealm_core::Registry;
use serde_json::{json, Value as Json};

use super::{serve, settled, Grants, Reply};
use crate::app_state::AppState;

/// Wakes the UI thread to serve what is waiting.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// Connections served at once, with or without a `hello`; more are refused,
/// so a flood of them costs a bounded number of threads. Eight workspaces at
/// their [`MAX_PER_WORKSPACE`] fill it.
const MAX_CONNECTIONS: usize = 64;
/// Connections past `hello` per workspace. An agent session holds one (the
/// MCP server) and `ghostrealm agent` one while it runs: room for several of
/// each in one workspace.
const MAX_PER_WORKSPACE: usize = 8;
/// How long a connection has to send its whole first line.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a connection may go without a request after a reply. An agent at
/// work asks every few seconds, and reconnecting is cheap.
const IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// The longest request line, newline included.
pub(crate) const MAX_LINE: usize = 1 << 20;
/// How long a reply may take to write before the connection is closed.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the accept thread looks up from waiting to see whether to stop.
const POLL: Duration = Duration::from_millis(250);
/// How often the accept thread checks the socket file is still there: a
/// system's temp-dir cleanup may delete a socket left idle for days.
const RECHECK: Duration = Duration::from_secs(60);

/// The reply to a `hello` whose token names no open workspace, and to a
/// first line that isn't a `hello`.
const REFUSED: &str = "token not valid (unknown, or its workspace closed)";
/// The reply to a `hello` past its workspace's [`MAX_PER_WORKSPACE`].
const WORKSPACE_FULL: &str = "too many connections for this workspace";

/// The server's timings; tests shorten them.
#[derive(Clone, Copy)]
struct Limits {
    /// How often the accept thread checks the socket file is still there.
    recheck: Duration,
    hello: Duration,
    idle: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self { recheck: RECHECK, hello: HELLO_TIMEOUT, idle: IDLE_TIMEOUT }
    }
}

/// The directory the window's socket goes in: `$XDG_RUNTIME_DIR` when set
/// and absolute, else the temp dir, then `ghostrealm-<uid>`.
pub fn dir(uid: u32) -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|p| p.is_absolute());
    base.unwrap_or_else(std::env::temp_dir).join(format!("ghostrealm-{uid}"))
}

/// This process's user.
pub fn uid() -> u32 {
    // SAFETY: getuid has no preconditions and can't fail.
    unsafe { libc::getuid() }
}

/// Serves the agent channel at a socket until dropped or [closed](Self::close).
pub struct Server {
    path: PathBuf,
    shared: Arc<Shared>,
    requests: mpsc::Receiver<Incoming>,
    waker: Waker,
    parked: Vec<Parked>,
    closed: bool,
}

/// What the accept and connection threads share with the server.
struct Shared {
    stop: AtomicBool,
    /// `(dev, ino)` of the socket file as bound: only that file is removed.
    bound: Mutex<Option<(u64, u64)>>,
    /// Each connection served, by id.
    conns: Mutex<HashMap<u64, Conn>>,
}

/// A connection served.
struct Conn {
    /// A handle on its stream, to shut it down on close.
    stream: UnixStream,
    /// The workspace its `hello` named, once accepted.
    workspace: Option<u64>,
}

/// A line from a connection, for the UI thread: `None` for its `hello`.
struct Incoming {
    token: String,
    line: Option<String>,
    reply: mpsc::Sender<Out>,
}

/// A reply line, and whether the connection closes after it.
struct Out {
    json: Json,
    close: bool,
}

/// A command's reply waiting for its file jobs.
struct Parked {
    since: u64,
    until: u64,
    message: Option<String>,
    reply: mpsc::Sender<Out>,
}

impl Server {
    /// Make (or check) `dir` and serve a new socket in it. `uid` must own
    /// `dir`, a directory (not a symlink) no other user may enter; otherwise
    /// nothing is bound, and the error says why.
    pub fn bind(dir: &Path, uid: u32, waker: Waker) -> io::Result<Self> {
        Self::bind_with(dir, uid, waker, Limits::default())
    }

    fn bind_with(dir: &Path, uid: u32, waker: Waker, limits: Limits) -> io::Result<Self> {
        checked_dir(dir, uid)?;
        // Off elsewhere until its tests run there (TODO.md).
        if cfg!(target_os = "macos") {
            sweep(dir, uid, running);
        }
        // A new name each time: nothing there is reused or removed first.
        let path = dir.join(format!("{}-{}.sock", std::process::id(), super::random_hex(4)?));
        fits_sun_path(&path)?;
        let listener = UnixListener::bind(&path)?;
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            bound: Mutex::new(identity(&path).ok()),
            conns: Mutex::new(HashMap::new()),
        });
        let (send, requests) = mpsc::channel();
        let accept = Accept { path: path.clone(), uid, shared: shared.clone(), send, waker: waker.clone(), limits };
        std::thread::Builder::new().name("agent-accept".into()).spawn(move || accept.run(listener))?;
        Ok(Self { path, shared, requests, waker, parked: Vec::new(), closed: false })
    }

    /// The socket's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Serve the requests waiting, each with the reach `grants` gives its
    /// workspace, for about `budget` at most; the loop is woken again for any
    /// left. Returns whether any was served.
    pub fn drain(&mut self, state: &mut AppState, registry: &mut Registry<AppState>, grants: &Grants, budget: Duration) -> bool {
        let start = Instant::now();
        let mut any = false;
        while let Ok(req) = self.requests.try_recv() {
            any = true;
            self.answer(req, state, registry, grants);
            if start.elapsed() >= budget {
                (self.waker)();
                break;
            }
        }
        any
    }

    fn answer(&mut self, req: Incoming, state: &mut AppState, registry: &mut Registry<AppState>, grants: &Grants) {
        let Some(vt) = state.agent_token_workspace(&req.token) else {
            let _ = req.reply.send(Out { json: json!({ "ok": false, "error": REFUSED }), close: true });
            return;
        };
        let Some(line) = req.line else {
            let _ = req.reply.send(Out { json: json!({ "ok": true, "workspace": vt.0 }), close: false });
            return;
        };
        match serve(&line, grants.scope(vt), state, registry) {
            Reply::Done(json) => {
                let _ = req.reply.send(Out { json, close: false });
            }
            Reply::Jobs { since, until, message } => self.parked.push(Parked { since, until, message, reply: req.reply }),
        }
    }

    /// Reply to the commands whose file jobs are done, taking those jobs'
    /// reports: the window toasts the rest, so this runs before it does.
    /// Returns whether any replied.
    pub fn settle(&mut self, state: &mut AppState) -> bool {
        if self.parked.is_empty() {
            return false;
        }
        state.poll_file_jobs();
        let before = self.parked.len();
        self.parked.retain(|p| match settled(state, p.since, p.until, &p.message) {
            Some(json) => {
                let _ = p.reply.send(Out { json, close: false });
                false
            }
            None => true,
        });
        self.parked.len() != before
    }

    /// Stop serving: the accept thread ends within a quarter second, every
    /// connection is shut down, and the socket file is removed if it is still
    /// the one bound. No thread is joined: one may be blocked on a client.
    /// AppKit's terminate may exit without dropping the window, so the window
    /// calls this as it exits.
    pub fn close(&mut self) {
        if std::mem::replace(&mut self.closed, true) {
            return;
        }
        self.shared.stop.store(true, Ordering::Release);
        for (_, conn) in lock(&self.shared.conns).drain() {
            let _ = conn.stream.shutdown(std::net::Shutdown::Both);
        }
        self.parked.clear();
        while self.requests.try_recv().is_ok() {}
        remove_if_bound(&self.path, *lock(&self.shared.bound), identity(&self.path));
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.close();
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Create `dir` (mode 0700) unless it exists, then check it: a directory,
/// not a symlink, owned by `uid`, with no group or other permission bits.
/// Never changes or removes it.
fn checked_dir(dir: &Path, uid: u32) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    let meta = fs::symlink_metadata(dir)?;
    let refuse = |why: &str| Err(io::Error::other(format!("{}: {why}", dir.display())));
    if !meta.file_type().is_dir() {
        return refuse("not a directory");
    }
    if meta.uid() != uid {
        return refuse("owned by another user");
    }
    if meta.mode() & 0o077 != 0 {
        return refuse("other users may enter it");
    }
    Ok(())
}

/// `(dev, ino)` of the socket at `path` itself (a symlink isn't followed);
/// an error for anything but a socket, so a file that reuses a freed inode
/// never matches.
fn identity(path: &Path) -> io::Result<(u64, u64)> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_socket() {
        return Err(io::Error::other(format!("{}: not a socket", path.display())));
    }
    Ok((meta.dev(), meta.ino()))
}

/// Remove `path` if it is still the socket bound: `now`, its
/// [`identity`] read now, is `bound`. Anything else keeps it: no record of
/// the bind, an error reading it, or another file (or a symlink) in its
/// place.
fn remove_if_bound(path: &Path, bound: Option<(u64, u64)>, now: io::Result<(u64, u64)>) {
    if bound.is_some_and(|ours| now.is_ok_and(|now| now == ours)) {
        let _ = fs::remove_file(path);
    }
}

/// Fail, naming the remedy, if `path` is too long for a socket address
/// (`sun_path`, its NUL included); std's own error names neither.
fn fits_sun_path(path: &Path) -> io::Result<()> {
    // SAFETY: sockaddr_un is plain data, valid zeroed.
    let addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let max = addr.sun_path.len() - 1;
    let len = path.as_os_str().len();
    if len > max {
        return Err(io::Error::other(format!(
            "socket path {} is {len} bytes, over the {max} this system allows; set XDG_RUNTIME_DIR or TMPDIR to a shorter directory",
            path.display()
        )));
    }
    Ok(())
}

/// Remove the sockets dead windows left in `dir`, which [`checked_dir`]
/// passed. An entry goes only if each check passes, in order: its name is
/// one [`Server::bind`] gives; it is a socket (not followed if a symlink)
/// owned by `uid`; `running` says its pid is gone; and connecting to it is
/// refused, so nothing listens. Any error keeps it. A live window's socket
/// is kept by its pid, so a refusal from a full backlog removes nothing.
fn sweep(dir: &Path, uid: u32, running: impl Fn(u32) -> bool) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(socket_pid) else { continue };
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else { continue };
        if !meta.file_type().is_socket() || meta.uid() != uid || running(pid) {
            continue;
        }
        if UnixStream::connect(&path).is_err_and(|e| e.raw_os_error() == Some(libc::ECONNREFUSED)) {
            let _ = fs::remove_file(&path);
        }
    }
}

/// The pid in a socket name [`Server::bind`] gives, exactly:
/// `<pid>-<8 lowercase hex digits>.sock`, the pid in `1..=i32::MAX` and
/// written as `to_string` writes it.
fn socket_pid(name: &str) -> Option<u32> {
    let (digits, rest) = name.split_once('-')?;
    let hex = rest.strip_suffix(".sock")?;
    let pid: u32 = digits.parse().ok()?;
    let hex_ok = hex.len() == 8 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    (hex_ok && pid.to_string() == digits && (1..=i32::MAX as u32).contains(&pid)).then_some(pid)
}

/// Whether process `pid` may be running: anything but "no such process"
/// (`ESRCH`) says it may, a process we may not signal included.
fn running(pid: u32) -> bool {
    // SAFETY: signal 0 sends nothing; it only checks the pid. `pid` is
    // positive (`socket_pid`), so no process group is addressed.
    let sent = unsafe { libc::kill(pid as libc::pid_t, 0) };
    sent == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// The accept thread.
struct Accept {
    path: PathBuf,
    uid: u32,
    shared: Arc<Shared>,
    send: mpsc::Sender<Incoming>,
    waker: Waker,
    limits: Limits,
}

impl Accept {
    fn run(self, mut listener: UnixListener) {
        let _ = listener.set_nonblocking(true);
        let mut next = 0u64;
        let mut checked = Instant::now();
        while !self.shared.stop.load(Ordering::Acquire) {
            if checked.elapsed() >= self.limits.recheck {
                checked = Instant::now();
                if let Some(again) = self.bind_again_if_gone() {
                    listener = again;
                }
            }
            if !readable(&listener, POLL) {
                continue;
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    next += 1;
                    self.admit(stream, next);
                }
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {}
                // Out of descriptors, say: wait rather than spin.
                Err(_) => std::thread::sleep(POLL),
            }
        }
    }

    /// Bind the socket's path again if its file is gone (a temp-dir cleanup):
    /// the window keeps serving the path its terminals were given. A file in
    /// its place is left alone.
    fn bind_again_if_gone(&self) -> Option<UnixListener> {
        match fs::symlink_metadata(&self.path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            _ => return None,
        }
        checked_dir(self.path.parent()?, self.uid).ok()?;
        let listener = UnixListener::bind(&self.path).ok()?;
        let _ = listener.set_nonblocking(true);
        *lock(&self.shared.bound) = identity(&self.path).ok();
        Some(listener)
    }

    fn admit(&self, stream: UnixStream, id: u64) {
        // An accepted socket may inherit the listener's non-blocking mode.
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
        let mut conns = lock(&self.shared.conns);
        let (Ok(handle), true) = (stream.try_clone(), conns.len() < MAX_CONNECTIONS) else {
            drop(conns);
            let _ = write_line(&mut &stream, &json!({ "ok": false, "error": "too many connections" }));
            return;
        };
        conns.insert(id, Conn { stream: handle, workspace: None });
        drop(conns);
        let conn = Connection {
            id,
            shared: self.shared.clone(),
            send: self.send.clone(),
            waker: self.waker.clone(),
            limits: self.limits,
        };
        if std::thread::Builder::new().name("agent-conn".into()).spawn(move || conn.run(stream)).is_err() {
            lock(&self.shared.conns).remove(&id);
        }
    }
}

/// Whether `listener` has a connection waiting, waiting up to `timeout`.
fn readable(listener: &UnixListener, timeout: Duration) -> bool {
    let mut fd = libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    // SAFETY: one valid pollfd, for the duration of the call.
    let n = unsafe { libc::poll(&mut fd, 1, timeout.as_millis() as libc::c_int) };
    n > 0
}

/// A connection's thread.
struct Connection {
    id: u64,
    shared: Arc<Shared>,
    send: mpsc::Sender<Incoming>,
    waker: Waker,
    limits: Limits,
}

/// A line read from a connection.
enum Read {
    Line(String),
    TooLong,
    End,
    /// Its deadline passed first.
    Idle,
}

impl Connection {
    fn run(self, stream: UnixStream) {
        let mut writer = &stream;
        let mut reader = BufReader::new(Timed { stream: &stream, deadline: Instant::now() + self.limits.hello });
        let refuse = |writer: &mut &UnixStream, error: &str| {
            let _ = write_line(writer, &json!({ "ok": false, "error": error }));
        };
        let token = match read_line(&mut reader) {
            Read::Line(line) => match hello_token(&line) {
                Some(token) => token,
                None => return refuse(&mut writer, REFUSED),
            },
            Read::TooLong => return refuse(&mut writer, "line too long"),
            Read::End | Read::Idle => return,
        };
        let mut line = None;
        loop {
            let hello = line.is_none();
            let (reply, replied) = mpsc::channel();
            if self.send.send(Incoming { token: token.clone(), line: line.take(), reply }).is_err() {
                return;
            }
            (self.waker)();
            // The server dropped the request: it is closing.
            let Ok(mut out) = replied.recv() else { return };
            if hello {
                out = self.claim(out);
            }
            if write_line(&mut writer, &out.json).is_err() || out.close {
                return;
            }
            reader.get_mut().deadline = Instant::now() + self.limits.idle;
            line = match read_line(&mut reader) {
                Read::Line(l) => Some(l),
                Read::TooLong => return refuse(&mut writer, "line too long"),
                Read::End => return,
                Read::Idle => {
                    // Out of the count first, so a client that reopens on
                    // reading the line finds its slot free.
                    lock(&self.shared.conns).remove(&self.id);
                    let error = format!("closed after {:?} without a request", self.limits.idle);
                    let _ = write_line(&mut writer, &json!({ "ok": false, "error": error, "closed": "idle" }));
                    return;
                }
            };
        }
    }

    /// Count this connection for the workspace an accepted `hello` (`out`)
    /// names, or refuse it once that workspace has [`MAX_PER_WORKSPACE`].
    fn claim(&self, out: Out) -> Out {
        let Some(vt) = out.json.get("workspace").and_then(Json::as_u64).filter(|_| out.json["ok"] == json!(true)) else {
            return out;
        };
        let mut conns = lock(&self.shared.conns);
        if conns.values().filter(|c| c.workspace == Some(vt)).count() >= MAX_PER_WORKSPACE {
            return Out { json: json!({ "ok": false, "error": WORKSPACE_FULL }), close: true };
        }
        if let Some(conn) = conns.get_mut(&self.id) {
            conn.workspace = Some(vt);
        }
        out
    }
}

/// A connection's stream read against a deadline for the whole line, not
/// per read: a client trickling a byte at a time still runs out of time.
struct Timed<'a> {
    stream: &'a UnixStream,
    deadline: Instant,
}

impl io::Read for Timed<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        // macOS refuses it (EINVAL) once the client has closed; the read
        // then returns what is buffered and the end without blocking.
        let _ = self.stream.set_read_timeout(Some(left));
        io::Read::read(&mut &*self.stream, buf)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        lock(&self.shared.conns).remove(&self.id);
    }
}

/// The next non-empty line, at most [`MAX_LINE`] long.
fn read_line(reader: &mut impl BufRead) -> Read {
    loop {
        let mut buf = Vec::new();
        match io::Read::take(&mut *reader, MAX_LINE as u64 + 1).read_until(b'\n', &mut buf) {
            Err(e) if matches!(e.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) => return Read::Idle,
            Ok(0) | Err(_) => return Read::End,
            Ok(_) if buf.len() > MAX_LINE => return Read::TooLong,
            Ok(_) => {
                let line = String::from_utf8_lossy(&buf).trim().to_string();
                if !line.is_empty() {
                    return Read::Line(line);
                }
            }
        }
    }
}

/// The token of a `hello` line, `None` for any other line.
fn hello_token(line: &str) -> Option<String> {
    let req: Json = serde_json::from_str(line).ok()?;
    (req.get("op")? == "hello").then(|| req.get("token").and_then(Json::as_str).unwrap_or("").to_string())
}

fn write_line(writer: &mut impl Write, json: &Json) -> io::Result<()> {
    let mut line = serde_json::to_vec(json)?;
    line.push(b'\n');
    writer.write_all(&line)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::build_registry;
    use crate::plugin::testing::TmpDir;
    use ghostrealm_core::VtabId;
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;

    /// The window's side: the app and its server, served on the test thread.
    struct Ui {
        state: AppState,
        registry: Registry<AppState>,
        server: Server,
        grants: Grants,
    }

    impl Ui {
        fn new(dir: &Path) -> Self {
            Self::with(dir, Limits::default())
        }

        fn with(dir: &Path, limits: Limits) -> Self {
            let server = Server::bind_with(dir, uid(), Arc::new(|| {}), limits).unwrap();
            let mut state = AppState::new().with_shell_line("sleep 5");
            state.set_agent_socket(server.path().to_path_buf());
            Self { state, registry: build_registry(), server, grants: Grants::default() }
        }

        fn serve(&mut self) {
            self.server.drain(&mut self.state, &mut self.registry, &self.grants, Duration::from_millis(50));
            self.server.settle(&mut self.state);
        }

        fn token(&self, vt: VtabId) -> String {
            self.state.agent_token(vt).expect("a token")
        }
    }

    struct Client {
        stream: UnixStream,
        buf: Vec<u8>,
    }

    impl Client {
        fn connect(path: &Path) -> Self {
            let stream = UnixStream::connect(path).unwrap();
            stream.set_read_timeout(Some(Duration::from_millis(10))).unwrap();
            Self { stream, buf: Vec::new() }
        }

        fn send(&mut self, line: &str) {
            writeln!(self.stream, "{line}").unwrap();
        }

        /// The next reply, serving the window meanwhile; `None` once the
        /// server closed the connection.
        fn recv(&mut self, ui: &mut Ui) -> Option<Json> {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(i) = self.buf.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = self.buf.drain(..=i).collect();
                    return Some(serde_json::from_slice(&line).unwrap());
                }
                ui.serve();
                let mut chunk = [0u8; 4096];
                match self.stream.read(&mut chunk) {
                    Ok(0) => return None,
                    Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                    Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
                    Err(_) => return None,
                }
                assert!(Instant::now() < deadline, "no reply");
            }
        }

        fn hello_as(&mut self, ui: &mut Ui, vt: VtabId) -> Json {
            let token = ui.token(vt);
            self.hello(ui, &token)
        }

        fn hello(&mut self, ui: &mut Ui, token: &str) -> Json {
            self.send(&json!({ "op": "hello", "token": token }).to_string());
            self.recv(ui).expect("a reply to hello")
        }
    }

    fn refused(reply: Option<Json>) -> bool {
        reply == Some(json!({ "ok": false, "error": REFUSED }))
    }

    /// Given the window's socket;
    /// when one client's first line isn't a `hello`, and another's `hello`
    /// has a token no workspace has;
    /// then each is refused and hung up on.
    #[test]
    fn a_connection_without_a_workspaces_token_is_refused_and_closed() {
        let dir = TmpDir::new("sock-hello");
        let mut ui = Ui::new(&dir.join("s"));
        ui.state.new_vtab().unwrap();
        let mut c = Client::connect(ui.server.path());
        c.send(r#"{"op":"get","what":"tree"}"#);
        assert!(refused(c.recv(&mut ui)));
        assert_eq!(c.recv(&mut ui), None);
        let mut c = Client::connect(ui.server.path());
        let reply = c.hello(&mut ui, &"0".repeat(64));
        assert!(refused(Some(reply)));
        assert_eq!(c.recv(&mut ui), None);
    }

    /// Given workspaces A and B, B active, each with a terminal;
    /// when a client says `hello` with A's token, then reads the tree and
    /// types into B's terminal;
    /// then it is A's: the tree is A's alone, and B's terminal is unknown.
    #[test]
    fn a_workspaces_token_reaches_that_workspace_alone() {
        let dir = TmpDir::new("sock-scope");
        let mut ui = Ui::new(&dir.join("s"));
        let a = ui.state.new_vtab().unwrap();
        let b = ui.state.new_vtab().unwrap();
        let b_term = ui.state.focused_surface().unwrap();
        let mut c = Client::connect(ui.server.path());
        assert_eq!(c.hello_as(&mut ui, a), json!({ "ok": true, "workspace": a.0 }));
        c.send(r#"{"op":"get","what":"tree"}"#);
        let tree = c.recv(&mut ui).unwrap()["tree"].as_str().unwrap().to_string();
        assert!(tree.starts_with(&format!("vtab {} ", a.0)) && !tree.contains(&format!("vtab {} ", b.0)), "{tree}");
        c.send(&format!(r#"{{"op":"input","id":{},"text":"x"}}"#, b_term.0));
        assert_eq!(c.recv(&mut ui), Some(json!({ "ok": false, "error": format!("unknown surface {}", b_term.0) })));
    }

    /// Given a client connected with workspace A's token;
    /// when A closes;
    /// then its next request is refused and it is hung up on, and a new
    /// `hello` with A's token is refused.
    #[test]
    fn a_closed_workspaces_token_is_refused() {
        let dir = TmpDir::new("sock-closed");
        let mut ui = Ui::new(&dir.join("s"));
        let a = ui.state.new_vtab().unwrap();
        ui.state.new_vtab().unwrap();
        let token = ui.token(a);
        let mut c = Client::connect(ui.server.path());
        assert_eq!(c.hello(&mut ui, &token)["ok"], json!(true));
        ui.state.close_vtab(a);
        c.send(r#"{"op":"pump"}"#);
        assert!(refused(c.recv(&mut ui)));
        assert_eq!(c.recv(&mut ui), None);
        let mut c = Client::connect(ui.server.path());
        assert!(refused(Some(c.hello(&mut ui, &token))));
    }

    /// Given a client stalled mid-line and another that sends requests and
    /// never reads its replies;
    /// when a third says `hello` and asks for the tree, twice;
    /// then it gets each reply at once.
    #[test]
    fn a_stalled_client_holds_up_no_one() {
        let dir = TmpDir::new("sock-stall");
        let mut ui = Ui::new(&dir.join("s"));
        let a = ui.state.new_vtab().unwrap();
        let token = ui.token(a);
        let mut stalled = Client::connect(ui.server.path());
        write!(stalled.stream, r#"{{"op":"hel"#).unwrap();
        let mut deaf = UnixStream::connect(ui.server.path()).unwrap();
        writeln!(deaf, "{}", json!({ "op": "hello", "token": token })).unwrap();
        let flood = std::thread::spawn(move || {
            for _ in 0..10_000 {
                if writeln!(deaf, r#"{{"op":"get","what":"tree"}}"#).is_err() {
                    break;
                }
            }
            deaf
        });
        let mut c = Client::connect(ui.server.path());
        assert_eq!(c.hello(&mut ui, &token)["ok"], json!(true));
        for _ in 0..2 {
            let start = Instant::now();
            c.send(r#"{"op":"get","what":"tree"}"#);
            assert_eq!(c.recv(&mut ui).unwrap()["ok"], json!(true));
            assert!(start.elapsed() < Duration::from_secs(2), "{:?}", start.elapsed());
            std::thread::sleep(Duration::from_millis(100));
        }
        drop(ui);
        let _ = flood.join();
    }

    /// Given a client past `hello`;
    /// when it sends a line over the limit;
    /// then it is told so and hung up on.
    #[test]
    fn an_over_long_line_is_refused_and_closed() {
        let dir = TmpDir::new("sock-long");
        let mut ui = Ui::new(&dir.join("s"));
        let a = ui.state.new_vtab().unwrap();
        let mut c = Client::connect(ui.server.path());
        assert_eq!(c.hello_as(&mut ui, a)["ok"], json!(true));
        c.send(&"x".repeat(MAX_LINE + 1));
        assert_eq!(c.recv(&mut ui), Some(json!({ "ok": false, "error": "line too long" })));
        assert_eq!(c.recv(&mut ui), None);
    }

    /// Wait up to 5 s for `done`.
    fn until(mut done: impl FnMut() -> bool, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Given as many connections as are served at once, none past `hello`;
    /// when one more connects;
    /// then it is refused and hung up on.
    #[test]
    fn a_connection_past_the_limit_is_refused() {
        let dir = TmpDir::new("sock-limit");
        let mut ui = Ui::new(&dir.join("s"));
        let _held: Vec<Client> = (0..MAX_CONNECTIONS).map(|_| Client::connect(ui.server.path())).collect();
        until(|| lock(&ui.server.shared.conns).len() == MAX_CONNECTIONS, "all admitted");
        let mut c = Client::connect(ui.server.path());
        assert_eq!(c.recv(&mut ui), Some(json!({ "ok": false, "error": "too many connections" })));
        assert_eq!(c.recv(&mut ui), None);
    }

    /// Given workspace A with as many connections past `hello` as it may
    /// have, and workspace B;
    /// when another connects with A's token, one with B's, and then one of
    /// A's closes and another connects with A's;
    /// then the first is refused and hung up on, B's is served, and the last
    /// is served once the closed one is gone.
    #[test]
    fn a_workspace_past_its_connection_cap_is_refused_and_others_are_not() {
        let dir = TmpDir::new("sock-cap");
        let mut ui = Ui::new(&dir.join("s"));
        let a = ui.state.new_vtab().unwrap();
        let b = ui.state.new_vtab().unwrap();
        let mut held: Vec<Client> = (0..MAX_PER_WORKSPACE)
            .map(|_| {
                let mut c = Client::connect(ui.server.path());
                assert_eq!(c.hello_as(&mut ui, a)["ok"], json!(true));
                c
            })
            .collect();
        let mut c = Client::connect(ui.server.path());
        assert_eq!(c.hello_as(&mut ui, a), json!({ "ok": false, "error": WORKSPACE_FULL }));
        assert_eq!(c.recv(&mut ui), None);
        let mut theirs = Client::connect(ui.server.path());
        assert_eq!(theirs.hello_as(&mut ui, b)["ok"], json!(true));
        drop(held.pop());
        let of_a = |ui: &Ui| lock(&ui.server.shared.conns).values().filter(|c| c.workspace == Some(a.0)).count();
        until(|| of_a(&ui) < MAX_PER_WORKSPACE, "the closed connection is no longer counted");
        let mut c = Client::connect(ui.server.path());
        assert_eq!(c.hello_as(&mut ui, a)["ok"], json!(true));
    }

    /// Given a server whose first-line deadline is 200 ms;
    /// when one client says nothing, and another sends its `hello` a byte
    /// every 50 ms;
    /// then each is hung up on within 2 s, with no reply.
    #[test]
    fn a_connection_without_a_hello_is_closed_at_its_deadline() {
        let dir = TmpDir::new("sock-hdl");
        let limits = Limits { hello: Duration::from_millis(200), ..Limits::default() };
        let mut ui = Ui::with(&dir.join("s"), limits);
        let a = ui.state.new_vtab().unwrap();
        let hello = json!({ "op": "hello", "token": ui.token(a) }).to_string();
        let start = Instant::now();
        let mut silent = Client::connect(ui.server.path());
        let trickle = UnixStream::connect(ui.server.path()).unwrap();
        let mut reader = trickle.try_clone().unwrap();
        reader.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let writer = std::thread::spawn(move || {
            for byte in hello.bytes() {
                if (&trickle).write_all(&[byte]).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        assert_eq!(silent.recv(&mut ui), None);
        assert_eq!(reader.read(&mut [0u8; 64]).unwrap(), 0);
        assert!(start.elapsed() < Duration::from_secs(2), "{:?}", start.elapsed());
        writer.join().unwrap();
    }

    /// Given a server whose idle deadline is 300 ms;
    /// when a client past `hello` sends nothing, and `ghostrealm agent`'s
    /// connection sends a request, waits past the deadline and sends another;
    /// then the first is told it was closed as idle and hung up on, and the
    /// connection's second request is answered.
    #[test]
    fn an_idle_connection_is_closed_and_the_client_reopens_it() {
        let dir = TmpDir::new("sock-idle");
        let limits = Limits { idle: Duration::from_millis(300), ..Limits::default() };
        let mut ui = Ui::with(&dir.join("s"), limits);
        let a = ui.state.new_vtab().unwrap();
        let mut c = Client::connect(ui.server.path());
        assert_eq!(c.hello_as(&mut ui, a)["ok"], json!(true));
        let idle = c.recv(&mut ui).expect("the idle line");
        assert_eq!((idle["ok"].clone(), idle["closed"].clone()), (json!(false), json!("idle")), "{idle}");
        assert_eq!(c.recv(&mut ui), None);
        let (path, token) = (ui.server.path().to_path_buf(), ui.token(a));
        let agent = std::thread::spawn(move || {
            let mut conn = super::super::client::Connection::open(&path, &token)?;
            let first = conn.request(r#"{"op":"pump"}"#)?;
            std::thread::sleep(Duration::from_millis(600));
            Ok::<_, anyhow::Error>((first, conn.request(r#"{"op":"pump"}"#)?))
        });
        until(
            || {
                ui.serve();
                agent.is_finished()
            },
            "the agent's requests answered",
        );
        let (first, second) = agent.join().unwrap().unwrap();
        assert_eq!((first.as_str(), second.as_str()), ("{\"ok\":true}\n", "{\"ok\":true}\n"));
    }

    /// Given a client with workspace A's token;
    /// when it duplicates a file;
    /// then the reply comes once the copy is done and carries its outcome,
    /// and the window has no report of it left to show.
    #[test]
    fn a_file_commands_reply_waits_for_its_job_and_takes_its_report() {
        let dir = TmpDir::new("sock-file");
        std::fs::write(dir.join("a.txt"), "A").unwrap();
        let mut ui = Ui::new(&dir.join("s"));
        let a = ui.state.new_vtab().unwrap();
        let mut c = Client::connect(ui.server.path());
        assert_eq!(c.hello_as(&mut ui, a)["ok"], json!(true));
        let path = dir.join("a.txt");
        c.send(&json!({ "op": "set", "id": "file.duplicate", "args": { "path": path } }).to_string());
        let want = format!("duplicated to {}", dir.join("a copy.txt").display());
        assert_eq!(c.recv(&mut ui), Some(json!({ "ok": true, "message": want })));
        assert!(dir.join("a copy.txt").exists());
        assert_eq!(ui.state.take_file_reports(), []);
    }

    /// Given a client with workspace A's token, and the person in workspace
    /// B;
    /// when it deletes a file git holds no copy of, and the window settles
    /// its reply after the check;
    /// then the reply says the delete awaits the person's answer, and the
    /// question names A, ignores keys for a moment and leaves the file.
    #[test]
    fn an_agents_delete_that_asks_names_its_workspace_and_says_it_awaits_the_person() {
        let dir = TmpDir::new("sock-delete");
        let path = dir.join("a.txt");
        std::fs::write(&path, "A").unwrap();
        let mut ui = Ui::new(&dir.join("s"));
        let a = ui.state.new_vtab().unwrap();
        ui.state.rename_active_vtab("agent's");
        ui.state.new_vtab().unwrap();
        let mut c = Client::connect(ui.server.path());
        assert_eq!(c.hello_as(&mut ui, a)["ok"], json!(true));
        c.send(&json!({ "op": "set", "id": "file.delete", "args": { "path": path } }).to_string());
        assert_eq!(c.recv(&mut ui), Some(json!({ "ok": true, "message": crate::app_state::AWAITS_ANSWER })));
        let asked = ui.state.confirm().expect("untracked: asks first");
        assert!(asked.message.starts_with("From workspace \u{201c}agent's\u{201d}: Move "), "{}", asked.message);
        assert!(!asked.takes_keys_at(Instant::now()) && asked.enter_cancels());
        assert!(path.exists());
    }

    /// Given a client with workspace A's token, A granted, and the person in
    /// workspace B;
    /// when it deletes, as B, a file git holds no copy of, and the window
    /// settles its reply after the check;
    /// then the question names A, not B, and ignores keys for a moment.
    #[test]
    fn a_granted_agents_delete_as_another_workspace_names_its_home() {
        let dir = TmpDir::new("sk-gdel");
        let path = dir.join("a.txt");
        std::fs::write(&path, "A").unwrap();
        let mut ui = Ui::new(&dir.join("s"));
        let a = ui.state.new_vtab().unwrap();
        ui.state.rename_active_vtab("agent's");
        let b = ui.state.new_vtab().unwrap();
        ui.grants.toggle(a);
        let mut c = Client::connect(ui.server.path());
        assert_eq!(c.hello_as(&mut ui, a)["ok"], json!(true));
        c.send(&json!({ "op": "set", "id": "file.delete", "args": { "path": path }, "workspace": b.0 }).to_string());
        assert_eq!(c.recv(&mut ui), Some(json!({ "ok": true, "message": crate::app_state::AWAITS_ANSWER })));
        let asked = ui.state.confirm().expect("untracked: asks first");
        assert!(asked.message.starts_with("From workspace \u{201c}agent's\u{201d}: Move "), "{}", asked.message);
        assert!(!asked.takes_keys_at(Instant::now()) && asked.enter_cancels());
        assert!(path.exists());
    }

    /// Given a client with workspace A's token, A granted, and workspace B;
    /// when it reads the tree and splits B, opens a workspace and a terminal
    /// there, then the grant is switched off between requests;
    /// then the tree lists B and the split lands there; the new terminal's
    /// token is ungranted; and once off, the same connection's split of B is
    /// refused while a request for A is served.
    #[test]
    fn switching_a_grant_off_narrows_a_live_connection_at_its_next_request() {
        let dir = TmpDir::new("sk-grant");
        let mut ui = Ui::new(&dir.join("s"));
        let a = ui.state.new_vtab().unwrap();
        let b = ui.state.new_vtab().unwrap();
        ui.grants.toggle(a);
        let mut c = Client::connect(ui.server.path());
        assert_eq!(c.hello_as(&mut ui, a)["ok"], json!(true));
        c.send(r#"{"op":"get","what":"tree"}"#);
        let reply = c.recv(&mut ui).unwrap();
        assert!(reply["tree"].as_str().unwrap().contains(&format!("vtab {} ", b.0)), "{reply:?}");
        let split = json!({ "op": "set", "id": "split.leftright", "workspace": b.0 }).to_string();
        c.send(&split);
        assert_eq!(c.recv(&mut ui).unwrap()["ok"], json!(true));
        assert_eq!(ui.state.tree.vtab(b).unwrap().panes().len(), 2);
        c.send(&json!({ "op": "set", "id": "workspace.new", "args": { "dir": &*dir } }).to_string());
        let reply = c.recv(&mut ui).unwrap();
        let new = reply["message"].as_str().and_then(|m| m.strip_prefix("opened workspace ")).unwrap_or_else(|| panic!("{reply:?}"));
        let new = VtabId(new.parse().unwrap());
        c.send(&json!({ "op": "set", "id": "terminal.new", "workspace": new.0 }).to_string());
        assert_eq!(c.recv(&mut ui).unwrap()["ok"], json!(true));
        let mut theirs = Client::connect(ui.server.path());
        assert_eq!(theirs.hello_as(&mut ui, new)["ok"], json!(true));
        theirs.send(&json!({ "op": "set", "id": "split.leftright", "workspace": a.0 }).to_string());
        let reply = theirs.recv(&mut ui).unwrap();
        assert!(reply["error"].as_str().unwrap().contains("outside this agent's workspace"), "{reply:?}");
        ui.grants.toggle(a);
        c.send(&split);
        let reply = c.recv(&mut ui).unwrap();
        assert!(reply["error"].as_str().unwrap().contains("outside this agent's workspace"), "{reply:?}");
        assert_eq!(ui.state.tree.vtab(b).unwrap().panes().len(), 2);
        c.send(r#"{"op":"pump"}"#);
        assert_eq!(c.recv(&mut ui), Some(json!({ "ok": true })));
    }

    /// `ghostrealm agent`'s relay of `input` with `token`, run on a thread
    /// while the window serves: its output and outcome.
    fn relayed(ui: &mut Ui, token: &str, input: &str) -> (String, anyhow::Result<()>) {
        let (path, token, input) = (ui.server.path().to_path_buf(), token.to_string(), input.to_string());
        let relay = std::thread::spawn(move || {
            let mut out = Vec::new();
            let result = super::super::client::relay(&path, &token, input.as_bytes(), &mut out);
            (String::from_utf8(out).unwrap(), result)
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while !relay.is_finished() {
            assert!(Instant::now() < deadline, "relayed");
            ui.serve();
            std::thread::sleep(Duration::from_millis(5));
        }
        relay.join().unwrap()
    }

    /// Given a window with workspaces A and B;
    /// when `ghostrealm agent` relays requests with A's token, and again with
    /// a token no workspace has;
    /// then each request's reply is written, one line each, for A; and the
    /// unknown token is an error that names no other way in.
    #[test]
    fn the_agent_command_relays_requests_as_its_terminals_workspace() {
        let dir = TmpDir::new("sock-relay");
        let mut ui = Ui::new(&dir.join("s"));
        let a = ui.state.new_vtab().unwrap();
        ui.state.new_vtab().unwrap();
        let token = ui.token(a);
        let (out, result) = relayed(&mut ui, &token, "{\"op\":\"get\",\"what\":\"tree\"}\n\n{\"op\":\"pump\"}\n");
        result.unwrap();
        let replies: Vec<Json> = out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(replies.len(), 2, "{out}");
        assert_eq!(replies[0]["tree"].as_str().unwrap().matches("vtab ").count(), 1, "A's alone: {out}");
        assert_eq!(replies[1], json!({ "ok": true }));
        let (out, result) = relayed(&mut ui, &"0".repeat(64), "{\"op\":\"pump\"}\n");
        let error = format!("{:#}", result.unwrap_err());
        assert_eq!(out, "");
        assert!(error.contains(REFUSED), "{error}");
        assert!(!error.contains("headless") && !error.contains("unset"), "{error}");
    }

    /// `ghostrealm agent --mcp`'s server on `input` with `token`, run on a
    /// thread while the window serves: its replies and outcome.
    fn mcp_served(ui: &mut Ui, token: &str, input: &[Json]) -> (Vec<Json>, anyhow::Result<()>) {
        let path = ui.server.path().to_path_buf();
        let token = token.to_string();
        let input: String = input.iter().map(|m| format!("{m}\n")).collect();
        let server = std::thread::spawn(move || {
            let mut out = Vec::new();
            let result = super::super::mcp::serve(&path, &token, input.as_bytes(), &mut out);
            (String::from_utf8(out).unwrap(), result)
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while !server.is_finished() {
            assert!(Instant::now() < deadline, "served");
            ui.serve();
            std::thread::sleep(Duration::from_millis(5));
        }
        let (out, result) = server.join().unwrap();
        (out.lines().map(|l| serde_json::from_str(l).unwrap()).collect(), result)
    }

    fn mcp_call(id: u32, name: &str, arguments: Json) -> Json {
        json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": name, "arguments": arguments } })
    }

    /// Given a window with workspaces A and B, A ungranted;
    /// when an MCP client with A's token initializes, lists the tools, reads
    /// the tree and `help`, then splits B, reads and types into B's terminal
    /// and splits B again; and a client with a token no workspace has
    /// starts;
    /// then the tree is A's alone and `help` lists no refused command; each
    /// call on B is a tool error with the socket's reason, the second split
    /// as much as the first, and B is untouched; the unknown token is an
    /// error before any reply.
    #[test]
    fn the_mcp_server_has_its_workspaces_reach_and_no_more() {
        let dir = TmpDir::new("sock-mcp");
        let mut ui = Ui::new(&dir.join("s"));
        let a = ui.state.new_vtab().unwrap();
        let b = ui.state.new_vtab().unwrap();
        let theirs = ui.state.focused_surface_in(b).unwrap();
        let token = ui.token(a);
        let split = mcp_call(5, "set", json!({ "id": "split.leftright", "workspace": b.0 }));
        let (replies, result) = mcp_served(
            &mut ui,
            &token,
            &[
                json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                    "protocolVersion": super::super::mcp::PROTOCOL_VERSION, "capabilities": {},
                    "clientInfo": { "name": "test", "version": "0" } } }),
                json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
                json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
                mcp_call(3, "get", json!({ "what": "tree" })),
                mcp_call(4, "help", json!({})),
                split.clone(),
                mcp_call(6, "get", json!({ "what": "surface", "id": theirs.0 })),
                mcp_call(7, "input", json!({ "text": "touch pwned\r", "id": theirs.0 })),
                split,
            ],
        );
        result.unwrap();
        let ids: Vec<Json> = replies.iter().map(|r| r["id"].clone()).collect();
        assert_eq!(ids, [1, 2, 3, 4, 5, 6, 7, 5].map(|n| json!(n)));
        assert_eq!(replies[0]["result"]["protocolVersion"], super::super::mcp::PROTOCOL_VERSION);
        let text = |r: &Json, i: usize| r["result"]["content"][i]["text"].as_str().unwrap().to_string();
        assert_eq!(replies[2]["result"]["isError"], false);
        assert_eq!(text(&replies[2], 1).matches("vtab ").count(), 1, "A's alone: {:?}", replies[2]);
        let help: Json = serde_json::from_str(&text(&replies[3], 0)).unwrap();
        let listed: Vec<&str> = help["commands"].as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap()).collect();
        assert!(listed.contains(&"split.leftright") && !listed.contains(&"file.reveal"), "{listed:?}");
        for (r, why) in [
            (&replies[4], "outside this agent's workspace"),
            (&replies[5], "unknown surface"),
            (&replies[6], "unknown surface"),
            (&replies[7], "outside this agent's workspace"),
        ] {
            assert_eq!(r["result"]["isError"], true, "{r}");
            assert!(text(r, 0).contains(why), "{r}");
        }
        assert_eq!(ui.state.tree.vtab(b).unwrap().panes().len(), 1);
        let (replies, result) = mcp_served(&mut ui, &"0".repeat(64), &[json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" })]);
        assert!(replies.is_empty());
        assert!(format!("{:#}", result.unwrap_err()).contains(REFUSED));
    }

    /// Given a server;
    /// when it closes;
    /// then its socket file is gone.
    #[test]
    fn closing_removes_the_socket_it_bound() {
        let dir = TmpDir::new("sock-rm");
        let server = Server::bind(&dir.join("s"), uid(), Arc::new(|| {})).unwrap();
        let path = server.path().to_path_buf();
        assert!(path.exists());
        drop(server);
        assert!(!path.exists());
    }

    /// Given a server whose socket file was replaced by a regular file;
    /// when it closes;
    /// then the file is kept.
    #[test]
    fn a_file_in_the_sockets_place_is_kept() {
        let dir = TmpDir::new("sk-file");
        let server = Server::bind(&dir.join("s"), uid(), Arc::new(|| {})).unwrap();
        let path = server.path().to_path_buf();
        // Made before the socket goes, so it can't reuse its inode.
        let new = dir.join("new");
        fs::write(&new, "someone's").unwrap();
        fs::rename(&new, &path).unwrap();
        drop(server);
        assert_eq!(fs::read_to_string(&path).unwrap(), "someone's");
    }

    /// Given a regular file and a record of its own `(dev, ino)` as bound;
    /// when it is removed if bound;
    /// then it is kept: only a socket matches.
    #[test]
    fn a_file_matching_the_bound_identity_but_not_a_socket_is_kept() {
        let dir = TmpDir::new("sk-notsock");
        let path = dir.join("f");
        fs::write(&path, "kept").unwrap();
        let meta = fs::symlink_metadata(&path).unwrap();
        remove_if_bound(&path, Some((meta.dev(), meta.ino())), identity(&path));
        assert_eq!(fs::read_to_string(&path).unwrap(), "kept");
    }

    /// Given a server whose socket file was replaced by a symlink to a file
    /// outside its directory;
    /// when it closes;
    /// then the symlink and its target are kept, the target unchanged.
    #[test]
    fn a_symlink_in_the_sockets_place_and_its_target_are_kept() {
        let dir = TmpDir::new("sk-link");
        let target = dir.join("target");
        fs::write(&target, "kept").unwrap();
        let server = Server::bind(&dir.join("s"), uid(), Arc::new(|| {})).unwrap();
        let path = server.path().to_path_buf();
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        drop(server);
        assert!(fs::symlink_metadata(&path).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(&target).unwrap(), "kept");
    }

    /// Given a socket as bound, in a directory it can be removed from;
    /// when it is removed if bound, and reading it now fails;
    /// then it is kept.
    #[test]
    fn an_error_reading_the_socket_keeps_it() {
        let dir = TmpDir::new("sk-unread");
        let path = dir.join("a.sock");
        let _listener = UnixListener::bind(&path).unwrap();
        let bound = identity(&path).unwrap();
        remove_if_bound(&path, Some(bound), Err(io::Error::other("unreadable")));
        assert!(fs::symlink_metadata(&path).is_ok(), "kept");
        remove_if_bound(&path, Some(bound), identity(&path));
        assert!(fs::symlink_metadata(&path).is_err(), "removed once read");
    }

    /// Given a socket directory that is a symlink, one other users may enter,
    /// and one checked for another user, the last two holding a dead
    /// window's socket;
    /// when a server binds in each;
    /// then none binds, and each directory (and the symlink's target) is
    /// left as it was, the dead sockets kept.
    #[test]
    fn a_directory_that_isnt_the_users_alone_is_refused() {
        let dir = TmpDir::new("sock-dir");
        let target = dir.join("target");
        fs::DirBuilder::new().mode(0o700).create(&target).unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let open = dir.join("open");
        fs::DirBuilder::new().mode(0o755).create(&open).unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).unwrap();
        let name = format!("{}-deadbeef.sock", dead_pid());
        stale_socket(&target.join(&name));
        stale_socket(&open.join(&name));
        let waker: Waker = Arc::new(|| {});
        let error = |dir: &Path, uid: u32| Server::bind(dir, uid, waker.clone()).err().expect("refused").to_string();
        assert!(error(&link, uid()).contains("not a directory"));
        assert!(error(&open, uid()).contains("other users may enter it"));
        assert!(error(&target, uid() + 1).contains("owned by another user"));
        let names = |dir: &Path| fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect::<Vec<_>>();
        assert_eq!(names(&target), std::slice::from_ref(&name));
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(fs::metadata(&open).unwrap().mode() & 0o777, 0o755);
        assert_eq!(names(&open), [name]);
    }

    /// The pid of a process that ran and was reaped.
    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    /// A socket nothing listens on, as a killed window leaves it.
    fn stale_socket(path: &Path) {
        drop(UnixListener::bind(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())));
    }

    fn is_socket(path: &Path) -> bool {
        fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket())
    }

    /// Given, in one directory, a dead window's socket, and near misses:
    /// dead, listener-less sockets whose names `bind` doesn't give, a regular
    /// file and a web host frame file with matching pids, a matching symlink
    /// to a dead socket outside the directory, a socket with a live listener,
    /// and a dead socket nobody may connect to;
    /// when it is swept with every pid taken for dead;
    /// then the dead window's socket goes and everything else stays, the
    /// listener still taking connections.
    #[test]
    fn stale_sockets_of_dead_windows_are_swept() {
        let dir = TmpDir::new("sw");
        let sockets = dir.join("s");
        fs::DirBuilder::new().mode(0o700).create(&sockets).unwrap();
        let pid = dead_pid();
        // Not `deadbeef`: APFS ignores case, and `DEADBEEF` is a near miss.
        let gone = sockets.join(format!("{pid}-0123abcd.sock"));
        stale_socket(&gone);
        let near_misses = [
            format!("{pid}.sock"),
            format!("{pid}-abc.sock"),
            format!("{pid}-deadbeef.sock.old"),
            format!("x{pid}-deadbeef.sock"),
            format!("+{pid}-deadbeef.sock"),
            format!("0{pid}-deadbeef.sock"),
            format!("{pid}-DEADBEEF.sock"),
            "0-deadbeef.sock".to_string(),
            "2147483648-deadbeef.sock".to_string(),
        ];
        for name in &near_misses {
            stale_socket(&sockets.join(name));
        }
        let file = sockets.join(format!("{pid}-0badf00d.sock"));
        fs::write(&file, "a file").unwrap();
        let frame = sockets.join(format!("ghostrealm-web-{pid}-1-1.bgra"));
        fs::write(&frame, "a frame").unwrap();
        let outside = dir.join("outside.sock");
        stale_socket(&outside);
        let link = sockets.join(format!("{pid}-1234abcd.sock"));
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let live = sockets.join(format!("{pid}-11111111.sock"));
        let listener = UnixListener::bind(&live).unwrap();
        let shut = sockets.join(format!("{pid}-22222222.sock"));
        stale_socket(&shut);
        fs::set_permissions(&shut, fs::Permissions::from_mode(0o000)).unwrap();
        sweep(&sockets, uid(), |_| false);
        assert!(!gone.exists() && fs::symlink_metadata(&gone).is_err(), "swept");
        for name in &near_misses {
            assert!(is_socket(&sockets.join(name)), "{name} kept");
        }
        assert_eq!(fs::read_to_string(&file).unwrap(), "a file");
        assert_eq!(fs::read_to_string(&frame).unwrap(), "a frame");
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert!(is_socket(&outside));
        assert!(is_socket(&live));
        UnixStream::connect(&live).unwrap();
        listener.set_nonblocking(true).unwrap();
        listener.accept().unwrap();
        assert!(is_socket(&shut));
    }

    /// Given a dead, listener-less socket whose pid is said to be running;
    /// when its directory is swept;
    /// then it is kept.
    #[test]
    fn the_sweep_keeps_a_running_pids_socket() {
        let dir = TmpDir::new("sk-alive");
        let path = dir.join("4242-deadbeef.sock");
        stale_socket(&path);
        sweep(&dir, uid(), |pid| pid == 4242);
        assert!(is_socket(&path));
    }

    /// Given a dead window's socket;
    /// when its directory is swept for another user;
    /// then it is kept.
    #[test]
    fn the_sweep_keeps_another_users_sockets() {
        let dir = TmpDir::new("sk-other");
        let path = dir.join(format!("{}-deadbeef.sock", dead_pid()));
        stale_socket(&path);
        sweep(&dir, uid() + 1, |_| false);
        assert!(is_socket(&path));
    }

    /// Given pid 1 (not ours to signal), our own, and a reaped child's;
    /// when each is checked;
    /// then the first two are running and the last is not.
    #[test]
    fn a_live_or_unsignalable_pid_is_running() {
        assert!(running(1));
        assert!(running(std::process::id()));
        assert!(!running(dead_pid()));
    }

    /// Given a window serving its socket, and a dead window's socket beside
    /// it;
    /// when another window binds in the same directory;
    /// then the dead window's socket is gone, and the live one still serves.
    #[test]
    fn binding_sweeps_dead_windows_sockets_and_keeps_live_ones() {
        let dir = TmpDir::new("sk-bind");
        let sockets = dir.join("s");
        let mut ui = Ui::new(&sockets);
        let a = ui.state.new_vtab().unwrap();
        let dead = sockets.join(format!("{}-0badf00d.sock", dead_pid()));
        stale_socket(&dead);
        let other = Server::bind(&sockets, uid(), Arc::new(|| {})).unwrap();
        if cfg!(target_os = "macos") {
            assert!(fs::symlink_metadata(&dead).is_err(), "swept");
        }
        assert!(is_socket(ui.server.path()) && is_socket(other.path()));
        let mut c = Client::connect(ui.server.path());
        assert_eq!(c.hello_as(&mut ui, a)["ok"], json!(true));
    }

    /// Given a socket directory whose path leaves no room in `sun_path` for
    /// the socket's name;
    /// when a server binds there;
    /// then the error names the path and how to shorten it.
    #[test]
    fn a_socket_path_past_sun_path_is_refused_clearly() {
        let dir = TmpDir::new("sk-long");
        let sockets = dir.join("d".repeat(110));
        let error = Server::bind(&sockets, uid(), Arc::new(|| {})).err().expect("refused").to_string();
        assert!(error.contains(&*sockets.to_string_lossy()) && error.contains("XDG_RUNTIME_DIR"), "{error}");
    }

    /// Given a window with workspace A;
    /// when an MCP client's `input` call, then `ghostrealm agent`'s relay,
    /// sends a request longer than the window takes, and then another;
    /// then each long one is answered with an error naming the limit, the
    /// next is served on the same connection, and neither ends in an error.
    #[test]
    fn an_over_long_request_is_refused_without_closing() {
        let dir = TmpDir::new("sock-over");
        let mut ui = Ui::new(&dir.join("s"));
        let a = ui.state.new_vtab().unwrap();
        let token = ui.token(a);
        let long = "x".repeat(MAX_LINE);
        let (replies, result) = mcp_served(
            &mut ui,
            &token,
            &[mcp_call(1, "input", json!({ "text": long })), mcp_call(2, "get", json!({ "what": "tree" }))],
        );
        result.unwrap();
        assert_eq!(replies.len(), 2, "{replies:?}");
        let text = |r: &Json| r["result"]["content"][0]["text"].as_str().unwrap().to_string();
        assert_eq!(replies[0]["result"]["isError"], true);
        assert!(text(&replies[0]).contains(&format!("at most {MAX_LINE}")), "{:?}", replies[0]);
        assert_eq!(replies[1]["result"]["isError"], false, "{:?}", replies[1]);
        let line = json!({ "op": "input", "text": long }).to_string();
        let (out, result) = relayed(&mut ui, &token, &format!("{line}\n{{\"op\":\"pump\"}}\n"));
        result.unwrap();
        let replies: Vec<Json> = out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(replies.len(), 2, "{out}");
        assert!(replies[0]["error"].as_str().unwrap().contains(&format!("at most {MAX_LINE}")), "{out}");
        assert_eq!(replies[1], json!({ "ok": true }));
    }

    /// Given a server whose socket file was deleted (a temp-dir cleanup);
    /// when its next check comes;
    /// then it binds the same path again and serves it.
    #[test]
    fn a_socket_deleted_while_serving_is_bound_again() {
        let dir = TmpDir::new("sock-again");
        let mut ui = Ui::with(&dir.join("s"), Limits { recheck: Duration::from_millis(50), ..Limits::default() });
        let a = ui.state.new_vtab().unwrap();
        fs::remove_file(ui.server.path()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ui.server.path().exists() {
            assert!(Instant::now() < deadline, "bound again");
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut c = Client::connect(ui.server.path());
        assert_eq!(c.hello_as(&mut ui, a)["ok"], json!(true));
        let path = ui.server.path().to_path_buf();
        drop(ui);
        assert!(!path.exists(), "the file bound again is the one removed");
    }

    /// Given a client connected and silent;
    /// when the server closes;
    /// then closing returns at once and the client is hung up on.
    #[test]
    fn closing_hangs_up_on_a_silent_client_without_waiting_for_it() {
        let dir = TmpDir::new("sock-close");
        let mut server = Server::bind(&dir.join("s"), uid(), Arc::new(|| {})).unwrap();
        let mut c = Client::connect(server.path());
        // Admitted: the accept thread has it.
        let deadline = Instant::now() + Duration::from_secs(5);
        while lock(&server.shared.conns).is_empty() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        c.stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let start = Instant::now();
        server.close();
        assert!(start.elapsed() < Duration::from_millis(100), "{:?}", start.elapsed());
        assert_eq!(c.stream.read(&mut [0u8; 8]).unwrap(), 0);
    }
}

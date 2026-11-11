//! The agent channel in the window: a Unix socket per window, in a directory
//! only the person's user can enter. A connection's first line is
//! `{"op":"hello","token":"…"}` with a workspace's token
//! ([`crate::agent::TOKEN_ENV`]); every later line is a request, served for
//! that workspace's agent ([`Scope::Workspace`]) while the workspace is open.
//!
//! Each connection has a thread that reads a line, hands it to the UI thread
//! (which owns the app state) through a channel and the waker, and writes the
//! reply; a client that stalls blocks only its own thread. The UI thread
//! serves what is waiting ([`Server::drain`]) and replies to commands whose
//! file jobs finished ([`Server::settle`]); it does no socket I/O.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use ghostrealm_core::Registry;
use serde_json::{json, Value as Json};

use super::{serve, settled, Reply, Scope};
use crate::app_state::AppState;

/// Wakes the UI thread to serve what is waiting.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// Connections served at once; more are refused, so a flood of them costs
/// a bounded number of threads.
const MAX_CONNECTIONS: usize = 32;
/// The longest request line, newline included.
const MAX_LINE: usize = 1 << 20;
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
    /// A handle on each connection's stream, to shut it down on close.
    conns: Mutex<HashMap<u64, UnixStream>>,
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
        Self::bind_with(dir, uid, waker, RECHECK)
    }

    fn bind_with(dir: &Path, uid: u32, waker: Waker, recheck: Duration) -> io::Result<Self> {
        checked_dir(dir, uid)?;
        // A new name each time: nothing there is reused or removed first.
        let path = dir.join(format!("{}-{}.sock", std::process::id(), super::random_hex(4)?));
        let listener = UnixListener::bind(&path)?;
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            bound: Mutex::new(identity(&path).ok()),
            conns: Mutex::new(HashMap::new()),
        });
        let (send, requests) = mpsc::channel();
        let accept = Accept { path: path.clone(), uid, shared: shared.clone(), send, waker: waker.clone(), recheck };
        std::thread::Builder::new().name("agent-accept".into()).spawn(move || accept.run(listener))?;
        Ok(Self { path, shared, requests, waker, parked: Vec::new(), closed: false })
    }

    /// The socket's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Serve the requests waiting, for about `budget` at most; the loop is
    /// woken again for any left. Returns whether any was served.
    pub fn drain(&mut self, state: &mut AppState, registry: &mut Registry<AppState>, budget: Duration) -> bool {
        let start = Instant::now();
        let mut any = false;
        while let Ok(req) = self.requests.try_recv() {
            any = true;
            self.answer(req, state, registry);
            if start.elapsed() >= budget {
                (self.waker)();
                break;
            }
        }
        any
    }

    fn answer(&mut self, req: Incoming, state: &mut AppState, registry: &mut Registry<AppState>) {
        let Some(vt) = state.agent_token_workspace(&req.token) else {
            let _ = req.reply.send(Out { json: json!({ "ok": false, "error": REFUSED }), close: true });
            return;
        };
        let Some(line) = req.line else {
            let _ = req.reply.send(Out { json: json!({ "ok": true, "workspace": vt.0 }), close: false });
            return;
        };
        match serve(&line, Scope::Workspace(vt), state, registry) {
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

    /// Stop serving: the accept thread ends within [`POLL`], every
    /// connection is shut down, and the socket file is removed if it is still
    /// the one bound. No thread is joined: one may be blocked on a client.
    /// AppKit's terminate may exit without dropping the window, so the window
    /// calls this as it exits.
    pub fn close(&mut self) {
        if std::mem::replace(&mut self.closed, true) {
            return;
        }
        self.shared.stop.store(true, Ordering::Release);
        for (_, stream) in lock(&self.shared.conns).drain() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        self.parked.clear();
        while self.requests.try_recv().is_ok() {}
        remove_if_bound(&self.path, *lock(&self.shared.bound));
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

/// `(dev, ino)` of `path` itself (a symlink isn't followed).
fn identity(path: &Path) -> io::Result<(u64, u64)> {
    let meta = fs::symlink_metadata(path)?;
    Ok((meta.dev(), meta.ino()))
}

/// Remove `path` if it is still the file bound (`bound`). Anything else
/// keeps it: no record of the bind, an error reading it, or another file
/// (or a symlink) in its place.
fn remove_if_bound(path: &Path, bound: Option<(u64, u64)>) {
    if bound.is_some_and(|ours| identity(path).is_ok_and(|now| now == ours)) {
        let _ = fs::remove_file(path);
    }
}

/// The accept thread.
struct Accept {
    path: PathBuf,
    uid: u32,
    shared: Arc<Shared>,
    send: mpsc::Sender<Incoming>,
    waker: Waker,
    recheck: Duration,
}

impl Accept {
    fn run(self, mut listener: UnixListener) {
        let _ = listener.set_nonblocking(true);
        let mut next = 0u64;
        let mut checked = Instant::now();
        while !self.shared.stop.load(Ordering::Acquire) {
            if checked.elapsed() >= self.recheck {
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
        conns.insert(id, handle);
        drop(conns);
        let conn = Connection { id, shared: self.shared.clone(), send: self.send.clone(), waker: self.waker.clone() };
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
}

/// A line read from a connection.
enum Read {
    Line(String),
    TooLong,
    End,
}

impl Connection {
    fn run(self, stream: UnixStream) {
        let mut writer = &stream;
        let mut reader = BufReader::new(&stream);
        let refuse = |writer: &mut &UnixStream, error: &str| {
            let _ = write_line(writer, &json!({ "ok": false, "error": error }));
        };
        let token = match read_line(&mut reader) {
            Read::Line(line) => match hello_token(&line) {
                Some(token) => token,
                None => return refuse(&mut writer, REFUSED),
            },
            Read::TooLong => return refuse(&mut writer, "line too long"),
            Read::End => return,
        };
        let mut line = None;
        loop {
            let (reply, replied) = mpsc::channel();
            if self.send.send(Incoming { token: token.clone(), line: line.take(), reply }).is_err() {
                return;
            }
            (self.waker)();
            // The server dropped the request: it is closing.
            let Ok(out) = replied.recv() else { return };
            if write_line(&mut writer, &out.json).is_err() || out.close {
                return;
            }
            line = match read_line(&mut reader) {
                Read::Line(l) => Some(l),
                Read::TooLong => return refuse(&mut writer, "line too long"),
                Read::End => return,
            };
        }
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
    }

    impl Ui {
        fn new(dir: &Path) -> Self {
            let server = Server::bind(dir, uid(), Arc::new(|| {})).unwrap();
            let mut state = AppState::new().with_shell_line("sleep 5");
            state.set_agent_socket(server.path().to_path_buf());
            Self { state, registry: build_registry(), server }
        }

        fn serve(&mut self) {
            self.server.drain(&mut self.state, &mut self.registry, Duration::from_millis(50));
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

    /// Given as many connections as are served at once;
    /// when one more connects;
    /// then it is refused and hung up on.
    #[test]
    fn a_connection_past_the_limit_is_refused() {
        let dir = TmpDir::new("sock-limit");
        let mut ui = Ui::new(&dir.join("s"));
        let a = ui.state.new_vtab().unwrap();
        let token = ui.token(a);
        let _held: Vec<Client> = (0..MAX_CONNECTIONS)
            .map(|_| {
                let mut c = Client::connect(ui.server.path());
                assert_eq!(c.hello(&mut ui, &token)["ok"], json!(true));
                c
            })
            .collect();
        let mut c = Client::connect(ui.server.path());
        assert_eq!(c.recv(&mut ui), Some(json!({ "ok": false, "error": "too many connections" })));
        assert_eq!(c.recv(&mut ui), None);
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
        fs::remove_file(&path).unwrap();
        fs::write(&path, "someone's").unwrap();
        drop(server);
        assert_eq!(fs::read_to_string(&path).unwrap(), "someone's");
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

    /// Given a server whose directory can no longer be searched;
    /// when it closes;
    /// then nothing is removed, and it doesn't panic.
    #[test]
    fn an_unreadable_directory_keeps_the_socket() {
        let dir = TmpDir::new("sk-unread");
        let sockets = dir.join("s");
        let server = Server::bind(&sockets, uid(), Arc::new(|| {})).unwrap();
        let path = server.path().to_path_buf();
        fs::set_permissions(&sockets, fs::Permissions::from_mode(0o200)).unwrap();
        drop(server);
        fs::set_permissions(&sockets, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(fs::symlink_metadata(&path).is_ok(), "kept");
    }

    /// Given a socket directory that is a symlink, one other users may enter,
    /// and one checked for another user;
    /// when a server binds in each;
    /// then none binds, and each directory (and the symlink's target) is
    /// left as it was.
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
        let waker: Waker = Arc::new(|| {});
        let error = |dir: &Path, uid: u32| Server::bind(dir, uid, waker.clone()).err().expect("refused").to_string();
        assert!(error(&link, uid()).contains("not a directory"));
        assert!(error(&open, uid()).contains("other users may enter it"));
        assert!(error(&target, uid() + 1).contains("owned by another user"));
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(fs::metadata(&open).unwrap().mode() & 0o777, 0o755);
        assert_eq!(fs::read_dir(&open).unwrap().count(), 0);
    }

    /// Given a server whose socket file was deleted (a temp-dir cleanup);
    /// when its next check comes;
    /// then it binds the same path again and serves it.
    #[test]
    fn a_socket_deleted_while_serving_is_bound_again() {
        let dir = TmpDir::new("sock-again");
        let server = Server::bind_with(&dir.join("s"), uid(), Arc::new(|| {}), Duration::from_millis(50)).unwrap();
        let mut ui = Ui { state: AppState::new().with_shell_line("sleep 5"), registry: build_registry(), server };
        ui.state.set_agent_socket(ui.server.path().to_path_buf());
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

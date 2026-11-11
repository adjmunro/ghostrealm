//! The agent control channel: a line-delimited JSON protocol over any
//! reader/writer, exposing the thin fixed tool set from the brief:
//!
//! - `{"op":"help"}`                         → all command metas + arg schemas
//! - `{"op":"search","query":"split"}`       → matching command ids + scores
//! - `{"op":"set","id":"...","args":{...}}`  → run a command (a file command
//!   replies once its copy, move, or git check is done)
//! - `{"op":"get","what":"tree"}`            → text dump of vtabs/panes/surfaces,
//!   then hidden surfaces (pop-ups an agent's eval opened)
//! - `{"op":"get","what":"surface","id":N}`  → a surface's content as text (a web
//!   page: its address, title, its hidden pop-ups, and text once loaded)
//! - `{"op":"input","text":"ls\r","id":N}`   → type into a surface (id optional,
//!   defaulting to the focused surface)
//! - `{"op":"pump"}`                         → drain background output
//!
//! One JSON object per request line, one JSON object per response line. Being
//! generic over `BufRead`/`Write` keeps it testable with in-memory buffers and
//! reusable for a socket later. It is the agent's drive + observe loop, and the
//! display-free way to test app behaviour end to end.

use std::io::{BufRead, Write};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Instant;

use ghostrealm_core::{AgentReach, Args, CloseOnExit, CommandMeta, Registry, SurfaceId, Value, VtabId};
use serde_json::{json, Value as Json};

use crate::app_state::{build_registry, AppState};

pub mod socket;

/// `bytes` random bytes from `/dev/urandom`, hex-encoded.
pub(crate) fn random_hex(bytes: usize) -> std::io::Result<String> {
    use std::io::Read;
    let mut buf = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Names the window's agent socket in a terminal's environment.
pub const SOCKET_ENV: &str = "GHOSTREALM_AGENT_SOCKET";

/// Holds the terminal's workspace's token, which a connection presents in
/// its `hello`.
pub const TOKEN_ENV: &str = "GHOSTREALM_AGENT_TOKEN";

/// Run the REPL until the input reaches EOF.
///
/// The web host's idle exit is checked before and after each request, and at
/// its deadline while the loop waits for the next one.
///
/// Exited terminals stay in the tree whatever the config says: the agent has to
/// read a finished command's output with `get surface`, which a closed surface
/// no longer answers.
pub fn run<R: BufRead + Send + 'static, W: Write>(
    input: R,
    mut output: W,
    mut state: AppState,
) -> anyhow::Result<()> {
    keep_exited(&mut state);
    let mut registry = build_registry();
    let mut lines = Lines::spawn(input)?;
    while let Some(line) = lines.next(&mut state)? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        state.tick_web(Instant::now());
        let response = serve(trimmed, Scope::Full, &mut state, &mut registry).wait(&mut state);
        state.tick_web(Instant::now());
        serde_json::to_writer(&mut output, &response)?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
    Ok(())
}

/// One reply from the reader thread: a line, `None` at EOF, or the read's
/// error.
type LineRead = std::io::Result<Option<String>>;

/// Request lines read on a thread, so the loop can tick the web host's idle
/// exit while it waits for one.
///
/// The thread reads a line only when [`next`](Self::next) asks for it, so
/// input is consumed exactly as a blocking `read_line` per request would.
/// Waiting on a line is the only time it is blocked reading.
struct Lines {
    want: Option<mpsc::Sender<()>>,
    got: mpsc::Receiver<LineRead>,
    /// A line was asked for and hasn't arrived: the thread may be blocked
    /// reading.
    reading: bool,
    thread: Option<JoinHandle<()>>,
}

impl Lines {
    fn spawn<R: BufRead + Send + 'static>(mut input: R) -> std::io::Result<Self> {
        let (want, wanted) = mpsc::channel::<()>();
        let (send, got) = mpsc::channel::<LineRead>();
        let thread = std::thread::Builder::new().name("agent-input".into()).spawn(move || {
            while wanted.recv().is_ok() {
                let mut line = String::new();
                let read = input.read_line(&mut line).map(|n| (n > 0).then_some(line));
                let last = !matches!(read, Ok(Some(_)));
                if send.send(read).is_err() || last {
                    break;
                }
            }
        })?;
        Ok(Self { want: Some(want), got, reading: false, thread: Some(thread) })
    }

    /// The next line, `None` at EOF. Ticks the web host's idle exit at its
    /// deadline while waiting.
    fn next(&mut self, state: &mut AppState) -> std::io::Result<Option<String>> {
        if !self.reading {
            // The thread is gone only after EOF or an error, both returned
            // before anything asks again.
            if let Some(want) = &self.want {
                let _ = want.send(());
            }
            self.reading = true;
        }
        let read = loop {
            let got = match state.next_web_deadline() {
                Some(at) => self.got.recv_timeout(at.saturating_duration_since(Instant::now())),
                None => self.got.recv().map_err(mpsc::RecvTimeoutError::from),
            };
            match got {
                Ok(read) => break read,
                Err(mpsc::RecvTimeoutError::Timeout) => state.tick_web(Instant::now()),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break Err(std::io::Error::other("the agent input reader stopped"))
                }
            }
        };
        self.reading = false;
        read
    }
}

impl Drop for Lines {
    /// Ends the thread with the loop. While it may be blocked reading (the
    /// loop unwinding from a panic mid-wait) it is left to end at its next
    /// line or EOF, since joining it could block forever on a quiet input.
    fn drop(&mut self) {
        self.want = None;
        if let Some(thread) = self.thread.take().filter(|_| !self.reading) {
            let _ = thread.join();
        }
    }
}

/// Keep exited terminals in the tree whatever the config says (see [`run`]).
fn keep_exited(state: &mut AppState) {
    state.set_close_on_exit(CloseOnExit::Never);
}

/// Whom a request is served for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// The headless channel: the whole app is the agent's.
    Full,
    /// A workspace's agent in the window: it reaches that workspace's
    /// terminals and Files surfaces, and runs the commands classified for it
    /// ([`AgentReach`]), as that workspace.
    Workspace(VtabId),
}

/// A request, parsed.
enum Op {
    Help,
    Search(String),
    Set { id: String, args: Args },
    GetTree,
    GetSurface(SurfaceId),
    Input { id: Option<SurfaceId>, text: String },
    Pump,
}

/// What [`serve`] answers.
#[must_use]
pub enum Reply {
    Done(Json),
    /// A command started file jobs `[since, until)`; the reply carries their
    /// outcome, once they are done ([`Reply::wait`], [`settled`]).
    Jobs { since: u64, until: u64, message: Option<String> },
}

impl Reply {
    /// The reply, once the jobs it waits for are done: blocks for them.
    pub fn wait(self, state: &mut AppState) -> Json {
        match self {
            Reply::Done(json) => json,
            Reply::Jobs { since, message, .. } => {
                // A job a result submits (past `until`) is waited for too.
                state.wait_file_jobs_since(since);
                finished(message, state.take_file_reports_since(since))
            }
        }
    }
}

/// The reply to a command that started file jobs `[since, until)`, once the
/// file worker holds none of them; `None` while it does. A job that finished
/// inside the command reported only a warning, so a missing report is no
/// reason to wait.
pub fn settled(state: &mut AppState, since: u64, until: u64, message: &Option<String>) -> Option<Json> {
    if state.file_jobs_hold(since..until) {
        return None;
    }
    Some(finished(message.clone(), state.take_file_reports_in(since..until)))
}

/// A command's reply: what it said, unless a job's report speaks for it (what
/// the command said was that the job is running).
fn finished(message: Option<String>, reports: Vec<crate::app_state::FileReport>) -> Json {
    match reports.into_iter().map(|r| r.result).collect::<Result<Vec<_>, _>>() {
        Err(e) => json!({ "ok": false, "error": e }),
        Ok(messages) if messages.is_empty() => json!({ "ok": true, "message": message }),
        Ok(messages) => json!({ "ok": true, "message": messages.into_iter().rfind(|m| !m.is_empty()) }),
    }
}

/// Serve one request line for `scope`: parse, authorize, run. Every request
/// from either channel passes here, and what a workspace's agent may do is
/// decided only here.
pub fn serve(line: &str, scope: Scope, state: &mut AppState, registry: &mut Registry<AppState>) -> Reply {
    let op = match parse(line) {
        Ok(op) => op,
        Err(e) => return Reply::Done(json!({ "ok": false, "error": e })),
    };
    if let Err(e) = authorize(&op, scope, state, registry) {
        return Reply::Done(json!({ "ok": false, "error": e }));
    }
    execute(op, scope, state, registry)
}

fn parse(line: &str) -> Result<Op, String> {
    let req: Json = serde_json::from_str(line).map_err(|e| format!("invalid json: {e}"))?;
    let str_of = |key| req.get(key).and_then(Json::as_str).unwrap_or("").to_string();
    Ok(match req.get("op").and_then(Json::as_str).unwrap_or("") {
        "help" => Op::Help,
        "search" => Op::Search(str_of("query")),
        "set" => Op::Set { id: str_of("id"), args: parse_args(req.get("args"))? },
        "get" => match req.get("what").and_then(Json::as_str).unwrap_or("tree") {
            "tree" => Op::GetTree,
            "surface" => match req.get("id").and_then(Json::as_u64) {
                Some(id) => Op::GetSurface(SurfaceId(id)),
                None => return Err("get surface needs an integer `id`".into()),
            },
            other => return Err(format!("unknown get target: {other}")),
        },
        "input" => Op::Input { id: req.get("id").and_then(Json::as_u64).map(SurfaceId), text: str_of("text") },
        "pump" => Op::Pump,
        "" => return Err("missing `op`".into()),
        other => return Err(format!("unknown op: {other}")),
    })
}

/// Whether `scope` may run `op`. No wildcard arm: a new op is classified
/// before it compiles.
fn authorize(op: &Op, scope: Scope, state: &AppState, registry: &Registry<AppState>) -> Result<(), String> {
    let Scope::Workspace(vt) = scope else { return Ok(()) };
    let reaches = |sid: SurfaceId| match state.agent_reaches(vt, sid) {
        true => Ok(()),
        false => Err(format!("unknown surface {}", sid.0)),
    };
    match op {
        // Listed for the scope; the tree is the workspace's.
        Op::Help | Op::Search(_) | Op::GetTree | Op::Pump => Ok(()),
        Op::GetSurface(sid) => reaches(*sid),
        Op::Input { id, .. } => match id.or_else(|| state.focused_surface_in(vt)) {
            Some(sid) => reaches(sid),
            None => Err("no focused surface".into()),
        },
        Op::Set { id, args } => {
            let meta = registry.meta(id).ok_or_else(|| format!("unknown command: {id}"))?;
            match meta.agent {
                AgentReach::Refused(why) => return Err(format!("{id} isn't open to a workspace's agent: {why}")),
                AgentReach::Workspace => {}
                AgentReach::FocusedPane if state.focused_pane_reachable(vt) => {}
                AgentReach::FocusedPane => {
                    return Err(format!("{id} would close what the workspace's agent can't reach"));
                }
            }
            match args.get("id") {
                Some(Value::Int(sid)) => reaches(SurfaceId(*sid as u64)),
                _ => Ok(()),
            }
        }
    }
}

fn execute(op: Op, scope: Scope, state: &mut AppState, registry: &mut Registry<AppState>) -> Reply {
    let allowed = |m: &CommandMeta| scope == Scope::Full || !matches!(m.agent, AgentReach::Refused(_));
    Reply::Done(match op {
        Op::Help => {
            let commands: Vec<Json> = registry.metas().filter(|m| allowed(m)).map(meta_json).collect();
            json!({ "ok": true, "commands": commands })
        }
        Op::Search(q) => {
            let hits: Vec<Json> = registry
                .search(&q, usize::MAX)
                .into_iter()
                .filter(|h| allowed(h.meta))
                .take(SEARCH_HITS)
                .map(|h| json!({ "id": h.meta.id, "title": h.meta.title, "score": h.score }))
                .collect();
            json!({ "ok": true, "hits": hits })
        }
        Op::Set { id, args } => {
            let mark = state.file_job_mark();
            let ran = match scope {
                Scope::Full => registry.execute(&id, &args, state),
                Scope::Workspace(vt) => match state.acting_as(vt, |s| registry.execute(&id, &args, s)) {
                    Ok(ran) => ran,
                    Err(e) => return Reply::Done(json!({ "ok": false, "error": e })),
                },
            };
            // A file command's reply waits for its jobs on the file worker,
            // and carries their outcome. Jobs the person started before
            // aren't waited for, and their reports stay queued for the
            // window.
            match ran {
                Err(e) => json!({ "ok": false, "error": e.to_string() }),
                Ok(out) if state.file_job_mark() > mark => {
                    return Reply::Jobs { since: mark, until: state.file_job_mark(), message: out.message };
                }
                Ok(out) => json!({ "ok": true, "message": out.message }),
            }
        }
        Op::GetTree => {
            state.pump_all();
            let tree = match scope {
                Scope::Full => state.describe(),
                Scope::Workspace(vt) => state.describe_for(vt),
            };
            json!({ "ok": true, "tree": tree })
        }
        Op::GetSurface(id) => {
            state.pump_all();
            match state.surface_text(id) {
                Some(text) => json!({ "ok": true, "text": text }),
                None => json!({ "ok": false, "error": format!("unknown surface {}", id.0) }),
            }
        }
        Op::Input { id, text } => {
            let id = match (id, scope) {
                (Some(id), _) => Some(id),
                (None, Scope::Full) => state.focused_surface(),
                (None, Scope::Workspace(vt)) => state.focused_surface_in(vt),
            };
            match id {
                Some(id) if state.write_input(id, text.as_bytes()) => json!({ "ok": true, "surface": id.0 }),
                Some(id) if state.has_surface(id) => {
                    json!({ "ok": false, "error": format!("surface {} takes no raw input", id.0) })
                }
                Some(id) => json!({ "ok": false, "error": format!("unknown surface {}", id.0) }),
                None => json!({ "ok": false, "error": "no focused surface" }),
            }
        }
        Op::Pump => {
            state.pump_all();
            json!({ "ok": true })
        }
    })
}

/// How many hits `search` returns.
const SEARCH_HITS: usize = 20;

fn meta_json(m: &CommandMeta) -> Json {
    let args: Vec<Json> = m
        .args
        .iter()
        .map(|a| {
            json!({
                "name": a.name,
                "kind": a.kind.to_string(),
                "required": a.required,
                "description": a.description,
            })
        })
        .collect();
    json!({ "id": m.id, "title": m.title, "description": m.description, "args": args })
}

/// Convert a JSON `args` object into an [`Args`] bag. Strings, integers, and
/// bools map to the matching [`Value`]; anything else is an error.
fn parse_args(value: Option<&Json>) -> Result<Args, String> {
    let mut args = Args::new();
    let Some(v) = value else { return Ok(args) };
    if v.is_null() {
        return Ok(args);
    }
    let obj = v
        .as_object()
        .ok_or_else(|| "`args` must be an object".to_string())?;
    for (k, val) in obj {
        let value = match val {
            Json::String(s) => Value::Str(s.clone()),
            Json::Bool(b) => Value::Bool(*b),
            Json::Number(n) if n.is_i64() => Value::Int(n.as_i64().unwrap()),
            other => return Err(format!("arg `{k}` has unsupported type: {other}")),
        };
        args.insert(k, value);
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Serve `line` headless, as [`run`] does.
    fn handle(line: &str, state: &mut AppState, registry: &mut Registry<AppState>) -> Json {
        serve(line, Scope::Full, state, registry).wait(state)
    }

    /// Drive the REPL with a script of request lines; return parsed responses.
    fn drive(lines: &[&str]) -> Vec<Json> {
        let input = Cursor::new(lines.join("\n").into_bytes());
        let mut out: Vec<u8> = Vec::new();
        // A cheap non-interactive shell keeps the test fast and deterministic.
        let state = AppState::new().with_shell_line("printf 'READY'; sleep 1");
        run(input, &mut out, state).expect("agent run");
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// Given the agent channel and a web host whose idle exit is immediate,
    /// which takes a moment to exit once its stdin closes, noting each start
    /// and that exit;
    /// when the agent opens a web surface, closes it, and opens another;
    /// then the first host is hung up on once the surface closed, the
    /// second starts only once the first has gone, and both opens succeed.
    #[cfg(unix)]
    #[test]
    fn the_web_host_is_hung_up_on_between_requests_once_idle() {
        use crate::plugins::web::tests::{stub_app_host, TEST_HOST, TEST_IDLE_EXIT};
        let dir = crate::plugin::testing::TmpDir::new("agent-web-idle");
        let log = dir.join("log");
        stub_app_host(&dir, &[], &format!("echo run >> '{0}'; cat > /dev/null; sleep 0.5; echo eof >> '{0}'", log.display()));
        TEST_IDLE_EXIT.set(Some(std::time::Duration::ZERO));
        let lines = [
            r#"{"op":"set","id":"web.new","args":{}}"#,
            r#"{"op":"set","id":"web.close","args":{}}"#,
            r#"{"op":"set","id":"web.new","args":{}}"#,
        ];
        let mut state = AppState::new().with_shell_line("sleep 1");
        state.new_empty_vtab();
        state.new_empty_vtab();
        let mut out: Vec<u8> = Vec::new();
        run(Cursor::new(lines.join("\n").into_bytes()), &mut out, state).unwrap();
        TEST_HOST.with_borrow_mut(|h| *h = None);
        TEST_IDLE_EXIT.set(None);
        let replies: Vec<Json> = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert!(replies.iter().all(|r| r["ok"] == json!(true)), "{replies:?}");
        // `web.new` returns once the host reports ready, which the stub does
        // before it notes its start: wait for the second host's note.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let log = loop {
            let got = std::fs::read_to_string(&log).unwrap();
            if got.matches("run\n").count() == 2 || std::time::Instant::now() >= deadline {
                break got;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert!(log.starts_with("run\neof\nrun\n"), "{log:?}");
    }

    /// Given the agent channel and a web host whose idle exit is 300 ms,
    /// noting each start and when its stdin closes;
    /// when the agent opens a web surface, closes it, and sends nothing more
    /// with its input still open;
    /// then the host is hung up on once 300 ms have passed, while the agent
    /// waits.
    #[cfg(unix)]
    #[test]
    fn the_web_host_is_hung_up_on_while_the_agent_waits_once_idle() {
        use crate::plugins::web::tests::{stub_app_host, TEST_HOST, TEST_IDLE_EXIT};
        use std::time::{Duration, Instant};
        const IDLE: Duration = Duration::from_millis(300);
        let dir = crate::plugin::testing::TmpDir::new("agent-web-idle-wait");
        let log = dir.join("log");
        stub_app_host(&dir, &[], &format!("echo run >> '{0}'; cat > /dev/null; echo eof >> '{0}'", log.display()));
        TEST_IDLE_EXIT.set(Some(IDLE));
        let (input, mut requests) = std::io::pipe().unwrap();
        let agent = std::thread::spawn(move || {
            let lines = [r#"{"op":"set","id":"web.new","args":{}}"#, r#"{"op":"set","id":"web.close","args":{}}"#];
            for l in lines {
                writeln!(requests, "{l}").unwrap();
            }
            let sent = Instant::now();
            let deadline = sent + Duration::from_secs(10);
            // The input stays open until the host has gone, or the wait
            // gives up.
            while !std::fs::read_to_string(&log).unwrap_or_default().contains("eof") && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            (Instant::now() < deadline).then(|| sent.elapsed())
        });
        let mut state = AppState::new().with_shell_line("sleep 1");
        state.new_empty_vtab();
        state.new_empty_vtab();
        let mut out: Vec<u8> = Vec::new();
        run(std::io::BufReader::new(input), &mut out, state).unwrap();
        TEST_HOST.with_borrow_mut(|h| *h = None);
        TEST_IDLE_EXIT.set(None);
        let replies: Vec<Json> = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(replies.len(), 2, "{replies:?}");
        assert!(replies.iter().all(|r| r["ok"] == json!(true)), "{replies:?}");
        let gone = agent.join().unwrap().expect("the host is hung up on while the input stays open");
        assert!(gone >= IDLE, "not before its idle deadline: {gone:?}");
    }

    #[test]
    fn help_lists_commands_with_schemas() {
        let r = &drive(&[r#"{"op":"help"}"#])[0];
        assert_eq!(r["ok"], json!(true));
        let ids: Vec<&str> = r["commands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap())
            .collect();
        assert!(
            ids.contains(&"split.leftright"),
            "help should list commands, got {ids:?}"
        );
        // workspace.rename should advertise its required `name` arg.
        let rename = r["commands"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == "workspace.rename")
            .unwrap();
        assert_eq!(rename["args"][0]["name"], json!("name"));
        assert_eq!(rename["args"][0]["required"], json!(true));
    }

    #[test]
    fn search_finds_split_commands() {
        let r = &drive(&[r#"{"op":"search","query":"split"}"#])[0];
        let ids: Vec<&str> = r["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["id"].as_str().unwrap())
            .collect();
        assert!(
            ids.iter().any(|id| id.starts_with("split.")),
            "search 'split' should return split.* commands, got {ids:?}"
        );
    }

    #[test]
    fn set_splits_and_get_tree_reflects_it() {
        let resps = drive(&[
            r#"{"op":"set","id":"workspace.new","args":{"dir":"/"}}"#,
            r#"{"op":"set","id":"split.leftright"}"#,
            r#"{"op":"get","what":"tree"}"#,
        ]);
        assert_eq!(
            resps[0]["ok"],
            json!(true),
            "workspace.new failed: {:?}",
            resps[0]
        );
        assert_eq!(resps[1]["ok"], json!(true), "split failed: {:?}", resps[1]);
        let tree = resps[2]["tree"].as_str().unwrap();
        let panes = tree.matches("pane ").count();
        assert_eq!(
            panes, 2,
            "after one split the active vtab should have 2 panes:\n{tree}"
        );
    }

    #[test]
    fn an_exited_terminal_stays_so_get_surface_reads_its_output() {
        let mut state = AppState::new().with_shell_line("printf 'FINISHED'; exit 0");
        state.new_vtab().unwrap();
        let id = state.focused_surface().unwrap().0;
        keep_exited(&mut state);
        let mut registry = build_registry();
        let mut ask = |state: &mut AppState, req: &str| handle(req, state, &mut registry);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let text = loop {
            let resp = ask(&mut state, &format!(r#"{{"op":"get","what":"surface","id":{id}}}"#));
            let text = resp["text"].as_str().unwrap_or_else(|| panic!("{resp:?}")).to_string();
            let exited = state.surface_lifecycle(SurfaceId(id)).is_some_and(|l| l.has_exited());
            if text.contains("FINISHED") && exited {
                break text;
            }
            assert!(std::time::Instant::now() < deadline, "no output and exit; got:\n{text}");
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert!(text.contains("FINISHED"), "the output stays readable:\n{text}");
        let resp = ask(&mut state, r#"{"op":"get","what":"tree"}"#);
        assert!(resp["tree"].as_str().unwrap().contains("surface "), "the surface is still in the tree");
    }

    #[test]
    fn a_file_command_finished_in_the_background_replies_with_its_outcome_not_its_progress() {
        let d = crate::plugin::testing::tmpdir("agent-file-job");
        std::fs::write(d.join("a.txt"), "A").unwrap();
        let mut state = AppState::new().with_file_job_wait(std::time::Duration::ZERO);
        state.new_empty_vtab();
        let mut registry = build_registry();
        let path = d.to_str().unwrap();
        let dup = format!(r#"{{"op":"set","id":"file.duplicate","args":{{"path":"{path}/a.txt"}}}}"#);
        let resp = handle(&dup, &mut state, &mut registry);
        assert_eq!(resp["message"], json!(format!("duplicated to {path}/a copy.txt")), "{resp:?}");
        // Delete's check says nothing once done (here it asks first).
        let del = format!(r#"{{"op":"set","id":"file.delete","args":{{"path":"{path}/a.txt"}}}}"#);
        let resp = handle(&del, &mut state, &mut registry);
        assert_eq!(resp, json!({ "ok": true, "message": null }));
        if ghostrealm_core::git::repo_root(&d).is_none() {
            assert!(state.confirm().is_some(), "untracked: asks first");
        }
    }

    #[test]
    fn a_user_file_job_finished_during_an_agent_command_stays_for_the_window() {
        let d = crate::plugin::testing::tmpdir("agent-user-job");
        std::fs::write(d.join("a.txt"), "A").unwrap();
        std::fs::write(d.join("b.txt"), "B").unwrap();
        let mut state = AppState::new().with_file_job_wait(std::time::Duration::ZERO);
        state.new_empty_vtab();
        let mut registry = build_registry();
        let path = d.to_str().unwrap();
        // The user's duplicate, still running or unreported when the agent asks.
        let mut args = Args::new();
        args.insert("path", Value::Str(format!("{path}/a.txt")));
        registry.execute("file.duplicate", &args, &mut state).unwrap();
        let dup = format!(r#"{{"op":"set","id":"file.duplicate","args":{{"path":"{path}/b.txt"}}}}"#);
        let resp = handle(&dup, &mut state, &mut registry);
        assert_eq!(resp["message"], json!(format!("duplicated to {path}/b copy.txt")), "{resp:?}");
        state.wait_file_jobs();
        let left: Vec<_> = state.take_file_reports().into_iter().map(|r| r.result).collect();
        assert_eq!(left, [Ok(format!("duplicated to {path}/a copy.txt"))], "the user's report stays");
    }

    /// Given a command whose file job finishes inside it (an inline wait);
    /// when its reply is settled;
    /// then it is ready at once, with the job's outcome.
    #[test]
    fn a_reply_whose_job_finished_inline_settles_at_once() {
        let d = crate::plugin::testing::tmpdir("agent-settle-inline");
        std::fs::write(d.join("a.txt"), "A").unwrap();
        let mut state = AppState::new().with_file_job_wait(std::time::Duration::from_secs(10));
        state.new_empty_vtab();
        let mut registry = build_registry();
        let req = set("file.duplicate", json!({ "path": d.join("a.txt") }));
        let Reply::Jobs { since, until, message } = serve(&req, Scope::Full, &mut state, &mut registry) else {
            panic!("a file job ran");
        };
        let want = json!({ "ok": true, "message": format!("duplicated to {}", d.join("a copy.txt").display()) });
        assert_eq!(settled(&mut state, since, until, &message), Some(want));
    }

    /// Given a workspace's agent's command whose file job runs in the
    /// background;
    /// when its reply is settled as the window polls;
    /// then it waits while the job is held, then carries the job's outcome
    /// and takes its report from the window.
    #[test]
    fn a_reply_whose_job_runs_in_the_background_settles_once_it_is_done() {
        let d = crate::plugin::testing::tmpdir("agent-settle-bg");
        std::fs::write(d.join("a.txt"), "A").unwrap();
        let mut state = AppState::new();
        let vt = state.new_empty_vtab();
        let mut registry = build_registry();
        let req = set("file.duplicate", json!({ "path": d.join("a.txt") }));
        let Reply::Jobs { since, until, message } = serve(&req, Scope::Workspace(vt), &mut state, &mut registry) else {
            panic!("a file job ran");
        };
        assert!(state.file_jobs_hold(since..until), "no inline wait under a workspace's agent");
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        let reply = loop {
            state.poll_file_jobs();
            let held = state.file_jobs_hold(since..until);
            match settled(&mut state, since, until, &message) {
                Some(reply) => break reply,
                None => assert!(held, "waits only while the job is held"),
            }
            assert!(Instant::now() < deadline, "settled");
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let want = json!({ "ok": true, "message": format!("duplicated to {}", d.join("a copy.txt").display()) });
        assert_eq!(reply, want);
        assert_eq!(state.take_file_reports(), []);
    }

    #[test]
    fn get_surface_shows_the_opened_file_else_the_sidebars_listing() {
        let d = crate::plugin::testing::tmpdir("agent-files");
        std::fs::write(d.join("notes.md"), "# Notes\nhello agent\n").unwrap();
        // The default config autosaves on losing focus; the agent channel
        // focuses nothing, so it never saves for the user.
        let mut state = AppState::new();
        let mut registry = build_registry();
        let path = d.to_str().unwrap();
        let mut ask = |state: &mut AppState, req: &str| handle(req, state, &mut registry);
        let resp = ask(&mut state, &format!(r#"{{"op":"set","id":"workspace.new","args":{{"dir":"{path}"}}}}"#));
        assert_eq!(resp["ok"], json!(true), "{resp:?}");
        let resp = ask(&mut state, r#"{"op":"set","id":"file_browser.new"}"#);
        assert_eq!(resp["ok"], json!(true), "{resp:?}");
        let id = state.focused_surface().unwrap().0;
        let get = format!(r#"{{"op":"get","what":"surface","id":{id}}}"#);
        let resp = ask(&mut state, &get);
        assert_eq!(resp["text"], json!(format!("{path}\n  notes.md\n")), "the listing with no document");
        let resp = ask(&mut state, &format!(r#"{{"op":"set","id":"file.open","args":{{"path":"{path}/notes.md"}}}}"#));
        assert_eq!(resp["ok"], json!(true), "{resp:?}");
        assert_eq!(state.focused_surface().map(|s| s.0), Some(id), "into the Files surface there");
        let resp = ask(&mut state, &get);
        assert_eq!(resp["text"], json!("# Notes\nhello agent"), "the file, as an editor reads it");
        // Edited, then another file opened: a tab beside it, shown, and the
        // edit stays unsaved.
        let resp = ask(&mut state, &format!(r#"{{"op":"input","id":{id},"text":"x"}}"#));
        assert_eq!(resp["ok"], json!(true), "{resp:?}");
        std::fs::write(d.join("b.txt"), "b").unwrap();
        let resp = ask(&mut state, &format!(r#"{{"op":"set","id":"file.open","args":{{"path":"{path}/b.txt"}}}}"#));
        assert_eq!(resp["ok"], json!(true), "{resp:?}");
        assert_eq!(state.focused_surface().map(|s| s.0), Some(id));
        let resp = ask(&mut state, &get);
        assert_eq!(resp["text"], json!("b"));
        let resp = ask(&mut state, &format!(r#"{{"op":"set","id":"file.open","args":{{"path":"{path}/notes.md"}}}}"#));
        assert_eq!(resp["ok"], json!(true), "{resp:?}");
        let resp = ask(&mut state, &get);
        assert_eq!(resp["text"], json!("x# Notes\nhello agent"));
        assert_eq!(std::fs::read_to_string(d.join("notes.md")).unwrap(), "# Notes\nhello agent\n", "unsaved");
    }

    #[test]
    fn set_validates_missing_args() {
        let resps = drive(&[
            r#"{"op":"set","id":"workspace.new"}"#,
            r#"{"op":"set","id":"workspace.rename"}"#,
        ]);
        assert_eq!(resps[1]["ok"], json!(false));
        assert!(
            resps[1]["error"].as_str().unwrap().contains("name"),
            "expected a missing-arg error mentioning `name`, got {:?}",
            resps[1]
        );
    }

    #[test]
    fn web_commands_are_listed_and_need_a_web_surface() {
        let resps = drive(&[
            r#"{"op":"help"}"#,
            r#"{"op":"set","id":"web.back"}"#,
            r#"{"op":"set","id":"web.navigate","args":{"url":"example.com","id":999}}"#,
        ]);
        let ids: Vec<&str> = resps[0]["commands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap())
            .collect();
        for id in ["web.new", "web.navigate", "web.back", "web.forward", "web.reload", "web.eval", "web.close"] {
            assert!(ids.contains(&id), "help should list {id}");
        }
        assert_eq!(resps[1]["ok"], json!(false));
        assert!(resps[1]["error"].as_str().unwrap().contains("no web surface"));
        assert!(resps[2]["error"].as_str().unwrap().contains("not a web surface"));
    }

    #[test]
    fn unknown_op_is_reported() {
        let r = &drive(&[r#"{"op":"frobnicate"}"#])[0];
        assert_eq!(r["ok"], json!(false));
        assert!(r["error"].as_str().unwrap().contains("frobnicate"));
    }

    /// Serve `req` for workspace `vt`'s agent, waiting for its file jobs.
    fn scoped(state: &mut AppState, registry: &mut Registry<AppState>, vt: VtabId, req: &str) -> Json {
        serve(req, Scope::Workspace(vt), state, registry).wait(state)
    }

    /// `{"op":"set","id":<id>,"args":<args>}`.
    fn set(id: &str, args: Json) -> String {
        json!({ "op": "set", "id": id, "args": args }).to_string()
    }

    fn error(reply: &Json) -> &str {
        reply["error"].as_str().unwrap_or_else(|| panic!("an error: {reply:?}"))
    }

    /// Given a registry with a command registered without a class;
    /// when a workspace's agent and the headless channel each run it;
    /// then the agent is refused and the headless channel runs it.
    #[test]
    fn a_command_nobody_classified_is_refused_to_a_workspace_and_runs_headless() {
        let mut state = AppState::new();
        let vt = state.new_empty_vtab();
        let mut registry = build_registry();
        registry.register(
            CommandMeta::new("test.unclassified", "Unclassified", "A command nobody classified"),
            Box::new(|_: &mut AppState, _| Ok(ghostrealm_core::CmdOutcome::msg("ran"))),
        );
        let req = set("test.unclassified", json!({}));
        let reply = scoped(&mut state, &mut registry, vt, &req);
        assert!(error(&reply).contains("isn't open to a workspace's agent"), "{reply:?}");
        assert_eq!(handle(&req, &mut state, &mut registry)["message"], json!("ran"));
    }

    /// The commands a workspace's agent may run, with their arguments' names
    /// and the arguments [`every_command_open_to_an_agent_leaves_other_workspaces_alone`]
    /// runs each with (`{d}`: its folder). A command that gains an argument
    /// fails the pin below until it is classified again here.
    const OPEN_TO_AGENTS: &[(&str, &[&str], &str)] = &[
        ("workspace.rename", &["name"], r#"{"name":"agent's"}"#),
        ("workspace.mark_read", &[], "{}"),
        ("workspace.mark_unread", &[], "{}"),
        ("workspace.needs_input", &[], "{}"),
        ("workspace.dismiss", &[], "{}"),
        ("split.leftright", &[], "{}"),
        ("split.topbottom", &[], "{}"),
        ("pane.focus_next", &[], "{}"),
        ("pane.focus_prev", &[], "{}"),
        ("surface.new", &[], "{}"),
        ("terminal.new", &[], "{}"),
        ("file_browser.new", &[], "{}"),
        ("editor.new", &[], "{}"),
        ("editor.open", &["path"], r#"{"path":"{d}/b.txt"}"#),
        ("editor.toggle_comment", &[], "{}"),
        ("editor.cycle_view_mode", &[], "{}"),
        ("editor.toggle_undo_history", &[], "{}"),
        ("files.focus_sidebar", &[], "{}"),
        ("files.next_tab", &[], "{}"),
        ("files.prev_tab", &[], "{}"),
        ("file.open", &["path"], r#"{"path":"{d}/c.txt"}"#),
        ("file.show_in_browser", &["path"], r#"{"path":"{d}/a.txt"}"#),
        ("file.open_in_terminal", &["path"], r#"{"path":"{d}"}"#),
        ("file.duplicate", &["path"], r#"{"path":"{d}/a.txt"}"#),
        ("file.delete", &["path"], r#"{"path":"{d}/shared.txt"}"#),
        ("file.new_file", &["name", "dir"], r#"{"name":"new.txt","dir":"{d}"}"#),
        ("file.new_folder", &["name", "dir"], r#"{"name":"new","dir":"{d}"}"#),
        ("file.rename", &["name", "path"], r#"{"name":"renamed.txt","path":"{d}/c.txt"}"#),
        ("file.add_to_gitignore", &["path"], r#"{"path":"{d}/a.txt"}"#),
        ("file.add_to_git_exclude", &["path"], r#"{"path":"{d}/a.txt"}"#),
        ("pane.close", &[], "{}"),
        ("pane.close_all", &[], "{}"),
    ];

    /// Given the registry;
    /// when its commands open to a workspace's agent are listed with their
    /// arguments' names;
    /// then they are exactly those classified above, and every other command
    /// is refused for a reason of its own.
    #[test]
    fn the_commands_open_to_agents_and_their_arguments_are_pinned() {
        let registry = build_registry();
        let mut open: Vec<(&str, Vec<&str>)> = Vec::new();
        for m in registry.metas() {
            match m.agent {
                AgentReach::Refused(why) => assert_ne!(why, "not classified for a workspace's agent", "{}", m.id),
                AgentReach::Workspace | AgentReach::FocusedPane => open.push((m.id, m.args.iter().map(|a| a.name).collect())),
            }
        }
        let mut pinned: Vec<(&str, Vec<&str>)> = OPEN_TO_AGENTS.iter().map(|(id, args, _)| (*id, args.to_vec())).collect();
        open.sort();
        pinned.sort();
        assert_eq!(open, pinned);
        let focused_pane: Vec<&str> =
            registry.metas().filter(|m| m.agent == AgentReach::FocusedPane).map(|m| m.id).collect();
        assert_eq!(focused_pane, ["pane.close", "pane.close_all"]);
    }

    /// Given workspace A with a terminal and a Files surface, and workspace B,
    /// active, with a Files surface showing a file of A's folder;
    /// when A's agent runs every command open to it, in turn;
    /// then B stays active and its panes, surfaces and titles stay as they
    /// were, and Delete of the file B shows is refused and leaves it.
    #[test]
    fn every_command_open_to_an_agent_leaves_other_workspaces_alone() {
        let d = crate::plugin::testing::tmpdir("agent-scope-every");
        for f in ["a.txt", "b.txt", "c.txt", "shared.txt"] {
            std::fs::write(d.join(f), f).unwrap();
        }
        let mut state = AppState::new().with_shell_line("sleep 5");
        let a = state.new_vtab().unwrap();
        state.set_active_root_dir(&*d);
        state.open_file_in_new_surface(&d.join("a.txt")).unwrap();
        let b = state.new_empty_vtab();
        state.open_file_in_new_surface(&d.join("shared.txt")).unwrap();
        let mut registry = build_registry();
        let b_tree = |s: &AppState| s.describe_for(b).split_once('\n').map(|(_, panes)| panes.to_string());
        let before = b_tree(&state);
        assert!(before.as_deref().is_some_and(|t| t.contains("shared.txt")), "{before:?}");
        for (id, _, args) in OPEN_TO_AGENTS {
            let args: Json = serde_json::from_str(&args.replace("{d}", d.to_str().unwrap())).unwrap();
            let reply = scoped(&mut state, &mut registry, a, &set(id, args));
            assert_eq!(state.tree.active_vtab(), Some(b), "{id} left B active: {reply:?}");
            assert_eq!(b_tree(&state), before, "{id} left B as it was: {reply:?}");
            if *id == "file.delete" {
                assert!(error(&reply).contains("open outside this workspace"), "{reply:?}");
                assert!(d.join("shared.txt").exists());
            }
        }
    }

    /// Given workspace A with two terminals in split panes, and workspace B,
    /// active, with a terminal;
    /// when A's agent sends input with no `id`;
    /// then it goes to A's focused terminal.
    #[test]
    fn input_with_no_id_goes_to_the_agents_focused_surface() {
        let mut state = AppState::new().with_shell_line("sleep 5");
        let a = state.new_vtab().unwrap();
        let a_term = state.focused_surface().unwrap();
        let _b = state.new_vtab().unwrap();
        let mut registry = build_registry();
        let reply = scoped(&mut state, &mut registry, a, r#"{"op":"input","text":"x"}"#);
        assert_eq!(reply, json!({ "ok": true, "surface": a_term.0 }));
    }

    /// Given a workspace's agent;
    /// when it asks for help and searches;
    /// then both list only the commands open to it.
    #[test]
    fn help_and_search_list_only_the_commands_open_to_a_workspace() {
        let mut state = AppState::new();
        let vt = state.new_empty_vtab();
        let mut registry = build_registry();
        let reply = scoped(&mut state, &mut registry, vt, r#"{"op":"help"}"#);
        let mut listed: Vec<&str> = reply["commands"].as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap()).collect();
        let mut open: Vec<&str> = OPEN_TO_AGENTS.iter().map(|(id, _, _)| *id).collect();
        listed.sort();
        open.sort();
        assert_eq!(listed, open);
        // More than 20 commands match "e": the refused ones mustn't take
        // places among the 20.
        let reply = scoped(&mut state, &mut registry, vt, r#"{"op":"search","query":"e"}"#);
        let hits: Vec<&str> = reply["hits"].as_array().unwrap().iter().map(|h| h["id"].as_str().unwrap()).collect();
        assert_eq!(hits.len(), SEARCH_HITS, "{hits:?}");
        assert!(hits.iter().all(|id| open.contains(id)), "{hits:?}");
    }

    /// Given the window's guard on unsaved changes, a file edited in
    /// workspace A, and the person in workspace B;
    /// when A's agent deletes the file and the person presses Enter at once;
    /// then the question names A, ignores keys for a moment, and Enter
    /// cancels it, leaving the file; and while it is open, another of the
    /// agent's questions is refused and leaves it.
    #[test]
    fn an_agents_question_is_late_and_never_replaces_one_already_open() {
        let d = crate::plugin::testing::tmpdir("agent-scope-ask");
        let path = d.join("a.txt");
        std::fs::write(&path, "A").unwrap();
        let mut state = AppState::new().with_unsaved_guard();
        let a = state.new_empty_vtab();
        let sid = state.open_file_in_new_surface(&path).unwrap();
        assert!(state.write_input(sid, b"edit "));
        state.rename_active_vtab("agent's");
        let _b = state.new_empty_vtab();
        let mut registry = build_registry();
        let delete = set("file.delete", json!({ "path": path }));
        let reply = scoped(&mut state, &mut registry, a, &delete);
        assert_eq!(reply["ok"], json!(true), "{reply:?}");
        let asked = state.confirm().expect("asks about the unsaved changes").clone();
        assert!(asked.message.starts_with("From workspace \u{201c}agent's\u{201d}: "), "{}", asked.message);
        assert!(!asked.takes_keys_at(Instant::now()) && asked.enter_cancels());
        let reply = scoped(&mut state, &mut registry, a, &set("pane.close", json!({})));
        assert!(error(&reply).contains("a question is open"), "{reply:?}");
        assert_eq!(state.confirm(), Some(&asked), "the open question stays");
        state.enter_confirm().unwrap();
        assert!(state.confirm().is_none() && path.exists(), "Enter cancelled it");
    }

    /// Given workspace A with a terminal and, beside it in the same pane, a
    /// web page the person opened, and workspace B with a terminal;
    /// when A's agent reads the tree and the page, types into the page and
    /// B's terminal, lists and runs web commands, and closes the page;
    /// then the page and B's terminal are absent or unknown to it, every web
    /// command is refused, and the page stays.
    #[cfg(unix)]
    #[test]
    fn web_pages_and_other_workspaces_are_out_of_an_agents_reach() {
        use crate::plugins::web::tests::{stub_app_host, TEST_HOST};
        let dir = crate::plugin::testing::TmpDir::new("agent-scope-web");
        stub_app_host(&dir, &[], "cat > /dev/null");
        // A waker, as the window gives: the page is the person's.
        let mut state = AppState::new().with_shell_line("sleep 5").with_waker(std::sync::Arc::new(|| {}));
        let a = state.new_vtab().unwrap();
        let a_term = state.focused_surface().unwrap();
        let mut registry = build_registry();
        assert_eq!(handle(&set("web.new", json!({})), &mut state, &mut registry)["ok"], json!(true));
        let page = state.focused_surface().unwrap();
        let b = state.new_vtab().unwrap();
        let b_term = state.focused_surface().unwrap();
        let mut ask = |state: &mut AppState, req: &str| scoped(state, &mut registry, a, req);
        let tree = ask(&mut state, r#"{"op":"get","what":"tree"}"#)["tree"].as_str().unwrap().to_string();
        assert!(tree.contains(&format!("surface {} ", a_term.0)), "{tree}");
        for gone in [page, b_term] {
            assert!(!tree.contains(&format!("surface {} ", gone.0)), "{tree}");
            let unknown = format!("unknown surface {}", gone.0);
            let reply = ask(&mut state, &format!(r#"{{"op":"get","what":"surface","id":{}}}"#, gone.0));
            assert_eq!(error(&reply), unknown);
            let reply = ask(&mut state, &format!(r#"{{"op":"input","id":{},"text":"x"}}"#, gone.0));
            assert_eq!(error(&reply), unknown);
        }
        assert!(!tree.contains(&format!("vtab {} ", b.0)), "{tree}");
        let help = ask(&mut state, r#"{"op":"help"}"#).to_string();
        let search = ask(&mut state, r#"{"op":"search","query":"web"}"#).to_string();
        assert!(!help.contains("\"web.") && !search.contains("\"web."), "{help}\n{search}");
        for id in ["web.new", "web.eval", "web.close"] {
            let reply = ask(&mut state, &set(id, json!({})));
            assert!(error(&reply).contains(crate::app_state::WEB_PAGES), "{reply:?}");
        }
        // The page is focused in A's focused pane.
        state.focus_vtab(a);
        assert_eq!(state.focused_surface(), Some(page));
        state.focus_vtab(b);
        let reply = ask(&mut state, &set("pane.close", json!({})));
        assert!(error(&reply).contains("can't reach"), "{reply:?}");
        assert!(state.has_surface(page), "the page stays");
        TEST_HOST.with_borrow_mut(|h| *h = None);
    }
}

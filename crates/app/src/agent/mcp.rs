//! `ghostrealm agent --mcp` in a window's terminal: an MCP server on
//! stdin/stdout (newline-delimited JSON-RPC 2.0, MCP 2025-11-25) whose tools
//! are the agent protocol's ops, each call relayed on one connection to the
//! window's socket ([`super::client::Connection`]) as the terminal's
//! workspace.
//!
//! It holds no reach of its own: a call becomes one protocol request, which
//! the socket authorizes like any other ([`super::serve`]). The tool set is
//! fixed, so `tools/list` never goes stale when the person switches the
//! workspace's grant; `help` and `search` list the commands open at that
//! moment.

use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::Result;
use serde_json::{json, Map, Value as Json};

use super::client::Connection;

/// The MCP revision this server implements, answered to a client asking for
/// one it doesn't support.
pub const PROTOCOL_VERSION: &str = "2025-11-25";

/// Revisions whose initialize handshake and tool messages this server
/// speaks; a client asking for one gets it back.
const SUPPORTED: [&str; 2] = [PROTOCOL_VERSION, "2025-06-18"];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

const INSTRUCTIONS: &str = "Drives the ghostrealm window this server's terminal runs in, as that \
terminal's workspace's agent: that workspace's terminals and Files surfaces (every workspace's \
once the person grants it), and the commands `help` lists. Anything else is refused.";

/// Connect to `socket` with `token`, then serve MCP messages from `input`
/// until it ends, writing replies to `output`. A socket that can't be
/// reached or a refused token is an error before anything is read; a window
/// that hangs up is an error after the call it broke is answered.
pub fn serve(socket: &Path, token: &str, input: impl BufRead, output: impl Write) -> Result<()> {
    let mut conn = Connection::open(socket, token)?;
    serve_with(input, output, |line| conn.request(line))
}

/// [`serve`] with `channel` sending one protocol request line and returning
/// its reply line.
fn serve_with(mut input: impl BufRead, mut output: impl Write, mut channel: impl FnMut(&str) -> Result<String>) -> Result<()> {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if input.read_until(b'\n', &mut buf)? == 0 {
            return Ok(());
        }
        let (reply, hung_up) = match std::str::from_utf8(&buf) {
            Ok(line) if line.trim().is_empty() => continue,
            Ok(line) => handle(line.trim(), &mut channel),
            Err(e) => (Some(error(Json::Null, PARSE_ERROR, format!("parse error: {e}"))), None),
        };
        if let Some(reply) = reply {
            serde_json::to_writer(&mut output, &reply)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
        if let Some(e) = hung_up {
            return Err(e);
        }
    }
}

/// The reply to one message (`None` for a notification or a response), and
/// the channel's error if the window hung up.
fn handle(line: &str, channel: &mut impl FnMut(&str) -> Result<String>) -> (Option<Json>, Option<anyhow::Error>) {
    let msg: Json = match serde_json::from_str(line) {
        Ok(msg) => msg,
        Err(e) => return (Some(error(Json::Null, PARSE_ERROR, format!("parse error: {e}"))), None),
    };
    let Some(msg) = msg.as_object() else {
        let what = if msg.is_array() { "batches aren't supported" } else { "a message must be an object" };
        return (Some(error(Json::Null, INVALID_REQUEST, what)), None);
    };
    let id = match msg.get("id") {
        None => None,
        Some(id @ (Json::String(_) | Json::Number(_))) => Some(id.clone()),
        Some(_) => return (Some(error(Json::Null, INVALID_REQUEST, "`id` must be a string or a number")), None),
    };
    if msg.get("jsonrpc") != Some(&json!("2.0")) {
        return (Some(error(id.unwrap_or(Json::Null), INVALID_REQUEST, "`jsonrpc` must be \"2.0\"")), None);
    }
    let method = msg.get("method").and_then(Json::as_str);
    let (method, id) = match (method, id) {
        // Notifications (`initialized`, `cancelled`) need nothing: each call
        // is answered before the next is read.
        (Some(_), None) => return (None, None),
        // A response: this server sends no requests to answer.
        (None, _) if msg.contains_key("result") || msg.contains_key("error") => return (None, None),
        (None, id) => return (Some(error(id.unwrap_or(Json::Null), INVALID_REQUEST, "missing `method`")), None),
        (Some(method), Some(id)) => (method, id),
    };
    let params = msg.get("params").cloned().unwrap_or(Json::Null);
    match method {
        "initialize" => (Some(result(id, initialize(&params))), None),
        "ping" => (Some(result(id, json!({}))), None),
        "tools/list" => (Some(result(id, json!({ "tools": tools() }))), None),
        "tools/call" => call(id, &params, channel),
        other => (Some(error(id, METHOD_NOT_FOUND, format!("method not found: {other}"))), None),
    }
}

fn initialize(params: &Json) -> Json {
    let asked = params.get("protocolVersion").and_then(Json::as_str);
    let version = asked.filter(|v| SUPPORTED.contains(v)).unwrap_or(PROTOCOL_VERSION);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "ghostrealm", "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    })
}

/// The tools: one per protocol op an agent drives the window with. `pump`
/// is left out: `get` drains background output itself.
fn tools() -> Json {
    let id = |what: &str| json!({ "type": "integer", "description": what });
    json!([
        {
            "name": "help",
            "description": "List the commands this agent may run with `set`: id, title, description and arguments.",
            "inputSchema": { "type": "object", "additionalProperties": false },
        },
        {
            "name": "search",
            "description": "Fuzzy-search the commands this agent may run; returns ids, titles and scores.",
            "inputSchema": {
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"],
            },
        },
        {
            "name": "set",
            "description": "Run a command by id. A file command replies once its copy, move or check is done.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The command's id, as `help` or `search` lists it." },
                    "args": {
                        "type": "object",
                        "description": "The command's arguments, as `help` lists them.",
                        "additionalProperties": { "type": ["string", "integer", "boolean"] },
                    },
                    "workspace": id("Run it as open workspace N; needs the person's grant."),
                },
                "required": ["id"],
            },
        },
        {
            "name": "get",
            "description": "Read the window. `tree`: the workspaces, panes and surfaces this agent reaches, with their ids. `surface`: surface `id`'s content as text.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "what": { "type": "string", "enum": ["tree", "surface"], "default": "tree" },
                    "id": id("The surface, for `what: surface`."),
                },
            },
        },
        {
            "name": "input",
            "description": "Type text into a surface, as keys (\\r is Enter). Without `id`, the focused surface of this agent's workspace.",
            "inputSchema": {
                "type": "object",
                "properties": { "text": { "type": "string" }, "id": id("The surface.") },
                "required": ["text"],
            },
        },
    ])
}

/// Relay a `tools/call` as the protocol request `{"op": name, ...arguments}`.
/// The name picks the op; an `op` among the arguments is overwritten.
fn call(id: Json, params: &Json, channel: &mut impl FnMut(&str) -> Result<String>) -> (Option<Json>, Option<anyhow::Error>) {
    let Some(name) = params.get("name").and_then(Json::as_str) else {
        return (Some(error(id, INVALID_PARAMS, "tools/call needs a tool `name`")), None);
    };
    if !tools().as_array().unwrap().iter().any(|t| t["name"] == name) {
        return (Some(error(id, INVALID_PARAMS, format!("Unknown tool: {name}"))), None);
    }
    let mut request = match params.get("arguments") {
        None | Some(Json::Null) => Map::new(),
        Some(Json::Object(args)) => args.clone(),
        Some(_) => return (Some(result(id, failed("`arguments` must be an object".into()))), None),
    };
    request.insert("op".into(), json!(name));
    let reply = match channel(&Json::Object(request).to_string()) {
        Ok(reply) => reply,
        Err(e) => return (Some(result(id, failed(format!("{e:#}")))), Some(e)),
    };
    let reply = match serde_json::from_str::<Json>(&reply) {
        Ok(Json::Object(reply)) => reply,
        _ => return (Some(result(id, failed(format!("the window sent a reply that isn't JSON: {}", reply.trim())))), None),
    };
    (Some(result(id, tool_result(reply))), None)
}

/// A protocol reply as a tool result: an error is `isError`; a tree or a
/// surface's text is its own text block, unescaped, after the reply's
/// other fields.
fn tool_result(mut reply: Map<String, Json>) -> Json {
    if reply.remove("ok") != Some(json!(true)) {
        return failed(match reply.remove("error") {
            Some(Json::String(e)) => e,
            other => other.map_or_else(|| "refused, with no reason given".into(), |e| e.to_string()),
        });
    }
    let raw: Vec<String> = ["tree", "text"]
        .into_iter()
        .filter_map(|key| match reply.remove(key) {
            Some(Json::String(s)) => Some(s),
            Some(other) => {
                reply.insert(key.into(), other);
                None
            }
            None => None,
        })
        .collect();
    let mut content = Vec::new();
    if !reply.is_empty() || raw.is_empty() {
        content.push(text(Json::Object(reply).to_string()));
    }
    content.extend(raw.into_iter().map(text));
    json!({ "content": content, "isError": false })
}

fn failed(message: String) -> Json {
    json!({ "content": [text(message)], "isError": true })
}

fn text(s: String) -> Json {
    json!({ "type": "text", "text": s })
}

fn result(id: Json, result: Json) -> Json {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Json, code: i64, message: impl Into<String>) -> Json {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serve `input` with a channel that answers each request with
    /// `answer`: the reply lines, the requests sent, and the outcome.
    fn served(input: &[u8], mut answer: impl FnMut(&Json) -> Result<Json>) -> (Vec<Json>, Vec<Json>, Result<()>) {
        let mut out = Vec::new();
        let mut sent = Vec::new();
        let outcome = serve_with(input, &mut out, |line| {
            let request: Json = serde_json::from_str(line).unwrap();
            assert!(!line.contains('\n'), "{line}");
            sent.push(request.clone());
            Ok(format!("{}\n", answer(&request)?))
        });
        let out = String::from_utf8(out).unwrap();
        let replies = out.lines().map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}"))).collect();
        (replies, sent, outcome)
    }

    fn call_line(id: u32, name: &str, arguments: Json) -> String {
        json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": name, "arguments": arguments } }).to_string()
    }

    /// Given a client asking for a revision this server speaks, and one
    /// asking for one it doesn't;
    /// when each initializes;
    /// then the first gets its revision back, the second this server's, and
    /// both a tools capability and the server's name.
    #[test]
    fn initialize_answers_the_clients_revision_or_this_servers() {
        let input = [
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"1999-01-01","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#,
        ]
        .join("\n");
        let (replies, sent, outcome) = served(input.as_bytes(), |_| unreachable!());
        outcome.unwrap();
        assert!(sent.is_empty());
        assert_eq!(replies[0]["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(replies[1]["result"]["protocolVersion"], PROTOCOL_VERSION);
        for r in &replies {
            assert_eq!(r["result"]["capabilities"], json!({ "tools": {} }));
            assert_eq!(r["result"]["serverInfo"]["name"], "ghostrealm");
        }
    }

    /// Given a stream of blank and CRLF-ended lines, notifications, a
    /// client's response, malformed JSON, invalid UTF-8, a batch, a message
    /// without `jsonrpc`, a null id, a ping and an unknown method with a
    /// string id;
    /// when the server reads it;
    /// then each request gets exactly one reply line, in order, with the
    /// JSON-RPC error its fault calls for; notifications and responses get
    /// none; nothing reaches the window.
    #[test]
    fn framing_answers_each_request_once_and_nothing_else() {
        let mut input = Vec::new();
        for line in [
            "",
            "   ",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}"#,
            r#"{"jsonrpc":"2.0","id":7,"result":{}}"#,
            "{not json",
        ] {
            input.extend_from_slice(line.as_bytes());
            input.extend_from_slice(b"\r\n");
        }
        input.extend_from_slice(b"\xff\xfe\n");
        for line in [
            r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#,
            r#"{"id":2,"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":3}"#,
            r#"{"jsonrpc":"2.0","id":4,"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":"five","method":"resources/list"}"#,
        ] {
            input.extend_from_slice(line.as_bytes());
            input.push(b'\n');
        }
        // The last line has no newline.
        input.extend_from_slice(br#"{"jsonrpc":"2.0","id":6,"method":"ping"}"#);
        let (replies, sent, outcome) = served(&input, |_| unreachable!());
        outcome.unwrap();
        assert!(sent.is_empty());
        let codes: Vec<(Json, Json)> = replies.iter().map(|r| (r["id"].clone(), r["error"]["code"].clone())).collect();
        assert_eq!(
            codes,
            [
                (Json::Null, json!(PARSE_ERROR)),
                (Json::Null, json!(PARSE_ERROR)),
                (Json::Null, json!(INVALID_REQUEST)),
                (json!(2), json!(INVALID_REQUEST)),
                (Json::Null, json!(INVALID_REQUEST)),
                (json!(3), json!(INVALID_REQUEST)),
                (json!(4), Json::Null),
                (json!("five"), json!(METHOD_NOT_FOUND)),
                (json!(6), Json::Null),
            ]
        );
        assert_eq!(replies[6]["result"], json!({}));
        assert!(replies.iter().all(|r| r["jsonrpc"] == "2.0"));
    }

    /// Given the tool list;
    /// when a client lists it;
    /// then it holds the protocol's ops an agent drives the window with,
    /// each with an object input schema.
    #[test]
    fn the_tools_are_the_protocols_ops() {
        let (replies, _, outcome) = served(br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#, |_| unreachable!());
        outcome.unwrap();
        let tools = replies[0]["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["help", "search", "set", "get", "input"]);
        assert!(tools.iter().all(|t| t["inputSchema"]["type"] == "object" && t["description"].is_string()));
    }

    /// Given calls to an unknown tool, with arguments that aren't an object,
    /// with an `op` of their own, to `get` a tree, and one the window
    /// refuses;
    /// when the server relays them;
    /// then the first two never reach the window (a protocol error, then a
    /// tool error); the tool's name is the request's op; a tree is its own
    /// unescaped text block; a refusal is a tool error carrying the
    /// window's reason.
    #[test]
    fn a_call_is_one_request_with_its_tools_op() {
        let input = [
            call_line(1, "hello", json!({ "token": "x" })),
            call_line(2, "get", json!("tree")),
            call_line(3, "get", json!({ "op": "hello", "what": "tree" })),
            call_line(4, "set", json!({ "id": "app.quit" })),
        ]
        .join("\n");
        let (replies, sent, outcome) = served(input.as_bytes(), |req| {
            Ok(match req["op"].as_str() {
                Some("get") => json!({ "ok": true, "tree": "vtab 1\n  pane 2", "granted": false }),
                _ => json!({ "ok": false, "error": "app.quit isn't open to a workspace's agent" }),
            })
        });
        outcome.unwrap();
        assert_eq!(sent, [json!({ "op": "get", "what": "tree" }), json!({ "op": "set", "id": "app.quit" })]);
        assert_eq!(replies[0]["error"]["code"], INVALID_PARAMS);
        assert_eq!(replies[1]["result"]["isError"], true);
        assert_eq!(
            replies[2]["result"],
            json!({ "content": [
                { "type": "text", "text": "{\"granted\":false}" },
                { "type": "text", "text": "vtab 1\n  pane 2" },
            ], "isError": false })
        );
        assert_eq!(
            replies[3]["result"],
            json!({ "content": [{ "type": "text", "text": "app.quit isn't open to a workspace's agent" }], "isError": true })
        );
    }

    /// Given a window that hangs up;
    /// when a call is relayed;
    /// then the call is answered as a tool error and the server stops with
    /// the channel's error, reading nothing more.
    #[test]
    fn a_hang_up_answers_the_call_then_stops() {
        let input = [call_line(1, "help", json!({})), call_line(2, "help", json!({}))].join("\n");
        let (replies, sent, outcome) =
            served(input.as_bytes(), |_| Err(anyhow::anyhow!("the window hung up the agent channel")));
        assert_eq!(sent.len(), 1);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0]["result"]["isError"], true);
        assert!(format!("{:#}", outcome.unwrap_err()).contains("hung up"));
    }
}

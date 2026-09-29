//! The agent control channel: a line-delimited JSON protocol over any
//! reader/writer, exposing the thin fixed tool set from the brief:
//!
//! - `{"op":"help"}`                         → all command metas + arg schemas
//! - `{"op":"search","query":"split"}`       → matching command ids + scores
//! - `{"op":"set","id":"...","args":{...}}`  → run a command
//! - `{"op":"get","what":"tree"}`            → text dump of vtabs/panes/surfaces
//! - `{"op":"get","what":"surface","id":N}`  → a surface's grid as text
//! - `{"op":"input","text":"ls\r","id":N}`   → type into a surface (id optional,
//!   defaulting to the focused surface)
//! - `{"op":"pump"}`                         → drain terminal output
//!
//! One JSON object per request line, one JSON object per response line. Being
//! generic over `BufRead`/`Write` keeps it testable with in-memory buffers and
//! reusable for a socket later. It is the agent's drive + observe loop, and the
//! display-free way to test app behaviour end to end.

use std::io::{BufRead, Write};

use ghostrealm_core::{Args, CommandMeta, Registry, SurfaceId, Value};
use serde_json::{json, Value as Json};

use crate::app_state::{build_registry, AppState};

/// Run the REPL until the input reaches EOF.
pub fn run<R: BufRead, W: Write>(
    mut input: R,
    mut output: W,
    mut state: AppState,
) -> anyhow::Result<()> {
    let mut registry = build_registry();
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let response = handle(trimmed, &mut state, &mut registry);
        serde_json::to_writer(&mut output, &response)?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
    Ok(())
}

fn handle(line: &str, state: &mut AppState, registry: &mut Registry<AppState>) -> Json {
    let req: Json = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return json!({ "ok": false, "error": format!("invalid json: {e}") }),
    };
    match req.get("op").and_then(Json::as_str).unwrap_or("") {
        "help" => {
            let commands: Vec<Json> = registry.metas().map(meta_json).collect();
            json!({ "ok": true, "commands": commands })
        }
        "search" => {
            let q = req.get("query").and_then(Json::as_str).unwrap_or("");
            let hits: Vec<Json> = registry
                .search(q, 20)
                .into_iter()
                .map(|h| json!({ "id": h.meta.id, "title": h.meta.title, "score": h.score }))
                .collect();
            json!({ "ok": true, "hits": hits })
        }
        "set" => {
            let id = req.get("id").and_then(Json::as_str).unwrap_or("");
            let args = match parse_args(req.get("args")) {
                Ok(a) => a,
                Err(e) => return json!({ "ok": false, "error": e }),
            };
            match registry.execute(id, &args, state) {
                Ok(out) => json!({ "ok": true, "message": out.message }),
                Err(e) => json!({ "ok": false, "error": e.to_string() }),
            }
        }
        "get" => {
            state.pump_all();
            match req.get("what").and_then(Json::as_str).unwrap_or("tree") {
                "tree" => json!({ "ok": true, "tree": state.describe() }),
                "surface" => match req.get("id").and_then(Json::as_u64) {
                    Some(id) => match state.surface_text(SurfaceId(id)) {
                        Some(text) => json!({ "ok": true, "text": text }),
                        None => json!({ "ok": false, "error": format!("unknown surface {id}") }),
                    },
                    None => json!({ "ok": false, "error": "get surface needs an integer `id`" }),
                },
                other => json!({ "ok": false, "error": format!("unknown get target: {other}") }),
            }
        }
        "input" => {
            let text = req.get("text").and_then(Json::as_str).unwrap_or("");
            let id = match req.get("id").and_then(Json::as_u64) {
                Some(id) => Some(SurfaceId(id)),
                None => state.focused_surface(),
            };
            match id {
                Some(id) if state.write_input(id, text.as_bytes()) => {
                    json!({ "ok": true, "surface": id.0 })
                }
                Some(id) => json!({ "ok": false, "error": format!("unknown surface {}", id.0) }),
                None => json!({ "ok": false, "error": "no focused surface" }),
            }
        }
        "pump" => {
            state.pump_all();
            json!({ "ok": true })
        }
        "" => json!({ "ok": false, "error": "missing `op`" }),
        other => json!({ "ok": false, "error": format!("unknown op: {other}") }),
    }
}

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
            r#"{"op":"set","id":"workspace.new"}"#,
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
    fn unknown_op_is_reported() {
        let r = &drive(&[r#"{"op":"frobnicate"}"#])[0];
        assert_eq!(r["ok"], json!(false));
        assert!(r["error"].as_str().unwrap().contains("frobnicate"));
    }
}

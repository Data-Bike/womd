//! MCP protocol layer: newline-delimited JSON-RPC 2.0 over stdio (ADR-007).
//!
//! stdout carries only protocol frames; all diagnostics go to stderr. Domain
//! failures (file not found, denied path, git error) are reported as tool
//! results with `isError: true`; protocol failures (bad JSON, unknown method)
//! use JSON-RPC error codes.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

use crate::manifest;
use crate::tools::Tools;
use crate::{MCP_API_VERSION, MCP_PROTOCOL_COMPAT, MCP_PROTOCOL_VERSION, SERVER_NAME};

// JSON-RPC error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

/// One JSON-RPC message is one line; reject oversized frames so a client
/// cannot make the server allocate unbounded buffers (a tool payload is at
/// most `MAX_WRITE_BYTES` of content plus envelope).
const MAX_FRAME: usize = 72 * 1024 * 1024;

fn err_obj(code: i64, message: impl Into<String>) -> Value {
    json!({ "code": code, "message": message.into() })
}

fn ok_response(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn err_response(id: &Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": err_obj(code, message) })
}

/// Read one newline-delimited frame with a HARD memory cap. `BufRead::lines`
/// buffers the entire line first — a sender pushing gigabytes without '\n'
/// would exhaust memory before the frame-size check could ever run. Here
/// bytes beyond `MAX_FRAME` are consumed and discarded, and the frame is
/// reported oversized. `None` = clean EOF, `Some(Ok(..))` = frame,
/// `Some(Err(()))` = oversized frame.
fn read_frame(reader: &mut impl BufRead) -> std::io::Result<Option<Result<Vec<u8>, ()>>> {
    let mut buf: Vec<u8> = Vec::new();
    let mut oversized = false;
    loop {
        let avail = reader.fill_buf()?;
        if avail.is_empty() {
            return Ok(match (buf.is_empty(), oversized) {
                (true, false) => None,
                (_, true) => Some(Err(())),
                _ => Some(Ok(buf)),
            });
        }
        let nl = avail
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| i + 1)
            .unwrap_or(avail.len());
        let line_ended = nl > 0 && avail[nl - 1] == b'\n';
        if !oversized && buf.len() + nl <= MAX_FRAME {
            buf.extend_from_slice(&avail[..nl]);
        } else {
            oversized = true;
        }
        reader.consume(nl);
        if line_ended {
            return Ok(Some(if oversized { Err(()) } else { Ok(buf) }));
        }
    }
}

/// The MCP server: wraps `Tools` with the protocol state machine.
pub struct McpServer {
    tools: Tools,
}

impl McpServer {
    pub fn new(tools: Tools) -> Self {
        Self { tools }
    }

    /// Serve stdin/stdout until EOF. Each line is one JSON-RPC message.
    pub fn run_stdio(
        &self,
        mut reader: impl BufRead,
        mut writer: impl Write,
    ) -> std::io::Result<()> {
        let write_resp = |w: &mut dyn Write, resp: &Value| -> std::io::Result<()> {
            writeln!(w, "{}", serde_json::to_string(resp).unwrap_or_default())?;
            w.flush()
        };
        loop {
            let frame = match read_frame(&mut reader)? {
                None => return Ok(()),
                Some(f) => f,
            };
            let bytes = match frame {
                Ok(b) => b,
                // Oversized frame — already consumed through its newline.
                Err(()) => {
                    write_resp(
                        &mut writer,
                        &err_response(
                            &Value::Null,
                            INVALID_REQUEST,
                            "message exceeds the frame limit",
                        ),
                    )?;
                    continue;
                }
            };
            let line = match String::from_utf8(bytes) {
                Ok(l) => l,
                // A malformed frame (non-UTF-8 bytes) must not kill the
                // session — report it and keep serving.
                Err(e) => {
                    write_resp(
                        &mut writer,
                        &err_response(&Value::Null, PARSE_ERROR, e.to_string()),
                    )?;
                    continue;
                }
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let msg: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(e) => {
                    write_resp(
                        &mut writer,
                        &err_response(&Value::Null, PARSE_ERROR, e.to_string()),
                    )?;
                    continue;
                }
            };
            if let Some(resp) = self.handle(&msg) {
                write_resp(&mut writer, &resp)?;
            }
        }
    }

    /// Handle one parsed message. Returns `None` for notifications.
    pub fn handle(&self, msg: &Value) -> Option<Value> {
        // A scalar/array (e.g. a JSON-RPC batch, which we don't support) is
        // not a request — answer Invalid Request instead of silently
        // dropping it and leaving the client waiting forever.
        if !msg.is_object() {
            return Some(err_response(
                &Value::Null,
                INVALID_REQUEST,
                "request must be a JSON-RPC object",
            ));
        }
        let method = msg.get("method").and_then(Value::as_str);
        let id = msg.get("id").cloned();
        let Some(method) = method else {
            return id.map(|i| err_response(&i, INVALID_REQUEST, "missing 'method'"));
        };
        match id {
            None => None, // notification: initialized, cancelled, progress — ignore
            Some(id) => Some(self.dispatch(
                &id,
                method,
                msg.get("params").cloned().unwrap_or(Value::Null),
            )),
        }
    }

    fn dispatch(&self, id: &Value, method: &str, params: Value) -> Value {
        match method {
            "initialize" => ok_response(id, self.initialize(&params)),
            "ping" => ok_response(id, json!({})),
            "tools/list" => ok_response(id, self.tools_list()),
            "tools/call" => self.tools_call(id, &params),
            "resources/list" => ok_response(id, self.resources_list()),
            // Spec method clients may call unconditionally; we publish the
            // womd://file/{path} template so generic clients can discover it.
            "resources/templates/list" => ok_response(
                id,
                json!({
                    "resourceTemplates": manifest::resource_template_specs()
                }),
            ),
            "resources/read" => self.resources_read(id, &params),
            "prompts/list" => ok_response(id, json!({ "prompts": manifest::prompt_specs() })),
            "prompts/get" => self.prompts_get(id, &params),
            "logging/setLevel" => ok_response(id, json!({})),
            // Lifecycle: spec requires `shutdown` to return null; `exit`
            // arrives as a notification (no id) — the stdio loop ends on EOF.
            "shutdown" => ok_response(id, Value::Null),
            "completion/complete" => ok_response(id, json!({ "completion": { "values": [] } })),
            _ => err_response(id, METHOD_NOT_FOUND, format!("unknown method '{method}'")),
        }
    }

    fn initialize(&self, params: &Value) -> Value {
        let requested = params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or(MCP_PROTOCOL_VERSION);
        let negotiated =
            if requested == MCP_PROTOCOL_VERSION || MCP_PROTOCOL_COMPAT.contains(&requested) {
                requested
            } else {
                MCP_PROTOCOL_VERSION
            };
        json!({
            "protocolVersion": negotiated,
            "capabilities": {
                "tools": { "listChanged": false },
                "resources": { "subscribe": false, "listChanged": false },
                "prompts": { "listChanged": false },
            },
            "serverInfo": {
                "name": SERVER_NAME,
                "version": env!("CARGO_PKG_VERSION"),
            },
            "womd": {
                "apiVersion": MCP_API_VERSION,
                "workspaceRoot": self.tools.workspace().root().display().to_string(),
            },
            "instructions": concat!(
                "WoMD document workspace for AI-assisted authoring. ",
                "All tools take workspace-relative paths; absolute paths, '..' escapes, ",
                "symlink escapes, .git internals, OS system folders and credential dirs are refused. ",
                "There is no code execution — documents only: executables, secrets ",
                "(.env, keys, token stores) and build/CI/app configuration cannot be ",
                "written (most are not even readable). ",
                "Every mutating tool creates its own git commit: pass 'commit_message' ",
                "(a name YOU choose) on each create/write/edit/delete/move; commit:false ",
                "opts out for scratch work. ",
                "Prefer read_document -> edit_document/markdown_block_edit over write_document ",
                "to keep diffs minimal. Call api_manifest for the full versioned contract ",
                "and womd_interaction_guide for the recommended workflow."
            ),
        })
    }

    fn tools_list(&self) -> Value {
        let tools: Vec<Value> = manifest::tool_specs()
            .iter()
            .map(|t| {
                let mut o = json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.input_schema,
                    "annotations": { "readOnlyHint": is_read_only(t.name) },
                    "womd": { "since": t.since },
                });
                if let Some(dep) = t.deprecated_in {
                    o["womd"]["deprecatedIn"] = json!(dep);
                }
                if let Some(rev) = t.revised_in {
                    o["womd"]["revisedIn"] = json!(rev);
                }
                o
            })
            .collect();
        json!({ "tools": tools })
    }

    fn tools_call(&self, id: &Value, params: &Value) -> Value {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        if name.is_empty() {
            return err_response(id, INVALID_PARAMS, "tools/call requires 'name'");
        }
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        // A panic inside a tool must not kill the session — every tool takes
        // `&self` so there is no shared mutable state to leave inconsistent;
        // report it as a tool error and keep serving.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.tools.call(name, &args)
        }));
        let result = match outcome {
            Ok(r) => r,
            Err(_) => Err("internal tool error (the operation failed unexpectedly)".to_string()),
        };
        match result {
            Ok(result) => ok_response(
                id,
                json!({
                    "content": [{
                        "type": "text",
                        "text": serde_json::to_string_pretty(&result).unwrap_or_default(),
                    }],
                    "structuredContent": result,
                    "isError": false,
                }),
            ),
            Err(message) => ok_response(
                id,
                json!({
                    "content": [{ "type": "text", "text": message }],
                    "isError": true,
                }),
            ),
        }
    }

    fn resources_list(&self) -> Value {
        json!({ "resources": manifest::resource_specs() })
    }

    fn resources_read(&self, id: &Value, params: &Value) -> Value {
        let uri = params.get("uri").and_then(Value::as_str).unwrap_or("");
        let mk = |text: String, mime: &str| {
            ok_response(
                id,
                json!({
                    "contents": [{ "uri": uri, "mimeType": mime, "text": text }]
                }),
            )
        };
        match uri {
            "womd://manifest" => mk(
                serde_json::to_string_pretty(&manifest::manifest_json(
                    &self.tools.workspace().root().display().to_string(),
                ))
                .unwrap_or_default(),
                "application/json",
            ),
            "womd://tree" => {
                match self
                    .tools
                    .call("list_directory", &json!({"path": ".", "depth": 3}))
                {
                    Ok(v) => mk(
                        serde_json::to_string_pretty(&v).unwrap_or_default(),
                        "application/json",
                    ),
                    Err(m) => err_response(id, INTERNAL_ERROR, m),
                }
            }
            "womd://docs/interaction-guide" => mk(interaction_guide(), "text/markdown"),
            _ if uri.starts_with("womd://file/") => {
                // Clients URI-encode the path segment (spaces, '#', '?') —
                // decode before resolving through the sandbox. A resource
                // must serve the RAW bytes — read_document's numbered,
                // line-capped view would silently corrupt the contract.
                let rel = &percent_decode(&uri["womd://file/".len()..]);
                match self.tools.read_document_raw(rel) {
                    Ok(v) => mk(v, "text/plain"),
                    Err(m) => err_response(id, INVALID_PARAMS, m),
                }
            }
            _ => err_response(id, INVALID_PARAMS, format!("unknown resource '{uri}'")),
        }
    }

    fn prompts_get(&self, id: &Value, params: &Value) -> Value {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let text = match name {
            "womd_interaction_guide" => interaction_guide(),
            "womd_editing_playbook" => editing_playbook(
                params
                    .get("arguments")
                    .and_then(|a| a.get("task"))
                    .and_then(Value::as_str),
            ),
            _ => return err_response(id, INVALID_PARAMS, format!("unknown prompt '{name}'")),
        };
        ok_response(
            id,
            json!({
                "description": name,
                "messages": [{
                    "role": "user",
                    "content": { "type": "text", "text": text }
                }]
            }),
        )
    }
}

/// Decode percent-encoded bytes in a URI path segment (`%20`, `%23`, …).
/// Invalid `%` sequences are left verbatim — the sandbox then resolves the
/// literal name safely.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| -> Option<u8> {
                match b {
                    b'0'..=b'9' => Some(b - b'0'),
                    b'a'..=b'f' => Some(b - b'a' + 10),
                    b'A'..=b'F' => Some(b - b'A' + 10),
                    _ => None,
                }
            };
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

fn is_read_only(name: &str) -> bool {
    matches!(
        name,
        "list_directory"
            | "read_document"
            | "document_info"
            | "search_documents"
            | "find_files"
            | "markdown_outline"
            | "git_status"
            | "git_diff"
            | "git_log"
            | "api_manifest"
    )
}

/// Strategy text served via prompt + `womd://docs/interaction-guide`.
/// Keep in sync with docs/mcp.md — it is part of the versioned contract.
fn interaction_guide() -> String {
    let names: Vec<&str> = manifest::tool_specs().iter().map(|t| t.name).collect();
    format!(
        "# WoMD MCP interaction guide (API {MCP_API_VERSION})\n\n\
         You are connected to a sandboxed document workspace. You may create, read,\n\
         edit, move, delete and search documents; you cannot run programs, access\n\
         network, or touch anything outside the workspace root.\n\n\
         ## Recommended workflow\n\
         1. `list_directory` (depth 2-3) to see what exists.\n\
         2. `read_document` / `markdown_outline` to understand a document.\n\
         3. Prefer `edit_document` (search/line/byte ops) or `markdown_block_edit`\n\
         (AST-level surgery) over `write_document` — they produce minimal diffs.\n\
         4. Use `dry_run: true` on `edit_document` before committing large changes.\n\
         5. `search_documents` / `find_files` to locate content across the workspace.\n\
         6. `git_status` / `git_diff` / `git_log` are read-only and safe to use for\n\
         context.\n\n\
         ## Versioning of changes (required)\n\
         Every mutating tool call becomes a separate git commit. Pass a short,\n\
         descriptive `commit_message` YOU write (e.g. \"add onboarding draft\",\n\
         \"tighten intro paragraph\") on every create/write/edit/delete/move.\n\
         `commit:false` opts out for scratch work. The workspace is `git init`'d\n\
         automatically on the first commit.\n\n\
         ## Safety rules enforced server-side\n\
         - Paths are relative to the workspace root; `..`, absolute paths and symlink\n\
         escapes are rejected. `.git` and `*.womd-tmp-*` are invisible to you.\n\
         - The root can never be the home directory, Desktop, a drive root, or an OS\n\
         system directory; nested folders inside a valid root are fully usable.\n\
         - You cannot create or modify executables (exe/bat/ps1/js/msi/app/dmg/…),\n\
         credential files (.env, id_rsa, *.pem/.key/.p12, .netrc), build/CI/app\n\
         configuration (build.rs, Makefile, package.json, Cargo.toml,\n\
         .github/workflows, CI configs, Dockerfile) or editor/agent config dirs\n\
         (.vscode, .claude, .cursor, .devin…). Credential names are not readable\n\
         either and never appear in listings.\n\
         - edit_document / append_document / markdown_block_edit /\n\
         markdown_toggle_task are compare-and-save: if the file changed on disk\n\
         since you read it, the call fails — re-read and retry. write_document\n\
         offers the same guarantee via optional expected_content. Bulk traversal\n\
         (list/find/search) never follows symlinks.\n\n\
         ## Versioning\n\
         This contract is semver-versioned ({MCP_API_VERSION}). `api_manifest` returns\n\
         every tool with the API version that introduced it (`since`) and deprecation\n\
         markers. If a tool is missing here, it does not exist — do not guess names.\n\n\
         Tools ({n}): {list}\n",
        n = names.len(),
        list = names.join(", "),
    )
}

fn editing_playbook(task: Option<&str>) -> String {
    let task = task.unwrap_or("edit a document safely");
    format!(
        "# WoMD editing playbook — task: {task}\n\n\
         Recipes (all paths workspace-relative):\n\n\
         - Rewrite whole file: `write_document` {{path, content}}.\n\
         - Fix a phrase: `edit_document` {{path, edits:[{{type:'replace', search, \
         replace, occurrence:'first'|'last'|'all', expected_count}}]}}.\n\
         - Rewrite lines N..M: {{type:'replace_lines', start_line:N, end_line:M, \
         content}}; insert before line N: {{type:'insert_lines', line:N, content}};\n\
         append: line = total_lines + 1.\n\
         - Markdown surgery: `markdown_outline` → pick `index` → `markdown_block_edit`\n\
         {{op:{{type:'replace_block'|'delete_block'|...}}}} or\n\
         {{type:'insert_block', at, content}}. Only that byte range changes.\n\
         - Toggle a checklist item: `markdown_toggle_task` {{path, line}}.\n\
         - Organize: `create_directory`, `move_document`, `delete_document`\n\
         (recursive:true for folders).\n\
         - Preview first: `edit_document` with `dry_run:true` returns the result\n\
         without saving.\n\n\
         Versioning (required): every mutating call creates its own git\n\
         commit — pass `commit_message` (a name YOU choose) on each\n\
         create/write/edit/delete/move; `commit:false` only for scratch work.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::Workspace;

    fn server() -> (tempfile::TempDir, McpServer) {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        (dir, McpServer::new(Tools::new(ws)))
    }

    #[test]
    fn initialize_negotiates_protocol() {
        let (_d, s) = server();
        let r = s
            .handle(&json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2024-11-05"}
            }))
            .unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2024-11-05");
        assert!(r["result"]["womd"]["apiVersion"].as_str().is_some());
    }

    #[test]
    fn tools_list_and_call() {
        let (_d, s) = server();
        let r = s
            .handle(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
            .unwrap();
        let tools = r["result"]["tools"].as_array().unwrap();
        assert!(tools.iter().any(|t| t["name"] == "edit_document"));

        let r = s.handle(&json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"create_document","arguments":{"path":"a.md","content":"# Hi\n","commit":false}}
        })).unwrap();
        assert_eq!(r["result"]["isError"], false);
    }

    /// `is_read_only` is a hand-maintained mirror of the tool list: a
    /// read-only tool added to `Tools::call` but not to that list would be
    /// advertised with `readOnlyHint: false`, and vice versa. The manifest
    /// schema is the source of truth — mutating tools carry the `commit`
    /// parameter, read-only ones do not.
    #[test]
    fn read_only_classification_matches_manifest() {
        for spec in manifest::tool_specs() {
            let ro = is_read_only(spec.name);
            let mutating = spec.input_schema["properties"].get("commit").is_some();
            assert_eq!(
                ro, !mutating,
                "tool '{}' readOnlyHint disagrees with its commit params",
                spec.name
            );
        }
    }

    #[test]
    fn tool_errors_are_iserror_not_protocol_errors() {
        let (_d, s) = server();
        let r = s
            .handle(&json!({
                "jsonrpc":"2.0","id":4,"method":"tools/call",
                "params":{"name":"read_document","arguments":{"path":"nope.md"}}
            }))
            .unwrap();
        assert_eq!(r["result"]["isError"], true);
        assert!(r.get("error").is_none());
    }

    #[test]
    fn unknown_method_and_notification() {
        let (_d, s) = server();
        let r = s
            .handle(&json!({"jsonrpc":"2.0","id":5,"method":"bogus/x"}))
            .unwrap();
        assert_eq!(r["error"]["code"], METHOD_NOT_FOUND);
        // notifications get no response
        assert!(
            s.handle(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
                .is_none()
        );
    }

    #[test]
    fn resources_and_prompts() {
        let (_d, s) = server();
        let r = s
            .handle(&json!({
                "jsonrpc":"2.0","id":6,"method":"resources/read",
                "params":{"uri":"womd://manifest"}
            }))
            .unwrap();
        assert!(
            r["result"]["contents"][0]["text"]
                .as_str()
                .unwrap()
                .contains("api_version")
        );

        let r = s
            .handle(&json!({
                "jsonrpc":"2.0","id":7,"method":"prompts/get",
                "params":{"name":"womd_interaction_guide"}
            }))
            .unwrap();
        assert!(
            r["result"]["messages"][0]["content"]["text"]
                .as_str()
                .unwrap()
                .contains("edit_document")
        );
    }

    #[test]
    fn malformed_frames_get_invalid_request() {
        let (_d, s) = server();
        for bad in [json!([{"id":1,"method":"ping"}]), json!(42), json!("x")] {
            let r = s
                .handle(&bad)
                .expect("malformed input must still get an error response");
            assert_eq!(r["error"]["code"], INVALID_REQUEST, "{bad}");
        }
        // id without method → error; method non-string → error.
        let r = s.handle(&json!({"jsonrpc":"2.0","id":9})).unwrap();
        assert_eq!(r["error"]["code"], INVALID_REQUEST);
        let r = s
            .handle(&json!({"jsonrpc":"2.0","id":10,"method":7}))
            .unwrap();
        assert_eq!(r["error"]["code"], INVALID_REQUEST);
        // A notification-shaped malformed call (method but no id) → silent.
        assert!(s.handle(&json!({"method":7})).is_none());
    }

    #[test]
    fn uri_paths_are_percent_decoded() {
        assert_eq!(percent_decode("dir%20a/f%23.md"), "dir a/f#.md");
        assert_eq!(percent_decode("a%2fb"), "a/b"); // %2F decodes; sandbox still blocks '..'
        assert_eq!(percent_decode("bad%zz%1"), "bad%zz%1"); // invalid stays verbatim
        assert_eq!(percent_decode("plain.md"), "plain.md");

        let (d, s) = server();
        std::fs::write(d.path().join("my doc.md"), b"hi").unwrap();
        let r = s
            .handle(&json!({
                "jsonrpc":"2.0","id":11,"method":"resources/read",
                "params":{"uri":"womd://file/my%20doc.md"}
            }))
            .unwrap();
        assert!(
            r["result"]["contents"][0]["text"]
                .as_str()
                .unwrap()
                .contains("hi")
        );
    }

    /// An endless non-newline stream must not allocate unbounded memory —
    /// the oversized frame is discarded and the NEXT frame still parses.
    #[test]
    fn oversized_frame_is_discarded_and_session_survives() {
        let (_d, s) = server();
        let mut input = vec![b'x'; MAX_FRAME + 100];
        input.push(b'\n');
        input.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n");
        let mut out = Vec::new();
        s.run_stdio(std::io::BufReader::new(&input[..]), &mut out)
            .unwrap();
        let text = String::from_utf8(out).unwrap();
        let mut lines = text.lines();
        let err: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(err["error"]["code"], INVALID_REQUEST);
        let ok: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(
            ok["id"], 1,
            "ping after the oversized frame must still be answered"
        );
    }

    #[test]
    fn read_frame_edges() {
        // EOF with no newline still yields the partial frame.
        let mut r = std::io::BufReader::new(&b"abc"[..]);
        assert!(matches!(read_frame(&mut r).unwrap(), Some(Ok(b)) if b == b"abc"));
        // Clean EOF → None.
        let mut r = std::io::BufReader::new(&b""[..]);
        assert!(read_frame(&mut r).unwrap().is_none());
        // Small frames split correctly.
        let mut r = std::io::BufReader::new(&b"a\nb\n"[..]);
        assert!(matches!(read_frame(&mut r).unwrap(), Some(Ok(b)) if b == b"a\n"));
        assert!(matches!(read_frame(&mut r).unwrap(), Some(Ok(b)) if b == b"b\n"));
        assert!(read_frame(&mut r).unwrap().is_none());
    }

    #[test]
    fn denied_path_surfaces_as_tool_error() {
        let (_d, s) = server();
        let r = s
            .handle(&json!({
                "jsonrpc":"2.0","id":8,"method":"tools/call",
                "params":{"name":"read_document","arguments":{"path":"../escape.txt"}}
            }))
            .unwrap();
        assert_eq!(r["result"]["isError"], true);
        assert!(
            r["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("workspace root")
        );
    }

    /// Spec lifecycle: `shutdown` is a request that must return `null`,
    /// not Method Not Found.
    #[test]
    fn shutdown_returns_null_result() {
        let (_d, s) = server();
        let r = s
            .handle(&json!({
                "jsonrpc":"2.0","id":12,"method":"shutdown"
            }))
            .unwrap();
        assert_eq!(r["id"], 12);
        assert!(
            r.get("result").is_some(),
            "shutdown must be a result, not an error"
        );
        assert!(r["result"].is_null());
    }

    /// `resources/templates/list` must advertise the womd://file/{path}
    /// template — spec-compliant discovery for the dynamic resource.
    #[test]
    fn resource_templates_advertise_file_uri_template() {
        let (_d, s) = server();
        let r = s
            .handle(&json!({
                "jsonrpc":"2.0","id":11,"method":"resources/templates/list"
            }))
            .unwrap();
        let tpls = r["result"]["resourceTemplates"].as_array().unwrap();
        assert!(
            tpls.iter()
                .any(|t| t["uriTemplate"] == "womd://file/{path}")
        );
    }

    /// `womd://file/` must serve raw bytes — not read_document's
    /// line-numbered view and not a 20 000-line-capped excerpt.
    #[test]
    fn file_resource_returns_raw_unnumbered_content() {
        let (d, s) = server();
        let body = (1..=3).map(|i| format!("line {i}\n")).collect::<String>();
        std::fs::write(d.path().join("doc.md"), &body).unwrap();
        let r = s
            .handle(&json!({
                "jsonrpc":"2.0","id":9,"method":"resources/read",
                "params":{"uri":"womd://file/doc.md"}
            }))
            .unwrap();
        let text = r["result"]["contents"][0]["text"].as_str().unwrap();
        assert_eq!(
            text, body,
            "resource must be verbatim, not '     1\\tline 1'"
        );
    }
}

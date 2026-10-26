// Copyright 2026 Oxide Computer Company

//! End-to-end test: run the server binary and speak JSON-RPC over stdio.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(20);

struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<Value>,
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Client {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_x4c-lsp"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn x4c-lsp");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let mut r = BufReader::new(stdout);
            loop {
                let mut len = None;
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let line = line.trim_end();
                    if line.is_empty() {
                        break;
                    }
                    if let Some(v) = line.strip_prefix("Content-Length: ") {
                        len = v.parse::<usize>().ok();
                    }
                }
                let mut buf = vec![0; len.expect("Content-Length header")];
                if r.read_exact(&mut buf).is_err() {
                    return;
                }
                let msg = serde_json::from_slice(&buf).expect("valid JSON");
                if tx.send(msg).is_err() {
                    return;
                }
            }
        });
        Self { child, stdin, rx }
    }

    fn send(&mut self, msg: Value) {
        let body = msg.to_string();
        let stdin = self.stdin.as_mut().unwrap();
        write!(stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        stdin.flush().unwrap();
    }

    fn request(&mut self, id: i64, method: &str, params: Value) {
        self.send(json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params,
        }));
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({
            "jsonrpc": "2.0", "method": method, "params": params,
        }));
    }

    /// Receive messages until one satisfies `pred`, answering server
    /// requests along the way. Panics after [`TIMEOUT`].
    fn wait_for(&mut self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let msg = self
                .rx
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
            if pred(&msg) {
                return msg;
            }
            if msg.get("method").is_some() && msg.get("id").is_some() {
                // A server-to-client request, e.g. client/registerCapability.
                let id = msg["id"].clone();
                self.send(json!({"jsonrpc": "2.0", "id": id, "result": null}));
            }
        }
    }

    fn response(&mut self, id: i64) -> Value {
        self.wait_for(&format!("response {id}"), |m| {
            m.get("method").is_none() && m["id"] == json!(id)
        })
    }

    fn diagnostics(&mut self, uri: &str) -> Value {
        self.wait_for(&format!("diagnostics for {uri}"), |m| {
            m["method"] == "textDocument/publishDiagnostics"
                && m["params"]["uri"] == uri
        })["params"]
            .clone()
    }
}

#[test]
fn version_flag() {
    let out = Command::new(env!("CARGO_BIN_EXE_x4c-lsp"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.starts_with("x4c-lsp "), "{stdout}");
}

#[test]
fn diagnostics_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().canonicalize().unwrap();
    let file = ws.join("prog.p4");
    // The string literal is unterminated. Its end column differs between
    // UTF-8 (16) and UTF-16 (13) because of 'é' and '𝄞'.
    let text = "// test\n@name(\"é𝄞abc\n) const bit<8> X = 1;\n";
    std::fs::write(&file, text).unwrap();
    let ws_uri = format!("file://{}", ws.display());
    let uri = format!("file://{}", file.display());

    let mut c = Client::spawn();
    c.request(
        1,
        "initialize",
        json!({
            "processId": null,
            "rootUri": ws_uri,
            "workspaceFolders": [{"uri": ws_uri, "name": "ws"}],
            "capabilities": {
                "general": {"positionEncodings": ["utf-8", "utf-16"]},
                "workspace": {
                    "didChangeWatchedFiles": {"dynamicRegistration": true},
                },
            },
        }),
    );
    let init = c.response(1);
    let caps = &init["result"]["capabilities"];
    assert_eq!(caps["positionEncoding"], "utf-8");
    assert_eq!(caps["textDocumentSync"]["change"], 2);
    assert_eq!(caps["renameProvider"]["prepareProvider"], true);
    assert_eq!(init["result"]["serverInfo"]["name"], "x4c-lsp");

    c.notify("initialized", json!({}));
    let reg = c.wait_for("client/registerCapability", |m| {
        m["method"] == "client/registerCapability"
    });
    assert_eq!(
        reg["params"]["registrations"][0]["method"],
        "workspace/didChangeWatchedFiles"
    );
    c.send(json!({"jsonrpc": "2.0", "id": reg["id"], "result": null}));

    c.notify(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": uri, "languageId": "p4", "version": 1, "text": text,
        }}),
    );
    let d = c.diagnostics(&uri);
    assert_eq!(d["version"], 1);
    let diags = d["diagnostics"].as_array().unwrap();
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0]["message"], "unterminated string literal");
    assert_eq!(diags[0]["source"], "x4c-lsp");
    assert_eq!(diags[0]["severity"], 1);
    assert_eq!(
        diags[0]["range"],
        json!({
            "start": {"line": 1, "character": 6},
            "end": {"line": 1, "character": 16},
        })
    );

    // Close the string with an incremental edit at the UTF-8 position.
    c.notify(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri, "version": 2},
            "contentChanges": [{
                "range": {
                    "start": {"line": 1, "character": 16},
                    "end": {"line": 1, "character": 16},
                },
                "text": "\"",
            }],
        }),
    );
    let d = c.diagnostics(&uri);
    assert_eq!(d["version"], 2);
    assert_eq!(d["diagnostics"], json!([]));

    // Errors never take the server down.
    c.request(2, "x4c/unknownMethod", json!({}));
    assert_eq!(c.response(2)["error"]["code"], -32601);
    c.request(3, "textDocument/hover", json!({"bogus": true}));
    assert_eq!(c.response(3)["error"]["code"], -32602);
    // Unknown notifications and cancellation are ignored.
    c.notify("$/cancelRequest", json!({"id": 2}));
    c.notify("x4c/unknownNotification", json!(null));

    // Queries on documents that belong to no analysis return empty results.
    c.request(
        4,
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": format!("{ws_uri}/notes.txt")}}),
    );
    assert_eq!(c.response(4)["result"], Value::Null);
    c.request(
        5,
        "textDocument/definition",
        json!({
            "textDocument": {"uri": uri},
            "position": {"line": 2, "character": 16},
        }),
    );
    assert!(c.response(5)["result"].is_array());

    c.request(6, "shutdown", Value::Null);
    assert_eq!(c.response(6)["result"], Value::Null);
    c.notify("exit", Value::Null);
    drop(c.stdin.take());
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = c.child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "server did not exit");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "{status}");
}

#[test]
fn exit_without_shutdown_fails() {
    let mut c = Client::spawn();
    c.request(1, "initialize", json!({"capabilities": {}}));
    let init = c.response(1);
    // No positionEncodings offered: UTF-16 is the mandatory default.
    assert_eq!(init["result"]["capabilities"]["positionEncoding"], "utf-16");
    c.notify("initialized", json!({}));
    c.notify("exit", Value::Null);
    drop(c.stdin.take());
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = c.child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "server did not exit");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(1));
}

/// Defines toggled with `workspace/executeCommand`, directly and through
/// the `window/showMessageRequest` menu, change what is analyzed.
#[test]
fn configuration_commands() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().canonicalize().unwrap();
    let file = ws.join("prog.p4");
    let text = "control c(inout bit<8> x) {
    apply {
#ifdef MULTICAST
        x = missing;
#endif
    }
}
";
    std::fs::write(&file, text).unwrap();
    let ws_uri = format!("file://{}", ws.display());
    let uri = format!("file://{}", file.display());
    let errors = |d: &Value| -> Vec<String> {
        d["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|d| d["severity"] == 1)
            .map(|d| d["message"].as_str().unwrap().to_string())
            .collect()
    };

    let mut c = Client::spawn();
    c.request(
        1,
        "initialize",
        json!({
            "processId": null,
            "rootUri": ws_uri,
            "workspaceFolders": [{"uri": ws_uri, "name": "ws"}],
            "capabilities": {},
        }),
    );
    let init = c.response(1);
    let commands =
        &init["result"]["capabilities"]["executeCommandProvider"]["commands"];
    assert!(commands
        .as_array()
        .unwrap()
        .contains(&json!("x4c.configure")));
    c.notify("initialized", json!({}));
    // A document named in the arguments is used even if none is open.
    c.request(
        9,
        "workspace/executeCommand",
        json!({"command": "x4c.showConfiguration", "arguments": [uri]}),
    );
    let msg =
        c.wait_for("showMessage", |m| m["method"] == "window/showMessage");
    assert_eq!(
        msg["params"]["message"],
        "x4c-lsp: prog.p4 is analyzed with no defines"
    );
    assert_eq!(c.response(9)["result"], Value::Null);
    c.notify(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": uri, "languageId": "p4", "version": 1, "text": text,
        }}),
    );
    assert!(errors(&c.diagnostics(&uri)).is_empty());

    c.request(
        2,
        "workspace/executeCommand",
        json!({"command": "x4c.toggleDefine", "arguments": ["MULTICAST"]}),
    );
    let msg =
        c.wait_for("showMessage", |m| m["method"] == "window/showMessage");
    assert_eq!(
        msg["params"]["message"],
        "x4c-lsp: prog.p4 is analyzed with defines MULTICAST \
         (changed from the configuration)"
    );
    assert_eq!(c.response(2)["result"], Value::Null);
    assert_eq!(
        errors(&c.diagnostics(&uri)),
        ["cannot find name `missing` in this scope"]
    );

    c.request(
        3,
        "workspace/executeCommand",
        json!({"command": "x4c.configure"}),
    );
    let menu = c.wait_for("showMessageRequest", |m| {
        m["method"] == "window/showMessageRequest"
    });
    let titles: Vec<&str> = menu["params"]["actions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["Undefine MULTICAST", "Reset to the configuration"]);
    c.send(json!({
        "jsonrpc": "2.0", "id": menu["id"],
        "result": {"title": "Reset to the configuration"},
    }));
    assert!(errors(&c.diagnostics(&uri)).is_empty());
}

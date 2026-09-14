//! End-to-end MCP test without a display: start `terratofu-gui --serve` on a free port,
//! speak Streamable HTTP to it, exercise tools, resources and the batch/undo contract.

#![cfg(feature = "mcp")]
#![allow(clippy::result_large_err)]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Server {
    child: Child,
    url: String,
    token: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn start(project: &str) -> Server {
    let port = free_port();
    let token = "test-token".to_string();
    let example = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(project);
    let mut child = Command::new(env!("CARGO_BIN_EXE_terratofu-gui"))
        .args(["--serve", "--port", &port.to_string(), "--token", &token])
        .arg(&example)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn terratofu-gui --serve");
    let stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        assert!(Instant::now() < deadline, "server did not report listening");
        match lines.next() {
            Some(Ok(l)) if l.contains("listening on") => break,
            Some(Ok(_)) => continue,
            _ => panic!("server exited before listening"),
        }
    }
    // Keep draining stdout in the background so the child never blocks on a full pipe.
    std::thread::spawn(move || for _ in lines {});
    Server {
        child,
        url: format!("http://127.0.0.1:{port}/mcp"),
        token,
    }
}

/// A minimal Streamable HTTP client: one POST per JSON-RPC message, SSE or JSON reply.
struct Client {
    url: String,
    token: String,
    session: Option<String>,
    next_id: u64,
}

impl Client {
    fn new(s: &Server) -> Self {
        Client {
            url: s.url.clone(),
            token: s.token.clone(),
            session: None,
            next_id: 1,
        }
    }

    fn post(&mut self, body: Value) -> Result<(u16, Option<String>, String), ureq::Error> {
        let mut req = ureq::post(&self.url)
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream");
        if let Some(s) = &self.session {
            req = req.set("Mcp-Session-Id", s);
        }
        let resp = req.send_string(&body.to_string())?;
        let status = resp.status();
        let sid = resp.header("mcp-session-id").map(|s| s.to_string());
        let ct = resp.header("content-type").unwrap_or("").to_string();
        let text = resp.into_string()?;
        let payload = if ct.starts_with("text/event-stream") {
            text.lines()
                .filter_map(|l| l.strip_prefix("data:"))
                .map(|l| l.trim().to_string())
                .rfind(|l| l.starts_with('{'))
                .unwrap_or_default()
        } else {
            text
        };
        Ok((status, sid, payload))
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let (status, sid, payload) = self
            .post(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .unwrap_or_else(|e| panic!("{method}: {e}"));
        assert!(status < 300, "{method}: HTTP {status}: {payload}");
        if sid.is_some() {
            self.session = sid;
        }
        let v: Value = serde_json::from_str(&payload).unwrap_or_else(|e| panic!("{method}: {e}: {payload}"));
        if let Some(err) = v.get("error") {
            panic!("{method} returned an error: {err}");
        }
        v["result"].clone()
    }

    fn notify(&mut self, method: &str) {
        let (status, _, _) = self.post(json!({"jsonrpc": "2.0", "method": method})).unwrap();
        assert!(status < 300, "{method}: HTTP {status}");
    }

    fn initialize(&mut self) -> Value {
        let r = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "ttg-test", "version": "0"}
            }),
        );
        self.notify("notifications/initialized");
        r
    }

    /// Call a tool; returns (is_error, parsed JSON of the first text block or the raw text).
    fn call(&mut self, tool: &str, args: Value) -> (bool, Value) {
        let r = self.request("tools/call", json!({"name": tool, "arguments": args}));
        let is_error = r["isError"].as_bool().unwrap_or(false);
        let text = r["content"][0]["text"].as_str().unwrap_or("").to_string();
        let v = serde_json::from_str(&text).unwrap_or(Value::String(text));
        (is_error, v)
    }
}

#[test]
fn headless_server_tools_resources_and_batch() {
    let server = start("three-tier.ttg.json");
    let mut c = Client::new(&server);

    let init = c.initialize();
    assert!(init["capabilities"]["tools"].is_object(), "{init}");
    assert!(init["capabilities"]["resources"].is_object(), "{init}");

    // Tools are listed, including the new ones.
    let tools = c.request("tools/list", json!({}));
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for want in [
        "project_summary",
        "entity_add",
        "project_apply",
        "export_diff",
        "project_changes",
        "undo",
    ] {
        assert!(names.contains(&want), "missing tool {want}: {names:?}");
    }

    let (err, summary) = c.call("project_summary", json!({}));
    assert!(!err, "{summary}");
    let nodes_before = summary["counts"]["nodes"]
        .as_u64()
        .unwrap_or_else(|| panic!("{summary}"));
    let rev0 = summary["revision"].as_u64().unwrap();

    // One write = one undo step, and the revision moves.
    let (err, added) = c.call(
        "entity_add",
        json!({"type_id": "subnet", "name": "agent subnet", "parent": "main", "x": 900, "y": 700}),
    );
    assert!(!err, "{added}");
    let (err, upd) = c.call(
        "entity_update",
        json!({"entity": "agent subnet", "config": {"cidr_block": "10.0.9.0/24"}}),
    );
    assert!(!err, "{upd}");
    let (_, ch) = c.call("project_changes", json!({"since": rev0}));
    assert_eq!(ch["changed"], json!(true), "{ch}");
    assert_eq!(ch["last_change_by"], json!("agent"), "{ch}");
    let rev1 = ch["revision"].as_u64().unwrap();
    assert!(rev1 > rev0);

    // A batch: two adds and a link, applied as one step...
    let (err, applied) = c.call(
        "project_apply",
        json!({"commands": [
            {"tool": "entity_add", "args": {"type_id": "security_group", "name": "batch sg", "parent": "main", "x": 900, "y": 800}},
            {"tool": "entity_add", "args": {"type_id": "compute_instance", "name": "batch vm", "parent": "main", "x": 1100, "y": 800}},
            {"tool": "link_add", "args": {"source": "batch vm", "target": "batch sg", "relation": "attribute_reference"}},
            {"tool": "link_add", "args": {"source": "batch vm", "target": "agent subnet", "relation": "network_membership"}}
        ]}),
    );
    assert!(!err, "{applied}");
    assert_eq!(applied["count"], json!(4), "{applied}");
    let (_, s2) = c.call("project_summary", json!({}));
    assert_eq!(s2["counts"]["nodes"].as_u64().unwrap(), nodes_before + 3);

    // ... so a single undo removes all of it, leaving the earlier add in place.
    let (err, _) = c.call("undo", json!({}));
    assert!(!err);
    let (_, s3) = c.call("project_summary", json!({}));
    assert_eq!(s3["counts"]["nodes"].as_u64().unwrap(), nodes_before + 1, "{s3}");

    // A batch with a failing command rolls back entirely.
    let (err, msg) = c.call(
        "project_apply",
        json!({"commands": [
            {"tool": "entity_add", "args": {"type_id": "subnet", "name": "half", "parent": "main"}},
            {"tool": "entity_add", "args": {"type_id": "no_such_type", "name": "boom"}}
        ]}),
    );
    assert!(err, "{msg}");
    assert!(msg.as_str().unwrap_or("").contains("rolled back"), "{msg}");
    let (_, s4) = c.call("project_summary", json!({}));
    assert_eq!(s4["counts"]["nodes"].as_u64().unwrap(), nodes_before + 1);

    // Disallowed tools are refused up front.
    let (err, msg) = c.call(
        "project_apply",
        json!({"commands": [{"tool": "project_save", "args": {}}]}),
    );
    assert!(err, "{msg}");

    // 1.1: native resources go through entity_add directly (schema_search hands out
    // `native:<provider>:<resource>` ids that used to only work via the palette, which
    // calls `Catalog::ensure_native` before looking the type up; the MCP handler now
    // does the same).
    let (err, endpoint) = c.call(
        "entity_add",
        json!({"type_id": "native:aws:aws_vpc_endpoint", "name": "s3 endpoint", "parent": "main"}),
    );
    assert!(!err, "{endpoint}");
    let (err, upd) = c.call(
        "entity_update",
        json!({
            "entity": "s3 endpoint",
            "extra": {
                "vpc_id": {"$ref": {"entity": "main", "attr": "id"}},
                "service_name": "com.amazonaws.eu-west-2.s3",
            },
        }),
    );
    assert!(!err, "{upd}");
    let (err, preview) = c.call("export_preview", json!({}));
    assert!(!err, "{preview}");
    let native_tf: String = preview["files"]["native.tf"]
        .as_str()
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        native_tf.contains("resource \"aws_vpc_endpoint\" \"s3_endpoint\""),
        "{native_tf}"
    );
    assert!(
        native_tf.contains("service_name = \"com.amazonaws.eu-west-2.s3\""),
        "{native_tf}"
    );
    assert!(native_tf.contains("vpc_id = aws_vpc.main.id"), "{native_tf}");

    // 1.2: an `entity_ref` item inside a struct_list (a security-group rule's
    // `source_group`) accepts a display name, resolved to the id the same way every
    // other tool addresses entities, so diagnostics never see a stray name.
    let (err, _) = c.call(
        "entity_add",
        json!({"type_id": "security_group", "name": "ref sg a", "parent": "main"}),
    );
    assert!(!err);
    let (err, upd_a) = c.call(
        "entity_update",
        json!({
            "entity": "ref sg a",
            "config": {"rules": [
                {"name": "allow-out", "direction": "egress", "protocol": "all", "from_port": 0, "to_port": 0, "cidr": "0.0.0.0/0"}
            ]},
        }),
    );
    assert!(!err, "{upd_a}");
    let (err, _) = c.call(
        "entity_add",
        json!({"type_id": "security_group", "name": "ref sg b", "parent": "main"}),
    );
    assert!(!err);
    let (err, upd) = c.call(
        "entity_update",
        json!({
            "entity": "ref sg b",
            "config": {"rules": [
                {"name": "from-a", "direction": "ingress", "protocol": "tcp", "from_port": 443, "to_port": 443, "source_group": "REF SG A"}
            ]},
        }),
    );
    assert!(!err, "{upd}");
    // Clean diagnostics for the entity: no "no longer exists" complaint about the name.
    assert_eq!(upd["diagnostics"], json!([]), "{upd}");
    let stored_id = upd["entity"]["config"]["rules"][0]["source_group"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert_ne!(stored_id, "REF SG A", "the name must be resolved to an id: {upd}");
    assert!(!stored_id.is_empty(), "{upd}");

    // export_diff against an empty directory: everything is added, nothing written.
    let dir = std::env::temp_dir().join(format!("ttg-diff-{}", std::process::id()));
    let (err, diff) = c.call("export_diff", json!({"dir": dir.to_string_lossy()}));
    assert!(!err, "{diff}");
    assert_eq!(diff["changed"], json!(true));
    assert!(
        diff["files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["status"] == "added"),
        "{diff}"
    );
    assert!(!dir.exists(), "export_diff must not write");

    // Resources: list, read a document, read the live project, a template instance.
    let res = c.request("resources/list", json!({}));
    let uris: Vec<&str> = res["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uri"].as_str().unwrap())
        .collect();
    assert!(uris.contains(&"ttg://project"), "{uris:?}");
    assert!(uris.contains(&"ttg://docs/mapping-format"), "{uris:?}");
    let doc = c.request("resources/read", json!({"uri": "ttg://docs/mapping-format"}));
    assert!(doc["contents"][0]["text"]
        .as_str()
        .unwrap()
        .contains("schema_version"));
    let proj = c.request("resources/read", json!({"uri": "ttg://project"}));
    let project: Value = serde_json::from_str(proj["contents"][0]["text"].as_str().unwrap()).unwrap();
    assert!(project["nodes"]
        .as_object()
        .unwrap()
        .values()
        .any(|n| n["name"] == "agent subnet"));
    let cat = c.request("resources/read", json!({"uri": "ttg://catalog/subnet"}));
    assert!(cat["contents"][0]["text"]
        .as_str()
        .unwrap()
        .contains("cidr_block"));
    let templates = c.request("resources/templates/list", json!({}));
    assert!(templates["resourceTemplates"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["uriTemplate"] == "ttg://catalog/{type_id}"));
    // Subscribing is accepted; unknown resources are not.
    c.request("resources/subscribe", json!({"uri": "ttg://project"}));
    let (status, _, payload) = c
        .post(
            json!({"jsonrpc": "2.0", "id": 99, "method": "resources/read", "params": {"uri": "ttg://nope"}}),
        )
        .unwrap();
    assert!(status < 300);
    assert!(payload.contains("error"), "{payload}");

    // Wrong token: 401 before anything else.
    let bad = ureq::post(&server.url)
        .set("Authorization", "Bearer wrong")
        .set("Content-Type", "application/json")
        .set("Accept", "application/json, text/event-stream")
        .send_string(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    match bad {
        Err(ureq::Error::Status(code, _)) => assert_eq!(code, 401),
        other => panic!("expected 401, got {other:?}"),
    }

    // Screenshots are refused headless rather than hanging.
    let (err, msg) = c.call("screenshot", json!({}));
    assert!(err, "{msg}");
}

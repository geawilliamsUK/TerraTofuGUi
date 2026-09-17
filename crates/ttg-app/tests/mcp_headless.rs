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

    // R2.7: an empty string in a bare `entity_ref` clears the field, and inside a
    // `struct_list` row it means "no reference" and is left as "" rather than being
    // rejected as an unknown entity name — every example file stores rows this way.
    let (err, upd) = c.call(
        "entity_update",
        json!({
            "entity": "ref sg b",
            "config": {"rules": [
                {"name": "from-anywhere", "direction": "ingress", "protocol": "tcp", "from_port": 80, "to_port": 80, "cidr": "0.0.0.0/0", "source_group": ""}
            ]},
        }),
    );
    assert!(!err, "{upd}");
    assert_eq!(
        upd["entity"]["config"]["rules"][0]["source_group"],
        json!(""),
        "{upd}"
    );

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

/// Views as documents: reading one back, drawing into a named view without switching to
/// it, notes and logical nodes, and the Markdown / Mermaid export.
#[test]
fn headless_view_documents() {
    let server = start("job-pipeline.ttg.json");
    let mut c = Client::new(&server);
    c.initialize();

    let tools = c.request("tools/list", json!({}));
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for want in [
        "view_get",
        "view_update",
        "view_fit",
        "view_export",
        "view_note_add",
        "view_logical_add",
    ] {
        assert!(names.contains(&want), "missing tool {want}: {names:?}");
    }

    // view_get reports what the view holds, including the members a box currently has.
    let (err, v) = c.call("view_get", json!({"name": "Data flow"}));
    assert!(!err, "{v}");
    assert!(v["description"].as_str().unwrap().len() > 20, "{v}");
    assert_eq!(v["active"], json!(false), "{v}");
    let ingress = v["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["label"] == "Ingress (public)")
        .unwrap_or_else(|| panic!("{v}"));
    assert!(
        ingress["members"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m == "JobGateway"),
        "{ingress}"
    );
    assert_eq!(v["logicals"][0]["name"], json!("users' browser"), "{v}");
    assert_eq!(v["flows"][0]["step"], json!(1), "{v}");
    assert_eq!(v["notes"][0]["anchor"], json!({"entity": "q-jobs"}), "{v}");

    // Nothing is active, so every write below relies on the `view` argument.
    let (_, summary) = c.call("project_summary", json!({}));
    assert_eq!(summary["views"], json!(["Data flow"]), "{summary}");

    let (err, added) = c.call(
        "view_logical_add",
        json!({"view": "Data flow", "name": "HuggingFace", "icon": "HF", "subtitle": "model download", "x": 1500, "y": 620}),
    );
    assert!(!err, "{added}");
    let (err, flow) = c.call(
        "view_flow_add",
        json!({"view": "Data flow", "from": "JobRunner", "to": "HuggingFace", "label": "pulls model", "dashed": true, "step": 5, "color": "#b05aa0"}),
    );
    assert!(!err, "{flow}");
    let (err, note) = c.call(
        "view_note_add",
        json!({"view": "Data flow", "title": "Cold start", "body": "The first job of the day waits for the model download.", "anchor": "HuggingFace"}),
    );
    assert!(!err, "{note}");
    // A flow to something that is not in the view is refused with a readable message.
    let (err, msg) = c.call(
        "view_flow_add",
        json!({"view": "Data flow", "from": "JobRunner", "to": "no such thing"}),
    );
    assert!(err, "{msg}");

    // entity_move with `view` lands in that view's own layout, not the shared one.
    let (err, moved) = c.call(
        "entity_move",
        json!({"entity": "JobGateway", "x": 150, "y": 260, "view": "Data flow"}),
    );
    assert!(!err, "{moved}");
    assert_eq!(moved["in_view"], json!("Data flow"), "{moved}");
    let proj = c.request("resources/read", json!({"uri": "ttg://project"}));
    let project: Value = serde_json::from_str(proj["contents"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        project["views"][0]["layout"]["positions"]["fn-gateway"],
        json!({"x": 150, "y": 260}),
        "{}",
        project["views"][0]["layout"]
    );
    assert_ne!(
        project["nodes"]["fn-gateway"]["position"],
        json!({"x": 150, "y": 260}),
        "the shared layout must not move"
    );

    // Description and legend through view_update.
    let (err, upd) = c.call(
        "view_update",
        json!({"view": "Data flow", "description": "How one job travels through the pipeline.", "legend": true}),
    );
    assert!(!err, "{upd}");
    assert_eq!(upd["view"]["legend"], json!(true), "{upd}");

    // The document mentions the new logical node, its flow and the note.
    let (err, md) = c.call("view_export", json!({"view": "Data flow"}));
    assert!(!err, "{md}");
    let text = md["text"].as_str().unwrap();
    assert!(
        text.contains("## Groups") && text.contains("HuggingFace"),
        "{text}"
    );
    assert!(text.contains("Cold start"), "{text}");
    let (err, mm) = c.call("view_export", json!({"view": "Data flow", "format": "mermaid"}));
    assert!(!err, "{mm}");
    assert!(mm["text"].as_str().unwrap().starts_with("flowchart LR"), "{mm}");

    // view_fit answers with the bounding box even without a window.
    let (err, fit) = c.call("view_fit", json!({"view": "Data flow"}));
    assert!(!err, "{fit}");
    assert!(fit["bounds"]["w"].as_f64().unwrap_or(0.0) > 0.0, "{fit}");

    // Removal by title, and the whole lot batched as one undo step.
    let (err, rm) = c.call(
        "view_annotation_remove",
        json!({"view": "Data flow", "key": "Cold start"}),
    );
    assert!(!err, "{rm}");
    let (err, applied) = c.call(
        "project_apply",
        json!({"commands": [
            {"tool": "view_logical_add", "args": {"view": "Data flow", "name": "pager", "x": 1500, "y": 800}},
            {"tool": "view_flow_add", "args": {"view": "Data flow", "from": "pipeline logs", "to": "pager", "label": "alerts"}},
            {"tool": "view_note_add", "args": {"view": "Data flow", "title": "On call", "body": "Alerts page the duty engineer.", "anchor": "pager"}}
        ]}),
    );
    assert!(!err, "{applied}");
    assert_eq!(applied["count"], json!(3), "{applied}");
    let (_, before_undo) = c.call("view_get", json!({"name": "Data flow"}));
    assert_eq!(before_undo["logicals"].as_array().unwrap().len(), 3);
    let (err, _) = c.call("undo", json!({}));
    assert!(!err);
    let (_, after_undo) = c.call("view_get", json!({"name": "Data flow"}));
    assert_eq!(
        after_undo["logicals"].as_array().unwrap().len(),
        2,
        "one undo must take the whole batch: {after_undo}"
    );
}

/// The `diagnostics` tool keeps the target provider's list and the other providers'
/// would-be errors apart, so the agent cannot mistake one for the other.
#[test]
fn headless_diagnostics_report_the_other_providers() {
    let server = start("edge.ttg.json");
    let mut c = Client::new(&server);
    c.initialize();

    // As drawn, every provider is happy.
    let (err, d) = c.call("diagnostics", json!({}));
    assert!(!err, "{d}");
    assert_eq!(d["provider"], json!("aws"), "{d}");
    assert!(d["diagnostics"].is_array(), "{d}");
    assert_eq!(d["other_providers"], json!([]), "{d}");

    // Take the certificate out of the Key Vault: Azure has nowhere to put it, AWS does
    // not care. The target's own list stays error-free; the other list says what Azure
    // would refuse.
    let (err, moved) = c.call(
        "entity_set_parent",
        json!({"entity": "cert-site", "parent": "rg-edge"}),
    );
    assert!(!err, "{moved}");
    let (err, d) = c.call("diagnostics", json!({}));
    assert!(!err, "{d}");
    assert!(
        d["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .all(|x| x["severity"] != json!("error")),
        "{d}"
    );
    let others = d["other_providers"].as_array().unwrap();
    let azure = others
        .iter()
        .find(|x| x["provider"] == json!("azure") && x["entity"] == json!("cert-site"))
        .unwrap_or_else(|| panic!("{d}"));
    assert_eq!(azure["severity"], json!("warning"), "{azure}");
    assert!(
        azure["message"]
            .as_str()
            .unwrap()
            .starts_with("[Microsoft Azure] "),
        "{azure}"
    );
    assert!(
        azure["message"]
            .as_str()
            .unwrap()
            .ends_with("(would block the Microsoft Azure export)"),
        "{azure}"
    );

    // The AWS export is not blocked by any of it.
    let (err, preview) = c.call("export_preview", json!({}));
    assert!(!err, "{preview}");
}

/// Round-2 gaps: saving over a view, deleting one, rewriting a saved filter, moving the
/// annotations, placing an anchored note beside its anchor, and reporting nesting.
#[test]
fn headless_view_editing() {
    let server = start("job-pipeline.ttg.json");
    let mut c = Client::new(&server);
    c.initialize();

    let tools = c.request("tools/list", json!({}));
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for want in ["view_delete", "view_save", "view_update"] {
        assert!(names.contains(&want), "missing tool {want}: {names:?}");
    }

    // --- view_save: a name already in use is refused, `replace` rewrites the filter.
    let (err, msg) = c.call("view_save", json!({"name": "data flow"}));
    assert!(err, "a duplicate name must be refused: {msg}");
    let dup = msg.as_str().unwrap_or_default().to_string();
    assert!(dup.contains("already exists") && dup.contains("replace"), "{dup}");

    c.call("view_activate", json!({"name": "All"}));
    let (err, applied) = c.call(
        "view_set",
        json!({"filter": {"hide_links": true, "containers": false}}),
    );
    assert!(!err, "{applied}");
    let (err, replaced) = c.call("view_save", json!({"name": "DATA FLOW", "replace": true}));
    assert!(!err, "{replaced}");
    assert_eq!(replaced["status"], json!("view replaced"), "{replaced}");
    // §2.3(a): `hide_links` is stored as the `hide_edges` flag and comes back in view_get.
    let (_, v) = c.call("view_get", json!({"name": "Data flow"}));
    assert_eq!(v["filter"]["hide_edges"], json!(true), "{}", v["filter"]);
    assert_eq!(v["filter"]["containers"], json!(false), "{}", v["filter"]);
    // Replacing kept everything else the view held.
    assert!(!v["groups"].as_array().unwrap().is_empty(), "{v}");
    assert!(!v["flows"].as_array().unwrap().is_empty(), "{v}");
    assert_eq!(v["name"], json!("Data flow"), "the original name is kept: {v}");

    // --- view_update { filter }: a saved filter is no longer frozen.
    let (err, upd) = c.call(
        "view_update",
        json!({"view": "Data flow", "filter": {"only": ["JobRunner"], "hide_edges": true}}),
    );
    assert!(!err, "{upd}");
    assert_eq!(upd["view"]["filter"]["only"], json!(["fn-runner"]), "{upd}");
    assert_eq!(upd["view"]["filter"]["hide_edges"], json!(true), "{upd}");
    let shown: Vec<&str> = upd["view"]["shown"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(shown.contains(&"JobRunner"), "{shown:?}");
    // And back to something that shows everything again.
    let (err, _) = c.call("view_update", json!({"view": "Data flow", "filter": {}}));
    assert!(!err);

    // --- view_set on an active view writes into that view rather than detaching.
    c.call("view_activate", json!({"name": "Data flow"}));
    let (err, set) = c.call("view_set", json!({"filter": {"name_glob": "job*"}}));
    assert!(!err, "{set}");
    assert_eq!(set["view"], json!("Data flow"), "{set}");
    assert!(
        set["note"].as_str().unwrap_or_default().contains("Data flow"),
        "{set}"
    );
    let (_, v) = c.call("view_get", json!({"name": "Data flow"}));
    assert_eq!(v["filter"]["name_glob"], json!("job*"), "{}", v["filter"]);
    assert_eq!(v["active"], json!(true), "{v}");
    c.call("view_update", json!({"view": "Data flow", "filter": {}}));

    // --- R2.6: a box drawn fully inside another is reported as nested, in view_get
    // and in the Markdown export's "Inside" column.
    // Drawn the way an agent builds a map: both boxes in one project_apply.
    let (err, drawn) = c.call(
        "project_apply",
        json!({"commands": [
            {"tool": "view_group_add", "args": {"view": "Data flow", "label": "Platform", "x": 3000, "y": 3000, "w": 900, "h": 700}},
            {"tool": "view_group_add", "args": {"view": "Data flow", "label": "Workers", "x": 3100, "y": 3100, "w": 300, "h": 200}}
        ]}),
    );
    assert!(!err, "{drawn}");
    let (_, v) = c.call("view_get", json!({"name": "Data flow"}));
    let workers = v["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["label"] == "Workers")
        .unwrap_or_else(|| panic!("{v}"));
    assert_eq!(workers["nested_in"], json!("Platform"), "{workers}");
    assert_eq!(
        workers["parent"],
        json!("Platform"),
        "both names answer: {workers}"
    );
    let platform = v["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["label"] == "Platform")
        .unwrap();
    assert_eq!(platform["nested_in"], json!(null), "{platform}");
    let (_, md) = c.call("view_export", json!({"view": "Data flow"}));
    let text = md["text"].as_str().unwrap();
    assert!(
        text.lines().any(|l| l.starts_with("| Workers | Platform |")),
        "the Inside column must name the outer box:\n{text}"
    );

    // --- R2.4: notes, logical nodes and boxes move and resize like entities.
    let (err, moved) = c.call(
        "entity_move",
        json!({"view": "Data flow", "entity": "Workers", "x": 3150, "y": 3150}),
    );
    assert!(!err, "{moved}");
    assert_eq!(moved["kind"], json!("group"), "{moved}");
    let (err, moved) = c.call(
        "entity_move",
        json!({"view": "Data flow", "entity": "users' browser", "x": 4200, "y": 120}),
    );
    assert!(!err, "{moved}");
    assert_eq!(moved["kind"], json!("logical"), "{moved}");
    let (err, sized) = c.call(
        "entity_resize",
        json!({"view": "Data flow", "entity": "Workers", "w": 420, "h": 260}),
    );
    assert!(!err, "{sized}");
    assert_eq!(sized["kind"], json!("group"), "{sized}");
    let (_, v) = c.call("view_get", json!({"name": "Data flow"}));
    let workers = v["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["label"] == "Workers")
        .unwrap();
    assert_eq!(workers["position"], json!({"x": 3150, "y": 3150}), "{workers}");
    assert_eq!(workers["size"], json!({"w": 420, "h": 260}), "{workers}");
    let browser = v["logicals"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["name"] == "users' browser")
        .unwrap();
    assert_eq!(browser["position"], json!({"x": 4200, "y": 120}), "{browser}");
    // A box does not carry its members: membership is geometric, so moving the box is
    // exactly what changes who is inside it.
    let proj = c.request("resources/read", json!({"uri": "ttg://project"}));
    let project: Value = serde_json::from_str(proj["contents"][0]["text"].as_str().unwrap()).unwrap();
    let runner = project["views"][0]["layout"]["positions"]["fn-runner"].clone();
    assert!(runner.is_object(), "{}", project["views"][0]["layout"]);
    let (rx, ry) = (runner["x"].as_i64().unwrap(), runner["y"].as_i64().unwrap());
    let (err, around) = c.call(
        "view_group_add",
        json!({"view": "Data flow", "label": "Around the runner", "x": rx - 60, "y": ry - 60, "w": 400, "h": 300}),
    );
    assert!(!err, "{around}");
    assert!(
        around["members"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m == "fn-runner"),
        "{around}"
    );
    let (err, _) = c.call(
        "entity_move",
        json!({"view": "Data flow", "entity": "Around the runner", "x": rx + 2000, "y": ry}),
    );
    assert!(!err);
    let proj = c.request("resources/read", json!({"uri": "ttg://project"}));
    let project: Value = serde_json::from_str(proj["contents"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        project["views"][0]["layout"]["positions"]["fn-runner"], runner,
        "moving the box must not drag what was inside it"
    );

    // An unknown key names all four kinds it looked for.
    let (err, msg) = c.call(
        "entity_move",
        json!({"view": "Data flow", "entity": "no such thing", "x": 0, "y": 0}),
    );
    assert!(err, "{msg}");
    assert!(msg.as_str().unwrap_or_default().contains("grouping box"), "{msg}");

    // --- R2.5: an anchored note with no position lands next to its anchor.
    let (err, note) = c.call(
        "view_note_add",
        json!({"view": "Data flow", "title": "Beside me", "body": "Anchored, no x/y.", "anchor": "users' browser"}),
    );
    assert!(!err, "{note}");
    let at = &note["at"];
    let (nx, ny) = (at["x"].as_f64().unwrap(), at["y"].as_f64().unwrap());
    // The browser is at (4200, 120); the note must be within a screen of it, not off
    // the right-hand edge of the whole diagram.
    assert!(
        (nx - 4200.0).abs() < 900.0 && (ny - 120.0).abs() < 900.0,
        "the note landed at ({nx}, {ny}), nowhere near its anchor: {note}"
    );
    // Moving a note by absolute position works even though it stores an offset.
    let (err, moved) = c.call(
        "entity_move",
        json!({"view": "Data flow", "entity": "Beside me", "x": 4600, "y": 400}),
    );
    assert!(!err, "{moved}");
    assert_eq!(moved["kind"], json!("note"), "{moved}");

    // --- R2.9: a sized screenshot is refused headless with the documented message.
    let (err, msg) = c.call("screenshot", json!({"width": 1920, "height": 1200, "fit": true}));
    assert!(err, "{msg}");
    assert_eq!(
        msg.as_str().unwrap_or_default(),
        "no display in --serve mode",
        "{msg}"
    );

    // --- R2.2: view_delete, and the fall back to All when it was active.
    let (err, msg) = c.call("view_delete", json!({"name": "nope"}));
    assert!(err, "{msg}");
    let (err, del) = c.call("view_delete", json!({"name": "data flow"}));
    assert!(!err, "{del}");
    assert_eq!(del["name"], json!("Data flow"), "{del}");
    assert_eq!(del["active"], json!("All"), "{del}");
    assert_eq!(del["views"], json!([]), "{del}");
    let (_, summary) = c.call("project_summary", json!({}));
    assert_eq!(summary["views"], json!([]), "{summary}");
    // One undo step brings the whole view back.
    let (err, _) = c.call("undo", json!({}));
    assert!(!err);
    let (_, back) = c.call("view_get", json!({"name": "Data flow"}));
    assert!(!back["flows"].as_array().unwrap().is_empty(), "{back}");
}

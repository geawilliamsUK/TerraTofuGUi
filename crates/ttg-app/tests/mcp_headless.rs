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

// ---------------------------------------------------------------------------------
// Server identity, export error text, note offsets, hidden flow ends and the scoped calls.
// ---------------------------------------------------------------------------------

fn tool_names(c: &mut Client) -> Vec<String> {
    let tools = c.request("tools/list", json!({}));
    tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

/// The project as the agent reads it, in one piece.
fn project_of(c: &mut Client) -> Value {
    let (err, p) = c.call("project_get", json!({}));
    assert!(!err, "{p}");
    p
}

/// R3.20: `serverInfo` names TerraTofu and its version, and carries the catalog
/// fingerprint, so a client that cached an older tool list knows to refresh it.
#[test]
fn headless_server_info_carries_the_version_and_catalog_hash() {
    let server = start("three-tier.ttg.json");
    let mut c = Client::new(&server);
    let init = c.initialize();

    let version = env!("CARGO_PKG_VERSION");
    let hash = ttg_catalog::Catalog::builtin().fingerprint;
    assert_eq!(hash.len(), 12, "{hash}");
    let info = &init["serverInfo"];
    assert_eq!(info["name"], json!("terratofu-gui"), "{init}");
    assert_ne!(
        info["name"].as_str().unwrap(),
        "rmcp",
        "serverInfo used to be rmcp's own"
    );
    assert_eq!(
        info["version"],
        json!(format!("{version}+catalog.{hash}")),
        "{init}"
    );
    assert_eq!(info["title"], json!(format!("TerraTofu GUI {version}")), "{init}");
    assert_eq!(
        info["description"],
        json!(format!("TerraTofu GUI {version}, catalog {hash}")),
        "{init}"
    );

    let instructions = init["instructions"].as_str().unwrap();
    assert!(
        instructions.starts_with(&format!("TerraTofu GUI {version}, catalog {hash}: ")),
        "{instructions}"
    );
    assert!(
        instructions.contains("if your tool list lacks view_delete or entity_preview, refresh it"),
        "{instructions}"
    );
    // The standing advice is still there after the identity line.
    assert!(instructions.contains("Never call project_save"), "{instructions}");
    assert!(instructions.contains("dry_run"), "{instructions}");

    // The two tools the instructions name are listed.
    let names = tool_names(&mut c);
    for want in ["view_delete", "entity_preview", "catalog_relations"] {
        assert!(names.iter().any(|n| n == want), "missing {want}: {names:?}");
    }
}

/// R3.15: a project with an export-blocking error gives every export tool the same
/// message, listing what blocks it; none of them answers with an empty error.
#[test]
fn headless_every_export_tool_reports_a_blocked_project_the_same_way() {
    let server = start("kubernetes.ttg.json");
    let mut c = Client::new(&server);
    c.initialize();

    // Overlapping subnets pass the write (each CIDR is valid on its own) and block
    // every provider's export.
    let (err, upd) = c.call(
        "entity_update",
        json!({"entity": "nodes b", "config": {"cidr_block": "10.0.10.0/24"}}),
    );
    assert!(!err, "{upd}");

    let dir = std::env::temp_dir().join(format!("ttg-blocked-{}", std::process::id()));
    let dir = dir.to_string_lossy().to_string();
    let (e1, preview) = c.call("export_preview", json!({}));
    let (e2, diff) = c.call("export_diff", json!({"dir": dir}));
    let (e3, run) = c.call("export_run", json!({"dir": dir}));
    let (e4, one) = c.call("entity_preview", json!({"entity": "platform"}));
    assert!(e1 && e2 && e3 && e4, "{preview} / {diff} / {run} / {one}");

    let text = preview.as_str().unwrap_or_default().to_string();
    assert!(
        text.contains("block export") && text.contains("overlaps with subnet"),
        "{text}"
    );
    for (tool, other) in [
        ("export_diff", &diff),
        ("export_run", &run),
        ("entity_preview", &one),
    ] {
        assert_eq!(
            other.as_str().unwrap_or_default(),
            text,
            "{tool} must say what export_preview says"
        );
    }
    assert!(
        !std::path::Path::new(&dir).exists(),
        "a blocked export writes nothing"
    );
}

/// R3.16: an anchored note is reported at the position it is drawn — what `entity_move`
/// takes — with the offset from its anchor that it actually stores beside it.
#[test]
fn headless_anchored_notes_report_position_and_offset() {
    let server = start("job-pipeline.ttg.json");
    let mut c = Client::new(&server);
    c.initialize();

    // The example's note is anchored to the jobs queue with a stored offset (30, -170).
    let (err, v) = c.call("view_get", json!({"name": "Data flow"}));
    assert!(!err, "{v}");
    let note = v["notes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == "note-queue")
        .unwrap_or_else(|| panic!("{v}"));
    assert_eq!(note["offset"], json!({"x": 30, "y": -170}), "{note}");
    // The queue sits at (460, 200) in this view's layout; the note is drawn 170 above
    // its top edge and 30 right of its right edge.
    assert_eq!(note["position"]["y"], json!(30), "{note}");
    let x0 = note["position"]["x"].as_i64().unwrap();
    let queue_width = x0 - 30 - 460;
    assert!(queue_width > 0, "{note}");

    // The report's case: move it to (-1850, -1850) and read it back.
    let (err, moved) = c.call(
        "entity_move",
        json!({"view": "Data flow", "entity": "note-queue", "x": -1850, "y": -1850}),
    );
    assert!(!err, "{moved}");
    assert_eq!(moved["kind"], json!("note"), "{moved}");
    assert_eq!(moved["position"], json!({"x": -1850, "y": -1850}), "{moved}");
    assert_eq!(
        moved["offset"],
        json!({"x": -1850 - 460 - queue_width, "y": -1850 - 200}),
        "the move reports the offset it stored: {moved}"
    );
    let (_, v) = c.call("view_get", json!({"name": "Data flow"}));
    let note = v["notes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == "note-queue")
        .unwrap();
    assert_eq!(
        note["position"],
        json!({"x": -1850, "y": -1850}),
        "position is absolute, so it is what entity_move was given: {note}"
    );
    assert_eq!(
        note["offset"],
        json!({"x": -1850 - 460 - queue_width, "y": -2050}),
        "{note}"
    );

    // A free note has no offset, and its position is its own.
    let (err, free) = c.call(
        "view_note_add",
        json!({"view": "Data flow", "title": "Loose", "body": "Not anchored.", "x": 900, "y": -300}),
    );
    assert!(!err, "{free}");
    let (_, v) = c.call("view_get", json!({"name": "Data flow"}));
    let loose = v["notes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["title"] == "Loose")
        .unwrap();
    assert_eq!(loose["position"], json!({"x": 900, "y": -300}), "{loose}");
    assert_eq!(loose["offset"], json!(null), "{loose}");
    let (err, moved) = c.call(
        "entity_move",
        json!({"view": "Data flow", "entity": "Loose", "x": 5, "y": 6}),
    );
    assert!(!err, "{moved}");
    assert_eq!(moved["offset"], json!(null), "{moved}");

    // The raw file shape is unchanged: project_get still holds the stored offset.
    let p = project_of(&mut c);
    let stored = p["views"][0]["notes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == "note-queue")
        .unwrap();
    assert_eq!(
        stored["position"],
        json!({"x": -1850 - 460 - queue_width, "y": -2050}),
        "{stored}"
    );
}

/// R3.17: a flow to something the view hides is accepted with a warning; `show_hidden`
/// shows it, in the same undo step, without rewriting what the view is for.
#[test]
fn headless_flows_to_hidden_entities_warn_and_can_show_them() {
    let server = start("job-pipeline.ttg.json");
    let mut c = Client::new(&server);
    c.initialize();
    let flows = |c: &mut Client| {
        let (_, v) = c.call("view_get", json!({"name": "Data flow"}));
        v["flows"].as_array().unwrap().len()
    };
    let filter_of = |c: &mut Client| {
        let (_, v) = c.call("view_get", json!({"name": "Data flow"}));
        v["filter"].clone()
    };
    let shown = |c: &mut Client, name: &str| {
        let (_, v) = c.call("view_get", json!({"name": "Data flow"}));
        v["shown"].as_array().unwrap().iter().any(|e| e["name"] == name)
    };

    // 1. Hidden by the filter's `hidden` list.
    let (err, upd) = c.call(
        "view_update",
        json!({"view": "Data flow", "filter": {"hidden": ["JobRunner"], "hide_edges": true}}),
    );
    assert!(!err, "{upd}");
    assert!(!shown(&mut c, "JobRunner"));
    let n0 = flows(&mut c);
    let (err, flow) = c.call(
        "view_flow_add",
        json!({"view": "Data flow", "from": "JobGateway", "to": "JobRunner", "label": "hands over", "step": 9}),
    );
    assert!(!err, "the flow is still accepted: {flow}");
    assert_eq!(flow["status"], json!("flow added"), "{flow}");
    assert_eq!(flow["hidden_ends"], json!(["JobRunner"]), "{flow}");
    let warning = flow["warning"].as_str().unwrap_or_else(|| panic!("{flow}"));
    for want in [
        "\"JobRunner\"",
        "hidden",
        "Data flow",
        "`hidden`",
        "view_update",
        "show_hidden",
    ] {
        assert!(warning.contains(want), "warning lacks {want}: {warning}");
    }
    assert_eq!(flows(&mut c), n0 + 1, "the flow is stored regardless");
    assert!(!shown(&mut c, "JobRunner"), "a warning changes nothing else");
    // A flow between things that are shown has no warning at all.
    let (err, ok) = c.call(
        "view_flow_add",
        json!({"view": "Data flow", "from": "JobGateway", "to": "jobs queue", "label": "enqueue"}),
    );
    assert!(!err, "{ok}");
    assert!(
        ok.get("warning").is_none() && ok.get("hidden_ends").is_none(),
        "{ok}"
    );

    // 2. `show_hidden` drops it from the list, and the flow and the filter are one step.
    let n1 = flows(&mut c);
    let (err, flow) = c.call(
        "view_flow_add",
        json!({"view": "Data flow", "from": "JobGateway", "to": "JobRunner", "label": "hands over", "show_hidden": true}),
    );
    assert!(!err, "{flow}");
    assert!(flow.get("warning").is_none(), "{flow}");
    assert_eq!(flow["shown"], json!(["JobRunner"]), "{flow}");
    assert_eq!(flows(&mut c), n1 + 1);
    assert_eq!(
        filter_of(&mut c),
        json!({"depth": 1, "hide_edges": true}),
        "only `hidden` was touched"
    );
    assert!(shown(&mut c, "JobRunner"));
    let (err, _) = c.call("undo", json!({}));
    assert!(!err);
    assert_eq!(flows(&mut c), n1, "one undo takes the flow and the filter change");
    assert_eq!(
        filter_of(&mut c)["hidden"],
        json!(["fn-runner"]),
        "and puts the hidden entry back"
    );

    // 3. An `only` filter shows what it names: the entity joins the list.
    c.call(
        "view_update",
        json!({"view": "Data flow", "filter": {"only": ["JobGateway"]}}),
    );
    let (err, flow) = c.call(
        "view_flow_add",
        json!({"view": "Data flow", "from": "JobGateway", "to": "jobs db", "show_hidden": true}),
    );
    assert!(!err, "{flow}");
    assert!(flow.get("warning").is_none(), "{flow}");
    let f = filter_of(&mut c);
    assert_eq!(f["only"], json!(["db-jobs", "fn-gateway"]), "{f}");

    // 4. What the view is *for* is not rewritten: the flow is added, the warning says why.
    c.call(
        "view_update",
        json!({"view": "Data flow", "filter": {"categories": ["serverless"]}}),
    );
    let (err, flow) = c.call(
        "view_flow_add",
        json!({"view": "Data flow", "from": "JobGateway", "to": "jobs db", "show_hidden": true}),
    );
    assert!(!err, "{flow}");
    assert_eq!(flow["hidden_ends"], json!(["jobs db"]), "{flow}");
    let warning = flow["warning"].as_str().unwrap_or_else(|| panic!("{flow}"));
    assert!(
        warning.contains("`categories`") && warning.contains("show_hidden only takes"),
        "{warning}"
    );
    assert_eq!(
        filter_of(&mut c),
        json!({"categories": ["serverless"], "depth": 1}),
        "categories are left alone"
    );
}

/// R3.19 through the tool: an entity an `omit` check leaves out of Azure shows up as an
/// info line while the target is AWS, and R3.21's filters find it.
#[test]
fn headless_diagnostics_filters_and_omitted_entities() {
    let server = start("operations.ttg.json");
    let mut c = Client::new(&server);
    c.initialize();

    // Azure Monitor has no platform metric for the oldest message age.
    let (err, upd) = c.call(
        "entity_update",
        json!({"entity": "dead letters piling up", "config": {"metric": "queue_oldest_message_age"}}),
    );
    assert!(!err, "{upd}");

    // AWS keeps the alarm and says nothing about it; Azure's loss is listed as info.
    let (_, d) = c.call("diagnostics", json!({}));
    assert!(
        d["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .all(|x| x["entity"] != json!("alm-dlq")),
        "{d}"
    );
    let left_out = d["other_providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["entity"] == json!("alm-dlq"))
        .unwrap_or_else(|| panic!("{d}"));
    assert_eq!(left_out["severity"], json!("info"), "{left_out}");
    assert_eq!(left_out["provider"], json!("azure"), "{left_out}");
    let msg = left_out["message"].as_str().unwrap();
    assert!(
        msg.starts_with("[Microsoft Azure] ") && msg.ends_with("; left out of the Microsoft Azure export"),
        "{msg}"
    );

    // `severity` and `entity` narrow both lists, and the reply counts what it kept.
    let (err, info) = c.call("diagnostics", json!({"severity": "info"}));
    assert!(!err, "{info}");
    assert!(
        info["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .chain(info["other_providers"].as_array().unwrap())
            .all(|x| x["severity"] == "info"),
        "{info}"
    );
    assert!(
        info["other_providers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["entity"] == "alm-dlq"),
        "{info}"
    );
    assert_eq!(
        info["matched"]["other_providers"],
        json!(info["other_providers"].as_array().unwrap().len()),
        "{info}"
    );
    assert!(
        info["of"]["diagnostics"].as_u64().unwrap() >= info["matched"]["diagnostics"].as_u64().unwrap(),
        "{info}"
    );
    let (err, one) = c.call("diagnostics", json!({"entity": "dead letters piling up"}));
    assert!(!err, "{one}");
    assert_eq!(one["diagnostics"], json!([]), "{one}");
    assert_eq!(one["other_providers"].as_array().unwrap().len(), 1, "{one}");

    // `provider` answers another provider's run: Azure's own warning, once.
    let (err, az) = c.call(
        "diagnostics",
        json!({"provider": "azure", "entity": "alm-dlq", "severity": "warning"}),
    );
    assert!(!err, "{az}");
    assert_eq!(az["provider"], json!("azure"), "{az}");
    assert_eq!(az["target_provider"], json!("aws"), "{az}");
    let mine = az["diagnostics"].as_array().unwrap();
    assert_eq!(mine.len(), 1, "{az}");
    assert!(
        mine[0]["message"]
            .as_str()
            .unwrap()
            .ends_with("; left out of the Microsoft Azure export"),
        "{az}"
    );
    assert!(
        mine[0]["provider"].is_null(),
        "the provider's own line is untagged: {az}"
    );
    // From Azure's point of view the other providers have nothing to say about it.
    assert!(
        az["other_providers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|x| x["entity"] != json!("alm-dlq")),
        "{az}"
    );
    // Asking for another provider does not change the target.
    let (_, s) = c.call("project_summary", json!({}));
    assert_eq!(s["target_provider"], json!("aws"), "{s}");

    // Bad filters are refused with the valid values.
    let (err, msg) = c.call("diagnostics", json!({"severity": "loud"}));
    assert!(
        err && msg.as_str().unwrap().contains("error | warning | info"),
        "{msg}"
    );
    let (err, msg) = c.call("diagnostics", json!({"provider": "oracle"}));
    assert!(err && msg.as_str().unwrap().contains("known:"), "{msg}");
    let (err, msg) = c.call("diagnostics", json!({"entity": "no such thing"}));
    assert!(err, "{msg}");
}

/// R3.21: `project_get` slices, `catalog_relations` answers, `entity_preview` shows one
/// entity's HCL.
#[test]
fn headless_slices_relations_and_entity_previews() {
    let server = start("hardened.ttg.json");
    let mut c = Client::new(&server);
    c.initialize();

    // --- project_get { fields }
    let (err, p) = c.call("project_get", json!({"fields": ["nodes", "edges"]}));
    assert!(!err, "{p}");
    let mut keys: Vec<&str> = p.as_object().unwrap().keys().map(|k| k.as_str()).collect();
    keys.sort();
    assert_eq!(keys, ["edges", "nodes"], "{keys:?}");
    assert!(p["nodes"].as_object().unwrap().len() > 5);
    // The default is unchanged: everything.
    let whole = project_of(&mut c);
    for k in [
        "schema_version",
        "name",
        "settings",
        "containers",
        "nodes",
        "edges",
    ] {
        assert!(whole.get(k).is_some(), "missing {k}");
    }

    // --- project_get { entities }: those entities and the links among them.
    let (err, s) = c.call(
        "project_get",
        json!({"entities": ["records store", "key-main", "access log store"]}),
    );
    assert!(!err, "{s}");
    let mut ids: Vec<&str> = s["nodes"]
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.as_str())
        .collect();
    ids.sort();
    assert_eq!(ids, ["key-main", "obj-audit", "obj-data"], "{s}");
    assert!(
        s.get("containers").is_some() && s["containers"].as_object().unwrap().is_empty(),
        "{s}"
    );
    assert!(s.get("settings").is_none() && s.get("views").is_none(), "{s}");
    let mut edges: Vec<String> = s["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            format!(
                "{} {} {}",
                e["source"].as_str().unwrap(),
                e["relation"].as_str().unwrap(),
                e["target"].as_str().unwrap()
            )
        })
        .collect();
    edges.sort();
    assert_eq!(
        edges,
        ["obj-data encrypted_with key-main", "obj-data logs_to obj-audit"],
        "only the links between the named entities"
    );
    // Both at once.
    let (_, s) = c.call(
        "project_get",
        json!({"entities": ["records store", "data key"], "fields": ["edges"]}),
    );
    assert_eq!(s.as_object().unwrap().len(), 1, "{s}");
    assert_eq!(s["edges"].as_array().unwrap().len(), 1, "{s}");
    // Refusals say what is valid.
    let (err, msg) = c.call("project_get", json!({"fields": ["nodez"]}));
    assert!(
        err && msg.as_str().unwrap().contains("fields: schema_version"),
        "{msg}"
    );
    let (err, msg) = c.call("project_get", json!({"entities": []}));
    assert!(err, "{msg}");
    let (err, msg) = c.call("project_get", json!({"entities": ["no such thing"]}));
    assert!(err, "{msg}");

    // --- catalog_relations
    let (err, r) = c.call(
        "catalog_relations",
        json!({"source_type": "object_storage", "target_type": "encryption_key"}),
    );
    assert!(!err, "{r}");
    let rows = r["relations"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{r}");
    assert_eq!(rows[0]["relation"], json!("encrypted_with"), "{r}");
    assert_eq!(rows[0]["label"], json!("Encrypted with"), "{r}");
    assert_eq!(rows[0]["cardinality"], json!("optional"), "{r}");
    assert_eq!(rows[0]["providers"], json!(["aws", "gcp"]), "{r}");
    assert!(rows[0]["targets"]
        .as_array()
        .unwrap()
        .contains(&json!("encryption_key")));
    assert_eq!(r["always_allowed"], json!(["depends_on"]), "{r}");
    // Every relation a type has, and everything that can point at a type.
    let (_, all) = c.call("catalog_relations", json!({"source_type": "object_storage"}));
    assert!(all["relations"].as_array().unwrap().len() >= 2, "{all}");
    assert!(all["relations"]
        .as_array()
        .unwrap()
        .iter()
        .all(|x| x["source_type"] == "object_storage"));
    let (_, into_key) = c.call("catalog_relations", json!({"target_type": "encryption_key"}));
    let sources: std::collections::BTreeSet<&str> = into_key["relations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["source_type"].as_str().unwrap())
        .collect();
    assert!(
        sources.contains("object_storage") && sources.contains("relational_database"),
        "{sources:?}"
    );
    let (err, msg) = c.call("catalog_relations", json!({"source_type": "no_such_type"}));
    assert!(err && msg.as_str().unwrap().contains("unknown type"), "{msg}");

    // --- entity_preview
    let (err, pv) = c.call("entity_preview", json!({"entity": "records store"}));
    assert!(!err, "{pv}");
    assert_eq!(pv["provider"], json!("aws"), "{pv}");
    assert_eq!(pv["file"], json!("storage.tf"), "{pv}");
    let hcl = pv["hcl"].as_str().unwrap();
    assert!(
        hcl.contains("resource \"aws_s3_bucket\" \"records_store\"")
            && !hcl.contains("resource \"aws_s3_bucket\" \"access_log_store\""),
        "{hcl}"
    );
    assert!(
        pv["addresses"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "aws_s3_bucket.records_store"),
        "{pv}"
    );
    // Exactly the text the full preview holds for it.
    let (_, full) = c.call("export_preview", json!({}));
    assert!(full["files"]["storage.tf"].as_str().unwrap().contains(hcl));
    // Another provider's blocks, from the same project.
    let (err, az) = c.call(
        "entity_preview",
        json!({"entity": "records store", "provider": "azure"}),
    );
    assert!(!err, "{az}");
    assert!(az["hcl"].as_str().unwrap().contains("azurerm_"), "{az}");
    // A grouping container on AWS produces nothing, and says why.
    let (err, rg) = c.call("entity_preview", json!({"entity": "hardened"}));
    assert!(!err, "{rg}");
    assert_eq!(rg["hcl"], json!(""), "{rg}");
    assert!(rg["no_blocks"].as_str().unwrap().contains("logical"), "{rg}");
    assert_eq!(rg["addresses"], json!([]), "{rg}");
    let (err, msg) = c.call(
        "entity_preview",
        json!({"entity": "records store", "provider": "oracle"}),
    );
    assert!(err && msg.as_str().unwrap().contains("unknown provider"), "{msg}");
}

/// R3.21: `project_apply { dry_run }` plays a batch and reports the diagnostics delta,
/// then leaves nothing: not the project, not the undo or redo history, not the revision.
#[test]
fn headless_dry_run_reports_the_diagnostics_delta_and_leaves_nothing() {
    let server = start("hardened.ttg.json");
    let mut c = Client::new(&server);
    c.initialize();

    // A real edit first, so there is history to keep (and something to undo and redo).
    let (err, added) = c.call(
        "entity_add",
        json!({"type_id": "event_queue", "name": "keeper", "x": 1500, "y": 900}),
    );
    assert!(!err, "{added}");
    let revision = |c: &mut Client| {
        c.call("project_changes", json!({})).1["revision"]
            .as_u64()
            .unwrap_or_default()
    };
    let (_, ch) = c.call("project_changes", json!({}));
    let rev0 = ch["revision"].as_u64().unwrap();
    let before = project_of(&mut c);
    let (_, s0) = c.call("diagnostics", json!({}));

    // The batch adds a second subnet on top of the first one's range: a new error.
    let batch = json!([
        {"tool": "entity_add", "args": {"type_id": "subnet", "name": "twin", "parent": "core"}},
        {"tool": "entity_update", "args": {"entity": "twin", "config": {"cidr_block": "10.0.1.0/24"}}},
        {"tool": "entity_update", "args": {"entity": "db a", "config": {"cidr_block": "10.0.1.0/24"}}}
    ]);
    let (err, dry) = c.call("project_apply", json!({"commands": batch, "dry_run": true}));
    assert!(!err, "{dry}");
    assert_eq!(dry["dry_run"], json!(true), "{dry}");
    assert_eq!(dry["count"], json!(3), "{dry}");
    assert_eq!(
        dry["results"].as_array().unwrap().len(),
        3,
        "per-command results: {dry}"
    );
    assert_eq!(dry["results"][0]["tool"], json!("EntityAdd"), "{dry}");
    let delta = &dry["diagnostics"];
    assert_eq!(delta["provider"], json!("aws"), "{dry}");
    let added_errors: Vec<&Value> = delta["added"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["severity"] == "error")
        .collect();
    assert!(
        added_errors
            .iter()
            .any(|d| d["message"].as_str().unwrap().contains("overlaps")),
        "the overlap the batch would create: {dry}"
    );
    assert!(
        delta["errors"]["after"].as_u64().unwrap() > delta["errors"]["before"].as_u64().unwrap(),
        "{dry}"
    );
    assert!(delta["removed"].is_array(), "{dry}");

    // Nothing landed: the same project, diagnostics, revision, and no `twin`.
    assert_eq!(project_of(&mut c), before, "the project must be untouched");
    let (_, s1) = c.call("diagnostics", json!({}));
    assert_eq!(s1, s0, "diagnostics are as they were");
    assert_eq!(revision(&mut c), rev0, "a dry run is not a change");
    let (_, ch) = c.call("project_changes", json!({}));
    assert_eq!(ch["dirty"], json!(true)); // from the real edit above, as before
    let (_, sum) = c.call("project_summary", json!({}));
    assert!(
        sum["entities"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["name"] != "twin"),
        "{sum}"
    );

    // ...and not the history: one undo takes the real edit, not a phantom dry-run step,
    // and one redo brings it back (the redo stack survives a dry run too).
    let (err, _) = c.call("undo", json!({}));
    assert!(!err);
    let (_, sum) = c.call("project_summary", json!({}));
    assert!(sum["entities"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["name"] != "keeper"));
    let (err, dry) = c.call("project_apply", json!({"commands": batch, "dry_run": true}));
    assert!(!err, "{dry}");
    let (err, _) = c.call("redo", json!({}));
    assert!(!err, "the dry run must not clear the redo stack");
    let (_, sum) = c.call("project_summary", json!({}));
    assert!(sum["entities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["name"] == "keeper"));

    // A batch that would fail fails the same way as a real one, and still leaves nothing.
    let rev = revision(&mut c);
    let (err, msg) = c.call(
        "project_apply",
        json!({"commands": [
            {"tool": "entity_add", "args": {"type_id": "subnet", "name": "half", "parent": "core"}},
            {"tool": "entity_add", "args": {"type_id": "no_such_type"}}
        ], "dry_run": true}),
    );
    assert!(err, "{msg}");
    assert!(msg.as_str().unwrap().contains("rolled back"), "{msg}");
    assert_eq!(revision(&mut c), rev);
    let (_, sum) = c.call("project_summary", json!({}));
    assert!(sum["entities"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["name"] != "half"));

    // A batch that would clear an error reports it as removed. Make one for real first:
    // two subnets on the same range.
    let (err, upd) = c.call(
        "entity_update",
        json!({"entity": "db b", "config": {"cidr_block": "10.40.10.0/24"}}),
    );
    assert!(!err, "{upd}");
    let (_, broken) = c.call("diagnostics", json!({"severity": "error"}));
    assert_eq!(
        broken["diagnostics"].as_array().unwrap().len(),
        2,
        "both subnets overlap: {broken}"
    );
    let (err, dry) = c.call(
        "project_apply",
        json!({"commands": [
            {"tool": "entity_update", "args": {"entity": "db b", "config": {"cidr_block": "10.40.11.0/24"}}}
        ], "dry_run": true}),
    );
    assert!(!err, "{dry}");
    let removed = dry["diagnostics"]["removed"].as_array().unwrap();
    assert_eq!(removed.len(), 2, "both overlap errors would go: {dry}");
    assert!(removed
        .iter()
        .all(|d| d["severity"] == "error" && d["message"].as_str().unwrap().contains("overlaps")));
    assert!(
        dry["diagnostics"]["added"].as_array().unwrap().is_empty(),
        "{dry}"
    );
    assert_eq!(
        dry["diagnostics"]["errors"],
        json!({"before": 2, "after": 0}),
        "{dry}"
    );
    // ...and the overlap is still there: the fix was never applied.
    let (_, still) = c.call("diagnostics", json!({"severity": "error"}));
    assert_eq!(still["diagnostics"].as_array().unwrap().len(), 2, "{still}");
}

/// R3.21: `entity_update { select }` and `link_add { select }` act on many entities as one
/// undo step, refuse an empty or non-matching selection, and are all-or-nothing.
#[test]
fn headless_bulk_update_and_link_are_one_undo_step() {
    let server = start("hardened.ttg.json");
    let mut c = Client::new(&server);
    c.initialize();
    let node = |c: &mut Client, id: &str| {
        let (_, s) = c.call("project_get", json!({"entities": [id]}));
        s["nodes"][id].clone()
    };
    let edge_count = |c: &mut Client| {
        let (_, s) = c.call("project_get", json!({"fields": ["edges"]}));
        s["edges"].as_array().unwrap().len()
    };
    let revision = |c: &mut Client| {
        c.call("project_changes", json!({})).1["revision"]
            .as_u64()
            .unwrap()
    };

    // --- entity_update { select }: the two buckets, one of which already has it on.
    assert_eq!(node(&mut c, "obj-audit")["config"]["versioning"], json!(false));
    let rev0 = revision(&mut c);
    let (err, r) = c.call(
        "entity_update",
        json!({"select": {"types": ["object_storage"]}, "config": {"versioning": true}}),
    );
    assert!(!err, "{r}");
    assert_eq!(r["selected"], json!(2), "{r}");
    assert_eq!(r["changed"], json!(1), "only the bucket that was off: {r}");
    assert_eq!(r["unchanged"], json!(1), "{r}");
    let by_name = |name: &str| {
        r["entities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == name)
            .unwrap_or_else(|| panic!("{r}"))
            .clone()
    };
    assert_eq!(by_name("access log store")["changed"], json!(true), "{r}");
    assert_eq!(by_name("records store")["changed"], json!(false), "{r}");
    assert_eq!(node(&mut c, "obj-audit")["config"]["versioning"], json!(true));
    assert_eq!(revision(&mut c), rev0 + 1, "one change, one revision");
    // One undo reverts both.
    let (err, _) = c.call("undo", json!({}));
    assert!(!err);
    assert_eq!(node(&mut c, "obj-audit")["config"]["versioning"], json!(false));
    // A glob selects by name, case-insensitively.
    let (err, r) = c.call(
        "entity_update",
        json!({"select": {"name_glob": "*STORE"}, "config": {"expire_days": 400}}),
    );
    assert!(!err, "{r}");
    assert_eq!(r["selected"], json!(2), "{r}");
    assert_eq!(node(&mut c, "obj-data")["config"]["expire_days"], json!(400));
    assert_eq!(node(&mut c, "obj-audit")["config"]["expire_days"], json!(400));
    let (err, _) = c.call("undo", json!({}));
    assert!(!err);
    assert_eq!(node(&mut c, "obj-audit")["config"]["expire_days"], json!(90));
    // `ids` take ids or names, and combine with the other criteria (all must hold).
    let (err, r) = c.call(
        "entity_update",
        json!({"select": {"ids": ["records store", "q-work"], "types": ["object_storage"]}, "config": {"versioning": true}}),
    );
    assert!(!err, "{r}");
    assert_eq!(r["selected"], json!(1), "{r}");

    // --- refusals: empty, matching nothing, one entity refusing, a rename.
    let (err, msg) = c.call(
        "entity_update",
        json!({"select": {}, "config": {"versioning": true}}),
    );
    assert!(
        err && msg.as_str().unwrap().contains("selection is empty"),
        "{msg}"
    );
    let (err, msg) = c.call(
        "entity_update",
        json!({"select": {"name_glob": ""}, "config": {"versioning": true}}),
    );
    assert!(
        err && msg.as_str().unwrap().contains("selection is empty"),
        "{msg}"
    );
    let (err, msg) = c.call(
        "entity_update",
        json!({"select": {"types": ["cache"]}, "config": {"size": "small"}}),
    );
    assert!(
        err && msg.as_str().unwrap().contains("matched no entity"),
        "{msg}"
    );
    let (err, msg) = c.call(
        "entity_update",
        json!({"select": {"types": ["no_such_type"]}, "config": {"size": "small"}}),
    );
    assert!(err && msg.as_str().unwrap().contains("unknown type"), "{msg}");
    let (err, msg) = c.call(
        "entity_update",
        json!({"select": {"types": ["object_storage"]}, "name": "same"}),
    );
    assert!(err && msg.as_str().unwrap().contains("cannot rename"), "{msg}");
    let (err, msg) = c.call("entity_update", json!({"select": {"types": ["object_storage"]}}));
    assert!(
        err && msg.as_str().unwrap().contains("nothing to update"),
        "{msg}"
    );
    let (err, msg) = c.call(
        "entity_update",
        json!({"entity": "records store", "select": {"types": ["object_storage"]}, "config": {}}),
    );
    assert!(err && msg.as_str().unwrap().contains("not both"), "{msg}");
    let (err, msg) = c.call("entity_update", json!({"config": {"versioning": true}}));
    assert!(err && msg.as_str().unwrap().contains("`entity`"), "{msg}");
    // All-or-nothing: the queue has no `versioning`, so the bucket is not touched either.
    let before = project_of(&mut c);
    let rev = revision(&mut c);
    let (err, msg) = c.call(
        "entity_update",
        json!({"select": {"ids": ["obj-audit", "q-work"]}, "config": {"versioning": true}}),
    );
    assert!(err, "{msg}");
    let text = msg.as_str().unwrap();
    assert!(
        text.contains("changed nothing")
            && text.contains("\"work queue\" (event_queue)")
            && text.contains("no field \"versioning\""),
        "{text}"
    );
    assert!(
        !text.contains("access log store"),
        "only the refusals are listed: {text}"
    );
    assert_eq!(project_of(&mut c), before, "nothing changed");
    assert_eq!(revision(&mut c), rev, "and no revision was spent");

    // --- link_add { select }: the pain point. Point every bucket at the key.
    let (err, rel) = c.call(
        "catalog_relations",
        json!({"source_type": "object_storage", "target_type": "encryption_key"}),
    );
    assert!(
        !err && rel["relations"][0]["relation"] == "encrypted_with",
        "{rel}"
    );
    let edges0 = edge_count(&mut c);
    let rev0 = revision(&mut c);
    let (err, r) = c.call(
        "link_add",
        json!({"select": {"types": ["object_storage"]}, "relation": "encrypted_with", "target": "data key"}),
    );
    assert!(!err, "{r}");
    assert_eq!(r["status"], json!("linked"), "{r}");
    assert_eq!(r["linked"], json!(["access log store"]), "{r}");
    assert_eq!(r["already_linked"], json!(["records store"]), "{r}");
    assert_eq!(r["skipped"], json!([]), "{r}");
    assert_eq!(edge_count(&mut c), edges0 + 1);
    assert_eq!(revision(&mut c), rev0 + 1);
    let (err, _) = c.call("undo", json!({}));
    assert!(!err);
    assert_eq!(edge_count(&mut c), edges0, "one undo removes every new link");
    // The target is skipped when it matches its own selection.
    let (err, r) = c.call(
        "link_add",
        json!({"select": {"name_glob": "*key"}, "relation": "depends_on", "target": "data key"}),
    );
    assert!(!err, "{r}");
    assert_eq!(r["status"], json!("no new links"), "{r}");
    assert_eq!(r["skipped"][0]["reason"], json!("it is the target itself"), "{r}");
    // A relation a type may not have refuses the whole call and names each refusal.
    let (err, msg) = c.call(
        "link_add",
        json!({"select": {"types": ["object_storage", "subnet"]}, "relation": "encrypted_with", "target": "data key"}),
    );
    assert!(err, "{msg}");
    let text = msg.as_str().unwrap();
    assert!(
        text.contains("changed nothing")
            && text.contains("\"db a\" (subnet)")
            && text.contains("\"db b\" (subnet)")
            && text.contains("does not allow 'encrypted_with'"),
        "{text}"
    );
    assert_eq!(edge_count(&mut c), edges0, "the buckets were not linked either");
    let (err, msg) = c.call(
        "link_add",
        json!({"select": {}, "relation": "encrypted_with", "target": "data key"}),
    );
    assert!(
        err && msg.as_str().unwrap().contains("selection is empty"),
        "{msg}"
    );
    let (err, msg) = c.call(
        "link_add",
        json!({"source": "records store", "select": {"types": ["object_storage"]}, "relation": "encrypted_with", "target": "data key"}),
    );
    assert!(err && msg.as_str().unwrap().contains("not both"), "{msg}");

    // --- both are batchable, and a dry run of a bulk write keeps nothing.
    let bulk = json!([
        {"tool": "entity_update", "args": {"select": {"types": ["object_storage"]}, "config": {"versioning": true}}},
        {"tool": "link_add", "args": {"select": {"types": ["object_storage"]}, "relation": "encrypted_with", "target": "data key"}}
    ]);
    let before = project_of(&mut c);
    let (err, dry) = c.call("project_apply", json!({"commands": bulk, "dry_run": true}));
    assert!(!err, "{dry}");
    assert_eq!(dry["count"], json!(2), "{dry}");
    assert_eq!(project_of(&mut c), before);
    let (err, applied) = c.call("project_apply", json!({"commands": bulk}));
    assert!(!err, "{applied}");
    assert_eq!(edge_count(&mut c), edges0 + 1);
    let (err, _) = c.call("undo", json!({}));
    assert!(!err);
    assert_eq!(
        project_of(&mut c),
        before,
        "one undo for the whole batch, bulk writes included"
    );
}

/// The Kubernetes manifests through the agent — the new workload fields and
/// links in `catalog_type`, the project setting, and the per-call `k8s` override.
#[test]
fn headless_kubernetes_manifests() {
    let server = start("kubernetes.ttg.json");
    let mut c = Client::new(&server);
    c.initialize();

    let (err, t) = c.call("catalog_type", json!({"type_id": "kubernetes_workload"}));
    assert!(!err, "{t}");
    let fields = t["fields"].as_array().unwrap();
    for name in [
        "image_tag",
        "repository",
        "port",
        "cpu",
        "memory",
        "gpu",
        "max_replicas",
    ] {
        let f = fields
            .iter()
            .find(|f| f["name"] == json!(name))
            .unwrap_or_else(|| panic!("no field {name}: {t}"));
        assert_eq!(f["manifests"], json!(true), "{f}");
    }
    let rels = t["relations"].as_array().unwrap();
    for label in [
        "Runs image from",
        "Schedules on (node pool)",
        "Receives traffic from (load balancer)",
    ] {
        let r = rels
            .iter()
            .find(|r| r["label"] == json!(label))
            .unwrap_or_else(|| panic!("no relation {label}: {t}"));
        assert_eq!(r["manifests_only"], json!(true), "{r}");
    }

    let k8s_files = |preview: &Value| -> Vec<String> {
        preview["files"]
            .as_object()
            .unwrap()
            .keys()
            .filter(|k| k.starts_with("k8s/"))
            .cloned()
            .collect()
    };
    // The example is saved with the manifests on; `k8s: false` leaves them out of one
    // call without touching the setting.
    let (err, on) = c.call("export_preview", json!({}));
    assert!(!err, "{on}");
    assert!(k8s_files(&on).contains(&"k8s/api.yaml".to_string()), "{on}");
    let (err, off) = c.call("export_preview", json!({"k8s": false}));
    assert!(!err, "{off}");
    assert!(k8s_files(&off).is_empty(), "{off}");
    assert!(
        off["manual_steps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s.as_str().unwrap().contains("Apply the Deployment")),
        "{off}"
    );

    // The setting itself.
    let (err, s) = c.call("settings_set", json!({"kubernetes_manifests": false}));
    assert!(!err, "{s}");
    assert_eq!(s["kubernetes_manifests"], json!(false), "{s}");
    let (_, off) = c.call("export_preview", json!({}));
    assert!(k8s_files(&off).is_empty(), "{off}");

    // export_run { k8s: true } writes them anyway.
    let dir = std::env::temp_dir().join(format!("ttg-mcp-k8s-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (err, run) = c.call(
        "export_run",
        json!({"dir": dir.to_string_lossy(), "provider": "aws", "k8s": true}),
    );
    assert!(!err, "{run}");
    assert!(dir.join("k8s/asr-worker.yaml").exists(), "{run}");
    assert!(dir.join("k8s/render.sh").exists(), "{run}");
    assert!(
        std::fs::read_to_string(dir.join("outputs.tf"))
            .unwrap()
            .contains("output \"k8s_jobs_url\""),
        "{run}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Round 3 (R3.2, R3.4, R3.14): the state backend, state encryption and provider version
/// pins round-trip through `settings_set` and `project_get`, bad values and unknown keys
/// are refused with a message that says what would be right, and the export carries the
/// result.
#[test]
fn headless_settings_state_and_versions() {
    let server = start("hardened.ttg.json");
    let mut c = Client::new(&server);
    c.initialize();

    // R3.14: an unknown key is refused, naming the valid ones; nothing changes.
    let (_, before) = c.call("project_get", json!({}));
    let (err, msg) = c.call(
        "settings_set",
        json!({"tool": "terraform", "backnd": {"type": "s3"}}),
    );
    assert!(err, "{msg}");
    let msg = msg.as_str().unwrap_or_default().to_string();
    assert!(msg.contains("settings_set does not take `backnd`"), "{msg}");
    for key in [
        "tool",
        "provider",
        "provider_settings",
        "tags",
        "kubernetes_manifests",
        "backend",
        "state_encryption",
        "state_encryption_key",
        "provider_versions",
    ] {
        assert!(msg.contains(key), "valid key {key} missing from: {msg}");
    }
    let (_, after) = c.call("project_get", json!({}));
    assert_eq!(
        before["settings"], after["settings"],
        "a refused call changed the settings"
    );
    // The same inside project_apply.
    let (err, msg) = c.call(
        "project_apply",
        json!({"commands": [{"tool": "settings_set", "args": {"state_encrypt": true}}]}),
    );
    assert!(err, "{msg}");
    assert!(msg.as_str().unwrap_or_default().contains("valid keys:"), "{msg}");

    // R3.2: a backend with a missing key or an unknown type is refused with the reason.
    let (err, msg) = c.call("settings_set", json!({"backend": {"type": "s3", "bucket": "b"}}));
    assert!(err, "{msg}");
    assert!(
        msg.as_str()
            .unwrap_or_default()
            .contains("the s3 backend needs bucket, region; missing: region"),
        "{msg}"
    );
    let (err, msg) = c.call("settings_set", json!({"backend": {"type": "consul"}}));
    assert!(err, "{msg}");
    assert!(
        msg.as_str()
            .unwrap_or_default()
            .contains("use one of: local, s3, azurerm, gcs"),
        "{msg}"
    );
    let (err, msg) = c.call("settings_set", json!({"state_encryption_key": "db password"}));
    assert!(err, "{msg}");
    assert!(
        msg.as_str()
            .unwrap_or_default()
            .contains("must be an Encryption Key"),
        "{msg}"
    );
    let (err, msg) = c.call("settings_set", json!({"provider_versions": {"aws": "latest"}}));
    assert!(err, "{msg}");

    // Before: local, unencrypted state, and a generated secret -> the warning.
    let (_, d) = c.call("diagnostics", json!({}));
    let warns = |d: &Value| -> Vec<String> {
        d["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|x| x["code"] == json!("State"))
            .map(|x| x["message"].as_str().unwrap().to_string())
            .collect()
    };
    assert!(
        warns(&d).iter().any(|m| m.contains(
            "stored in plain text in local state; configure a remote backend and state encryption (Settings)"
        )),
        "{d}"
    );

    // Set everything in one call: the key by name, the backend flat.
    let (err, set) = c.call(
        "settings_set",
        json!({
            "backend": {"type": "s3", "bucket": "hardened-tfstate-x7q2", "region": "eu-west-2", "key_prefix": "platform/hardened"},
            "state_encryption": true,
            "state_encryption_key": "data key",
            "provider_versions": {"aws": "~> 6.0"}
        }),
    );
    assert!(!err, "{set}");
    assert_eq!(set["status"], json!("settings updated"), "{set}");

    // project_get shows it, in the project file's own shape...
    let (_, p) = c.call("project_get", json!({}));
    let s = &p["settings"];
    assert_eq!(
        s["backend"],
        json!({"type": "s3", "args": {"bucket": "hardened-tfstate-x7q2", "key_prefix": "platform/hardened", "region": "eu-west-2"}}),
        "{s}"
    );
    assert_eq!(s["state_encryption"], json!(true), "{s}");
    assert_eq!(s["state_encryption_key"], json!("key-main"), "{s}");
    assert_eq!(s["provider_versions"], json!({"aws": "~> 6.0"}), "{s}");
    // ... which settings_set takes back unchanged.
    let (err, again) = c.call("settings_set", json!({"backend": s["backend"].clone()}));
    assert!(!err, "{again}");
    let (_, p2) = c.call("project_get", json!({}));
    assert_eq!(p2["settings"]["backend"], s["backend"]);

    // The warning is gone, and the export carries backend, encryption and bootstrap root.
    let (_, d) = c.call("diagnostics", json!({}));
    assert!(warns(&d).is_empty(), "{d}");
    let (err, preview) = c.call("export_preview", json!({}));
    assert!(!err, "{preview}");
    let v = preview["files"]["versions.tf"].as_str().unwrap();
    assert!(v.contains("backend \"s3\""), "{v}");
    assert!(
        v.contains("key          = \"platform/hardened/terraform.tfstate\""),
        "{v}"
    );
    assert!(v.contains("key_provider \"aws_kms\" \"state\""), "{v}");
    assert!(v.contains("version = \"~> 6.0\""), "{v}");
    assert!(preview["files"]["bootstrap/storage.tf"].is_string(), "{preview}");
    assert!(preview["files"]["bootstrap/security.tf"].is_string(), "{preview}");

    // null clears the backend and the key; an empty pin removes the pin.
    let (err, _) = c.call(
        "settings_set",
        json!({"backend": null, "state_encryption_key": null, "provider_versions": {"aws": ""}}),
    );
    assert!(!err);
    let (_, p) = c.call("project_get", json!({}));
    let s = &p["settings"];
    assert_eq!(s["backend"], json!(null), "{s}");
    assert!(s.get("state_encryption_key").is_none_or(|k| k.is_null()), "{s}");
    assert!(s.get("provider_versions").is_none_or(|v| v == &json!({})), "{s}");
    // One undo brings them back.
    let (err, _) = c.call("undo", json!({}));
    assert!(!err);
    let (_, p) = c.call("project_get", json!({}));
    assert_eq!(p["settings"]["state_encryption_key"], json!("key-main"));
}

/// R3.12: `cost_estimate` prices the layer, groups by entity / type / view group, takes
/// one-call assumptions, refuses what it does not know, and writes nothing.
#[test]
fn headless_cost_estimate() {
    let server = start("job-pipeline.ttg.json");
    let mut c = Client::new(&server);
    c.initialize();
    assert!(tool_names(&mut c).iter().any(|t| t == "cost_estimate"));
    let (_, ch) = c.call("project_changes", json!({}));
    let rev0 = ch["revision"].as_u64().unwrap();

    let (err, e) = c.call("cost_estimate", json!({}));
    assert!(!err, "{e}");
    assert_eq!(e["provider"], json!("aws"), "{e}");
    assert_eq!(e["currency"], json!("USD"));
    assert!(e["prices_retrieved"].as_str().unwrap().starts_with("20"), "{e}");
    assert!(e["caveat"].as_str().unwrap().contains("Not a quote"), "{e}");
    let total = e["monthly"].as_f64().unwrap();
    assert!(total > 10.0, "{e}");
    let lines = e["lines"].as_array().unwrap();
    // Largest first, each with its charges.
    let first = lines[0]["monthly"].as_f64().unwrap();
    assert!(lines.iter().all(|l| l["monthly"].as_f64().unwrap() <= first));
    let nat = lines
        .iter()
        .find(|l| l["type"] == json!("nat_gateway"))
        .expect("NAT line");
    assert!(
        nat["charges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["unit"] == json!("hour")),
        "{nat}"
    );
    // Free things are summarised per type rather than listed one by one.
    assert!(
        e["free"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["type"] == json!("subnet")),
        "{e}"
    );
    assert!(e["assumptions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["key"] == json!("nat_data_gb")));

    // Per type, and per group of the one view.
    let (err, t) = c.call("cost_estimate", json!({"group_by": "type"}));
    assert!(!err, "{t}");
    assert!(
        t["types"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["type"] == json!("relational_database")),
        "{t}"
    );
    let (err, g) = c.call("cost_estimate", json!({"group_by": "group", "view": "data FLOW"}));
    assert!(!err, "{g}");
    assert_eq!(g["view"], json!("Data flow"));
    assert_eq!(g["groups"].as_array().unwrap().len(), 4, "{g}");
    assert!(g["monthly"].as_f64().unwrap() <= total);

    // One-call assumptions move the figure and are reported as such; nothing is saved.
    let (err, more) = c.call("cost_estimate", json!({"assumptions": {"nat_data_gb": 10000}}));
    assert!(!err, "{more}");
    assert!(more["monthly"].as_f64().unwrap() > total + 100.0, "{more}");
    let a = more["assumptions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["key"] == json!("nat_data_gb"))
        .unwrap()
        .clone();
    assert_eq!(a["source"], json!("call"), "{a}");

    // An environment the project does not have is refused naming why (headless_environments
    // prices real ones); a region is priced as asked.
    let (err, env) = c.call(
        "cost_estimate",
        json!({"environment": "prod", "region": "us-east-1"}),
    );
    assert!(err, "{env}");
    assert!(env.to_string().contains("no environments"), "{env}");
    let (err, env) = c.call("cost_estimate", json!({"region": "us-east-1"}));
    assert!(!err, "{env}");
    assert_eq!(env["price_region"], json!("us-east-1"));

    // Refusals name what would have worked.
    let (err, bad) = c.call("cost_estimate", json!({"assumptions": {"gpu_hours": 4}}));
    assert!(
        err && bad.as_str().unwrap().contains("pool_node_hours_per_day"),
        "{bad}"
    );
    let (err, bad) = c.call("cost_estimate", json!({"view": "nope"}));
    assert!(err && bad.as_str().unwrap().contains("Data flow"), "{bad}");
    let (err, bad) = c.call("cost_estimate", json!({"group_by": "colour"}));
    assert!(err, "{bad}");
    let (err, bad) = c.call("cost_estimate", json!({"provider": "oracle"}));
    assert!(err, "{bad}");

    // A read: the project did not move.
    let (_, ch) = c.call("project_changes", json!({}));
    assert_eq!(ch["revision"].as_u64().unwrap(), rev0);
}

/// Views and metadata: flows generated from the links, tidy by flows with
/// the notes put back beside their anchors, the sequence export, and an entity's
/// classification, description and owner — round trip, filter, and the personal-data
/// view built from them.
#[test]
fn headless_views_and_metadata() {
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
    for want in ["view_generate", "view_arrange_notes"] {
        assert!(names.contains(&want), "missing tool {want}: {names:?}");
    }
    let flows_in = |c: &mut Client| -> usize {
        let (_, v) = c.call("view_get", json!({"name": "Data flow"}));
        v["flows"].as_array().unwrap().len()
    };

    // --- Flows from the links, one undo step, never duplicating a pair.
    let before = flows_in(&mut c);
    let (err, gen) = c.call("view_generate", json!({"view": "Data flow", "kind": "data_flow"}));
    assert!(!err, "{gen}");
    let added = gen["added"].as_array().unwrap();
    assert!(!added.is_empty(), "{gen}");
    // The secrets are read by the runner: the data moves secret -> runner.
    assert!(
        added.iter().any(|f| f["from"] == json!("DBUser")
            && f["to"] == json!("JobRunner")
            && f["label"] == json!("read by")),
        "{gen}"
    );
    // The gateway -> queue flow was drawn by hand; it is not drawn again.
    assert!(!added.iter().any(|f| f["to"] == json!("jobs queue")), "{gen}");
    assert_eq!(flows_in(&mut c), before + added.len());
    let (err, again) = c.call("view_generate", json!({"view": "Data flow", "kind": "data_flow"}));
    assert!(!err, "{again}");
    assert_eq!(again["added"], json!([]), "{again}");
    // A generation that changed nothing left no undo step behind: one undo takes the
    // first one back.
    let (err, _) = c.call("undo", json!({}));
    assert!(!err);
    assert_eq!(flows_in(&mut c), before, "one undo per generation");
    let (err, msg) = c.call("view_generate", json!({"view": "Data flow", "kind": "nonsense"}));
    assert!(err, "{msg}");

    // --- A note far from its anchor comes back beside it.
    let (err, moved) = c.call(
        "entity_move",
        json!({"view": "Data flow", "entity": "Why a queue in the middle", "x": 4000, "y": 3000}),
    );
    assert!(!err, "{moved}");
    let note_at = |c: &mut Client| -> (f64, f64) {
        let (_, v) = c.call("view_get", json!({"name": "Data flow"}));
        let p = &v["notes"][0]["offset"];
        (p["x"].as_f64().unwrap(), p["y"].as_f64().unwrap())
    };
    let far = note_at(&mut c);
    assert!(far.0 > 1000.0, "{far:?}");
    let (err, arranged) = c.call("view_arrange_notes", json!({"view": "Data flow"}));
    assert!(!err, "{arranged}");
    assert_eq!(arranged["moved"], json!(1), "{arranged}");
    // The stored position is the offset from the anchor: now a short one.
    let near = note_at(&mut c);
    assert!(near.0.abs() < 600.0 && near.1.abs() < 600.0, "{near:?}");

    // --- Tidy by flows, in the view's own layout only, notes following.
    let proj = |c: &mut Client| -> Value {
        let r = c.request("resources/read", json!({"uri": "ttg://project"}));
        serde_json::from_str(r["contents"][0]["text"].as_str().unwrap()).unwrap()
    };
    let shared_before = proj(&mut c)["nodes"]["fn-runner"]["position"].clone();
    let (err, msg) = c.call("layout_tidy", json!({"by": "flows"}));
    assert!(err, "flows need a view: {msg}");
    let (err, tidied) = c.call("layout_tidy", json!({"view": "Data flow", "by": "flows"}));
    assert!(!err, "{tidied}");
    assert!(tidied["moved"].as_u64().unwrap() > 0, "{tidied}");
    let p = proj(&mut c);
    assert_eq!(
        p["nodes"]["fn-runner"]["position"], shared_before,
        "the shared layout never moves"
    );
    let pos = &p["views"][0]["layout"]["positions"];
    let x = |id: &str| pos[id]["x"].as_i64().unwrap_or_else(|| panic!("{id}: {pos}"));
    // Left to right in step order: gateway (2) -> queue (3) -> runner (4) -> db.
    assert!(
        x("fn-gateway") < x("q-jobs") && x("q-jobs") < x("fn-runner") && x("fn-runner") < x("db-jobs"),
        "{pos}"
    );
    let (err, _) = c.call("layout_tidy", json!({"view": "Data flow"}));
    assert!(!err);

    // --- The numbered flows as a sequence diagram.
    let (err, seq) = c.call("view_export", json!({"view": "Data flow", "format": "sequence"}));
    assert!(!err, "{seq}");
    let text = seq["text"].as_str().unwrap();
    assert!(text.starts_with("sequenceDiagram"), "{text}");
    assert!(text.contains("2. job requests"), "{text}");

    // --- Classification, description and owner round trip.
    let (err, upd) = c.call(
        "entity_update",
        json!({"entity": "jobs db", "classification": "personal", "description": "Job state and results, per customer", "owner": "Data team"}),
    );
    assert!(!err, "{upd}");
    assert_eq!(upd["entity"]["classification"], json!("personal"), "{upd}");
    assert_eq!(upd["entity"]["owner"], json!("Data team"), "{upd}");
    let (err, msg) = c.call(
        "entity_update",
        json!({"entity": "jobs db", "classification": "secret-ish"}),
    );
    assert!(err, "{msg}");
    let p = proj(&mut c);
    assert_eq!(p["nodes"]["db-jobs"]["classification"], json!("personal"));
    assert_eq!(
        p["nodes"]["db-jobs"]["description"],
        json!("Job state and results, per customer")
    );
    let (_, summary) = c.call("project_summary", json!({}));
    assert_eq!(summary["classified"]["personal"], json!(1), "{summary}");
    let db = summary["entities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == json!("db-jobs"))
        .unwrap();
    assert_eq!(db["owner"], json!("Data team"), "{db}");
    // One undo takes the whole update back.
    let (err, _) = c.call("undo", json!({}));
    assert!(!err);
    assert!(proj(&mut c)["nodes"]["db-jobs"].get("classification").is_none());
    let (err, _) = c.call("redo", json!({}));
    assert!(!err);
    // The Markdown export gains the columns.
    let (_, md) = c.call("view_export", json!({"view": "Data flow"}));
    assert!(
        md["text"]
            .as_str()
            .unwrap()
            .contains("| Classification | Description | Owner |"),
        "{md}"
    );

    // The bulk form sets them on every match, as one undo step.
    let (err, bulk) = c.call(
        "entity_update",
        json!({"select": {"types": ["object_storage"]}, "owner": "Storage team", "classification": "internal"}),
    );
    assert!(!err, "{bulk}");
    assert_eq!(bulk["changed"], json!(2), "{bulk}");
    let p = proj(&mut c);
    for id in ["obj-gateway", "obj-runner"] {
        assert_eq!(p["nodes"][id]["owner"], json!("Storage team"), "{id}");
        assert_eq!(p["nodes"][id]["classification"], json!("internal"), "{id}");
    }
    let (err, _) = c.call("undo", json!({}));
    assert!(!err);
    assert!(proj(&mut c)["nodes"]["obj-gateway"].get("owner").is_none());

    // The classification filter shows only what is classified that way.
    let (err, set) = c.call(
        "view_set",
        json!({"filter": {"classifications": ["personal"], "containers": false}}),
    );
    assert!(!err, "{set}");
    let (_, fit) = c.call("view_fit", json!({}));
    assert_eq!(fit["shown"], json!(1), "{fit}");
    let (err, _) = c.call("view_set", json!({"filter": {}}));
    assert!(!err);

    // "Where personal data goes": the database and the runner that uses it.
    let (err, pd) = c.call("view_generate", json!({"kind": "personal_data"}));
    assert!(!err, "{pd}");
    assert_eq!(pd["status"], json!("view created"), "{pd}");
    assert_eq!(pd["sources"], json!(["jobs db"]), "{pd}");
    assert!(
        pd["reached"].as_array().unwrap().contains(&json!("JobRunner")),
        "{pd}"
    );
    let (_, v) = c.call("view_get", json!({"name": "Where personal data goes"}));
    let shown: Vec<&str> = v["shown"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(shown.contains(&"jobs db") && shown.contains(&"JobRunner"), "{v}");
    assert!(!v["flows"].as_array().unwrap().is_empty(), "{v}");
    let (err, again) = c.call("view_generate", json!({"kind": "personal_data"}));
    assert!(!err, "{again}");
    assert_eq!(again["status"], json!("view refreshed"), "{again}");
}

// ------------------------------------------------------------------ remote access

/// `--serve` with extra flags, its stdout forwarded line by line (OAuth prints the
/// one-time codes there).
fn start_with(project: &str, extra: &[&str]) -> (Server, std::sync::mpsc::Receiver<String>, u16) {
    let port = free_port();
    let token = "test-token".to_string();
    let example = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(project);
    let mut child = Command::new(env!("CARGO_BIN_EXE_terratofu-gui"))
        .args(["--serve", "--port", &port.to_string(), "--token", &token])
        .args(extra)
        .arg(&example)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn terratofu-gui --serve");
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for l in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(l).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(l) if l.contains("listening on") => break,
            Ok(_) => continue,
            Err(_) => panic!("server did not report listening"),
        }
    }
    let server = Server {
        child,
        url: format!("http://127.0.0.1:{port}/mcp"),
        token,
    };
    (server, rx, port)
}

/// Status, headers and body of any response, 4xx/5xx included.
fn raw(r: Result<ureq::Response, ureq::Error>) -> (u16, ureq::Response) {
    match r {
        Ok(resp) => (resp.status(), resp),
        Err(ureq::Error::Status(code, resp)) => (code, resp),
        Err(e) => panic!("transport error: {e}"),
    }
}

fn body_json(resp: ureq::Response) -> Value {
    let text = resp.into_string().unwrap();
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"))
}

fn pkce_challenge(verifier: &str) -> String {
    use base64::Engine;
    use sha2::Digest;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()))
}

/// The value of `key` in a URL's query string.
fn query_param(url: &str, key: &str) -> Option<String> {
    let q = url.split_once('?')?.1;
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| {
            // Tokens and states here are URL-safe; only `%3A` / `%2F` appear in `iss`.
            v.replace("%3A", ":").replace("%2F", "/")
        })
    })
}

fn mcp_status(url: &str, bearer: &str) -> (u16, Option<String>) {
    let (status, resp) = raw(ureq::post(url)
        .set("Authorization", &format!("Bearer {bearer}"))
        .set("Content-Type", "application/json")
        .set("Accept", "application/json, text/event-stream")
        .send_string(
            &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                    "protocolVersion": "2025-06-18", "capabilities": {},
                    "clientInfo": {"name": "probe", "version": "0"}}})
            .to_string(),
        ));
    (status, resp.header("www-authenticate").map(str::to_string))
}

/// The whole OAuth flow a claude.ai connector goes through, headless: discovery from
/// the 401, registration, authorization with PKCE approved by the one-time code printed
/// on stdout, the token exchange, MCP calls with the access token, a refresh (rotating
/// the refresh token), and revocation ending access at once. The bearer token keeps
/// working throughout, and an unknown Host is refused.
#[test]
fn headless_oauth_sign_in_refresh_and_revoke() {
    let (server, lines, port) = start_with("three-tier.ttg.json", &["--oauth"]);
    let base = format!("http://127.0.0.1:{port}");
    let agent = ureq::AgentBuilder::new().redirects(0).build();

    // An unauthenticated call says where to sign in.
    let (status, challenge) = mcp_status(&server.url, "nothing");
    assert_eq!(status, 401);
    let challenge = challenge.expect("WWW-Authenticate on the 401");
    let meta_url = format!("{base}/.well-known/oauth-protected-resource/mcp");
    assert!(
        challenge.contains(&format!("resource_metadata=\"{meta_url}\"")),
        "{challenge}"
    );
    assert!(challenge.contains("invalid_token"), "{challenge}");

    let (status, resp) = raw(agent.get(&meta_url).call());
    assert_eq!(status, 200);
    let prm = body_json(resp);
    assert_eq!(prm["resource"], json!(format!("{base}/mcp")), "{prm}");
    assert_eq!(prm["authorization_servers"], json!([base]), "{prm}");

    let (status, resp) = raw(agent
        .get(&format!("{base}/.well-known/oauth-authorization-server"))
        .call());
    assert_eq!(status, 200);
    let asm = body_json(resp);
    assert_eq!(asm["issuer"], json!(base), "{asm}");
    assert_eq!(asm["code_challenge_methods_supported"], json!(["S256"]), "{asm}");
    let register_url = asm["registration_endpoint"].as_str().unwrap().to_string();
    let authorize_url = asm["authorization_endpoint"].as_str().unwrap().to_string();
    let token_url = asm["token_endpoint"].as_str().unwrap().to_string();
    let revoke_url = asm["revocation_endpoint"].as_str().unwrap().to_string();

    // Registration: only https or loopback redirects.
    let (status, resp) = raw(agent
        .post(&register_url)
        .set("Content-Type", "application/json")
        .send_string(
            &json!({"client_name": "Evil", "redirect_uris": ["http://evil.example/cb"]}).to_string(),
        ));
    assert_eq!(status, 400);
    assert_eq!(body_json(resp)["error"], "invalid_redirect_uri");
    let redirect = "http://127.0.0.1:1/callback";
    let (status, resp) = raw(agent
        .post(&register_url)
        .set("Content-Type", "application/json")
        .send_string(
            &json!({
                "client_name": "Test connector",
                "redirect_uris": [redirect],
                "token_endpoint_auth_method": "none",
            })
            .to_string(),
        ));
    assert_eq!(status, 201);
    let reg = body_json(resp);
    let client_id = reg["client_id"].as_str().unwrap().to_string();
    assert!(reg.get("client_secret").is_none(), "{reg}");

    // Authorization without PKCE goes back to the client with an error.
    let (status, resp) = raw(agent
        .get(&authorize_url)
        .query("response_type", "code")
        .query("client_id", &client_id)
        .query("redirect_uri", redirect)
        .query("state", "s1")
        .call());
    assert_eq!(status, 302);
    let loc = resp.header("location").unwrap().to_string();
    assert_eq!(
        query_param(&loc, "error").as_deref(),
        Some("invalid_request"),
        "{loc}"
    );

    // With PKCE: the browser lands on a page asking for the one-time code.
    let verifier = "verifier-0123456789-abcdefghijklmnopqrstuvwxyz-ABCDEFG";
    let (status, resp) = raw(agent
        .get(&authorize_url)
        .query("response_type", "code")
        .query("client_id", &client_id)
        .query("redirect_uri", redirect)
        .query("state", "xyz")
        .query("code_challenge", &pkce_challenge(verifier))
        .query("code_challenge_method", "S256")
        .query("resource", &format!("{base}/mcp"))
        .call());
    assert_eq!(status, 303);
    let wait = resp.header("location").unwrap().to_string();
    let request = query_param(&wait, "request").expect("request id");
    let (status, resp) = raw(agent.get(&format!("{base}{wait}")).call());
    assert_eq!(status, 200);
    let page = resp.into_string().unwrap();
    assert!(
        page.contains("Test connector") && page.contains("one-time code"),
        "{page}"
    );

    // The code is printed on stdout, for the person at the terminal.
    let deadline = Instant::now() + Duration::from_secs(10);
    let code = loop {
        let l = lines
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the one-time code was printed");
        if let Some(rest) = l.split("One-time code: ").nth(1) {
            assert!(l.contains("Test connector"), "{l}");
            break rest.split_whitespace().next().unwrap().to_string();
        }
    };

    // A wrong code is refused; the right one sends the browser back with a code.
    let (status, resp) = raw(agent.post(&format!("{base}/authorize/wait")).send_form(&[
        ("request", &request),
        ("code", "WRONG-CODE"),
        ("action", "allow"),
    ]));
    assert_eq!(status, 401);
    assert!(resp.into_string().unwrap().contains("not right"));
    let (status, resp) = raw(agent.post(&format!("{base}/authorize/wait")).send_form(&[
        ("request", &request),
        ("code", &code.to_lowercase()),
        ("action", "allow"),
    ]));
    assert_eq!(status, 302);
    let back = resp.header("location").unwrap().to_string();
    assert!(back.starts_with(redirect), "{back}");
    assert_eq!(query_param(&back, "state").as_deref(), Some("xyz"));
    assert_eq!(
        query_param(&back, "iss").as_deref(),
        Some(base.as_str()),
        "{back}"
    );
    let auth_code = query_param(&back, "code").expect("authorization code");

    // Token exchange: PKCE checked, the code good once.
    let exchange = |code: &str, verifier: &str| {
        raw(agent.post(&token_url).send_form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect),
            ("client_id", &client_id),
            ("code_verifier", verifier),
        ]))
    };
    let (status, resp) = exchange(&auth_code, verifier);
    assert_eq!(status, 200);
    assert_eq!(resp.header("cache-control"), Some("no-store"));
    let tok = body_json(resp);
    assert_eq!(tok["token_type"], "Bearer", "{tok}");
    assert_eq!(tok["expires_in"], 3600, "{tok}");
    let access = tok["access_token"].as_str().unwrap().to_string();
    let refresh = tok["refresh_token"].as_str().unwrap().to_string();
    let (status, resp) = exchange(&auth_code, verifier);
    assert_eq!(status, 400);
    assert_eq!(body_json(resp)["error"], "invalid_grant");

    // The access token opens the MCP endpoint.
    let mut c = Client {
        url: server.url.clone(),
        token: access.clone(),
        session: None,
        next_id: 1,
    };
    c.initialize();
    let tools = c.request("tools/list", json!({}));
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for want in ["approval_status", "project_import", "project_summary"] {
        assert!(names.contains(&want), "missing {want}: {names:?}");
    }
    let (err, summary) = c.call("project_summary", json!({}));
    assert!(!err, "{summary}");

    // Refresh rotates the refresh token: the new pair works, the old refresh does not.
    let refresh_with = |rt: &str| {
        raw(agent.post(&token_url).send_form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", rt),
            ("client_id", &client_id),
        ]))
    };
    let (status, resp) = refresh_with(&refresh);
    assert_eq!(status, 200);
    let tok2 = body_json(resp);
    let access2 = tok2["access_token"].as_str().unwrap().to_string();
    let refresh2 = tok2["refresh_token"].as_str().unwrap().to_string();
    assert_ne!(refresh2, refresh);
    let (status, _) = refresh_with(&refresh);
    assert_eq!(status, 400, "a used refresh token is refused");
    assert_eq!(mcp_status(&server.url, &access2).0, 200);

    // Revoking the refresh token ends the grant: its access tokens stop at once.
    let (status, _) = raw(agent.post(&revoke_url).send_form(&[("token", &refresh2)]));
    assert_eq!(status, 200);
    let (status, challenge) = mcp_status(&server.url, &access2);
    assert_eq!(status, 401);
    assert!(challenge.unwrap_or_default().contains("invalid_token"));
    assert_eq!(mcp_status(&server.url, &access).0, 401);
    assert_eq!(refresh_with(&refresh2).0, 400);

    // The bearer token still works for local clients.
    assert_eq!(mcp_status(&server.url, &server.token).0, 200);

    // A Host the server was not told about is refused (DNS rebinding).
    let (status, _) = raw(agent
        .get(&format!("{base}/.well-known/oauth-authorization-server"))
        .set("Host", "attacker.example")
        .call());
    assert_eq!(status, 403);
}

/// `--public-url`: requests under the public host name get it as the issuer and
/// resource, so a client behind the tunnel sees one consistent origin; without
/// `--oauth` there are no OAuth endpoints and a 401 names none.
#[test]
fn headless_public_url_and_oauth_off() {
    let (server, _lines, port) = start_with(
        "three-tier.ttg.json",
        &["--oauth", "--public-url", "https://ttg-test.example.com/"],
    );
    let (status, resp) = raw(ureq::get(&format!(
        "http://127.0.0.1:{port}/.well-known/oauth-protected-resource"
    ))
    .set("Host", "ttg-test.example.com")
    .call());
    assert_eq!(status, 200);
    let prm = body_json(resp);
    assert_eq!(prm["resource"], "https://ttg-test.example.com/mcp", "{prm}");
    assert_eq!(
        prm["authorization_servers"],
        json!(["https://ttg-test.example.com"])
    );
    drop(server);

    let (server, _lines, port) = start_with("three-tier.ttg.json", &[]);
    let (status, _) = raw(ureq::get(&format!(
        "http://127.0.0.1:{port}/.well-known/oauth-authorization-server"
    ))
    .call());
    assert_eq!(status, 404);
    let (status, challenge) = mcp_status(&server.url, "wrong");
    assert_eq!(status, 401);
    assert!(!challenge.unwrap_or_default().contains("resource_metadata"));
}

/// `project_import` replaces the project from `.ttg.json` text (schema problems come
/// back with their lines, a non-empty project needs `replace`, one undo restores it),
/// and `approval_status` reports unknown tickets and lists known ones.
#[test]
fn headless_project_import_and_approval_status() {
    let server = start("three-tier.ttg.json");
    let mut c = Client::new(&server);
    c.initialize();
    let minimal: Value = serde_json::from_str(
        &std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/minimal.ttg.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let (_, before) = c.call("project_summary", json!({}));

    let (err, msg) = c.call("project_import", json!({"json": minimal}));
    assert!(err, "{msg}");
    assert!(msg.as_str().unwrap().contains("replace: true"), "{msg}");

    let broken = "{\n  \"schema_version\": 1,\n  \"name\": 5,\n  \"nodes\": {\"x\": {\"id\": \"x\", \"name\": \"x\"}}\n}";
    let (err, msg) = c.call("project_import", json!({"json": broken, "replace": true}));
    assert!(err, "{msg}");
    let text = msg.as_str().unwrap();
    assert!(text.contains("line 3") && text.contains("/name"), "{text}");
    assert!(text.contains("resource_type"), "{text}");
    let (_, same) = c.call("project_summary", json!({}));
    assert_eq!(same["name"], before["name"], "nothing changed");

    let (err, imported) = c.call("project_import", json!({"json": minimal, "replace": true}));
    assert!(!err, "{imported}");
    assert_eq!(imported["status"], "imported", "{imported}");
    assert_eq!(imported["counts"]["nodes"], 3, "{imported}");
    assert_eq!(imported["diagnostics"]["errors"], 0, "{imported}");
    assert_eq!(imported["path"], Value::Null);
    let (_, now) = c.call("project_summary", json!({}));
    assert_eq!(now["name"], "minimal");
    assert_eq!(now["dirty"], true);

    let (err, _) = c.call("undo", json!({}));
    assert!(!err);
    let (_, back) = c.call("project_summary", json!({}));
    assert_eq!(back["name"], before["name"]);
    assert_eq!(back["counts"], before["counts"]);

    let (err, msg) = c.call("approval_status", json!({"ticket": "apv-nope"}));
    assert!(err, "{msg}");
    assert!(msg.as_str().unwrap().contains("unknown ticket"), "{msg}");
    let (err, list) = c.call("approval_status", json!({}));
    assert!(!err, "{list}");
    assert_eq!(list["tickets"], json!([]), "headless never prompts");
}

/// Terse replies, renaming over MCP, a saved untitled project taking its file's name,
/// subnets that do not collide, and numeric `extra` values stored as the schema wants.
#[test]
fn headless_terse_replies_rename_and_defaults() {
    let server = start("managed-data.ttg.json");
    let mut c = Client::new(&server);
    c.initialize();

    // A new subnet in the network takes the first free /24, the next one the one after.
    let (err, a) = c.call(
        "entity_add",
        json!({"type_id": "subnet", "name": "extra a", "parent": "core"}),
    );
    assert!(!err, "{a}");
    assert_eq!(a["entity"]["config"]["cidr_block"], json!("10.50.2.0/24"), "{a}");
    let (err, b) = c.call(
        "entity_add",
        json!({"type_id": "subnet", "name": "extra b", "parent": "core", "verbose": false}),
    );
    assert!(!err, "{b}");
    // verbose: false answers with the status, the id and whether anything changed.
    assert_eq!(b.as_object().unwrap().len(), 3, "{b}");
    assert_eq!(b["changed"], json!(true), "{b}");
    let id = b["id"].as_str().unwrap().to_string();
    let p = project_of(&mut c);
    assert_eq!(p["nodes"][&id]["config"]["cidr_block"], json!("10.50.3.0/24"));

    // The session default, and a call's own verbose winning over it.
    let (err, s) = c.call("settings_set", json!({"verbose": false, "name": "zip staging"}));
    assert!(!err, "{s}");
    assert_eq!(s["name"], json!("zip staging"), "{s}");
    let (err, u) = c.call(
        "entity_update",
        json!({"entity": "app db", "config": {"storage": "40"}}),
    );
    assert!(!err, "{u}");
    assert_eq!(u, json!({"status": "updated", "id": "db-app", "changed": true}));
    let p = project_of(&mut c);
    assert_eq!(p["name"], json!("zip staging"));
    assert_eq!(
        p["nodes"]["db-app"]["config"]["storage"],
        json!(40),
        "stored as a number"
    );
    let (err, l) = c.call(
        "link_add",
        json!({"source": "app db", "target": "app config", "relation": "reads"}),
    );
    assert!(!err, "{l}");
    assert_eq!(l["id"], json!("db-app -reads-> sec-app"), "{l}");
    let (err, full) = c.call(
        "entity_update",
        json!({"entity": "web lb", "extra": {"deregistration_delay": 30}, "extra_block": "tg", "extra_provider": "aws", "verbose": true}),
    );
    assert!(!err, "{full}");
    assert!(
        full.get("entity").is_some(),
        "verbose: true gives the whole entity: {full}"
    );
    let p = project_of(&mut c);
    assert_eq!(
        p["nodes"]["lb-web"]["extra"]["aws"]["tg"]["deregistration_delay"],
        json!("30"),
        "a number for a string argument is stored as the string"
    );

    // An untitled project saved to a named file takes the file's name.
    let (err, _) = c.call("settings_set", json!({"name": "untitled"}));
    assert!(!err);
    let dir = std::env::temp_dir().join(format!("ttg-save-name-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("zipos-aws-staging.ttg.json");
    let (err, saved) = c.call("project_save", json!({"path": path.to_string_lossy()}));
    assert!(!err, "{saved}");
    assert_eq!(project_of(&mut c)["name"], json!("zipos-aws-staging"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Native data sources through the tools: schema_search labels both kinds, schema_show
/// reads a data source, entity_add creates one (and refuses a type the schema lacks), a
/// `$ref` reaches it, and `null` on an argument the mapping sets removes it until
/// `$restore` brings it back.
#[test]
fn headless_native_data_sources_and_extra_removal() {
    let server = start("three-tier.ttg.json");
    let mut c = Client::new(&server);
    c.initialize();

    let (err, hits) = c.call("schema_search", json!({"query": "managed_prefix_list"}));
    assert!(!err, "{hits}");
    let hits = hits["hits"].as_array().unwrap().clone();
    assert!(
        hits.iter().any(|h| h["kind"] == json!("data")
            && h["type_id"] == json!("native:aws:data.aws_ec2_managed_prefix_list")),
        "{hits:?}"
    );
    assert!(
        hits.iter().any(|h| h["kind"] == json!("resource")
            && h["type_id"] == json!("native:aws:aws_ec2_managed_prefix_list")),
        "{hits:?}"
    );
    let (err, data_only) = c.call(
        "schema_search",
        json!({"query": "managed_prefix_list", "kind": "data"}),
    );
    assert!(!err, "{data_only}");
    assert!(
        data_only["hits"]
            .as_array()
            .unwrap()
            .iter()
            .all(|h| h["kind"] == json!("data")),
        "{data_only}"
    );
    let (err, shown) = c.call(
        "schema_show",
        json!({"resource": "data.aws_ec2_managed_prefix_list", "depth": 0}),
    );
    assert!(!err, "{shown}");
    assert!(shown["schema"]["attributes"]["name"].is_object(), "{shown}");

    let (err, msg) = c.call(
        "entity_add",
        json!({"type_id": "native:aws:data.aws_no_such_lookup", "name": "nope"}),
    );
    assert!(err, "{msg}");
    let (err, added) = c.call(
        "entity_add",
        json!({"type_id": "native:aws:data.aws_ec2_managed_prefix_list", "name": "cf prefix", "parent": "main"}),
    );
    assert!(!err, "{added}");
    let (err, upd) = c.call(
        "entity_update",
        json!({"entity": "cf prefix", "extra": {"name": "com.amazonaws.global.cloudfront.origin-facing"}}),
    );
    assert!(!err, "{upd}");
    let (err, rule) = c.call(
        "entity_add",
        json!({"type_id": "native:aws:aws_vpc_security_group_ingress_rule", "name": "https from cloudfront", "parent": "main"}),
    );
    assert!(!err, "{rule}");
    let (err, upd) = c.call(
        "entity_update",
        json!({
            "entity": "https from cloudfront",
            "extra": {
                "security_group_id": {"$ref": {"entity": "web sg", "attr": "id"}},
                "ip_protocol": "tcp",
                "from_port": 443,
                "to_port": 443,
                "prefix_list_id": {"$ref": {"entity": "cf prefix", "attr": "id"}},
            },
        }),
    );
    assert!(!err, "{upd}");
    let (err, preview) = c.call("export_preview", json!({}));
    assert!(!err, "{preview}");
    let native = preview["files"]["native.tf"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        native.contains("data \"aws_ec2_managed_prefix_list\" \"cf_prefix\""),
        "{native}"
    );
    assert!(
        native.contains("prefix_list_id    = data.aws_ec2_managed_prefix_list.cf_prefix.id"),
        "{native}"
    );

    // `null` on an argument the database mapping sets removes it from the block.
    let (err, upd) = c.call(
        "entity_update",
        json!({"entity": "app db", "extra": {"manage_master_user_password": true, "password": null}}),
    );
    assert!(!err, "{upd}");
    assert_eq!(
        upd["entity"]["extra"]["aws"]["main"]["password"],
        Value::Null,
        "{upd}"
    );
    assert!(
        upd["entity"]["extra"]["aws"]["main"]
            .as_object()
            .unwrap()
            .contains_key("password"),
        "the removal is stored: {upd}"
    );
    let (_, preview) = c.call("export_preview", json!({}));
    let db = preview["files"]["database.tf"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        !db.lines().any(|l| l.trim_start().starts_with("password ")),
        "{db}"
    );
    assert!(db.contains("manage_master_user_password"), "{db}");
    // `$restore` brings the mapping's own value back (the mapping sets both arguments:
    // `manage_master_user_password` from the database's own option).
    let (err, upd) = c.call(
        "entity_update",
        json!({"entity": "app db", "extra": {"password": {"$restore": true}, "manage_master_user_password": {"$restore": true}}}),
    );
    assert!(!err, "{upd}");
    assert!(
        upd["entity"]["extra"].as_object().is_none_or(|x| x.is_empty()),
        "{upd}"
    );
    let (_, preview) = c.call("export_preview", json!({}));
    let db = preview["files"]["database.tf"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(db.contains("password             = var.db_password"), "{db}");
}

/// Environments through MCP: `settings_set { environments, name_prefix, variables }`,
/// `entity_update { environment }` (one entity, a selection, presence), what the export
/// makes of them, and the cost estimate of one environment. `plan_run` answers in its
/// own shape even where no tool or credentials make a plan.
#[test]
fn headless_environments() {
    let server = start("three-tier.ttg.json");
    let mut c = Client::new(&server);
    c.initialize();
    assert!(tool_names(&mut c).iter().any(|t| t == "plan_run"));
    let (err, s) = c.call(
        "settings_set",
        json!({
            "environments": ["pilot", "prod"],
            "name_prefix": "tt-${var.environment}",
            "variables": {"db_class": {"value": "db.t4g.small", "environments": {"prod": "db.r6g.large"}}},
            "tool": "opentofu"
        }),
    );
    assert!(!err, "{s}");
    let (err, bad) = c.call("settings_set", json!({"environments": ["Prod"]}));
    assert!(err, "upper case is refused: {bad}");
    let (err, u) = c.call(
        "entity_update",
        json!({"entity": "app db", "environment": "prod", "config": {"high_availability": true},
               "provider_config": {"aws": {"instance_class": "${var.db_class}"}}}),
    );
    assert!(!err, "{u}");
    assert_eq!(
        u["entities"][0]["overrides"]["config"]["high_availability"],
        json!(true),
        "{u}"
    );
    let (err, u) = c.call(
        "entity_update",
        json!({"select": {"types": ["nat_gateway"]}, "environment": "pilot", "present": false}),
    );
    assert!(!err, "{u}");
    let (err, refused) = c.call(
        "entity_update",
        json!({"entity": "app db", "environment": "qa", "config": {}}),
    );
    assert!(err, "{refused}");
    let (err, refused) = c.call("entity_update", json!({"entity": "app db", "present": false}));
    assert!(err, "present needs an environment: {refused}");
    let (_, p) = c.call("project_get", json!({"fields": ["settings", "nodes"]}));
    assert_eq!(p["settings"]["environments"], json!(["pilot", "prod"]));
    let (err, pre) = c.call("export_preview", json!({"provider": "aws"}));
    assert!(!err, "{pre}");
    let files = &pre["files"];
    assert!(
        files["environments/prod.tfvars"]
            .as_str()
            .unwrap()
            .contains("db.r6g.large"),
        "{files}"
    );
    assert!(
        files["network.tf"].as_str().unwrap().contains("count"),
        "the NAT is counted"
    );
    let (err, d) = c.call("diagnostics", json!({}));
    assert!(!err, "{d}");
    assert!(d["other_environments"].is_array(), "{d}");
    let (err, cost) = c.call("cost_estimate", json!({"environment": "prod"}));
    assert!(!err, "{cost}");
    let (err, cost_pilot) = c.call("cost_estimate", json!({"environment": "pilot"}));
    assert!(!err, "{cost_pilot}");
    assert!(
        cost["monthly"].as_f64().unwrap() > cost_pilot["monthly"].as_f64().unwrap(),
        "prod (Multi-AZ, a NAT) costs more than pilot"
    );
    let dir = std::env::temp_dir().join(format!("ttg-headless-plan-{}", std::process::id()));
    let (err, plan) = c.call(
        "plan_run",
        json!({"environment": "pilot", "dir": dir.to_string_lossy(), "vars": {"db_password": "x-not-real-x"}}),
    );
    assert!(!err, "{plan}");
    assert!(
        ["planned", "no_credentials", "failed", "binary_not_found"]
            .contains(&plan["status"].as_str().unwrap()),
        "{plan}"
    );
    assert!(
        plan["entities"].is_array() && plan["diagnostics"].is_array(),
        "{plan}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

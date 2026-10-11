//! The diff, checkpoint, pattern, adoption and directory-import tools over MCP, against
//! `terratofu-gui --serve` on a copy of an example (checkpoints write a file beside the
//! project, saved patterns a folder of their own: both go to a scratch directory).

#![cfg(feature = "mcp")]
#![allow(clippy::result_large_err)]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Server {
    child: Child,
    url: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

const TOKEN: &str = "test-token";

fn start(project: &Path, patterns: &Path) -> Server {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut child = Command::new(env!("CARGO_BIN_EXE_terratofu-gui"))
        .args(["--serve", "--port", &port.to_string(), "--token", TOKEN])
        .arg(project)
        .env("TTG_PATTERNS_DIR", patterns)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn terratofu-gui --serve");
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        assert!(Instant::now() < deadline, "server did not report listening");
        match lines.next() {
            Some(Ok(l)) if l.contains("listening on") => break,
            Some(Ok(_)) => continue,
            _ => panic!("server exited before listening"),
        }
    }
    std::thread::spawn(move || for _ in lines {});
    Server {
        child,
        url: format!("http://127.0.0.1:{port}/mcp"),
    }
}

struct Client {
    url: String,
    session: Option<String>,
    next_id: u64,
}

impl Client {
    fn new(s: &Server) -> Self {
        let mut c = Client {
            url: s.url.clone(),
            session: None,
            next_id: 1,
        };
        c.request(
            "initialize",
            json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "ttg-test", "version": "0"}}),
        );
        let _ = c.post(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        c
    }

    fn post(&mut self, body: Value) -> (Option<String>, String) {
        let mut req = ureq::post(&self.url)
            .set("Authorization", &format!("Bearer {TOKEN}"))
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream");
        if let Some(s) = &self.session {
            req = req.set("Mcp-Session-Id", s);
        }
        let resp = req.send_string(&body.to_string()).expect("post");
        let sid = resp.header("mcp-session-id").map(|s| s.to_string());
        let ct = resp.header("content-type").unwrap_or("").to_string();
        let text = resp.into_string().unwrap();
        let payload = if ct.starts_with("text/event-stream") {
            text.lines()
                .filter_map(|l| l.strip_prefix("data:"))
                .map(|l| l.trim().to_string())
                .rfind(|l| l.starts_with('{'))
                .unwrap_or_default()
        } else {
            text
        };
        (sid, payload)
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let (sid, payload) =
            self.post(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        if sid.is_some() {
            self.session = sid;
        }
        let v: Value = serde_json::from_str(&payload).unwrap_or_else(|e| panic!("{method}: {e}: {payload}"));
        assert!(v.get("error").is_none(), "{method}: {v}");
        v["result"].clone()
    }

    fn call(&mut self, tool: &str, args: Value) -> (bool, Value) {
        let r = self.request("tools/call", json!({"name": tool, "arguments": args}));
        let is_error = r["isError"].as_bool().unwrap_or(false);
        let text = r["content"][0]["text"].as_str().unwrap_or("").to_string();
        (
            is_error,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    fn ok(&mut self, tool: &str, args: Value) -> Value {
        let (err, v) = self.call(tool, args.clone());
        assert!(!err, "{tool} {args}: {v}");
        v
    }

    fn fails(&mut self, tool: &str, args: Value) -> String {
        let (err, v) = self.call(tool, args.clone());
        assert!(err, "{tool} {args} should fail: {v}");
        v.as_str().unwrap_or_default().to_string()
    }

    fn counts(&mut self) -> Value {
        self.ok("project_summary", json!({}))["counts"].clone()
    }
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ttg-mcp-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn copy_example(name: &str, into: &Path) -> PathBuf {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(name);
    let dst = into.join(name);
    std::fs::copy(src, &dst).unwrap();
    dst
}

#[test]
fn headless_diff_checkpoints_and_patterns() {
    let dir = scratch("revisions");
    let project = copy_example("three-tier.ttg.json", &dir);
    let patterns = dir.join("patterns");
    let server = start(&project, &patterns);
    let mut c = Client::new(&server);

    let tools: Vec<String> = c.request("tools/list", json!({}))["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    for t in [
        "project_diff",
        "checkpoint_create",
        "checkpoint_list",
        "checkpoint_restore",
        "pattern_list",
        "pattern_apply",
        "pattern_save",
        "project_import_dir",
        "entity_update",
    ] {
        assert!(tools.iter().any(|x| x == t), "{t} missing from {tools:?}");
    }

    // Nothing changed since the file was opened.
    let d = c.ok("project_diff", json!({"against": "saved"}));
    assert_eq!(d["changed"], false, "{d}");

    // A checkpoint beside the file, then changes, then the diff against it.
    let cp = c.ok("checkpoint_create", json!({"name": "before"}));
    assert_eq!(cp["status"], "created");
    assert!(dir.join("three-tier.checkpoints.json").is_file());
    let msg = c.fails("checkpoint_create", json!({"name": "Before"}));
    assert!(msg.contains("replace: true"), "{msg}");
    c.ok("checkpoint_create", json!({"name": "before", "replace": true}));
    c.ok(
        "entity_update",
        json!({"entity": "assets", "name": "static assets", "config": {"versioning": false}}),
    );
    c.ok(
        "entity_add",
        json!({"type_id": "event_queue", "name": "jobs", "parent": "app"}),
    );
    let d = c.ok("project_diff", json!({"against": "before"}));
    assert_eq!(d["changed"], true);
    let md = d["markdown"].as_str().unwrap();
    assert!(md.contains("renamed from \"assets\""), "{md}");
    assert!(
        md.contains("`config.versioning`: `true` \u{2192} `false`"),
        "{md}"
    );
    assert!(md.contains("| jobs | Event Queue | app |"), "{md}");
    assert_eq!(d["diff"]["entities"]["added"][0]["name"], "jobs");
    let only_md = c.ok("project_diff", json!({"against": "before", "format": "markdown"}));
    assert!(only_md.get("diff").is_none() && only_md["markdown"].is_string());
    let msg = c.fails("project_diff", json!({"against": "nothing like this"}));
    assert!(msg.contains("checkpoint") && msg.contains("before"), "{msg}");

    let list = c.ok("checkpoint_list", json!({}));
    assert_eq!(list["checkpoints"].as_array().unwrap().len(), 1);
    assert!(list["checkpoints"][0]["changes_from_current"]
        .as_str()
        .unwrap()
        .contains("1 removed"));

    // Restoring is one undo step.
    let changed = c.counts();
    let r = c.ok("checkpoint_restore", json!({"name": "before"}));
    assert_eq!(r["status"], "restored", "{r}");
    let restored = c.counts();
    assert_ne!(restored, changed);
    c.ok("undo", json!({}));
    assert_eq!(c.counts(), changed);
    c.ok("checkpoint_restore", json!({"name": "before"}));

    // Patterns: the built-ins, one applied in the resource group as one undo step.
    let pl = c.ok("pattern_list", json!({}));
    let names: Vec<&str> = pl["patterns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    for want in [
        "Hardened bucket",
        "Queue with dead-letter queue and alarms",
        "Queue-driven GPU worker",
        "Web service behind a load balancer",
    ] {
        assert!(names.contains(&want), "{names:?}");
    }
    let before = c.counts();
    let applied = c.ok(
        "pattern_apply",
        json!({"name": "queue with dead letter queue and alarms", "params": {"name": "orders"}, "parent": "app"}),
    );
    assert_eq!(applied["status"], "applied", "{applied}");
    assert_eq!(applied["entities"].as_array().unwrap().len(), 5);
    assert!(applied["entities"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["id"].is_string()));
    assert_eq!(applied["links"], 3);
    assert_ne!(c.counts(), before);
    c.ok("undo", json!({}));
    assert_eq!(c.counts(), before, "one undo takes the whole pattern back");
    let msg = c.fails(
        "pattern_apply",
        json!({"name": "hardened bucket", "params": {"colour": "red"}}),
    );
    assert!(msg.contains("unknown parameter \"colour\""), "{msg}");
    let msg = c.fails("pattern_apply", json!({"name": "no such pattern"}));
    assert!(msg.contains("Hardened bucket"), "{msg}");
    assert_eq!(c.counts(), before);

    // Saving a selection as a pattern, then applying it again under another name.
    c.ok(
        "pattern_apply",
        json!({"name": "queue with dead letter queue and alarms", "params": {"name": "orders"}, "parent": "app"}),
    );
    let saved = c.ok(
        "pattern_save",
        json!({"name": "Orders queue", "entities": ["orders", "orders dead letters"]}),
    );
    assert_eq!(saved["status"], "saved", "{saved}");
    assert!(patterns.join("orders_queue.json").is_file());
    let pl = c.ok("pattern_list", json!({}));
    let mine = pl["patterns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "Orders queue")
        .expect("the saved pattern is listed")
        .clone();
    assert_eq!(mine["builtin"], false);
    assert_eq!(mine["params"][0]["name"], "name");
    let again = c.ok(
        "pattern_apply",
        json!({"name": "orders queue", "params": {"name": "refunds"}, "parent": "app"}),
    );
    let mut added: Vec<&str> = again["entities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    added.sort();
    assert_eq!(added, ["refunds", "refunds dead letters"]);
    let msg = c.fails(
        "pattern_save",
        json!({"name": "hardened bucket", "entities": ["orders"]}),
    );
    assert!(msg.contains("built-in"), "{msg}");
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn headless_adopt_and_import_dir() {
    let dir = scratch("import");
    let project = copy_example("three-tier.ttg.json", &dir);
    let server = start(&project, &dir.join("patterns"));
    let mut c = Client::new(&server);

    // Adopting the network: an import block for it, warnings for nothing else.
    let r = c.ok(
        "entity_update",
        json!({"entity": "main", "adopt": {"main": "vpc-0abc1234"}}),
    );
    assert_eq!(r["entity"]["adopt"]["aws"]["main"], "vpc-0abc1234", "{r}");
    assert_eq!(r["imports"][0]["address"], "aws_vpc.main", "{r}");
    let msg = c.fails("entity_update", json!({"entity": "main", "adopt": {"nope": "x"}}));
    assert!(
        msg.contains("names no block") && msg.contains("aws_vpc.main"),
        "{msg}"
    );
    // Together with another change: one undo step for both.
    let r = c.ok(
        "entity_update",
        json!({"entity": "app server", "description": "adopted", "adopt": {"aws": {"main": "i-0123456789abcdef0"}}}),
    );
    assert_eq!(r["entity"]["description"], "adopted", "{r}");
    assert!(r["adopt_warnings"].as_array().is_some());
    c.ok("undo", json!({}));
    let p = c.ok("project_get", json!({"entities": ["app server"]}));
    let server_node = p["nodes"].as_object().unwrap().values().next().unwrap().clone();
    assert!(
        server_node.get("adopt").is_none() && server_node.get("description").is_none(),
        "{server_node}"
    );
    let out = dir.join("out");
    let ex = c.ok("export_run", json!({"dir": out.display().to_string()}));
    assert!(
        ex["files"].as_array().unwrap().iter().any(|f| f == "imports.tf"),
        "{ex}"
    );
    let imports = std::fs::read_to_string(out.join("imports.tf")).unwrap();
    assert!(
        imports.contains("to = aws_vpc.main") && imports.contains("id = \"vpc-0abc1234\""),
        "{imports}"
    );

    // Hand edits to the export come back as a preview, then as one undo step.
    let storage = out.join("storage.tf");
    let text = std::fs::read_to_string(&storage).unwrap();
    std::fs::write(
        &storage,
        text.replacen(
            "bucket = \"three-tier-assets-x7q2\"",
            "bucket        = \"three-tier-assets-x7q2\"\n  force_destroy = true",
            1,
        ),
    )
    .unwrap();
    let rev = c.ok("project_changes", json!({}))["revision"].clone();
    let pv = c.ok("project_import_dir", json!({"dir": out.display().to_string()}));
    assert_eq!(pv["status"], "preview", "{pv}");
    assert_eq!(pv["applied"], false);
    assert_eq!(
        pv["report"]["overrides"][0]["args"]["force_destroy"], true,
        "{pv}"
    );
    assert!(
        pv["markdown"]
            .as_str()
            .unwrap()
            .contains("extra.aws.main.force_destroy"),
        "{pv}"
    );
    assert_eq!(
        c.ok("project_changes", json!({}))["revision"],
        rev,
        "a preview changes nothing"
    );
    let ap = c.ok(
        "project_import_dir",
        json!({"dir": out.display().to_string(), "apply": true}),
    );
    assert_eq!(ap["status"], "applied", "{ap}");
    let p = c.ok("project_get", json!({"entities": ["assets"]}));
    let bucket = p["nodes"].as_object().unwrap().values().next().unwrap().clone();
    assert_eq!(bucket["extra"]["aws"]["main"]["force_destroy"], true, "{bucket}");
    c.ok("undo", json!({}));
    let p = c.ok("project_get", json!({"entities": ["assets"]}));
    let bucket = p["nodes"].as_object().unwrap().values().next().unwrap().clone();
    assert!(bucket.get("extra").is_none(), "{bucket}");
    let msg = c.fails("project_import_dir", json!({"dir": dir.display().to_string()}));
    assert!(msg.contains(".tf"), "{msg}");
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}

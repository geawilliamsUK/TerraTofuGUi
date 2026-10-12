//! View notes over MCP without a display: an anchored note goes where it is asked to,
//! keeps its offset when its resource moves, can be edited in place, and carries a colour
//! into the Markdown export.

#![cfg(feature = "mcp")]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::PathBuf;
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

fn start(project: &str) -> Server {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let example = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(project);
    let mut child = Command::new(env!("CARGO_BIN_EXE_terratofu-gui"))
        .args(["--serve", "--port", &port.to_string(), "--token", TOKEN])
        .arg(&example)
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
        c.post(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        c
    }

    fn post(&mut self, body: Value) -> String {
        let mut req = ureq::post(&self.url)
            .set("Authorization", &format!("Bearer {TOKEN}"))
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream");
        if let Some(s) = &self.session {
            req = req.set("Mcp-Session-Id", s);
        }
        let resp = req.send_string(&body.to_string()).expect("request");
        if let Some(sid) = resp.header("mcp-session-id") {
            self.session = Some(sid.to_string());
        }
        let sse = resp
            .header("content-type")
            .unwrap_or("")
            .starts_with("text/event-stream");
        let text = resp.into_string().unwrap();
        if sse {
            text.lines()
                .filter_map(|l| l.strip_prefix("data:"))
                .map(|l| l.trim().to_string())
                .rfind(|l| l.starts_with('{'))
                .unwrap_or_default()
        } else {
            text
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let payload = self.post(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let v: Value = serde_json::from_str(&payload).unwrap_or_else(|e| panic!("{method}: {e}: {payload}"));
        assert!(v.get("error").is_none(), "{method}: {v}");
        v["result"].clone()
    }

    fn call(&mut self, tool: &str, args: Value) -> (bool, Value) {
        let r = self.request("tools/call", json!({"name": tool, "arguments": args}));
        let text = r["content"][0]["text"].as_str().unwrap_or("").to_string();
        (
            r["isError"].as_bool().unwrap_or(false),
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    fn note(&mut self, title: &str) -> Value {
        let (err, v) = self.call("view_get", json!({"name": "Data flow"}));
        assert!(!err, "{v}");
        v["notes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["title"] == title)
            .cloned()
            .unwrap_or_else(|| panic!("no note {title}: {v}"))
    }
}

#[test]
fn notes_keep_their_place_and_colour_and_can_be_edited() {
    let server = start("job-pipeline.ttg.json");
    let mut c = Client::new(&server);

    // An anchored note goes where it is asked to, at the size asked for.
    let (err, added) = c.call(
        "view_note_add",
        json!({"view": "Data flow", "title": "Never fires", "body": "Threshold above the class's limit.",
               "anchor": "JobGateway", "x": 1000, "y": 900, "w": 300, "h": 100, "color": "Warning"}),
    );
    assert!(!err, "{added}");
    assert_eq!(
        added["at"],
        json!({"x": 1000.0, "y": 900.0, "w": 300.0, "h": 100.0}),
        "{added}"
    );
    let n = c.note("Never fires");
    assert_eq!(n["position"], json!({"x": 1000, "y": 900}), "{n}");
    assert_eq!(n["color"], json!("warning"), "{n}");
    assert_eq!(n["anchor"], json!({"entity": "fn-gateway"}), "{n}");

    // Moving the resource carries the note at the same offset.
    let (err, moved) = c.call(
        "entity_move",
        json!({"entity": "JobGateway", "x": 300, "y": 300, "view": "Data flow"}),
    );
    assert!(!err, "{moved}");
    let after = c.note("Never fires");
    assert_eq!(after["offset"], n["offset"], "{after}");
    let (err, moved) = c.call(
        "entity_move",
        json!({"entity": "JobGateway", "x": 340, "y": 320, "view": "Data flow"}),
    );
    assert!(!err, "{moved}");
    let again = c.note("Never fires");
    assert_eq!(
        again["position"]["x"].as_i64().unwrap() - after["position"]["x"].as_i64().unwrap(),
        40
    );
    assert_eq!(
        again["position"]["y"].as_i64().unwrap() - after["position"]["y"].as_i64().unwrap(),
        20
    );

    // The colour reaches the Markdown export as an alert.
    let (_, md) = c.call("view_export", json!({"view": "Data flow"}));
    let text = md["text"].as_str().unwrap();
    assert!(
        text.contains("> [!WARNING]\n> Threshold above the class's limit."),
        "{text}"
    );

    // Edited in place: text, colour, position, then the anchor removed.
    let (err, upd) = c.call(
        "view_note_update",
        json!({"view": "Data flow", "note": "Never fires", "title": "Fires at 69", "color": "#336699",
               "x": 500, "y": 600, "w": 280}),
    );
    assert!(!err, "{upd}");
    let n = c.note("Fires at 69");
    assert_eq!(n["position"], json!({"x": 500, "y": 600}), "{n}");
    assert_eq!(n["size"], json!({"w": 280, "h": 100}), "{n}");
    assert_eq!(n["color"], json!("#336699"), "{n}");
    assert!(n["anchor"].is_object(), "{n}");
    let (err, upd) = c.call(
        "view_note_update",
        json!({"view": "Data flow", "note": "Fires at 69", "anchor": null, "color": null}),
    );
    assert!(!err, "{upd}");
    let n = c.note("Fires at 69");
    assert!(n["anchor"].is_null(), "{n}");
    assert!(n["color"].is_null(), "{n}");
    assert_eq!(
        n["position"],
        json!({"x": 500, "y": 600}),
        "unpinned where it was drawn: {n}"
    );

    // A colour that is no colour is refused, and nothing changes.
    let (err, msg) = c.call(
        "view_note_update",
        json!({"view": "Data flow", "note": "Fires at 69", "color": "orange", "title": "x"}),
    );
    assert!(err, "{msg}");
    assert!(
        msg.to_string()
            .contains("neither one of neutral, info, warning, danger, success"),
        "{msg}"
    );
    c.note("Fires at 69");

    // One undo step per change.
    let (err, _) = c.call("undo", json!({}));
    assert!(!err);
    let n = c.note("Fires at 69");
    assert_eq!(n["color"], json!("#336699"), "{n}");
}

/// `view_arrange_notes` also places free notes: those on top of a resource or of each
/// other go to a column right of the content, in reading order; clear ones stay; and
/// `free_notes: false` leaves every free note alone.
#[test]
fn arranging_notes_places_the_free_ones_in_the_way() {
    let server = start("job-pipeline.ttg.json");
    let mut c = Client::new(&server);
    let (_, view) = c.call("view_get", json!({"name": "Data flow"}));
    let at = |id: &str| &view["layout"]["positions"][id];
    let (gx, gy) = (
        at("fn-gateway")["x"].as_i64().unwrap(),
        at("fn-gateway")["y"].as_i64().unwrap(),
    );
    // The right edge of what the view draws: every shown entity is a plain node here.
    let right = view["shown"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| view["layout"]["positions"][e["id"].as_str().unwrap()]["x"].as_i64())
        .max()
        .unwrap()
        + 176;
    // Two free notes on the gateway (the lower one added first) and one far from anything.
    for (title, x, y) in [
        ("Low", gx + 10, gy + 30),
        ("High", gx + 10, gy + 10),
        ("Far", 9000, 9000),
    ] {
        let (err, added) = c.call(
            "view_note_add",
            json!({"view": "Data flow", "title": title, "x": x, "y": y}),
        );
        assert!(!err, "{added}");
    }
    let pos = |c: &mut Client, t: &str| {
        let n = c.note(t);
        (
            n["position"]["x"].as_i64().unwrap(),
            n["position"]["y"].as_i64().unwrap(),
        )
    };
    let before = [pos(&mut c, "Low"), pos(&mut c, "High"), pos(&mut c, "Far")];

    // Told to leave them alone, it does.
    let (err, left) = c.call(
        "view_arrange_notes",
        json!({"view": "Data flow", "free_notes": false}),
    );
    assert!(!err, "{left}");
    assert_eq!(left["free"], json!(0), "{left}");
    assert_eq!(
        [pos(&mut c, "Low"), pos(&mut c, "High"), pos(&mut c, "Far")],
        before
    );

    // By default the two in the way move; the one in free space stays.
    let (err, arranged) = c.call("view_arrange_notes", json!({"view": "Data flow"}));
    assert!(!err, "{arranged}");
    assert_eq!(arranged["free"], json!(2), "{arranged}");
    assert_eq!(arranged["moved"], json!(2), "{arranged}");
    let (low, high, far) = (pos(&mut c, "Low"), pos(&mut c, "High"), pos(&mut c, "Far"));
    assert_eq!(far, before[2]);
    assert!(
        low.0 > right && high.0 > right,
        "right of the content: {low:?} {high:?}"
    );
    // Reading order is kept: "High" was higher up. They do not overlap.
    let h = c.note("High")["size"]["h"].as_i64().unwrap();
    assert!(high.1 + h <= low.1, "{high:?} {low:?}");

    // Nothing left to do, and one undo puts them back.
    let (_, again) = c.call("view_arrange_notes", json!({"view": "Data flow"}));
    assert_eq!(again["moved"], json!(0), "{again}");
    let (err, _) = c.call("undo", json!({}));
    assert!(!err);
    assert_eq!(
        [pos(&mut c, "Low"), pos(&mut c, "High"), pos(&mut c, "Far")],
        before
    );
}

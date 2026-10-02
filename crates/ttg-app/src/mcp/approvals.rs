//! Approval tickets: a write that needs the user's Allow / Deny answers its caller at
//! once with a ticket instead of holding the call open while the prompt waits.
//!
//! MCP clients time out long before a person gets round to a prompt (often after 60 s),
//! and a call that times out tells the caller nothing about whether the write will still
//! happen. So the UI thread parks the command, replies `pending_approval` with a ticket
//! straight away, and records the outcome here when the user answers: applied (with the
//! command's result), failed, denied, or expired when nobody answered in time. The server
//! thread reads this table directly for `approval_status`, so polling never queues
//! behind the UI.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Default time a ticket waits for the user before it expires unapplied.
pub const DEFAULT_TTL_SECS: u64 = 600;

/// How long an answered ticket's outcome stays readable through `approval_status`.
const KEEP_RESOLVED: Duration = Duration::from_secs(30 * 60);

/// At most this many answered tickets are kept; the oldest go first.
const KEEP_MAX: usize = 200;

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Waiting for the user (or for an earlier prompt to be answered first).
    Pending,
    /// Allowed and run; the command's own reply.
    Applied(Value),
    /// Allowed, but the command itself failed, so nothing was applied.
    Failed(String),
    Denied,
    /// Nobody answered before the ticket's time ran out; nothing was applied.
    Expired,
}

#[derive(Debug, Clone)]
pub struct Ticket {
    pub id: String,
    /// The tool, as the activity log names it (`ProjectSave`).
    pub tool: String,
    /// What the user is asked to allow ("save the project to out.ttg.json").
    pub what: String,
    pub created: Instant,
    pub expires: Instant,
    pub resolved: Option<Instant>,
    pub outcome: Outcome,
}

/// Every ticket handed out, shared by the UI thread (which opens and resolves them) and
/// the server thread (which reports them).
#[derive(Debug, Default)]
pub struct Approvals {
    tickets: Mutex<BTreeMap<String, Ticket>>,
}

impl Approvals {
    /// A new pending ticket for a write the user has to allow.
    pub fn open(&self, tool: &str, what: &str, ttl: Duration) -> String {
        let id = format!("apv-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        let now = Instant::now();
        let mut t = self.tickets.lock().unwrap();
        prune(&mut t, now);
        t.insert(
            id.clone(),
            Ticket {
                id: id.clone(),
                tool: tool.to_string(),
                what: what.to_string(),
                created: now,
                expires: now + ttl,
                resolved: None,
                outcome: Outcome::Pending,
            },
        );
        id
    }

    /// Record the outcome of a ticket that was still pending. An outcome is final: a
    /// ticket that already expired stays expired.
    pub fn resolve(&self, id: &str, outcome: Outcome) {
        if let Some(t) = self.tickets.lock().unwrap().get_mut(id) {
            if t.outcome == Outcome::Pending {
                t.outcome = outcome;
                t.resolved = Some(Instant::now());
            }
        }
    }

    /// Add `key` to an applied ticket's result: what happened after the command itself,
    /// such as the validate run of an approved export.
    pub fn amend(&self, id: &str, key: &str, value: Value) {
        if let Some(t) = self.tickets.lock().unwrap().get_mut(id) {
            if let Outcome::Applied(Value::Object(o)) = &mut t.outcome {
                o.insert(key.to_string(), value);
            }
        }
    }

    /// Whether a pending ticket's time has run out (and so must not be run any more).
    pub fn is_expired(&self, id: &str, now: Instant) -> bool {
        self.tickets.lock().unwrap().get(id).is_none_or(|t| {
            t.outcome == Outcome::Expired || (t.outcome == Outcome::Pending && now >= t.expires)
        })
    }

    pub fn get(&self, id: &str) -> Option<Ticket> {
        self.tickets.lock().unwrap().get(id).cloned()
    }

    /// The ids of every ticket still waiting, oldest first.
    pub fn pending_ids(&self) -> Vec<String> {
        let t = self.tickets.lock().unwrap();
        let mut v: Vec<&Ticket> = t.values().filter(|t| t.outcome == Outcome::Pending).collect();
        v.sort_by_key(|t| t.created);
        v.into_iter().map(|t| t.id.clone()).collect()
    }

    /// One ticket as `approval_status` (and the call that opened it) reports it.
    pub fn status_json(&self, id: &str) -> Option<Value> {
        let t = self.get(id)?;
        let queue = self.pending_ids();
        Some(ticket_json(&t, queue.iter().position(|q| q == id)))
    }

    /// Every ticket still known, newest first.
    pub fn list_json(&self) -> Value {
        let queue = self.pending_ids();
        let t = self.tickets.lock().unwrap();
        let mut v: Vec<&Ticket> = t.values().collect();
        v.sort_by_key(|t| std::cmp::Reverse(t.created));
        Value::Array(
            v.into_iter()
                .map(|t| ticket_json(t, queue.iter().position(|q| *q == t.id)))
                .collect(),
        )
    }
}

/// Drop answered tickets past their keeping time, and the oldest beyond the cap.
fn prune(t: &mut BTreeMap<String, Ticket>, now: Instant) {
    t.retain(|_, x| x.resolved.is_none_or(|r| now.duration_since(r) < KEEP_RESOLVED));
    while t.len() > KEEP_MAX {
        let oldest = t
            .values()
            .filter(|x| x.outcome != Outcome::Pending)
            .min_by_key(|x| x.created)
            .map(|x| x.id.clone());
        match oldest {
            Some(id) => {
                t.remove(&id);
            }
            None => break,
        }
    }
}

/// `position`: where a pending ticket is in the queue of prompts (0 = on screen now).
fn ticket_json(t: &Ticket, position: Option<usize>) -> Value {
    let now = Instant::now();
    let base = json!({
        "ticket": t.id,
        "tool": t.tool,
        "what": t.what,
    });
    let mut o = base.as_object().cloned().unwrap_or_default();
    let mut put = |k: &str, v: Value| {
        o.insert(k.to_string(), v);
    };
    match &t.outcome {
        Outcome::Pending => {
            put("status", json!("pending_approval"));
            put("applied", json!(false));
            put("waiting_s", json!(now.duration_since(t.created).as_secs()));
            put(
                "expires_in_s",
                json!(t.expires.saturating_duration_since(now).as_secs()),
            );
            let ahead = position.unwrap_or(0);
            put("prompts_ahead", json!(ahead));
            put(
                "message",
                json!(if ahead == 0 {
                    "Waiting for the user to Allow or Deny this in TerraTofu GUI. Nothing has been applied yet. Do not repeat the call: poll approval_status with this ticket."
                } else {
                    "Queued behind another approval prompt in TerraTofu GUI; it is shown once that one is answered. Nothing has been applied yet. Do not repeat the call: poll approval_status with this ticket."
                }),
            );
        }
        Outcome::Applied(result) => {
            put("status", json!("applied"));
            put("applied", json!(true));
            put("result", result.clone());
        }
        Outcome::Failed(e) => {
            put("status", json!("failed"));
            put("applied", json!(false));
            put("error", json!(e));
            put(
                "message",
                json!("The user allowed it, but the command failed, so nothing was applied."),
            );
        }
        Outcome::Denied => {
            put("status", json!("denied"));
            put("applied", json!(false));
            put("message", json!("The user denied it; nothing was applied."));
        }
        Outcome::Expired => {
            put("status", json!("expired"));
            put("applied", json!(false));
            put(
                "message",
                json!("Nobody answered the prompt in time; nothing was applied. Ask the user before trying again."),
            );
        }
    }
    Value::Object(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ticket_reports_each_outcome_with_whether_it_applied() {
        let a = Approvals::default();
        let id = a.open("ProjectSave", "save the project", Duration::from_secs(60));
        let s = a.status_json(&id).unwrap();
        assert_eq!(s["status"], "pending_approval");
        assert_eq!(s["applied"], false);
        assert_eq!(s["prompts_ahead"], 0);
        assert!(s["expires_in_s"].as_u64().unwrap() <= 60);

        let second = a.open("ExportRun", "write an export", Duration::from_secs(60));
        assert_eq!(a.status_json(&second).unwrap()["prompts_ahead"], 1);

        a.resolve(&id, Outcome::Applied(json!({"status": "saved"})));
        a.amend(&id, "validate", json!("Success"));
        let s = a.status_json(&id).unwrap();
        assert_eq!(s["status"], "applied");
        assert_eq!(s["applied"], true);
        assert_eq!(s["result"]["status"], "saved");
        assert_eq!(s["result"]["validate"], "Success");
        // An outcome is final.
        a.resolve(&id, Outcome::Denied);
        assert_eq!(a.status_json(&id).unwrap()["status"], "applied");
        // The second is now the one on screen.
        assert_eq!(a.status_json(&second).unwrap()["prompts_ahead"], 0);

        a.resolve(&second, Outcome::Denied);
        assert_eq!(a.status_json(&second).unwrap()["applied"], false);
        assert_eq!(a.list_json().as_array().unwrap().len(), 2);
    }

    #[test]
    fn a_ticket_past_its_time_is_expired() {
        let a = Approvals::default();
        let id = a.open("ProjectSave", "save", Duration::from_millis(0));
        assert!(a.is_expired(&id, Instant::now()));
        a.resolve(&id, Outcome::Expired);
        assert_eq!(a.status_json(&id).unwrap()["status"], "expired");
        // Unknown tickets count as expired: there is nothing left to run.
        assert!(a.is_expired("apv-nope", Instant::now()));
        assert!(a.status_json("apv-nope").is_none());
    }
}

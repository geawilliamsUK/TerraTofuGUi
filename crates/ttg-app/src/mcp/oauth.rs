//! A small OAuth 2.1 authorization server for the MCP endpoint, so a client that can
//! only sign in with OAuth (a claude.ai custom connector, a cloud agent session) can use
//! the server once it is reachable over HTTPS through a tunnel.
//!
//! It follows the MCP authorization spec (2025-06-18 onward):
//!
//! - `GET /.well-known/oauth-protected-resource[/mcp]` (RFC 9728) names this server as
//!   the authorization server for `<base>/mcp`, and a 401 from `/mcp` carries
//!   `WWW-Authenticate: Bearer resource_metadata="…"` pointing at it.
//! - `GET /.well-known/oauth-authorization-server` (RFC 8414; also served as
//!   `openid-configuration`) lists the endpoints below.
//! - `POST /register`: dynamic client registration (RFC 7591).
//! - `GET /authorize`: authorization code with PKCE S256 only. The person decides: in the
//!   window app a prompt asks "Allow <client> to edit this project?" (the browser page and
//!   the prompt show the same short code, so the person can tell their own request from
//!   someone else's); headless (`--serve`) a one-time code is printed on stdout and has to
//!   be typed into the page.
//! - `POST /token`: `authorization_code` and `refresh_token` grants. Access tokens last
//!   an hour, refresh tokens thirty days and are rotated on every use.
//! - `POST /revoke` (RFC 7009).
//!
//! Only SHA-256 hashes of tokens, codes and client secrets are kept. Registered clients
//! and grants persist (the window app through its own storage, `--serve` in the file
//! given with `--grants`); access tokens, codes and pending requests live in memory, so a
//! restart costs a client one refresh. Grants are listed, and revoked, in Agent ▸
//! Settings & activity.

use axum::{
    extract::{Form, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Access-token lifetime.
pub const ACCESS_TTL: u64 = 60 * 60;
/// Refresh-token lifetime; every refresh starts it again.
pub const REFRESH_TTL: u64 = 30 * 24 * 60 * 60;
/// An authorization code must be exchanged within this.
const CODE_TTL: u64 = 5 * 60;
/// A sign-in request waits this long for the person to decide.
const REQUEST_TTL: u64 = 10 * 60;
/// Sign-in requests waiting at once; more are refused, so nobody can flood the prompt.
const MAX_PENDING: usize = 5;
/// Registered clients kept; the oldest without a grant go first.
const MAX_CLIENTS: usize = 50;
/// Wrong one-time codes before a request is thrown away.
const MAX_CODE_ATTEMPTS: u32 = 5;
/// The one scope there is: use the MCP endpoint.
pub const SCOPE: &str = "mcp";

/// Unambiguous characters for codes a person reads or types (no 0/O, 1/I/L).
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    rand::fill(&mut b[..]);
    b
}

/// A bearer secret: 256 random bits, URL-safe, with a prefix saying what it is.
fn secret(prefix: &str) -> String {
    use base64::Engine;
    format!(
        "{prefix}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes::<32>())
    )
}

/// `len` characters a person can read aloud, grouped in fours (or threes).
fn human_code(len: usize) -> String {
    let raw = random_bytes::<16>();
    let chars: Vec<char> = raw
        .iter()
        .take(len)
        .map(|b| CODE_ALPHABET[*b as usize % CODE_ALPHABET.len()] as char)
        .collect();
    let group = if len.is_multiple_of(4) { 4 } else { 3 };
    chars
        .chunks(group)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-")
}

/// What a person typed, as it is compared: case, spaces and dashes do not matter.
fn normalise_code(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

pub fn hash(s: &str) -> String {
    let d = Sha256::digest(s.as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// PKCE S256: `BASE64URL(SHA256(verifier)) == challenge`.
pub fn pkce_matches(verifier: &str, challenge: &str) -> bool {
    use base64::Engine;
    let ok_len = (43..=128).contains(&verifier.len());
    let ok_chars = verifier
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c));
    ok_len
        && ok_chars
        && base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
            == challenge
}

/// A client that registered itself (RFC 7591).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Client {
    pub id: String,
    /// Hash of the client secret, for clients that asked for one.
    pub secret_hash: Option<String>,
    pub name: String,
    pub redirect_uris: Vec<String>,
    pub created: u64,
}

/// One client's standing permission to use the server: what a person allowed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Grant {
    pub id: String,
    pub client_id: String,
    pub client_name: String,
    pub created: u64,
    pub last_used: u64,
    pub refresh_hash: String,
    pub refresh_expires: u64,
}

/// A sign-in waiting for the person.
#[derive(Debug, Clone)]
struct AuthRequest {
    id: String,
    client_id: String,
    client_name: String,
    redirect_uri: String,
    state: Option<String>,
    challenge: String,
    created: u64,
    /// Shown on the browser page and in the app's prompt, so the two can be matched.
    verify: String,
    /// Headless: hash of the one-time code printed on stdout.
    one_time_hash: Option<String>,
    attempts: u32,
    decision: Option<bool>,
}

#[derive(Debug, Clone)]
struct Code {
    hash: String,
    client_id: String,
    redirect_uri: String,
    challenge: String,
    created: u64,
}

#[derive(Debug, Clone)]
struct Access {
    grant_id: String,
    expires: u64,
}

#[derive(Default)]
struct Store {
    clients: BTreeMap<String, Client>,
    grants: Vec<Grant>,
    requests: Vec<AuthRequest>,
    codes: Vec<Code>,
    access: HashMap<String, Access>,
}

/// What is persisted.
#[derive(Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    clients: Vec<Client>,
    #[serde(default)]
    grants: Vec<Grant>,
}

/// A sign-in the person has to allow or deny in the window app.
#[derive(Debug, Clone)]
pub struct Consent {
    pub id: String,
    pub client_name: String,
    /// Where the client will receive the result (`claude.ai`).
    pub redirect_host: String,
    pub verify: String,
    pub waiting_s: u64,
}

/// The authorization server's state, shared by the UI thread (prompts, grant list) and
/// the server thread (endpoints, token checks).
#[derive(Default)]
pub struct OAuth {
    store: Mutex<Store>,
    /// `--grants FILE`: written after every change. Unset in the window app, which saves
    /// [`OAuth::to_json`] with its other settings.
    file: Mutex<Option<PathBuf>>,
    /// `--serve`: consent by a one-time code on stdout rather than a prompt.
    headless: AtomicBool,
    /// Woken when a sign-in request arrives, so the prompt shows without waiting for
    /// the next mouse move.
    waker: Mutex<Option<egui::Context>>,
}

impl OAuth {
    /// Restore clients and grants saved by [`OAuth::to_json`]. Expired grants are dropped.
    pub fn load_json(&self, text: &str) {
        let Ok(saved) = serde_json::from_str::<Saved>(text) else {
            return;
        };
        let t = now();
        let mut s = self.store.lock().unwrap();
        s.clients = saved.clients.into_iter().map(|c| (c.id.clone(), c)).collect();
        s.grants = saved
            .grants
            .into_iter()
            .filter(|g| g.refresh_expires > t)
            .collect();
    }

    pub fn to_json(&self) -> String {
        let s = self.store.lock().unwrap();
        serde_json::to_string(&Saved {
            clients: s.clients.values().cloned().collect(),
            grants: s.grants.clone(),
        })
        .unwrap_or_default()
    }

    /// Persist to (and first load from) a file; used by `--serve --grants FILE`.
    pub fn use_file(&self, path: PathBuf) -> Result<(), String> {
        if path.exists() {
            let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            self.load_json(&text);
        }
        *self.file.lock().unwrap() = Some(path);
        Ok(())
    }

    pub fn set_headless(&self, on: bool) {
        self.headless.store(on, Ordering::Relaxed);
    }

    pub fn set_waker(&self, ctx: egui::Context) {
        *self.waker.lock().unwrap() = Some(ctx);
    }

    fn save(&self) {
        let Some(path) = self.file.lock().unwrap().clone() else {
            return;
        };
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, self.to_json()).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    fn wake(&self) {
        if let Some(ctx) = self.waker.lock().unwrap().as_ref() {
            ctx.request_repaint();
        }
    }

    /// Whether `token` is a live access token; marks its grant as used.
    pub fn check_access(&self, token: &str) -> bool {
        let h = hash(token);
        let t = now();
        let mut s = self.store.lock().unwrap();
        let Some(a) = s.access.get(&h).cloned() else {
            return false;
        };
        if a.expires <= t {
            s.access.remove(&h);
            return false;
        }
        match s.grants.iter_mut().find(|g| g.id == a.grant_id) {
            Some(g) => {
                g.last_used = t;
                true
            }
            None => {
                s.access.remove(&h);
                false
            }
        }
    }

    /// Sign-ins waiting for the person (window app).
    pub fn pending_consents(&self) -> Vec<Consent> {
        let t = now();
        let mut s = self.store.lock().unwrap();
        s.requests.retain(|r| t < r.created + REQUEST_TTL);
        s.requests
            .iter()
            .filter(|r| r.decision.is_none() && r.one_time_hash.is_none())
            .map(|r| Consent {
                id: r.id.clone(),
                client_name: r.client_name.clone(),
                redirect_host: redirect_host(&r.redirect_uri),
                verify: r.verify.clone(),
                waiting_s: t.saturating_sub(r.created),
            })
            .collect()
    }

    /// The person's answer to a sign-in prompt.
    pub fn decide(&self, request: &str, allow: bool) {
        let mut s = self.store.lock().unwrap();
        if let Some(r) = s.requests.iter_mut().find(|r| r.id == request) {
            r.decision = Some(allow);
        }
    }

    /// Every grant, newest first.
    pub fn grants(&self) -> Vec<Grant> {
        let mut g = self.store.lock().unwrap().grants.clone();
        g.sort_by_key(|g| std::cmp::Reverse(g.created));
        g
    }

    /// Withdraw a grant: its access and refresh tokens stop working at once.
    pub fn revoke(&self, grant_id: &str) {
        {
            let mut s = self.store.lock().unwrap();
            s.grants.retain(|g| g.id != grant_id);
            s.access.retain(|_, a| a.grant_id != grant_id);
        }
        self.save();
    }

    pub fn revoke_all(&self) {
        {
            let mut s = self.store.lock().unwrap();
            s.grants.clear();
            s.access.clear();
        }
        self.save();
    }
}

fn redirect_host(uri: &str) -> String {
    uri.parse::<axum::http::Uri>()
        .ok()
        .and_then(|u| u.host().map(|h| h.to_string()))
        .unwrap_or_else(|| uri.to_string())
}

fn is_loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

/// Redirect URIs a client may register: HTTPS anywhere, plain HTTP only to this machine
/// (native clients such as Claude Code listen on a loopback port).
fn redirect_allowed(uri: &str) -> bool {
    let Ok(u) = uri.parse::<axum::http::Uri>() else {
        return false;
    };
    let host = u.host().unwrap_or("");
    !host.is_empty()
        && match u.scheme_str() {
            Some("https") => true,
            Some("http") => is_loopback(host),
            _ => false,
        }
}

/// Exact match, except that a loopback redirect may come back on any port (RFC 8252).
fn redirect_matches(registered: &str, given: &str) -> bool {
    if registered == given {
        return true;
    }
    let (Ok(a), Ok(b)) = (
        registered.parse::<axum::http::Uri>(),
        given.parse::<axum::http::Uri>(),
    ) else {
        return false;
    };
    a.scheme_str() == Some("http")
        && b.scheme_str() == Some("http")
        && a.host().is_some_and(is_loopback)
        && a.host() == b.host()
        && a.path_and_query() == b.path_and_query()
}

fn with_query(uri: &str, params: &[(&str, &str)]) -> String {
    let q = serde_urlencoded::to_string(params).unwrap_or_default();
    let sep = if uri.contains('?') { '&' } else { '?' };
    format!("{uri}{sep}{q}")
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// A client-chosen name, safe to show: printable, at most 60 characters.
fn clean_name(s: &str) -> String {
    let s: String = s.chars().filter(|c| !c.is_control()).take(60).collect();
    let s = s.trim().to_string();
    if s.is_empty() {
        "An unnamed client".to_string()
    } else {
        s
    }
}

// ------------------------------------------------------------------ HTTP

/// Where the server is reached: the configured public URL when a request arrives under
/// its host name, otherwise `http://<Host>` (this machine).
#[derive(Clone)]
pub struct Endpoint {
    pub public_url: Option<String>,
}

impl Endpoint {
    /// The public URL's `host[:port]`, when one is configured.
    pub fn public_authority(&self) -> Option<String> {
        let u = self.public_url.as_ref()?.parse::<axum::http::Uri>().ok()?;
        u.authority().map(|a| a.as_str().to_ascii_lowercase())
    }

    pub fn base(&self, headers: &HeaderMap) -> String {
        let host = headers
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("127.0.0.1")
            .to_ascii_lowercase();
        if let (Some(url), Some(auth)) = (&self.public_url, self.public_authority()) {
            let name = |a: &str| a.split(':').next().unwrap_or("").to_string();
            if name(&host) == name(&auth) {
                return url.trim_end_matches('/').to_string();
            }
        }
        format!("http://{host}")
    }
}

#[derive(Clone)]
struct Ctx {
    oauth: Arc<OAuth>,
    endpoint: Endpoint,
}

/// The `WWW-Authenticate` value a 401 from `/mcp` carries, so a client can find out how
/// to sign in.
pub fn challenge(endpoint: &Endpoint, headers: &HeaderMap, had_token: bool) -> String {
    let base = endpoint.base(headers);
    let err = if had_token {
        ", error=\"invalid_token\", error_description=\"the access token is unknown, expired or revoked\""
    } else {
        ""
    };
    format!("Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource/mcp\", scope=\"{SCOPE}\"{err}")
}

/// The authorization endpoints, mounted beside `/mcp` when OAuth sign-in is on.
pub fn routes(oauth: Arc<OAuth>, endpoint: Endpoint) -> Router {
    let ctx = Ctx { oauth, endpoint };
    Router::new()
        .route("/.well-known/oauth-protected-resource", get(resource_metadata))
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(resource_metadata),
        )
        .route("/.well-known/oauth-authorization-server", get(server_metadata))
        .route(
            "/.well-known/oauth-authorization-server/mcp",
            get(server_metadata),
        )
        .route("/.well-known/openid-configuration", get(server_metadata))
        .route("/.well-known/openid-configuration/mcp", get(server_metadata))
        .route("/register", post(register))
        .route("/authorize", get(authorize))
        .route("/authorize/wait", get(wait).post(enter_code))
        .route("/token", post(token))
        .route("/revoke", post(revoke))
        .with_state(ctx)
}

fn no_store(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    h.insert(header::PRAGMA, "no-cache".parse().unwrap());
    r
}

fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    no_store(
        (
            status,
            Json(json!({"error": error, "error_description": description})),
        )
            .into_response(),
    )
}

fn redirect(to: &str, status: StatusCode) -> Response {
    (status, [(header::LOCATION, to.to_string())]).into_response()
}

async fn resource_metadata(State(c): State<Ctx>, headers: HeaderMap) -> Response {
    let base = c.endpoint.base(&headers);
    Json(json!({
        "resource": format!("{base}/mcp"),
        "authorization_servers": [base],
        "bearer_methods_supported": ["header"],
        "scopes_supported": [SCOPE],
        "resource_name": "TerraTofu GUI",
    }))
    .into_response()
}

async fn server_metadata(State(c): State<Ctx>, headers: HeaderMap) -> Response {
    let base = c.endpoint.base(&headers);
    Json(json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/authorize"),
        "token_endpoint": format!("{base}/token"),
        "registration_endpoint": format!("{base}/register"),
        "revocation_endpoint": format!("{base}/revoke"),
        "response_types_supported": ["code"],
        "response_modes_supported": ["query"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none", "client_secret_post", "client_secret_basic"],
        "revocation_endpoint_auth_methods_supported": ["none", "client_secret_post", "client_secret_basic"],
        "scopes_supported": [SCOPE],
        "authorization_response_iss_parameter_supported": true,
        "service_documentation": "https://github.com/geawilliamsUK/TerraTofuGUi/blob/master/docs/MCP_PLAN.md",
    }))
    .into_response()
}

#[derive(Deserialize)]
struct Registration {
    #[serde(default)]
    redirect_uris: Vec<String>,
    client_name: Option<String>,
    token_endpoint_auth_method: Option<String>,
}

async fn register(State(c): State<Ctx>, body: axum::body::Bytes) -> Response {
    let Ok(reg) = serde_json::from_slice::<Registration>(&body) else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_client_metadata",
            "the body must be a JSON client registration",
        );
    };
    if reg.redirect_uris.is_empty() {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            "redirect_uris is required",
        );
    }
    if let Some(bad) = reg.redirect_uris.iter().find(|u| !redirect_allowed(u)) {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            &format!("{bad}: redirect URIs must be https, or http to localhost"),
        );
    }
    // RFC 7591: no method given means client_secret_basic.
    let method = reg
        .token_endpoint_auth_method
        .unwrap_or_else(|| "client_secret_basic".into());
    if !["none", "client_secret_post", "client_secret_basic"].contains(&method.as_str()) {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_client_metadata",
            "token_endpoint_auth_method must be none, client_secret_post or client_secret_basic",
        );
    }
    let client_secret = (method != "none").then(|| secret("ttg_cs_"));
    let t = now();
    let client = Client {
        id: format!("ttg-client-{}", uuid::Uuid::new_v4().simple()),
        secret_hash: client_secret.as_deref().map(hash),
        name: clean_name(reg.client_name.as_deref().unwrap_or("")),
        redirect_uris: reg.redirect_uris,
        created: t,
    };
    {
        let mut s = c.oauth.store.lock().unwrap();
        // Keep the list bounded: forget the oldest clients nobody ever authorized.
        while s.clients.len() >= MAX_CLIENTS {
            let granted: Vec<String> = s.grants.iter().map(|g| g.client_id.clone()).collect();
            let oldest = s
                .clients
                .values()
                .filter(|c| !granted.contains(&c.id))
                .min_by_key(|c| c.created)
                .map(|c| c.id.clone());
            match oldest {
                Some(id) => {
                    s.clients.remove(&id);
                }
                None => break,
            }
        }
        s.clients.insert(client.id.clone(), client.clone());
    }
    c.oauth.save();
    let mut out = json!({
        "client_id": client.id,
        "client_id_issued_at": t,
        "client_name": client.name,
        "redirect_uris": client.redirect_uris,
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": method,
        "scope": SCOPE,
    });
    if let Some(sec) = client_secret {
        out["client_secret"] = json!(sec);
        out["client_secret_expires_at"] = json!(0);
    }
    no_store((StatusCode::CREATED, Json(out)).into_response())
}

/// An HTML page in the app's plain style.
fn page(title: &str, body: &str, refresh: bool) -> Response {
    let meta = if refresh {
        "<meta http-equiv=\"refresh\" content=\"2\">"
    } else {
        ""
    };
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">{meta}\
         <title>{t}</title><style>\
         body{{font-family:system-ui,sans-serif;max-width:34rem;margin:3rem auto;padding:0 1rem;color:#222;background:#fafafa}}\
         h1{{font-size:1.3rem}} .code{{font:600 1.6rem ui-monospace,monospace;letter-spacing:.15em;padding:.4rem .8rem;background:#eef;border-radius:6px;display:inline-block}}\
         input{{font:1.2rem ui-monospace,monospace;padding:.4rem;width:12rem;text-transform:uppercase}}\
         button{{font-size:1rem;padding:.4rem 1rem;margin-right:.5rem}} .err{{color:#b00}} .small{{color:#666;font-size:.9rem}}\
         @media (prefers-color-scheme: dark){{body{{background:#1b1b1b;color:#ddd}} .code{{background:#2a2a40}} .small{{color:#999}}}}\
         </style></head><body><h1>{t}</h1>{body}</body></html>",
        t = esc(title),
    );
    no_store(Html(html).into_response())
}

fn error_page(status: StatusCode, msg: &str) -> Response {
    let mut r = page(
        "TerraTofu GUI: sign-in failed",
        &format!("<p class=\"err\">{}</p>", esc(msg)),
        false,
    );
    *r.status_mut() = status;
    r
}

#[derive(Deserialize)]
struct AuthorizeQuery {
    response_type: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    state: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    resource: Option<String>,
}

async fn authorize(State(c): State<Ctx>, headers: HeaderMap, Query(q): Query<AuthorizeQuery>) -> Response {
    let client = {
        let s = c.oauth.store.lock().unwrap();
        q.client_id.as_ref().and_then(|id| s.clients.get(id).cloned())
    };
    let Some(client) = client else {
        return error_page(
            StatusCode::BAD_REQUEST,
            "Unknown client. The client has to register (POST /register) before it can sign in.",
        );
    };
    let redirect_uri = match &q.redirect_uri {
        Some(r) if client.redirect_uris.iter().any(|reg| redirect_matches(reg, r)) => r.clone(),
        None if client.redirect_uris.len() == 1 => client.redirect_uris[0].clone(),
        _ => {
            return error_page(
                StatusCode::BAD_REQUEST,
                "The redirect_uri is not one this client registered.",
            )
        }
    };
    // From here on, errors go back to the client the way OAuth says they should.
    let state = q.state.clone();
    let back = |error: &str, description: &str| {
        let mut p = vec![("error", error), ("error_description", description)];
        if let Some(s) = &state {
            p.push(("state", s));
        }
        redirect(&with_query(&redirect_uri, &p), StatusCode::FOUND)
    };
    if q.response_type.as_deref() != Some("code") {
        return back(
            "unsupported_response_type",
            "only response_type=code is supported",
        );
    }
    let Some(challenge) = q.code_challenge.clone().filter(|c| !c.is_empty()) else {
        return back(
            "invalid_request",
            "PKCE is required: send code_challenge with S256",
        );
    };
    if q.code_challenge_method.as_deref() != Some("S256") {
        return back("invalid_request", "code_challenge_method must be S256");
    }
    if let Some(r) = &q.resource {
        let base = c.endpoint.base(&headers);
        let ok = [
            format!("{base}/mcp"),
            format!("{base}/mcp/"),
            base.clone(),
            format!("{base}/"),
        ];
        if !ok.contains(r) {
            return back("invalid_target", "resource must be this server's /mcp endpoint");
        }
    }
    let headless = c.oauth.headless.load(Ordering::Relaxed);
    let one_time = headless.then(|| human_code(8));
    let t = now();
    let req = AuthRequest {
        id: secret(""),
        client_id: client.id.clone(),
        client_name: client.name.clone(),
        redirect_uri,
        state,
        challenge,
        created: t,
        verify: human_code(6),
        one_time_hash: one_time.as_deref().map(|c| hash(&normalise_code(c))),
        attempts: 0,
        decision: None,
    };
    {
        let mut s = c.oauth.store.lock().unwrap();
        s.requests.retain(|r| t < r.created + REQUEST_TTL);
        if s.requests.iter().filter(|r| r.decision.is_none()).count() >= MAX_PENDING {
            return error_page(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many sign-in requests are waiting. Answer or let them expire, then try again.",
            );
        }
        s.requests.push(req.clone());
    }
    if let Some(code) = one_time {
        use std::io::Write;
        println!(
            "[oauth] \"{}\" asks to use this server (it signs in through {}). One-time code: {code} (valid {} minutes). Type it into the browser page to allow; ignore it to refuse.",
            req.client_name,
            redirect_host(&req.redirect_uri),
            REQUEST_TTL / 60
        );
        let _ = std::io::stdout().flush();
    } else {
        c.oauth.wake();
    }
    redirect(
        &format!("/authorize/wait?request={}", req.id),
        StatusCode::SEE_OTHER,
    )
}

#[derive(Deserialize)]
struct WaitQuery {
    request: String,
}

/// Turn an allowed request into an authorization code and send the browser back to the
/// client with it; a denied one goes back with `access_denied`.
fn finish(c: &Ctx, headers: &HeaderMap, req: AuthRequest, allow: bool) -> Response {
    let mut params: Vec<(&str, String)> = Vec::new();
    if allow {
        let code = secret("ttg_ac_");
        let mut s = c.oauth.store.lock().unwrap();
        let t = now();
        s.codes.retain(|k| t < k.created + CODE_TTL);
        s.codes.push(Code {
            hash: hash(&code),
            client_id: req.client_id.clone(),
            redirect_uri: req.redirect_uri.clone(),
            challenge: req.challenge.clone(),
            created: t,
        });
        params.push(("code", code));
    } else {
        params.push(("error", "access_denied".into()));
        params.push(("error_description", "the user did not allow it".into()));
    }
    if let Some(s) = &req.state {
        params.push(("state", s.clone()));
    }
    params.push(("iss", c.endpoint.base(headers)));
    let p: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
    redirect(&with_query(&req.redirect_uri, &p), StatusCode::FOUND)
}

fn take_request(c: &Ctx, id: &str) -> Option<AuthRequest> {
    let mut s = c.oauth.store.lock().unwrap();
    let t = now();
    s.requests.retain(|r| t < r.created + REQUEST_TTL);
    let i = s.requests.iter().position(|r| r.id == id)?;
    Some(s.requests.remove(i))
}

fn expired_page() -> Response {
    error_page(
        StatusCode::GONE,
        "This sign-in request has expired or was already used. Start again from the client.",
    )
}

fn code_form(req: &AuthRequest, error: Option<&str>) -> Response {
    let err = error
        .map(|e| format!("<p class=\"err\">{}</p>", esc(e)))
        .unwrap_or_default();
    page(
        "Allow access to TerraTofu GUI?",
        &format!(
            "<p><b>{name}</b> wants to read and edit the project open in TerraTofu GUI. It signs in through <b>{host}</b>.</p>\
             <p>To allow it, type the one-time code that <code>terratofu-gui --serve</code> printed for this request.</p>{err}\
             <form method=\"post\" action=\"/authorize/wait\">\
             <input type=\"hidden\" name=\"request\" value=\"{id}\">\
             <p><input name=\"code\" autocomplete=\"one-time-code\" autofocus placeholder=\"XXXX-XXXX\"></p>\
             <p><button name=\"action\" value=\"allow\">Allow</button><button name=\"action\" value=\"deny\">Deny</button></p></form>\
             <p class=\"small\">Did not start this yourself? Press Deny.</p>",
            name = esc(&req.client_name),
            host = esc(&redirect_host(&req.redirect_uri)),
            id = esc(&req.id),
        ),
        false,
    )
}

async fn wait(State(c): State<Ctx>, headers: HeaderMap, Query(q): Query<WaitQuery>) -> Response {
    let req = {
        let mut s = c.oauth.store.lock().unwrap();
        let t = now();
        s.requests.retain(|r| t < r.created + REQUEST_TTL);
        s.requests.iter().find(|r| r.id == q.request).cloned()
    };
    let Some(req) = req else {
        return expired_page();
    };
    match req.decision {
        Some(allow) => match take_request(&c, &req.id) {
            Some(r) => finish(&c, &headers, r, allow),
            None => expired_page(),
        },
        None if req.one_time_hash.is_some() => code_form(&req, None),
        None => page(
            "Allow access in TerraTofu GUI",
            &format!(
                "<p><b>{name}</b> wants to read and edit the project open in TerraTofu GUI.</p>\
                 <p>Switch to the TerraTofu GUI window and answer its prompt. Allow it only if the prompt shows this code:</p>\
                 <p class=\"code\">{verify}</p><p class=\"small\">This page continues by itself once you have answered.</p>",
                name = esc(&req.client_name),
                verify = esc(&req.verify),
            ),
            true,
        ),
    }
}

#[derive(Deserialize)]
struct CodeForm {
    request: String,
    code: Option<String>,
    action: Option<String>,
}

async fn enter_code(State(c): State<Ctx>, headers: HeaderMap, Form(f): Form<CodeForm>) -> Response {
    if f.action.as_deref() == Some("deny") {
        return match take_request(&c, &f.request) {
            Some(r) => finish(&c, &headers, r, false),
            None => expired_page(),
        };
    }
    let typed = hash(&normalise_code(f.code.as_deref().unwrap_or("")));
    let outcome = {
        let mut s = c.oauth.store.lock().unwrap();
        let t = now();
        s.requests.retain(|r| t < r.created + REQUEST_TTL);
        match s.requests.iter().position(|r| r.id == f.request) {
            None => Err(None),
            Some(i) => {
                let r = &mut s.requests[i];
                if r.one_time_hash.as_deref() == Some(typed.as_str()) {
                    Ok(s.requests.remove(i))
                } else {
                    r.attempts += 1;
                    if r.attempts >= MAX_CODE_ATTEMPTS || r.one_time_hash.is_none() {
                        s.requests.remove(i);
                        Err(None)
                    } else {
                        Err(Some((r.clone(), MAX_CODE_ATTEMPTS - r.attempts)))
                    }
                }
            }
        }
    };
    match outcome {
        Ok(req) => finish(&c, &headers, req, true),
        Err(Some((req, left))) => {
            let mut r = code_form(&req, Some(&format!("That code is not right ({left} tries left).")));
            *r.status_mut() = StatusCode::UNAUTHORIZED;
            r
        }
        Err(None) => error_page(
            StatusCode::GONE,
            "This sign-in request has expired, was already used, or had too many wrong codes. Start again from the client.",
        ),
    }
}

#[derive(Deserialize)]
struct TokenForm {
    grant_type: Option<String>,
    code: Option<String>,
    redirect_uri: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
}

/// The client a token request authenticates as: HTTP Basic, or `client_id` (and
/// `client_secret`) in the form. A client registered with a secret must present it.
fn authenticate(
    c: &Ctx,
    headers: &HeaderMap,
    form_id: Option<&str>,
    form_secret: Option<&str>,
) -> Result<Client, Box<Response>> {
    use base64::Engine;
    let basic = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b.trim()).ok())
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|s| s.split_once(':').map(|(i, p)| (i.to_string(), p.to_string())));
    let (id, secret_given) = match (&basic, form_id) {
        (Some((i, p)), _) => (i.clone(), Some(p.clone())),
        (None, Some(i)) => (i.to_string(), form_secret.map(str::to_string)),
        (None, None) => {
            return Err(Box::new(oauth_error(
                StatusCode::UNAUTHORIZED,
                "invalid_client",
                "client_id is required",
            )))
        }
    };
    let client = c.oauth.store.lock().unwrap().clients.get(&id).cloned();
    let Some(client) = client else {
        return Err(Box::new(oauth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "unknown client; register again",
        )));
    };
    if let Some(want) = &client.secret_hash {
        if secret_given.as_deref().map(hash).as_deref() != Some(want.as_str()) {
            return Err(Box::new(oauth_error(
                StatusCode::UNAUTHORIZED,
                "invalid_client",
                "wrong or missing client secret",
            )));
        }
    }
    Ok(client)
}

/// Issue an access token (and the grant's new refresh token) as the token endpoint's reply.
fn issue(s: &mut Store, grant_id: &str, refresh: &str) -> Response {
    let access = secret("ttg_at_");
    s.access.insert(
        hash(&access),
        Access {
            grant_id: grant_id.to_string(),
            expires: now() + ACCESS_TTL,
        },
    );
    no_store(
        Json(json!({
            "access_token": access,
            "token_type": "Bearer",
            "expires_in": ACCESS_TTL,
            "refresh_token": refresh,
            "scope": SCOPE,
        }))
        .into_response(),
    )
}

async fn token(State(c): State<Ctx>, headers: HeaderMap, Form(f): Form<TokenForm>) -> Response {
    let client = match authenticate(&c, &headers, f.client_id.as_deref(), f.client_secret.as_deref()) {
        Ok(cl) => cl,
        Err(r) => return *r,
    };
    let t = now();
    let reply = match f.grant_type.as_deref() {
        Some("authorization_code") => {
            let presented = hash(f.code.as_deref().unwrap_or(""));
            let mut s = c.oauth.store.lock().unwrap();
            s.codes.retain(|k| t < k.created + CODE_TTL);
            // A code is good for one attempt, right or wrong.
            let Some(i) = s.codes.iter().position(|k| k.hash == presented) else {
                return oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_grant",
                    "the authorization code is unknown, expired or already used",
                );
            };
            let code = s.codes.remove(i);
            if code.client_id != client.id {
                return oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_grant",
                    "the code was issued to another client",
                );
            }
            if f.redirect_uri.as_deref().is_some_and(|r| r != code.redirect_uri) {
                return oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_grant",
                    "redirect_uri does not match",
                );
            }
            if !pkce_matches(f.code_verifier.as_deref().unwrap_or(""), &code.challenge) {
                return oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_grant",
                    "PKCE verification failed",
                );
            }
            let refresh = secret("ttg_rt_");
            let grant = Grant {
                id: format!("grant-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]),
                client_id: client.id.clone(),
                client_name: client.name.clone(),
                created: t,
                last_used: t,
                refresh_hash: hash(&refresh),
                refresh_expires: t + REFRESH_TTL,
            };
            let gid = grant.id.clone();
            s.grants.push(grant);
            issue(&mut s, &gid, &refresh)
        }
        Some("refresh_token") => {
            let presented = hash(f.refresh_token.as_deref().unwrap_or(""));
            let mut s = c.oauth.store.lock().unwrap();
            s.grants.retain(|g| g.refresh_expires > t);
            let Some(g) = s
                .grants
                .iter_mut()
                .find(|g| g.refresh_hash == presented && g.client_id == client.id)
            else {
                return oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_grant",
                    "the refresh token is unknown, expired, revoked or already used",
                );
            };
            // Rotate: the token just presented stops working.
            let refresh = secret("ttg_rt_");
            g.refresh_hash = hash(&refresh);
            g.refresh_expires = t + REFRESH_TTL;
            g.last_used = t;
            let gid = g.id.clone();
            issue(&mut s, &gid, &refresh)
        }
        _ => {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "unsupported_grant_type",
                "grant_type must be authorization_code or refresh_token",
            )
        }
    };
    c.oauth.save();
    reply
}

#[derive(Deserialize)]
struct RevokeForm {
    token: Option<String>,
}

async fn revoke(State(c): State<Ctx>, Form(f): Form<RevokeForm>) -> Response {
    let h = hash(f.token.as_deref().unwrap_or(""));
    // Revoking a refresh token ends the whole grant (RFC 7009 §2.1); an access token
    // only ends itself.
    let grant = {
        let mut s = c.oauth.store.lock().unwrap();
        if s.access.remove(&h).is_some() {
            None
        } else {
            s.grants
                .iter()
                .find(|g| g.refresh_hash == h)
                .map(|g| g.id.clone())
        }
    };
    if let Some(g) = grant {
        c.oauth.revoke(&g);
    }
    StatusCode::OK.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_s256_matches_the_rfc_example() {
        // RFC 7636 appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert!(pkce_matches(
            verifier,
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        ));
        assert!(!pkce_matches(
            verifier,
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cX"
        ));
        assert!(!pkce_matches("short", "anything"));
    }

    #[test]
    fn redirect_uris_are_https_or_loopback() {
        assert!(redirect_allowed("https://claude.ai/api/mcp/auth_callback"));
        assert!(redirect_allowed("http://localhost:3118/callback"));
        assert!(redirect_allowed("http://127.0.0.1:1/cb"));
        assert!(!redirect_allowed("http://evil.example/cb"));
        assert!(!redirect_allowed("javascript:alert(1)"));
        assert!(redirect_matches(
            "http://127.0.0.1:1/cb",
            "http://127.0.0.1:5555/cb"
        ));
        assert!(!redirect_matches(
            "https://claude.ai/cb",
            "https://claude.ai/other"
        ));
        assert!(!redirect_matches(
            "http://127.0.0.1:1/cb",
            "http://127.0.0.1:1/other"
        ));
    }

    #[test]
    fn codes_are_readable_and_compared_loosely() {
        let c = human_code(8);
        assert_eq!(c.len(), 9, "{c}");
        assert_eq!(
            normalise_code(&c.to_lowercase().replace('-', " ")),
            normalise_code(&c)
        );
        assert_eq!(human_code(6).len(), 7);
        assert!(secret("ttg_at_").starts_with("ttg_at_"));
        assert_ne!(secret(""), secret(""));
    }

    #[test]
    fn grants_survive_a_round_trip_and_revocation_ends_access() {
        let a = OAuth::default();
        {
            let mut s = a.store.lock().unwrap();
            s.grants.push(Grant {
                id: "g1".into(),
                client_id: "c1".into(),
                client_name: "Claude".into(),
                created: now(),
                last_used: now(),
                refresh_hash: hash("r"),
                refresh_expires: now() + 100,
            });
            s.access.insert(
                hash("tok"),
                Access {
                    grant_id: "g1".into(),
                    expires: now() + 100,
                },
            );
        }
        assert!(a.check_access("tok"));
        assert!(!a.check_access("other"));
        let b = OAuth::default();
        b.load_json(&a.to_json());
        assert_eq!(b.grants().len(), 1);
        a.revoke("g1");
        assert!(!a.check_access("tok"));
        assert!(a.grants().is_empty());
    }

    /// The window app's path: the sign-in becomes a prompt carrying the code the browser
    /// page shows, and the page sends the browser back with a code once it is allowed
    /// (or with access_denied once it is denied).
    #[test]
    fn a_window_app_sign_in_waits_for_the_prompt() {
        let oauth = Arc::new(OAuth::default());
        oauth.store.lock().unwrap().clients.insert(
            "c1".into(),
            Client {
                id: "c1".into(),
                secret_hash: None,
                name: "Claude".into(),
                redirect_uris: vec!["https://claude.ai/api/mcp/auth_callback".into()],
                created: now(),
            },
        );
        let c = Ctx {
            oauth: oauth.clone(),
            endpoint: Endpoint { public_url: None },
        };
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:9337".parse().unwrap());
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let start = |state: &str| {
            let q = AuthorizeQuery {
                response_type: Some("code".into()),
                client_id: Some("c1".into()),
                redirect_uri: None,
                state: Some(state.into()),
                code_challenge: Some("x".repeat(43)),
                code_challenge_method: Some("S256".into()),
                resource: None,
            };
            let r = rt.block_on(authorize(State(c.clone()), headers.clone(), Query(q)));
            assert_eq!(r.status(), StatusCode::SEE_OTHER);
            let loc = r.headers()[header::LOCATION].to_str().unwrap().to_string();
            loc.split("request=").nth(1).unwrap().to_string()
        };
        let poll = |id: &str| {
            rt.block_on(wait(
                State(c.clone()),
                headers.clone(),
                Query(WaitQuery { request: id.into() }),
            ))
        };

        let id = start("one");
        let waiting = oauth.pending_consents();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].client_name, "Claude");
        assert_eq!(waiting[0].redirect_host, "claude.ai");
        // Until the person answers, the page keeps waiting.
        assert_eq!(poll(&id).status(), StatusCode::OK);
        oauth.decide(&waiting[0].id, true);
        let r = poll(&id);
        assert_eq!(r.status(), StatusCode::FOUND);
        let back = r.headers()[header::LOCATION].to_str().unwrap();
        assert!(
            back.starts_with("https://claude.ai/api/mcp/auth_callback?code=ttg_ac_"),
            "{back}"
        );
        assert!(back.contains("state=one"), "{back}");
        // The request is used up.
        assert_eq!(poll(&id).status(), StatusCode::GONE);
        assert!(oauth.pending_consents().is_empty());

        let id = start("two");
        oauth.decide(&oauth.pending_consents()[0].id, false);
        let r = poll(&id);
        let back = r.headers()[header::LOCATION].to_str().unwrap();
        assert!(
            back.contains("error=access_denied") && back.contains("state=two"),
            "{back}"
        );
    }

    #[test]
    fn the_base_url_follows_the_host_the_request_used() {
        let e = Endpoint {
            public_url: Some("https://ttg.example.com/".into()),
        };
        let mut h = HeaderMap::new();
        h.insert(header::HOST, "ttg.example.com".parse().unwrap());
        assert_eq!(e.base(&h), "https://ttg.example.com");
        h.insert(header::HOST, "127.0.0.1:9337".parse().unwrap());
        assert_eq!(e.base(&h), "http://127.0.0.1:9337");
        assert_eq!(e.public_authority().as_deref(), Some("ttg.example.com"));
    }
}

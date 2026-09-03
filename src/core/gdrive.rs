//! **Read-only** Google Drive client for "Google Drive" music sources: the
//! Drive REST API v3 over blocking `ureq`, authenticated with OAuth 2.0 for
//! installed apps (loopback redirect + PKCE). No SDK — three endpoints
//! (`files.list`, `files.get?alt=media`, `about`) are all that is needed.
//!
//! * **OAuth client:** Google requires every app to bring its own OAuth client
//!   ("Desktop app" type). It is baked in at build time via
//!   `EMILIA_GDRIVE_CLIENT_ID`/`EMILIA_GDRIVE_CLIENT_SECRET`, or the user
//!   pastes one into the setup dialog (kept as secret settings). The "secret"
//!   of a desktop client is not confidential by Google's own definition; PKCE
//!   protects the code exchange.
//! * **Tokens:** the long-lived *refresh token* is what a source stores (in the
//!   Secret Service when available, `source.password`); short-lived access
//!   tokens are cached in memory per source and renewed on demand.
//! * **Paths:** the app addresses files by `/`-paths relative to a music
//!   folder; Drive addresses them by id. Ids are resolved by walking folder
//!   names from `root` and cached process-wide — a listing fills the cache, so
//!   browsing and playing from the file list costs no extra lookups. Files
//!   with duplicate names in one folder resolve to the first match.
//! * **Streaming** goes through [`crate::core::media_proxy`] (byte ranges via
//!   [`open_range`]); a bearer header cannot ride on a plain playbin URI.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use base64::Engine;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use sha2::{Digest, Sha256};

use crate::core::db::Library;
use crate::core::net;
use crate::core::remote::{clamp_range, normalize_music_path, RangeBody, RemoteEntry};
use crate::core::scanner;
use crate::model::Source;

pub const SCOPE: &str = "https://www.googleapis.com/auth/drive.readonly";
const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const API: &str = "https://www.googleapis.com/drive/v3";
const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
const SHORTCUT_MIME: &str = "application/vnd.google-apps.shortcut";
/// Settings keys of the user-supplied OAuth client (secret settings).
pub const SETTING_CLIENT_ID: &str = "gdrive_client_id";
pub const SETTING_CLIENT_SECRET: &str = "gdrive_client_secret";
/// Access tokens are renewed this long before Google's stated expiry.
const TOKEN_SLACK: Duration = Duration::from_secs(90);
/// How long the loopback listener waits for the browser to come back.
pub const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);
const ACCEPT_POLL: Duration = Duration::from_millis(150);

// ---------------------------------------------------------------------------
// OAuth client (app registration)
// ---------------------------------------------------------------------------

/// The app's OAuth 2.0 client registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthClient {
    pub id: String,
    pub secret: String,
}

/// The client compiled into this build, if any.
pub fn builtin_oauth_client() -> Option<OAuthClient> {
    let id = option_env!("EMILIA_GDRIVE_CLIENT_ID")?.trim();
    let secret = option_env!("EMILIA_GDRIVE_CLIENT_SECRET")?.trim();
    if id.is_empty() || secret.is_empty() {
        return None;
    }
    Some(OAuthClient {
        id: id.to_string(),
        secret: secret.to_string(),
    })
}

/// Process-wide copy of the configured client, so credential loading on hot
/// paths (play/next) needs no DB round-trip.
static OAUTH_CLIENT: Mutex<Option<OAuthClient>> = Mutex::new(None);

/// The OAuth client to use: the user-supplied one from the settings, else the
/// built-in one. Cached after the first successful load.
pub fn oauth_client() -> Option<OAuthClient> {
    if let Ok(g) = OAUTH_CLIENT.lock() {
        if let Some(c) = g.as_ref() {
            return Some(c.clone());
        }
    }
    let loaded = Library::open()
        .ok()
        .and_then(|lib| oauth_client_from(&lib))
        .or_else(builtin_oauth_client);
    if let (Some(c), Ok(mut g)) = (loaded.as_ref(), OAUTH_CLIENT.lock()) {
        *g = Some(c.clone());
    }
    loaded
}

/// Reads the user-supplied client from the settings (no built-in fallback).
pub fn oauth_client_from(lib: &Library) -> Option<OAuthClient> {
    let id = lib.get_secret_setting(SETTING_CLIENT_ID).ok().flatten()?;
    let secret = lib
        .get_secret_setting(SETTING_CLIENT_SECRET)
        .ok()
        .flatten()?;
    if id.trim().is_empty() || secret.trim().is_empty() {
        return None;
    }
    Some(OAuthClient {
        id: id.trim().to_string(),
        secret: secret.trim().to_string(),
    })
}

/// Stores a user-supplied client (Secret Service when available) and refreshes
/// the cache.
pub fn set_oauth_client(lib: &Library, client: &OAuthClient) -> Result<()> {
    lib.set_secret_setting(SETTING_CLIENT_ID, &client.id)?;
    lib.set_secret_setting(SETTING_CLIENT_SECRET, &client.secret)?;
    if let Ok(mut g) = OAUTH_CLIENT.lock() {
        *g = Some(client.clone());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Sign-in (loopback + PKCE)
// ---------------------------------------------------------------------------

/// A sign-in in progress: the loopback listener the browser will be redirected
/// to, plus the PKCE verifier and CSRF state of this attempt.
pub struct OAuthFlow {
    listener: TcpListener,
    redirect_uri: String,
    verifier: String,
    state: String,
    /// The consent URL to open in the browser.
    pub url: String,
}

/// Tokens obtained by a completed sign-in or a refresh.
#[derive(Debug, Clone)]
pub struct TokenSet {
    /// Long-lived; what the source stores.
    pub refresh_token: String,
    pub access_token: String,
    pub expires_at: Instant,
}

fn random_urlsafe(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::getrandom(&mut buf).expect("system randomness not available");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                utf8_percent_encode(k, NON_ALPHANUMERIC),
                utf8_percent_encode(v, NON_ALPHANUMERIC)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Binds the loopback listener and builds the consent URL.
pub fn oauth_begin(client: &OAuthClient) -> Result<OAuthFlow> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .map_err(|e| anyhow!("cannot listen on localhost for the sign-in redirect: {e}"))?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}");
    let verifier = random_urlsafe(48);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    let state = random_urlsafe(24);
    let url = format!(
        "{AUTH_URL}?{}",
        form_encode(&[
            ("client_id", client.id.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("response_type", "code"),
            ("scope", SCOPE),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("state", state.as_str()),
            ("access_type", "offline"),
            // Always re-ask for consent: only then does Google hand out a
            // refresh token again for an already-authorized client.
            ("prompt", "consent"),
        ])
    );
    Ok(OAuthFlow {
        listener,
        redirect_uri,
        verifier,
        state,
        url,
    })
}

/// Splits a raw query string into decoded key/value pairs.
fn query_pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            let dec = |s: &str| {
                percent_encoding::percent_decode_str(&s.replace('+', " "))
                    .decode_utf8_lossy()
                    .into_owned()
            };
            (dec(k), dec(v))
        })
        .collect()
}

/// Extracts the authorization code from the redirect request, verifying the
/// CSRF state. `Err` describes a denied/invalid sign-in.
fn code_from_redirect(query: &str, expected_state: &str) -> Result<String> {
    let pairs = query_pairs(query);
    let get = |k: &str| {
        pairs
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    };
    if let Some(err) = get("error") {
        return Err(anyhow!("sign-in was not completed: {err}"));
    }
    if get("state") != Some(expected_state) {
        return Err(anyhow!("sign-in response did not match this attempt"));
    }
    get("code")
        .filter(|c| !c.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("sign-in response carried no code"))
}

/// Waits (blocking, up to `timeout`) for the browser redirect, answers it with
/// a small "you can close this window" page and exchanges the code for tokens.
/// Run on a worker thread.
pub fn oauth_finish(flow: OAuthFlow, client: &OAuthClient, timeout: Duration) -> Result<TokenSet> {
    let deadline = Instant::now() + timeout;
    let code = loop {
        if Instant::now() > deadline {
            return Err(anyhow!("timed out waiting for the browser sign-in"));
        }
        match flow.listener.accept() {
            Ok((mut sock, _)) => {
                let _ = sock.set_nonblocking(false);
                let _ = sock.set_read_timeout(Some(Duration::from_secs(10)));
                let _ = sock.set_write_timeout(Some(Duration::from_secs(10)));
                let Ok(req) = crate::core::http::read_head(&mut sock) else {
                    continue;
                };
                // Browsers also ask for /favicon.ico; only the redirect target
                // (root path with a query) counts.
                if req.path != "/" || req.query.is_empty() {
                    crate::core::http::write_status(&mut sock, 404);
                    continue;
                }
                let result = code_from_redirect(&req.query, &flow.state);
                let (title, text) = match &result {
                    Ok(_) => (
                        "Emilia",
                        "Signed in. You can close this window and return to Emilia.",
                    ),
                    Err(_) => (
                        "Emilia",
                        "Sign-in failed. Please return to Emilia and try again.",
                    ),
                };
                let body = format!(
                    "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title>\
                     <style>body{{font-family:sans-serif;margin:3em;color:#222}}</style></head>\
                     <body><h1>{title}</h1><p>{text}</p></body></html>"
                );
                crate::core::http::write_response(
                    &mut sock,
                    200,
                    "text/html; charset=utf-8",
                    body.as_bytes(),
                );
                let _ = sock.flush();
                break result?;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL);
            }
            Err(e) => return Err(anyhow!("sign-in listener failed: {e}")),
        }
    };
    exchange_code(client, &flow.redirect_uri, &flow.verifier, &code)
}

fn token_request(form: &[(&str, &str)]) -> Result<serde_json::Value> {
    let resp = ureq::post(TOKEN_URL)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .timeout(Duration::from_secs(30))
        .send_string(&form_encode(form));
    let resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            let detail = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| {
                    v.get("error_description")
                        .or_else(|| v.get("error"))
                        .and_then(|d| d.as_str())
                        .map(str::to_string)
                })
                .unwrap_or(body);
            return Err(anyhow!("Google token endpoint returned {code}: {detail}"));
        }
        Err(e) => return Err(anyhow!("Google token request failed: {e}")),
    };
    net::json_capped(resp, net::MAX_JSON_BYTES)
}

fn expires_at(v: &serde_json::Value) -> Instant {
    let secs = v.get("expires_in").and_then(|e| e.as_u64()).unwrap_or(3600);
    Instant::now() + Duration::from_secs(secs)
}

/// Exchanges the authorization code for a refresh + access token.
fn exchange_code(
    client: &OAuthClient,
    redirect_uri: &str,
    verifier: &str,
    code: &str,
) -> Result<TokenSet> {
    let v = token_request(&[
        ("code", code),
        ("client_id", client.id.as_str()),
        ("client_secret", client.secret.as_str()),
        ("redirect_uri", redirect_uri),
        ("grant_type", "authorization_code"),
        ("code_verifier", verifier),
    ])?;
    let access_token = v
        .get("access_token")
        .and_then(|t| t.as_str())
        .ok_or_else(|| anyhow!("token response without access token"))?
        .to_string();
    let refresh_token = v
        .get("refresh_token")
        .and_then(|t| t.as_str())
        .ok_or_else(|| anyhow!("Google did not issue a refresh token – remove Emilia under \
                                 myaccount.google.com → Security → Third-party access and sign in again"))?
        .to_string();
    Ok(TokenSet {
        refresh_token,
        access_token,
        expires_at: expires_at(&v),
    })
}

/// Renews the access token from a refresh token.
fn refresh(client: &OAuthClient, refresh_token: &str) -> Result<(String, Instant)> {
    let v = token_request(&[
        ("refresh_token", refresh_token),
        ("client_id", client.id.as_str()),
        ("client_secret", client.secret.as_str()),
        ("grant_type", "refresh_token"),
    ])?;
    let access = v
        .get("access_token")
        .and_then(|t| t.as_str())
        .ok_or_else(|| anyhow!("refresh response without access token"))?
        .to_string();
    Ok((access, expires_at(&v)))
}

/// The signed-in account's e-mail address (display name of a source).
pub fn account_email(access_token: &str) -> Result<String> {
    let resp = ureq::get(&format!("{API}/about"))
        .query("fields", "user(emailAddress,displayName)")
        .set("Authorization", &format!("Bearer {access_token}"))
        .timeout(Duration::from_secs(20))
        .call()
        .map_err(|e| anyhow!("Drive about request failed: {e}"))?;
    let v: serde_json::Value = net::json_capped(resp, net::MAX_JSON_BYTES)?;
    v.pointer("/user/emailAddress")
        .and_then(|e| e.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("Drive about response without an e-mail address"))
}

// ---------------------------------------------------------------------------
// Credentials + token cache
// ---------------------------------------------------------------------------

/// Credentials + music root of a Drive source.
#[derive(Clone)]
pub struct GdCreds {
    /// Key of the token/id caches. `0` while a source is being set up.
    pub source_id: i64,
    pub client: OAuthClient,
    pub refresh_token: String,
    /// Account e-mail (informational).
    pub account: String,
    /// Subpath to the music (normalized; empty = "My Drive" root).
    pub music_path: String,
}

impl std::fmt::Debug for GdCreds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GdCreds")
            .field("source_id", &self.source_id)
            .field("account", &self.account)
            .field("music_path", &self.music_path)
            .finish_non_exhaustive()
    }
}

impl GdCreds {
    /// From a `gdrive` source row. `None` without a refresh token or an OAuth
    /// client.
    pub fn from_source(s: &Source) -> Option<Self> {
        let refresh_token =
            crate::core::secrets::resolve_source_password(s.id, s.password.as_deref()?)?;
        let account = s
            .username
            .as_deref()
            .and_then(|u| crate::core::secrets::resolve_source_username(s.id, u))
            .unwrap_or_default();
        Some(Self {
            source_id: s.id,
            client: oauth_client()?,
            refresh_token,
            account,
            music_path: normalize_music_path(s.music_path.as_deref().unwrap_or("")),
        })
    }
}

/// Access tokens per source id (+ expiry).
static TOKENS: OnceLock<Mutex<HashMap<i64, (String, Instant)>>> = OnceLock::new();
/// Resolved Drive ids per (source id, full path from the Drive root).
static IDS: OnceLock<Mutex<HashMap<(i64, String), Node>>> = OnceLock::new();

fn tokens() -> &'static Mutex<HashMap<i64, (String, Instant)>> {
    TOKENS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn ids() -> &'static Mutex<HashMap<(i64, String), Node>> {
    IDS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Seeds the access-token cache (after a fresh sign-in, before the source has
/// an id, or for a reused account under its real id).
pub fn seed_token(source_id: i64, access_token: &str, expires_at: Instant) {
    if let Ok(mut g) = tokens().lock() {
        g.insert(source_id, (access_token.to_string(), expires_at));
    }
}

/// Forgets cached ids of a source (after its music path changed or it was
/// removed).
pub fn forget_source(source_id: i64) {
    if let Ok(mut g) = ids().lock() {
        g.retain(|(id, _), _| *id != source_id);
    }
    if let Ok(mut g) = tokens().lock() {
        g.remove(&source_id);
    }
}

fn access_token(c: &GdCreds, force: bool) -> Result<String> {
    if !force {
        if let Ok(g) = tokens().lock() {
            if let Some((tok, exp)) = g.get(&c.source_id) {
                if Instant::now() + TOKEN_SLACK < *exp {
                    return Ok(tok.clone());
                }
            }
        }
    }
    let (tok, exp) = refresh(&c.client, &c.refresh_token)?;
    seed_token(c.source_id, &tok, exp);
    Ok(tok)
}

// ---------------------------------------------------------------------------
// Drive API
// ---------------------------------------------------------------------------

/// A resolved Drive item.
#[derive(Debug, Clone)]
struct Node {
    id: String,
    is_dir: bool,
    size: Option<u64>,
}

/// Escapes a value for a Drive `q` string literal.
fn escape_q(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

/// Authenticated GET returning JSON; a 401 renews the token once.
fn api_get(c: &GdCreds, url: &str, query: &[(&str, &str)]) -> Result<serde_json::Value> {
    let mut force = false;
    for attempt in 0..2 {
        let token = access_token(c, force)?;
        let mut req = ureq::get(url)
            .set("Authorization", &format!("Bearer {token}"))
            .timeout(Duration::from_secs(30));
        for (k, v) in query {
            req = req.query(k, v);
        }
        match req.call() {
            Ok(resp) => return net::json_capped(resp, net::MAX_JSON_BYTES),
            Err(ureq::Error::Status(401, _)) if attempt == 0 => force = true,
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                return Err(anyhow!("Drive API returned {code}: {}", body.trim()));
            }
            Err(e) => return Err(anyhow!("Drive API request failed: {e}")),
        }
    }
    Err(anyhow!("Drive API authentication failed"))
}

fn node_from(v: &serde_json::Value) -> Option<(String, Node)> {
    let id = v.get("id")?.as_str()?.to_string();
    let name = v.get("name")?.as_str()?.to_string();
    let mime = v.get("mimeType").and_then(|m| m.as_str()).unwrap_or("");
    if mime == SHORTCUT_MIME {
        return None; // shortcuts point elsewhere – not followed
    }
    let size = v
        .get("size")
        .and_then(|s| s.as_str())
        .and_then(|s| s.parse::<u64>().ok());
    Some((
        name,
        Node {
            id,
            is_dir: mime == FOLDER_MIME,
            size,
        },
    ))
}

/// All children of a folder (paged), as (name, node).
fn children(c: &GdCreds, parent_id: &str, extra_q: &str) -> Result<Vec<(String, Node)>> {
    let q = format!(
        "'{}' in parents and trashed = false{extra_q}",
        escape_q(parent_id)
    );
    let mut out = Vec::new();
    let mut page_token: Option<String> = None;
    loop {
        let mut query: Vec<(&str, &str)> = vec![
            ("q", q.as_str()),
            ("fields", "nextPageToken,files(id,name,mimeType,size)"),
            ("pageSize", "1000"),
            ("orderBy", "folder,name"),
            ("supportsAllDrives", "true"),
            ("includeItemsFromAllDrives", "true"),
        ];
        if let Some(t) = page_token.as_deref() {
            query.push(("pageToken", t));
        }
        let v = api_get(c, &format!("{API}/files"), &query)?;
        if let Some(files) = v.get("files").and_then(|f| f.as_array()) {
            out.extend(files.iter().filter_map(node_from));
        }
        match v.get("nextPageToken").and_then(|t| t.as_str()) {
            Some(t) if !t.is_empty() => page_token = Some(t.to_string()),
            _ => break,
        }
        if out.len() > 50_000 {
            break;
        }
    }
    Ok(out)
}

/// Full Drive path of a music-root-relative path.
fn full_path(c: &GdCreds, rel: &str) -> String {
    format!("{}{}", c.music_path, rel)
}

fn cached(c: &GdCreds, full: &str) -> Option<Node> {
    ids()
        .lock()
        .ok()?
        .get(&(c.source_id, full.to_string()))
        .cloned()
}

fn remember(c: &GdCreds, full: &str, node: &Node) {
    if let Ok(mut g) = ids().lock() {
        g.insert((c.source_id, full.to_string()), node.clone());
    }
}

fn forget(c: &GdCreds, full: &str) {
    if let Ok(mut g) = ids().lock() {
        g.remove(&(c.source_id, full.to_string()));
    }
}

/// Resolves a full Drive path (`/Music/Alben/X`) to its node by walking the
/// folder names from the root, caching each level.
fn resolve(c: &GdCreds, full: &str) -> Result<Node> {
    if let Some(n) = cached(c, full) {
        return Ok(n);
    }
    let mut node = Node {
        id: "root".to_string(),
        is_dir: true,
        size: None,
    };
    let mut walked = String::new();
    for seg in full.split('/').filter(|s| !s.is_empty()) {
        walked.push('/');
        walked.push_str(seg);
        if let Some(n) = cached(c, &walked) {
            node = n;
            continue;
        }
        let extra = format!(" and name = '{}'", escape_q(seg));
        let found = children(c, &node.id, &extra)?
            .into_iter()
            .map(|(_, n)| n)
            .next()
            .ok_or_else(|| anyhow!("not found on Drive: {walked}"))?;
        remember(c, &walked, &found);
        node = found;
    }
    Ok(node)
}

/// Lists a folder (relative to the music root): subfolders + audio files.
pub fn list(c: &GdCreds, rel: &str) -> Result<Vec<RemoteEntry>> {
    let full = full_path(c, rel);
    let folder = resolve(c, &full)?;
    if !folder.is_dir {
        return Err(anyhow!("not a folder: {rel}"));
    }
    let mut out = Vec::new();
    for (name, node) in children(c, &folder.id, "")? {
        if !node.is_dir && !scanner::is_audio(std::path::Path::new(&name)) {
            continue;
        }
        let child_full = format!("{}/{name}", full.trim_end_matches('/'));
        remember(c, &child_full, &node);
        out.push(RemoteEntry {
            rel_path: format!("{rel}/{name}"),
            name,
            is_dir: node.is_dir,
        });
    }
    Ok(out)
}

/// Connection test: the music folder must resolve to a folder.
pub fn test_connection(c: &GdCreds) -> Result<()> {
    let node = resolve(c, &full_path(c, ""))?;
    if node.is_dir {
        Ok(())
    } else {
        Err(anyhow!("the music path is not a folder"))
    }
}

/// Fetches `bytes=start-end` of a file's content with one token renewal on
/// 401. Returns the response (206 or 200 when the server ignored the range).
fn media_get(c: &GdCreds, id: &str, start: u64, end: Option<u64>) -> Result<ureq::Response> {
    let range = match end {
        Some(e) => format!("bytes={start}-{e}"),
        None => format!("bytes={start}-"),
    };
    let mut force = false;
    for attempt in 0..2 {
        let token = access_token(c, force)?;
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(30))
            .build();
        let req = agent
            .get(&format!(
                "{API}/files/{}",
                utf8_percent_encode(id, NON_ALPHANUMERIC)
            ))
            .query("alt", "media")
            .query("supportsAllDrives", "true")
            .set("Authorization", &format!("Bearer {token}"))
            .set("Range", &range);
        match req.call() {
            Ok(resp) => return Ok(resp),
            Err(ureq::Error::Status(401, _)) if attempt == 0 => force = true,
            Err(ureq::Error::Status(code, _)) => {
                return Err(anyhow!("Drive download returned {code}"));
            }
            Err(e) => return Err(anyhow!("Drive download failed: {e}")),
        }
    }
    Err(anyhow!("Drive API authentication failed"))
}

/// Resolves a file (not a folder) under the music root; a stale cache entry
/// is dropped and re-resolved once.
fn resolve_file(c: &GdCreds, rel: &str) -> Result<Node> {
    let full = full_path(c, rel);
    let node = resolve(c, &full)?;
    if node.is_dir {
        forget(c, &full);
        return Err(anyhow!("not a file: {rel}"));
    }
    Ok(node)
}

/// Reads up to `len` bytes from the start of a file.
pub fn fetch_prefix(c: &GdCreds, rel: &str, len: u64) -> Result<Vec<u8>> {
    let node = resolve_file(c, rel)?;
    let resp = match media_get(c, &node.id, 0, Some(len.saturating_sub(1))) {
        Ok(r) => r,
        Err(e) => {
            // The id may be stale (file replaced/moved): resolve afresh once.
            forget(c, &full_path(c, rel));
            let node = resolve_file(c, rel).map_err(|_| e)?;
            media_get(c, &node.id, 0, Some(len.saturating_sub(1)))?
        }
    };
    let mut buf = Vec::new();
    resp.into_reader().take(len).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Total file size from a range response (`Content-Range: bytes a-b/total`),
/// else `Content-Length` of a full response, else the size Drive listed.
fn total_from(resp: &ureq::Response, listed: Option<u64>) -> Option<u64> {
    if resp.status() == 206 {
        if let Some(cr) = resp.header("Content-Range") {
            if let Some(total) = cr
                .rsplit('/')
                .next()
                .and_then(|t| t.trim().parse::<u64>().ok())
            {
                return Some(total);
            }
        }
    } else if let Some(len) = resp
        .header("Content-Length")
        .and_then(|s| s.trim().parse::<u64>().ok())
    {
        return Some(len);
    }
    listed
}

/// Opens a byte range of a file for streaming.
pub fn open_range(c: &GdCreds, rel: &str, start: u64, end: Option<u64>) -> Result<RangeBody> {
    let node = resolve_file(c, rel)?;
    // Refuse ranges past the end up front when the size is known (HTTP 416).
    if let Some(total) = node.size {
        if clamp_range(total, start, end).is_none() {
            return Err(anyhow!("range not satisfiable"));
        }
    }
    let resp = match media_get(c, &node.id, start, end) {
        Ok(r) => r,
        Err(e) => {
            forget(c, &full_path(c, rel));
            let node = resolve_file(c, rel).map_err(|_| e)?;
            media_get(c, &node.id, start, end)?
        }
    };
    let total = total_from(&resp, node.size).ok_or_else(|| anyhow!("unknown file size"))?;
    let (start, end) = if resp.status() == 206 {
        clamp_range(total, start, end).ok_or_else(|| anyhow!("range not satisfiable"))?
    } else {
        // The server sent the whole file: serve from byte 0.
        (0, total.saturating_sub(1))
    };
    let len = end - start + 1;
    Ok(RangeBody {
        total,
        start,
        end,
        body: Box::new(resp.into_reader().take(len)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn q_literals_are_escaped() {
        assert_eq!(escape_q("Rock 'n' Roll"), "Rock \\'n\\' Roll");
        assert_eq!(escape_q("a\\b"), "a\\\\b");
    }

    #[test]
    fn redirect_code_is_checked_against_state() {
        assert_eq!(
            code_from_redirect("state=abc&code=4%2FxyZ&scope=x", "abc").unwrap(),
            "4/xyZ"
        );
        assert!(code_from_redirect("state=other&code=1", "abc").is_err());
        assert!(code_from_redirect("state=abc&error=access_denied", "abc").is_err());
        assert!(code_from_redirect("state=abc", "abc").is_err());
    }

    #[test]
    fn form_encoding_escapes_reserved_characters() {
        assert_eq!(
            form_encode(&[("a b", "c&d"), ("scope", "https://x/y")]),
            "a%20b=c%26d&scope=https%3A%2F%2Fx%2Fy"
        );
    }

    #[test]
    fn pkce_challenge_is_url_safe_sha256() {
        // RFC 7636 appendix B test vector.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn builds_full_paths_under_the_music_root() {
        let c = GdCreds {
            source_id: 1,
            client: OAuthClient {
                id: "i".into(),
                secret: "s".into(),
            },
            refresh_token: "r".into(),
            account: String::new(),
            music_path: "/Music".into(),
        };
        assert_eq!(full_path(&c, "/Alben/X"), "/Music/Alben/X");
        assert_eq!(full_path(&c, ""), "/Music");
    }
}

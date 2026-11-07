//! The HTTP API behind Traefik. Every request is authenticated before any bulk
//! data is read; failures cost the caller exponentially growing lockouts.

// Rejections carry a ready response; they are rare, so their size does not matter.
#![allow(clippy::result_large_err)]

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

use super::deploy::{self, Caller};
use super::guard::{Backoff, IpKey, IssueError, NonceResult, Nonces, RateLimiter, SIG_CHECKS_PER_SEC, TICKET_TTL, Ticket, Tickets};
use super::{DeployLock, Progress, api, clean_tmp, free_disk, stats};
use crate::auth::{self, AuthorizedKey, Need, Scope};
use crate::bundle::{UnpackLimits, unpack};
use crate::config::{Paths, ServerConfig};
use crate::spec::{is_valid_env_name, is_valid_name};

pub const MAX_SMALL_BODY: u64 = 64 * 1024;
const MAX_CONNECTIONS: usize = 64;
const MAX_CLOCK_SKEW: u64 = 30;
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const SMALL_BODY_TIMEOUT: Duration = Duration::from_secs(10);
const UPLOAD_STALL_TIMEOUT: Duration = Duration::from_secs(15);
const UPLOAD_TOTAL_TIMEOUT: Duration = Duration::from_secs(600);
const KEYS_RELOAD: Duration = Duration::from_secs(5);
const MIN_FREE_DISK: u64 = 1 << 30;

type Body = UnsyncBoxBody<Bytes, Infallible>;

struct KeyCache {
    keys: Vec<AuthorizedKey>,
    checked: Option<Instant>,
    mtime: Option<SystemTime>,
}

struct Guards {
    backoff: Backoff,
    nonces: Nonces,
    tickets: Tickets,
    sig_limit: RateLimiter,
}

pub struct State {
    paths: Paths,
    keys: Mutex<KeyCache>,
    guards: Mutex<Guards>,
    /// Serializes read-modify-write of the secrets files.
    secrets_lock: Mutex<()>,
    stats: Arc<Mutex<stats::StatsStore>>,
}

impl State {
    pub fn new(paths: Paths) -> State {
        let retention = ServerConfig::load(&paths).map(|c| c.stats.retention_days).unwrap_or(90);
        State {
            stats: Arc::new(Mutex::new(stats::StatsStore::load(&paths, retention))),
            paths,
            keys: Mutex::new(KeyCache { keys: Vec::new(), checked: None, mtime: None }),
            guards: Mutex::new(Guards {
                backoff: Backoff::default(),
                nonces: Nonces::default(),
                tickets: Tickets::default(),
                sig_limit: RateLimiter::new(SIG_CHECKS_PER_SEC, Instant::now()),
            }),
            secrets_lock: Mutex::new(()),
        }
    }

    fn guards(&self) -> std::sync::MutexGuard<'_, Guards> {
        self.guards.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Looks up a key, re-reading `authorized_keys` at most every 5 seconds.
    /// If the file cannot be parsed, no key is accepted until it is fixed.
    fn find_key(&self, key_id: &str) -> Option<AuthorizedKey> {
        let mut cache = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if cache.checked.is_none_or(|t| now.duration_since(t) >= KEYS_RELOAD) {
            cache.checked = Some(now);
            let path = self.paths.authorized_keys();
            let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            if mtime != cache.mtime || mtime.is_none() {
                cache.mtime = mtime;
                cache.keys = match std::fs::read_to_string(&path).map_err(anyhow::Error::from).and_then(|t| auth::parse_authorized_keys(&t)) {
                    Ok(k) => k,
                    Err(e) => {
                        tracing::error!("authorized_keys is unusable, rejecting all keys: {e:#}");
                        Vec::new()
                    }
                };
            }
        }
        cache.keys.iter().find(|k| k.key_id == key_id).cloned()
    }

    #[cfg(test)]
    fn force_key_reload(&self) {
        let mut cache = self.keys.lock().unwrap();
        cache.checked = None;
        cache.mtime = None;
    }
}

pub async fn run(paths: Paths) -> Result<()> {
    let cfg = ServerConfig::load(&paths)?;
    let addr: SocketAddr = cfg.listen.parse().with_context(|| format!("invalid listen address {:?}", cfg.listen))?;
    if !addr.ip().is_loopback() {
        bail!("the daemon must listen on a loopback address, not {addr}");
    }
    match DeployLock::try_acquire(&paths)? {
        Some(_lock) => {
            clean_tmp(&paths)?;
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            deploy::recover(&paths, &tx).await.unwrap_or_else(|e| tracing::error!("recovery failed: {e:#}"));
            drop(tx);
            while let Some(line) = rx.recv().await {
                tracing::warn!("{line}");
            }
        }
        None => tracing::warn!("a deploy is running, skipping temp cleanup and recovery"),
    }
    let listener = TcpListener::bind(addr).await.with_context(|| format!("cannot listen on {addr}"))?;
    tracing::info!("mypaas daemon listening on {addr}");
    let state = Arc::new(State::new(paths.clone()));
    if cfg.stats.enabled {
        tokio::spawn(stats::run(paths, state.stats.clone()));
    }
    serve(listener, state).await
}

pub async fn serve(listener: TcpListener, state: Arc<State>) -> Result<()> {
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!("accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(permit) = connections.clone().try_acquire_owned() else {
            drop(stream);
            continue;
        };
        let state = state.clone();
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |req| handle(state.clone(), peer, req));
            let conn = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_TIMEOUT)
                .max_buf_size(8 * 1024)
                .max_headers(64)
                .serve_connection(TokioIo::new(stream), service);
            if let Err(e) = conn.await {
                tracing::debug!("connection from {peer}: {e}");
            }
            drop(permit);
        });
    }
}

fn full(status: StatusCode, body: impl Into<Bytes>, content_type: &str) -> Response<Body> {
    let mut r = Response::new(Full::new(body.into()).boxed_unsync());
    *r.status_mut() = status;
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_str(content_type).unwrap());
    r
}

fn json(status: StatusCode, value: serde_json::Value) -> Response<Body> {
    full(status, value.to_string(), "application/json")
}

fn error(status: StatusCode, msg: &str) -> Response<Body> {
    json(status, serde_json::json!({ "error": msg, "server_time": auth::unix_now() }))
}

fn denied() -> Response<Body> {
    error(StatusCode::FORBIDDEN, "denied")
}

fn not_found() -> Response<Body> {
    error(StatusCode::NOT_FOUND, "not found")
}

fn too_many(retry_after: Duration) -> Response<Body> {
    let secs = retry_after.as_secs() + u64::from(retry_after.subsec_nanos() > 0);
    let mut r = error(StatusCode::TOO_MANY_REQUESTS, "too many requests");
    r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(secs.max(1)));
    r
}

/// Progress lines as a streamed `text/plain` body. The last line is `result: ok`
/// or `result: failed`, since the status code is sent before the work runs.
fn streamed(rx: tokio::sync::mpsc::UnboundedReceiver<String>) -> Response<Body> {
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        let line = rx.recv().await?;
        Some((Ok::<_, Infallible>(Frame::data(Bytes::from(line + "\n"))), rx))
    });
    let mut r = Response::new(StreamBody::new(stream).boxed_unsync());
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// Behind Traefik the client is the last `X-Forwarded-For` entry, trusted only from loopback.
fn client_ip(peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
    if peer.ip().is_loopback()
        && let Some(last) = headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .last()
        && let Ok(ip) = last.trim().parse::<IpAddr>()
    {
        return ip;
    }
    peer.ip()
}

#[derive(Debug, PartialEq, Eq)]
enum Route {
    DeployTickets,
    Deploy,
    Services,
    Status(String),
    Logs(String),
    Stats(String),
    Rollback(String),
    Remove(String),
    Secrets(String),
    SetSecret(String, String),
    RemoveSecret(String, String),
}

fn route(method: &Method, path: &str) -> Option<Route> {
    let segments: Vec<&str> = path.strip_prefix("/v1/")?.split('/').collect();
    let name = |s: &str| is_valid_name(s).then(|| s.to_string());
    match (method, segments.as_slice()) {
        (&Method::POST, ["deploy-tickets"]) => Some(Route::DeployTickets),
        (&Method::PUT, ["deploy"]) => Some(Route::Deploy),
        (&Method::GET, ["services"]) => Some(Route::Services),
        (&Method::GET, ["services", n]) => Some(Route::Status(name(n)?)),
        (&Method::GET, ["services", n, "logs"]) => Some(Route::Logs(name(n)?)),
        (&Method::GET, ["services", n, "stats"]) => Some(Route::Stats(name(n)?)),
        (&Method::POST, ["services", n, "rollback"]) => Some(Route::Rollback(name(n)?)),
        (&Method::DELETE, ["services", n]) => Some(Route::Remove(name(n)?)),
        (&Method::GET, ["services", n, "secrets"]) => Some(Route::Secrets(name(n)?)),
        (&Method::PUT, ["services", n, "secrets", k]) if is_valid_env_name(k) => Some(Route::SetSecret(name(n)?, k.to_string())),
        (&Method::DELETE, ["services", n, "secrets", k]) if is_valid_env_name(k) => {
            Some(Route::RemoveSecret(name(n)?, k.to_string()))
        }
        _ => None,
    }
}

async fn handle(state: Arc<State>, peer: SocketAddr, req: Request<Incoming>) -> Result<Response<Body>, Infallible> {
    let ip = client_ip(peer, req.headers());
    let ipk = IpKey::from(ip);
    // The lockout check comes first and costs no cryptography.
    if let Some(wait) = state.guards().backoff.locked(ipk, Instant::now()) {
        return Ok(too_many(wait));
    }
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let (resp, outcome) = handle_inner(&state, ipk, req).await;
    match &outcome {
        Outcome::Denied(reason) => {
            state.guards().backoff.fail(ipk, Instant::now());
            tracing::info!(%ip, %method, %path, "denied: {reason}");
        }
        Outcome::Ok(key) => tracing::info!(%ip, %method, %path, key = %key, status = resp.status().as_u16(), "ok"),
        Outcome::Other => tracing::info!(%ip, %method, %path, status = resp.status().as_u16(), "rejected"),
    }
    Ok(resp)
}

enum Outcome {
    /// Counts as an authentication failure for the backoff.
    Denied(&'static str),
    /// Authenticated; holds the key name for the log.
    Ok(String),
    Other,
}

async fn handle_inner(state: &Arc<State>, ipk: IpKey, req: Request<Incoming>) -> (Response<Body>, Outcome) {
    let Some(route) = route(req.method(), req.uri().path()) else {
        return (not_found(), Outcome::Other);
    };
    if req.headers().contains_key(header::TRANSFER_ENCODING) {
        return (error(StatusCode::LENGTH_REQUIRED, "chunked bodies are not accepted, send a Content-Length"), Outcome::Other);
    }
    let length = match content_length(req.headers()) {
        Ok(l) => l,
        Err(()) => return (error(StatusCode::BAD_REQUEST, "invalid Content-Length"), Outcome::Other),
    };
    if route == Route::Deploy {
        return upload(state, ipk, req, length).await;
    }
    let max = if matches!(route, Route::SetSecret(..)) { api::MAX_SECRET_VALUE as u64 } else { MAX_SMALL_BODY };
    if length > max {
        return (error(StatusCode::PAYLOAD_TOO_LARGE, "body too large"), Outcome::Other);
    }
    let query = req.uri().query().unwrap_or("").to_string();
    let (key, body) = match verify_signed(state, req, length).await {
        Ok(x) => x,
        Err(Reject::Denied(reason)) => return (denied(), Outcome::Denied(reason)),
        Err(Reject::Response(r)) => return (r, Outcome::Other),
    };
    let need = match &route {
        Route::DeployTickets => None, // depends on the body, checked in the handler
        Route::Services | Route::Status(_) | Route::Stats(_) => Some(Need::Read),
        Route::Logs(_) => Some(Need::Logs),
        Route::Rollback(n) => Some(Need::Deploy(n)),
        Route::Remove(n) => Some(Need::Rm(n)),
        Route::Secrets(n) | Route::SetSecret(n, _) | Route::RemoveSecret(n, _) => Some(Need::Secrets(n)),
        Route::Deploy => unreachable!(),
    };
    if let Some(need) = need
        && !auth::allows(&key.scopes, need)
    {
        return (denied(), Outcome::Denied("out of scope"));
    }
    let resp = match route {
        Route::DeployTickets => match deploy_ticket(state, &key, &body) {
            Ok(r) => r,
            Err(Reject::Denied(reason)) => return (denied(), Outcome::Denied(reason)),
            Err(Reject::Response(r)) => r,
        },
        Route::Services => json_result(api::list_services(&state.paths, &state.stats).await.map(Some)),
        Route::Stats(name) => traffic(state, &name, &query),
        Route::Status(name) => json_result(api::status(&state.paths, &name).await),
        Route::Logs(name) => logs(&name, &query).await,
        Route::Rollback(name) => rollback(state, &key, name),
        Route::Remove(name) => remove(state, name),
        Route::Secrets(name) => {
            json_result(api::secret_names(&state.paths, &name).map(|names| Some(serde_json::json!({ "names": names }))))
        }
        Route::SetSecret(name, k) => set_secret(state, &name, &k, &body),
        Route::RemoveSecret(name, k) => remove_secret(state, &name, &k),
        Route::Deploy => unreachable!(),
    };
    state.guards().backoff.success(ipk);
    (resp, Outcome::Ok(key.name))
}

fn content_length(headers: &HeaderMap) -> Result<u64, ()> {
    let mut values = headers.get_all(header::CONTENT_LENGTH).iter();
    let Some(v) = values.next() else { return Ok(0) };
    if values.next().is_some() {
        return Err(());
    }
    v.to_str().map_err(|_| ())?.parse::<u64>().map_err(|_| ())
}

enum Reject {
    Denied(&'static str),
    Response(Response<Body>),
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

async fn verify_signed(state: &Arc<State>, req: Request<Incoming>, length: u64) -> Result<(AuthorizedKey, Bytes), Reject> {
    let (parts, body) = req.into_parts();
    let h = &parts.headers;
    let (Some(key_id), Some(time), Some(nonce), Some(sig)) = (
        header_str(h, auth::HEADER_KEY),
        header_str(h, auth::HEADER_TIME),
        header_str(h, auth::HEADER_NONCE),
        header_str(h, auth::HEADER_SIG),
    ) else {
        return Err(Reject::Denied("missing signature headers"));
    };
    let Ok(time) = time.parse::<u64>() else { return Err(Reject::Denied("bad time")) };
    if auth::unix_now().abs_diff(time) > MAX_CLOCK_SKEW {
        return Err(Reject::Denied("timestamp outside the allowed window"));
    }
    if !auth::is_valid_nonce(nonce) {
        return Err(Reject::Denied("bad nonce"));
    }
    let Some(key) = state.find_key(key_id) else { return Err(Reject::Denied("unknown key")) };
    if key.is_expired(auth::unix_now()) {
        return Err(Reject::Denied("expired key"));
    }
    if !state.guards().sig_limit.try_take(Instant::now()) {
        return Err(Reject::Response(too_many(Duration::from_secs(1))));
    }
    let body = match tokio::time::timeout(SMALL_BODY_TIMEOUT, http_body_util::Limited::new(body, length as usize).collect()).await {
        Ok(Ok(collected)) => collected.to_bytes(),
        Ok(Err(_)) => return Err(Reject::Response(error(StatusCode::BAD_REQUEST, "body does not match Content-Length"))),
        Err(_) => return Err(Reject::Response(error(StatusCode::REQUEST_TIMEOUT, "body read timed out"))),
    };
    if body.len() as u64 != length {
        return Err(Reject::Response(error(StatusCode::BAD_REQUEST, "body does not match Content-Length")));
    }
    let path_and_query = parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let canonical = auth::canonical_string(parts.method.as_str(), path_and_query, time, nonce, &body);
    if !auth::verify(&key.key, &canonical, sig) {
        return Err(Reject::Denied("bad signature"));
    }
    if state.guards().nonces.check_and_insert(&key.key_id, nonce, Instant::now()) != NonceResult::Fresh {
        return Err(Reject::Denied("replayed nonce"));
    }
    Ok((key, body))
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TicketRequest {
    pub service: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TicketResponse {
    pub ticket: String,
    pub expires_in: u64,
}

fn deploy_ticket(state: &Arc<State>, key: &AuthorizedKey, body: &[u8]) -> Result<Response<Body>, Reject> {
    let bad = |m: &str| Reject::Response(error(StatusCode::BAD_REQUEST, m));
    let req: TicketRequest = serde_json::from_slice(body).map_err(|_| bad("expected JSON {service, size, sha256}"))?;
    if !is_valid_name(&req.service) {
        return Err(bad("invalid service name"));
    }
    if !auth::allows(&key.scopes, Need::Deploy(&req.service)) {
        return Err(Reject::Denied("out of scope"));
    }
    if req.sha256.len() != 64 || !req.sha256.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(bad("sha256 must be 64 hex characters"));
    }
    let internal = |e: anyhow::Error| {
        tracing::error!("{e:#}");
        Reject::Response(error(StatusCode::INTERNAL_SERVER_ERROR, "server configuration error"))
    };
    let limits = ServerConfig::load(&state.paths).and_then(|c| c.limits()).map_err(internal)?;
    if req.size == 0 || req.size > limits.max_upload {
        return Err(Reject::Response(error(
            StatusCode::PAYLOAD_TOO_LARGE,
            &format!("bundle size {} is above the limit of {} bytes", req.size, limits.max_upload),
        )));
    }
    let free = free_disk(&state.paths.tmp()).map_err(internal)?;
    if free < MIN_FREE_DISK + 4 * req.size {
        return Err(Reject::Response(error(StatusCode::INSUFFICIENT_STORAGE, "not enough free disk space on the server")));
    }
    let now = Instant::now();
    let ticket = Ticket {
        key_id: key.key_id.clone(),
        key_name: key.name.clone(),
        admin: auth::allows(&key.scopes, Need::Admin),
        service: req.service,
        size: req.size,
        sha256: req.sha256.to_ascii_lowercase(),
        expires: now + TICKET_TTL,
    };
    match state.guards().tickets.issue(ticket, now) {
        Ok(token) => Ok(json(
            StatusCode::OK,
            serde_json::to_value(TicketResponse { ticket: token, expires_in: TICKET_TTL.as_secs() }).unwrap(),
        )),
        Err(IssueError::TooManyOpen | IssueError::TooManyForKey) => Err(Reject::Response(too_many(Duration::from_secs(10)))),
    }
}

/// `PUT /v1/deploy`: decided from the headers alone before a byte of the body is read.
async fn upload(state: &Arc<State>, ipk: IpKey, req: Request<Incoming>, length: u64) -> (Response<Body>, Outcome) {
    let token = header_str(req.headers(), "authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|t| t.len() == 64 && t.bytes().all(|c| c.is_ascii_hexdigit()))
        .map(str::to_string);
    let Some(token) = token else { return (denied(), Outcome::Denied("missing ticket")) };
    let Some(ticket) = state.guards().tickets.peek(&token, Instant::now()) else {
        return (denied(), Outcome::Denied("unknown or expired ticket"));
    };
    if length != ticket.size {
        state.guards().tickets.consume(&token, Instant::now());
        return (error(StatusCode::BAD_REQUEST, "Content-Length does not match the ticket; the ticket is now void"), Outcome::Other);
    }
    let lock = match DeployLock::try_acquire(&state.paths) {
        Ok(Some(l)) => l,
        // The ticket stays valid so the client can retry.
        Ok(None) => return (error(StatusCode::CONFLICT, "busy"), Outcome::Other),
        Err(e) => {
            tracing::error!("{e:#}");
            return (error(StatusCode::INTERNAL_SERVER_ERROR, "cannot lock"), Outcome::Other);
        }
    };
    let Some(ticket) = state.guards().tickets.consume(&token, Instant::now()) else {
        return (denied(), Outcome::Denied("ticket used concurrently"));
    };
    state.guards().backoff.success(ipk);
    let key_name = ticket.key_name.clone();

    let id = format!("{}-{:08x}", auth::unix_now(), rand::random::<u32>());
    let bundle = state.paths.tmp().join(format!("upload-{id}.tar.zst"));
    if let Err(resp) = receive(req.into_body(), &bundle, &ticket).await {
        let _ = std::fs::remove_file(&bundle);
        return (resp, Outcome::Ok(key_name));
    }

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let paths = state.paths.clone();
    tokio::spawn(async move {
        let _lock = lock;
        let dir = paths.tmp().join(format!("unpack-{id}"));
        let result = unpack_and_deploy(&paths, &bundle, &dir, &ticket, &tx).await;
        let _ = std::fs::remove_file(&bundle);
        let _ = std::fs::remove_dir_all(&dir);
        finish(&tx, &ticket.service, result);
    });
    (streamed(rx), Outcome::Ok(key_name))
}

fn finish(tx: &Progress, what: &str, result: Result<()>) {
    match result {
        Ok(()) => {
            tracing::info!("{what}: ok");
            let _ = tx.send("result: ok".into());
        }
        Err(e) => {
            tracing::warn!("{what}: failed: {e:#}");
            let _ = tx.send(format!("error: {e:#}"));
            let _ = tx.send("result: failed".into());
        }
    }
}

/// Streams the body to `path`, counting and hashing. Stops as soon as the count
/// exceeds the ticket's size, or the hash does not match at the end.
async fn receive(mut body: Incoming, path: &std::path::Path, ticket: &Ticket) -> Result<(), Response<Body>> {
    let internal = |e: std::io::Error| {
        tracing::error!("upload: {e}");
        error(StatusCode::INTERNAL_SERVER_ERROR, "cannot store upload")
    };
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        let f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path).map_err(internal)?;
        tokio::fs::File::from_std(f)
    };
    let mut hasher = Sha256::new();
    let mut count = 0u64;
    let deadline = tokio::time::Instant::now() + UPLOAD_TOTAL_TIMEOUT;
    loop {
        let next = tokio::time::timeout_at(deadline.min(tokio::time::Instant::now() + UPLOAD_STALL_TIMEOUT), body.frame()).await;
        let frame = match next {
            Err(_) => return Err(error(StatusCode::REQUEST_TIMEOUT, "upload stalled or took too long")),
            Ok(None) => break,
            Ok(Some(Err(_))) => return Err(error(StatusCode::BAD_REQUEST, "upload interrupted")),
            Ok(Some(Ok(f))) => f,
        };
        let Ok(data) = frame.into_data() else { continue };
        count += data.len() as u64;
        if count > ticket.size {
            return Err(error(StatusCode::PAYLOAD_TOO_LARGE, "upload is larger than declared"));
        }
        hasher.update(&data);
        file.write_all(&data).await.map_err(internal)?;
    }
    file.flush().await.map_err(internal)?;
    if count != ticket.size {
        return Err(error(StatusCode::BAD_REQUEST, "upload is shorter than declared"));
    }
    if hex::encode(hasher.finalize()) != ticket.sha256 {
        return Err(error(StatusCode::BAD_REQUEST, "upload does not match the signed sha256"));
    }
    Ok(())
}

async fn unpack_and_deploy(paths: &Paths, bundle: &std::path::Path, dir: &std::path::Path, ticket: &Ticket, tx: &Progress) -> Result<()> {
    let cfg = ServerConfig::load(paths)?;
    let l = cfg.limits()?;
    let limits = UnpackLimits { max_unpacked: l.max_unpacked, max_files: l.max_files, max_depth: l.max_depth };
    std::fs::create_dir(dir)?;
    let (b, d) = (bundle.to_path_buf(), dir.to_path_buf());
    let size = tokio::task::spawn_blocking(move || unpack(&b, &d, limits)).await??;
    let _ = tx.send(format!("received {} ({} bytes unpacked)", ticket.service, size));
    let caller = Caller { key_name: ticket.key_name.clone(), admin: ticket.admin };
    deploy::deploy_dir(paths, dir, &caller, Some(&ticket.service), tx).await
}

fn rollback(state: &Arc<State>, key: &AuthorizedKey, service: String) -> Response<Body> {
    let lock = match DeployLock::try_acquire(&state.paths) {
        Ok(Some(l)) => l,
        Ok(None) => return error(StatusCode::CONFLICT, "busy"),
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "cannot lock"),
    };
    let caller = Caller { key_name: key.name.clone(), admin: key.scopes.contains(&Scope::Admin) };
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let paths = state.paths.clone();
    tokio::spawn(async move {
        let _lock = lock;
        let result = deploy::rollback(&paths, &service, &caller, &tx).await;
        finish(&tx, &format!("rollback {service}"), result);
    });
    streamed(rx)
}

fn remove(state: &Arc<State>, service: String) -> Response<Body> {
    let lock = match DeployLock::try_acquire(&state.paths) {
        Ok(Some(l)) => l,
        Ok(None) => return error(StatusCode::CONFLICT, "busy"),
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "cannot lock"),
    };
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let paths = state.paths.clone();
    let stats = state.stats.clone();
    tokio::spawn(async move {
        let _lock = lock;
        let result = deploy::remove_service(&paths, &service, &tx).await;
        if result.is_ok() {
            stats.lock().unwrap_or_else(|e| e.into_inner()).remove(&service);
        }
        finish(&tx, &format!("rm {service}"), result);
    });
    streamed(rx)
}

/// `?since=7d&by=day`; by hour up to two days, by day beyond.
fn traffic(state: &Arc<State>, service: &str, query: &str) -> Response<Body> {
    let mut since = 86_400;
    let mut by = None;
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        match pair.split_once('=') {
            Some(("since", v)) => match crate::spec::parse_duration_ms(v) {
                Ok(ms) if (3_600_000..=3650 * 86_400_000).contains(&ms) => since = ms / 1000,
                _ => return error(StatusCode::BAD_REQUEST, "since must be a duration between 1h and 3650d"),
            },
            Some(("by", v @ ("hour" | "day"))) => by = Some(v == "day"),
            _ => return error(StatusCode::BAD_REQUEST, "unknown stats option; use since and by=hour|day"),
        }
    }
    let by_day = by.unwrap_or(since > 2 * 86_400);
    if !by_day && since > 14 * 86_400 {
        return error(StatusCode::BAD_REQUEST, "by=hour covers at most 14 days");
    }
    let v = state.stats.lock().unwrap_or_else(|e| e.into_inner()).query(service, since, by_day, auth::unix_now());
    json(StatusCode::OK, v)
}

/// A JSON document, or 404 when the service does not exist.
fn json_result(r: Result<Option<serde_json::Value>>) -> Response<Body> {
    match r {
        Ok(Some(v)) => json(StatusCode::OK, v),
        Ok(None) => error(StatusCode::NOT_FOUND, "no such service"),
        Err(e) => {
            tracing::error!("{e:#}");
            error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

async fn logs(service: &str, query: &str) -> Response<Body> {
    let q = match api::LogQuery::parse(query) {
        Ok(q) => q,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("{e:#}")),
    };
    match api::logs(service, q).await {
        Ok(Some(rx)) => {
            let stream = futures_util::stream::unfold(rx, |mut rx| async move {
                let line = rx.recv().await?;
                Some((Ok::<_, Infallible>(Frame::data(Bytes::from(line + "\n"))), rx))
            });
            let mut r = Response::new(StreamBody::new(stream).boxed_unsync());
            r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/x-ndjson"));
            r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            r
        }
        Ok(None) => error(StatusCode::NOT_FOUND, "no such service or container"),
        Err(e) => {
            tracing::error!("{e:#}");
            error(StatusCode::INTERNAL_SERVER_ERROR, "cannot read logs")
        }
    }
}

fn set_secret(state: &Arc<State>, service: &str, key: &str, body: &[u8]) -> Response<Body> {
    let value = match api::validate_secret(key, body) {
        Ok(v) => v,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("{e:#}")),
    };
    let _guard = state.secrets_lock.lock().unwrap_or_else(|e| e.into_inner());
    match api::set_secret(&state.paths, service, key, value) {
        Ok(()) => json(StatusCode::OK, serde_json::json!({ "result": "ok" })),
        Err(e) => {
            tracing::error!("{e:#}");
            error(StatusCode::INTERNAL_SERVER_ERROR, "cannot store secret")
        }
    }
}

fn remove_secret(state: &Arc<State>, service: &str, key: &str) -> Response<Body> {
    let _guard = state.secrets_lock.lock().unwrap_or_else(|e| e.into_inner());
    match api::remove_secret(&state.paths, service, key) {
        Ok(true) => json(StatusCode::OK, serde_json::json!({ "result": "ok" })),
        Ok(false) => error(StatusCode::NOT_FOUND, "no such secret"),
        Err(e) => {
            tracing::error!("{e:#}");
            error(StatusCode::INTERNAL_SERVER_ERROR, "cannot remove secret")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::tests::test_key;
    use tokio::io::AsyncReadExt;

    struct Server {
        addr: SocketAddr,
        _dir: tempfile::TempDir,
        signing: ed25519_dalek::SigningKey,
        key_id: String,
        state: Arc<State>,
    }

    async fn start(scope: &str) -> Server {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { root: dir.path().to_path_buf() };
        std::fs::create_dir_all(paths.tmp()).unwrap();
        std::fs::write(paths.config(), toml::to_string(&ServerConfig::default()).unwrap()).unwrap();
        let (signing, public) = test_key();
        let line = format!("{} test scope={scope}\n", public.trim());
        std::fs::write(paths.authorized_keys(), &line).unwrap();
        let key_id = auth::parse_authorized_line(&line).unwrap().key_id;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(State::new(paths));
        tokio::spawn(serve(listener, state.clone()));
        Server { addr, _dir: dir, signing, key_id, state }
    }

    /// Sends a raw request and returns the status code and whole response.
    async fn raw(addr: SocketAddr, request: &[u8]) -> (u16, String) {
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(request).await.unwrap();
        let mut buf = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut buf)).await;
        let text = String::from_utf8_lossy(&buf).into_owned();
        let status = text.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
        (status, text)
    }

    fn signed(srv: &Server, method: &str, path: &str, body: &str, xff: &str) -> Vec<u8> {
        signed_as(&srv.signing, &srv.key_id, method, path, body, xff, auth::unix_now())
    }

    fn signed_as(
        key: &ed25519_dalek::SigningKey,
        key_id: &str,
        method: &str,
        path: &str,
        body: &str,
        xff: &str,
        time: u64,
    ) -> Vec<u8> {
        let nonce = hex::encode(rand::random::<[u8; 16]>());
        let sig = auth::sign(key, &auth::canonical_string(method, path, time, &nonce, body.as_bytes()));
        format!(
            "{method} {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\nX-Forwarded-For: {xff}\r\n\
             X-Mypaas-Key: {key_id}\r\nX-Mypaas-Time: {time}\r\nX-Mypaas-Nonce: {nonce}\r\nX-Mypaas-Sig: {sig}\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    impl Server {
        /// Authorizes another key and returns it with its id.
        fn add_key(&self, scope: &str, expires: Option<&str>) -> (ed25519_dalek::SigningKey, String) {
            let (signing, public) = test_key();
            let mut line = format!("{} k{} scope={scope}", public.trim(), rand::random::<u32>());
            if let Some(e) = expires {
                line.push_str(&format!(" expires={e}"));
            }
            let key_id = auth::parse_authorized_line(&line).unwrap().key_id;
            let path = self.state.paths.authorized_keys();
            let mut text = std::fs::read_to_string(&path).unwrap();
            text.push_str(&line);
            text.push('\n');
            std::fs::write(path, text).unwrap();
            self.state.force_key_reload();
            (signing, key_id)
        }

        async fn ticket(&self, service: &str, size: u64, sha256: &str, xff: &str) -> String {
            let body = format!(r#"{{"service":"{service}","size":{size},"sha256":"{sha256}"}}"#);
            let (status, text) = raw(self.addr, &signed(self, "POST", "/v1/deploy-tickets", &body, xff)).await;
            assert_eq!(status, 200, "{text}");
            json_body(&text)["ticket"].as_str().unwrap().to_string()
        }
    }

    fn json_body(text: &str) -> serde_json::Value {
        let body = &text[text.find("\r\n\r\n").expect("no body") + 4..];
        serde_json::from_str(body.trim()).unwrap_or_else(|_| panic!("not JSON: {body}"))
    }

    fn put_upload(ticket: &str, body: &[u8]) -> Vec<u8> {
        let mut req = format!(
            "PUT /v1/deploy HTTP/1.1\r\nHost: x\r\nConnection: close\r\nAuthorization: Bearer {ticket}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        req.extend_from_slice(body);
        req
    }

    #[tokio::test]
    async fn every_auth_failure_looks_the_same() {
        let srv = start("read").await;
        let (expired, expired_id) = srv.add_key("read", Some("2020-01-01"));
        // A valid key that is simply not in authorized_keys.
        let (unknown, unknown_public) = test_key();
        let unknown_id = auth::parse_public_key(&unknown_public).unwrap().2;
        let (out_of_scope, oos_id) = srv.add_key("deploy:other", None);
        let now = auth::unix_now();
        // A well-formed signature by the right key, but over a different request.
        let good = String::from_utf8(signed(&srv, "GET", "/v1/services", "", "10.0.0.4")).unwrap();
        let other_sig = auth::sign(&srv.signing, "mypaas/v1\nGET\n/v1/other\n0\n00\n00");
        let start = good.find("X-Mypaas-Sig: ").unwrap() + 14;
        let end = start + good[start..].find("\r\n").unwrap();
        let bad_sig = format!("{}{other_sig}{}", &good[..start], &good[end..]).into_bytes();
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("unknown key", signed_as(&unknown, &unknown_id, "GET", "/v1/services", "", "10.0.0.1", now)),
            ("expired key", signed_as(&expired, &expired_id, "GET", "/v1/services", "", "10.0.0.2", now)),
            ("out of scope", signed_as(&out_of_scope, &oos_id, "GET", "/v1/services", "", "10.0.0.3", now)),
            ("bad signature", bad_sig),
            ("too old", signed_as(&srv.signing, &srv.key_id, "GET", "/v1/services", "", "10.0.0.5", now - 40)),
            ("too new", signed_as(&srv.signing, &srv.key_id, "GET", "/v1/services", "", "10.0.0.6", now + 40)),
            ("no headers", b"GET /v1/services HTTP/1.1\r\nHost: x\r\nConnection: close\r\nX-Forwarded-For: 10.0.0.7\r\n\r\n".to_vec()),
        ];
        for (what, req) in cases {
            let (status, text) = raw(srv.addr, &req).await;
            assert_eq!(status, 403, "{what}: {text}");
            let mut body = json_body(&text);
            assert!(body["server_time"].is_u64(), "{what}");
            body.as_object_mut().unwrap().remove("server_time");
            assert_eq!(body, serde_json::json!({ "error": "denied" }), "{what} must not reveal why");
        }
        // Inside the 30 second window is fine (kept clear of the edge, since a
        // second may tick between signing and checking).
        let ok = signed_as(&srv.signing, &srv.key_id, "GET", "/v1/secrets-free-route", "", "10.0.0.8", now - 25);
        assert_eq!(raw(srv.addr, &ok).await.0, 404, "unknown paths stay 404");
        let ok = signed_as(&srv.signing, &srv.key_id, "GET", "/v1/services/x/secrets", "", "10.0.0.8", now - 25);
        assert_eq!(raw(srv.addr, &ok).await.0, 403, "read does not include secrets");
        let (sk, id) = srv.add_key("secrets:x", None);
        let ok = signed_as(&sk, &id, "GET", "/v1/services/x/secrets", "", "10.0.0.9", now + 25);
        assert_eq!(raw(srv.addr, &ok).await.0, 200);
    }

    #[tokio::test]
    async fn oversized_bundle_refused_at_the_ticket() {
        let srv = start("deploy:*").await;
        let max = ServerConfig::default().limits().unwrap().max_upload;
        for size in [max + 1, 0] {
            let body = format!(r#"{{"service":"app","size":{size},"sha256":"{}"}}"#, "a".repeat(64));
            let (status, text) = raw(srv.addr, &signed(&srv, "POST", "/v1/deploy-tickets", &body, "1.2.3.4")).await;
            assert_eq!(status, 413, "size {size}: {text}");
        }
        assert!(srv.ticket("app", max, &"a".repeat(64), "1.2.3.4").await.len() == 64);
    }

    #[tokio::test]
    async fn busy_keeps_the_ticket_and_tickets_are_single_use() {
        let srv = start("deploy:*").await;
        let ticket = srv.ticket("app", 5, &"a".repeat(64), "2.3.4.5").await;
        let lock = DeployLock::try_acquire(&srv.state.paths).unwrap().unwrap();
        let (status, text) = raw(srv.addr, &put_upload(&ticket, b"12345")).await;
        assert_eq!(status, 409, "{text}");
        assert!(srv.state.guards().tickets.peek(&ticket, Instant::now()).is_some(), "busy must not burn the ticket");
        drop(lock);
        // Used once (the hash is wrong, so nothing is deployed)...
        assert_eq!(raw(srv.addr, &put_upload(&ticket, b"12345")).await.0, 400);
        // ...and never again.
        assert_eq!(raw(srv.addr, &put_upload(&ticket, b"12345")).await.0, 403);
    }

    /// Step 7 of the deploy flow: a ticket for one service cannot deploy another,
    /// even with a correct hash, because the Dockerfile's name is checked.
    #[tokio::test]
    async fn ticket_cannot_deploy_another_service() {
        let srv = start("deploy:app").await;
        let proj = tempfile::tempdir().unwrap();
        std::fs::write(proj.path().join("Dockerfile"), "# mypaas.service=victim\nFROM busybox\n").unwrap();
        let bundle = proj.path().join("b.tar.zst");
        let packed = crate::bundle::pack(proj.path(), &proj.path().join("Dockerfile"), &bundle).unwrap();
        let ticket = srv.ticket("app", packed.size, &packed.sha256, "3.4.5.6").await;
        let (status, text) = raw(srv.addr, &put_upload(&ticket, &std::fs::read(&bundle).unwrap())).await;
        assert_eq!(status, 200);
        assert!(text.contains("names service \"victim\", but the ticket is for \"app\""), "{text}");
        assert!(text.trim_end().trim_end_matches(['0', '\r', '\n']).ends_with("result: failed"), "{text}");
        assert_eq!(std::fs::read_dir(srv.state.paths.tmp()).unwrap().count(), 0, "temp files are cleaned");
    }

    #[tokio::test]
    async fn secret_values_are_write_only() {
        let srv = start("secrets:app").await;
        let value = "very-secret-value-123";
        let (status, _) = raw(srv.addr, &signed(&srv, "PUT", "/v1/services/app/secrets/TOKEN", value, "4.5.6.7")).await;
        assert_eq!(status, 200);
        let (status, text) = raw(srv.addr, &signed(&srv, "GET", "/v1/services/app/secrets", "", "4.5.6.7")).await;
        assert_eq!(status, 200);
        assert_eq!(json_body(&text), serde_json::json!({ "names": ["TOKEN"] }));
        assert!(!text.contains(value));
        // Another service's secrets are out of this key's scope.
        let (status, _) = raw(srv.addr, &signed(&srv, "GET", "/v1/services/other/secrets", "", "4.5.6.7")).await;
        assert_eq!(status, 403);
        let big = "x".repeat(api::MAX_SECRET_VALUE + 1);
        let (status, _) = raw(srv.addr, &signed(&srv, "PUT", "/v1/services/app/secrets/BIG", &big, "4.5.6.7")).await;
        assert_eq!(status, 413);
    }

    /// Every test in this binary logs into one shared buffer. A thread-local
    /// subscriber misses events when tests run in parallel, because tracing
    /// caches per-callsite interest globally.
    fn captured_logs() -> Arc<Mutex<Vec<u8>>> {
        static LOGS: std::sync::OnceLock<Arc<Mutex<Vec<u8>>>> = std::sync::OnceLock::new();
        LOGS.get_or_init(|| {
            #[derive(Clone)]
            struct Buf(Arc<Mutex<Vec<u8>>>);
            impl std::io::Write for Buf {
                fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                    self.0.lock().unwrap().extend_from_slice(b);
                    Ok(b.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            let buf = Arc::new(Mutex::new(Vec::new()));
            let writer = Buf(buf.clone());
            let _ = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::DEBUG)
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .try_init();
            buf
        })
        .clone()
    }

    /// The log records key, endpoint and outcome, never tickets or secret values.
    #[tokio::test]
    async fn logs_never_contain_tickets_or_secrets() {
        let logs = captured_logs();
        let srv = start("deploy:*,secrets:*").await;
        let value = format!("do-not-log-{}", rand::random::<u64>());
        raw(srv.addr, &signed(&srv, "PUT", "/v1/services/logtest/secrets/TOKEN", &value, "5.6.7.8")).await;
        let ticket = srv.ticket("logtest", 5, &"a".repeat(64), "5.6.7.8").await;
        raw(srv.addr, &put_upload(&ticket, b"12345")).await;
        raw(srv.addr, &put_upload(&ticket, b"12345")).await;
        let text = String::from_utf8_lossy(&logs.lock().unwrap()).into_owned();
        assert!(text.contains("/v1/services/logtest/secrets/TOKEN"), "the test must actually capture logs");
        assert!(text.contains("unknown or expired ticket"), "the reused ticket must be logged as denied");
        assert!(!text.contains(&value), "secret value logged");
        assert!(!text.contains(&ticket), "ticket logged");
    }

    #[tokio::test]
    async fn traffic_stats_endpoint() {
        let srv = start("read").await;
        let now = auth::unix_now();
        {
            let mut st = srv.state.stats.lock().unwrap();
            st.record("shop", now - 60, 200, 5_000_000, now);
            st.record("shop", now - 3 * 86_400, 503, 5_000_000, now);
        }
        let get = |path: &str, ip: &str| signed(&srv, "GET", path, "", ip);
        let (status, text) = raw(srv.addr, &get("/v1/services/shop/stats", "7.0.0.1")).await;
        assert_eq!(status, 200, "{text}");
        let v = json_body(&text);
        assert_eq!((v["by"].as_str(), v["total"]["requests"].as_u64()), (Some("hour"), Some(1)), "default: last 24h by hour");
        let v = json_body(&raw(srv.addr, &get("/v1/services/shop/stats?since=7d", "7.0.0.1")).await.1);
        assert_eq!(v["by"], "day");
        assert_eq!(v["total"]["requests"], 2);
        assert_eq!(v["total"]["status"]["5xx"], 1);
        assert_eq!(v["buckets"].as_array().unwrap().len(), 8, "every day in range, empty ones included");
        for bad in ["?since=10m", "?since=forever", "?by=week", "?since=30d&by=hour", "?colour=red"] {
            let (status, _) = raw(srv.addr, &get(&format!("/v1/services/shop/stats{bad}"), "7.0.0.2")).await;
            assert_eq!(status, 400, "{bad}");
        }
        // Stats need the read scope like everything else.
        let (k, id) = srv.add_key("logs", None);
        let req = signed_as(&k, &id, "GET", "/v1/services/shop/stats", "", "7.0.0.3", now);
        assert_eq!(raw(srv.addr, &req).await.0, 403);
    }

    #[tokio::test]
    async fn oversized_headers_refused() {
        let srv = start("read").await;
        let req = format!("GET /v1/services HTTP/1.1\r\nHost: x\r\nConnection: close\r\nX-Pad: {}\r\n\r\n", "a".repeat(9000));
        assert_eq!(raw(srv.addr, req.as_bytes()).await.0, 431);
    }

    /// The real client's signing against the real daemon, including exit codes.
    #[tokio::test]
    async fn client_interoperates_with_daemon() {
        let srv = start("secrets:app,logs").await;
        let base = format!("http://{}", srv.addr);
        let api = crate::client::Api::new(base.clone(), srv.signing.clone(), srv.key_id.clone()).unwrap();
        api.signed(reqwest::Method::PUT, "/v1/services/app/secrets/A_B", b"v".to_vec()).await.unwrap();
        let v: serde_json::Value = api.signed(reqwest::Method::GET, "/v1/services/app/secrets", Vec::new()).await.unwrap().json().await.unwrap();
        assert_eq!(v["names"][0], "A_B");
        // A signed query string must verify too.
        let r = api.signed(reqwest::Method::GET, "/v1/services/app/logs?tail=5&since=10m", Vec::new()).await;
        assert!(!matches!(r.as_ref().err().and_then(|e| e.downcast_ref::<crate::client::Exit>()), Some(x) if x.code == crate::client::EXIT_DENIED));
        let stranger = crate::client::Api::new(base, test_key().0, srv.key_id.clone()).unwrap();
        let err = stranger.signed(reqwest::Method::GET, "/v1/services/app/secrets", Vec::new()).await.unwrap_err();
        assert_eq!(err.downcast_ref::<crate::client::Exit>().unwrap().code, crate::client::EXIT_DENIED);
    }

    #[tokio::test]
    async fn signed_request_and_scope() {
        let srv = start("deploy:shop-*").await;
        let body = format!(r#"{{"service":"shop-web","size":100,"sha256":"{}"}}"#, "a".repeat(64));
        let (status, text) = raw(srv.addr, &signed(&srv, "POST", "/v1/deploy-tickets", &body, "1.1.1.1")).await;
        assert_eq!(status, 200, "{text}");
        assert!(text.contains("\"ticket\""));
        // Out of scope gets the same 403 as a bad signature.
        let body = format!(r#"{{"service":"blog","size":100,"sha256":"{}"}}"#, "a".repeat(64));
        let (status, text) = raw(srv.addr, &signed(&srv, "POST", "/v1/deploy-tickets", &body, "1.1.1.1")).await;
        assert_eq!(status, 403);
        assert!(text.contains("\"denied\"") && text.contains("server_time"));
        let (status, _) = raw(srv.addr, &signed(&srv, "GET", "/v1/services", "", "1.1.1.1")).await;
        assert_eq!(status, 403);
    }

    #[tokio::test]
    async fn replay_and_tamper_rejected() {
        let srv = start("read").await;
        let req = signed(&srv, "GET", "/v1/services", "", "2.2.2.2");
        let (status, _) = raw(srv.addr, &req).await;
        assert_ne!(status, 403);
        let (status, _) = raw(srv.addr, &req).await;
        assert_eq!(status, 403, "replay must be denied");
        let mut req = signed(&srv, "GET", "/v1/services", "", "2.2.2.3");
        let text = String::from_utf8(req.clone()).unwrap().replace("/v1/services", "/v1/services?x=1");
        req = text.into_bytes();
        let (status, _) = raw(srv.addr, &req).await;
        assert_eq!(status, 403);
    }

    #[tokio::test]
    async fn backoff_after_failures() {
        let srv = start("read").await;
        let bad = b"GET /v1/services HTTP/1.1\r\nHost: x\r\nConnection: close\r\nX-Forwarded-For: 9.9.9.9\r\n\r\n";
        for _ in 0..4 {
            assert_eq!(raw(srv.addr, bad).await.0, 403);
        }
        let (status, text) = raw(srv.addr, bad).await;
        assert_eq!(status, 429);
        assert!(text.to_ascii_lowercase().contains("retry-after: 2"));
        // Even a correctly signed request is refused while locked out...
        assert_eq!(raw(srv.addr, &signed(&srv, "GET", "/v1/services", "", "9.9.9.9")).await.0, 429);
        // ...but another address is unaffected.
        assert_ne!(raw(srv.addr, &signed(&srv, "GET", "/v1/services", "", "9.9.9.10")).await.0, 403);
    }

    #[tokio::test]
    async fn unknown_paths_and_bodies() {
        let srv = start("read").await;
        let (status, _) = raw(srv.addr, b"GET /admin HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await;
        assert_eq!(status, 404);
        let (status, _) = raw(srv.addr, b"GET /v1/deploy HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await;
        assert_eq!(status, 404);
        let chunked = b"POST /v1/deploy-tickets HTTP/1.1\r\nHost: x\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n";
        assert_eq!(raw(srv.addr, chunked).await.0, 411);
        let big = b"POST /v1/deploy-tickets HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Length: 100000\r\n\r\n";
        assert_eq!(raw(srv.addr, big).await.0, 413);
    }

    #[tokio::test]
    async fn upload_rejected_from_headers_alone() {
        let srv = start("deploy:*").await;
        // A bogus ticket with a huge declared body is refused at once, without reading it.
        let req = format!(
            "PUT /v1/deploy HTTP/1.1\r\nHost: x\r\nConnection: close\r\nX-Forwarded-For: 3.3.3.3\r\n\
             Authorization: Bearer {}\r\nContent-Length: 10000000000\r\n\r\n",
            "ab".repeat(32)
        );
        let start = Instant::now();
        let (status, _) = raw(srv.addr, req.as_bytes()).await;
        assert_eq!(status, 403);
        assert!(start.elapsed() < Duration::from_secs(2));

        // A real ticket with a different Content-Length is refused and voided.
        let body = format!(r#"{{"service":"app","size":100,"sha256":"{}"}}"#, "a".repeat(64));
        let (_, text) = raw(srv.addr, &signed(&srv, "POST", "/v1/deploy-tickets", &body, "3.3.3.4")).await;
        let json_start = text.find('{').unwrap();
        let ticket: TicketResponse = serde_json::from_str(text[json_start..].trim()).unwrap();
        let req = format!(
            "PUT /v1/deploy HTTP/1.1\r\nHost: x\r\nConnection: close\r\nX-Forwarded-For: 3.3.3.4\r\n\
             Authorization: Bearer {}\r\nContent-Length: 101\r\n\r\n",
            ticket.ticket
        );
        assert_eq!(raw(srv.addr, req.as_bytes()).await.0, 400);
        assert!(srv.state.guards().tickets.peek(&ticket.ticket, Instant::now()).is_none());
    }

    #[tokio::test]
    async fn upload_hash_mismatch() {
        let srv = start("deploy:*").await;
        let payload = "x".repeat(100);
        let body = format!(r#"{{"service":"app","size":100,"sha256":"{}"}}"#, "a".repeat(64));
        let (_, text) = raw(srv.addr, &signed(&srv, "POST", "/v1/deploy-tickets", &body, "4.4.4.4")).await;
        let ticket: TicketResponse = serde_json::from_str(text[text.find('{').unwrap()..].trim()).unwrap();
        let req = format!(
            "PUT /v1/deploy HTTP/1.1\r\nHost: x\r\nConnection: close\r\nAuthorization: Bearer {}\r\nContent-Length: 100\r\n\r\n{payload}",
            ticket.ticket
        );
        let (status, text) = raw(srv.addr, req.as_bytes()).await;
        assert_eq!(status, 400);
        assert!(text.contains("sha256"));
        assert_eq!(std::fs::read_dir(srv.state.paths.tmp()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn removed_key_stops_working() {
        let srv = start("read").await;
        assert_ne!(raw(srv.addr, &signed(&srv, "GET", "/v1/services", "", "5.5.5.5")).await.0, 403);
        std::fs::write(srv.state.paths.authorized_keys(), "").unwrap();
        srv.state.force_key_reload();
        assert_eq!(raw(srv.addr, &signed(&srv, "GET", "/v1/services", "", "5.5.5.5")).await.0, 403);
    }

    #[test]
    fn routes() {
        assert_eq!(route(&Method::POST, "/v1/deploy-tickets"), Some(Route::DeployTickets));
        assert_eq!(route(&Method::PUT, "/v1/deploy"), Some(Route::Deploy));
        assert_eq!(route(&Method::POST, "/v1/deploy"), None);
        assert_eq!(route(&Method::DELETE, "/v1/services/shop"), Some(Route::Remove("shop".into())));
        assert_eq!(route(&Method::DELETE, "/v1/services/../x"), None);
        assert_eq!(route(&Method::GET, "/v1x/services"), None);
        assert_eq!(route(&Method::GET, "/v1/services/a/logs"), Some(Route::Logs("a".into())));
        assert_eq!(route(&Method::PUT, "/v1/services/a/secrets/DB_PW"), Some(Route::SetSecret("a".into(), "DB_PW".into())));
        assert_eq!(route(&Method::PUT, "/v1/services/a/secrets/bad-name"), None);
        assert_eq!(route(&Method::GET, "/v1/services/a/secrets/DB_PW"), None, "secret values cannot be read");
        // Keys are managed on the server only.
        for path in ["/v1/keys", "/v1/authorized_keys", "/v1/keys/x"] {
            for m in [Method::GET, Method::POST, Method::PUT, Method::DELETE] {
                assert_eq!(route(&m, path), None);
            }
        }
    }

    #[test]
    fn forwarded_for_only_from_loopback() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", HeaderValue::from_static("6.6.6.6, 7.7.7.7"));
        assert_eq!(client_ip("127.0.0.1:1".parse().unwrap(), &h), "7.7.7.7".parse::<IpAddr>().unwrap());
        assert_eq!(client_ip("8.8.8.8:1".parse().unwrap(), &h), "8.8.8.8".parse::<IpAddr>().unwrap());
    }
}

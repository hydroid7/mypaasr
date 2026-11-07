//! A read-only filter in front of the Docker socket, for Traefik.
//!
//! Traefik is the internet-facing process; with the raw socket it would be root
//! on the host. Through this proxy it can only list and inspect containers, read
//! the version and follow events. Everything else gets a 403 and is logged, so
//! a Traefik upgrade that needs more calls is easy to spot.
//!
//! Container inspection would also reveal every environment variable, and with
//! it every secret, so `Config.Env` is removed from those answers.

use std::convert::Infallible;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Semaphore;

use crate::config::Paths;

pub const UPSTREAM: &str = "/var/run/docker.sock";
const MAX_CONNECTIONS: usize = 32;
const MAX_INSPECT_BODY: usize = 8 << 20;

type Body = UnsyncBoxBody<Bytes, hyper::Error>;

/// Where the filtered socket lives; Traefik mounts the directory, not the
/// socket file, so a restarted proxy is picked up without restarting Traefik.
pub fn socket_dir(paths: &Paths) -> PathBuf {
    paths.root.join("run")
}

pub fn socket_path(paths: &Paths) -> PathBuf {
    socket_dir(paths).join("docker.sock")
}

/// Strips an optional API version prefix like `/v1.47`.
fn unversioned(path: &str) -> &str {
    if let Some(rest) = path.strip_prefix("/v")
        && let Some(i) = rest.find('/')
        && rest[..i].split('.').all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit()))
    {
        return &rest[i..];
    }
    path
}

fn is_id(s: &str) -> bool {
    // Starting with an alphanumeric rules out `.` and `..`.
    s.len() <= 128
        && s.bytes().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && s.bytes().all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
}

/// The calls Traefik's Docker provider makes, and nothing else. Verified against
/// Traefik 3.7.17: HEAD /_ping, GET /version, /containers/json, /containers/<id>/json
/// and /events.
pub fn allowed(method: &Method, path: &str) -> bool {
    if method != Method::GET && method != Method::HEAD {
        return false;
    }
    let segments: Vec<&str> = unversioned(path).trim_start_matches('/').split('/').collect();
    match segments.as_slice() {
        ["_ping"] | ["version"] | ["events"] | ["containers", "json"] => true,
        ["containers", id, "json"] => is_id(id),
        _ => false,
    }
}

pub async fn run(paths: &Paths, upstream: &Path) -> Result<()> {
    let dir = socket_dir(paths);
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o750))?;
    let path = socket_path(paths);
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).with_context(|| format!("cannot listen on {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660))?;
    tracing::info!("docker socket proxy listening on {}", path.display());
    serve(listener, upstream.to_path_buf()).await
}

pub async fn serve(listener: UnixListener, upstream: PathBuf) -> Result<()> {
    let upstream = Arc::new(upstream);
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let (stream, _) = listener.accept().await?;
        let Ok(permit) = connections.clone().try_acquire_owned() else {
            drop(stream);
            continue;
        };
        let upstream = upstream.clone();
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |req| handle(upstream.clone(), req));
            if let Err(e) = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), service).await {
                tracing::debug!("proxy connection: {e}");
            }
            drop(permit);
        });
    }
}

fn reply(status: StatusCode, msg: &str) -> Response<Body> {
    let body = serde_json::json!({ "message": msg }).to_string();
    let mut r = Response::new(Full::new(Bytes::from(body)).map_err(|never: Infallible| match never {}).boxed_unsync());
    *r.status_mut() = status;
    r.headers_mut().insert("content-type", "application/json".parse().unwrap());
    r
}

async fn handle(upstream: Arc<PathBuf>, req: Request<Incoming>) -> Result<Response<Body>, Infallible> {
    let has_body = req.headers().contains_key("transfer-encoding")
        || req.headers().get("content-length").is_some_and(|v| v.as_bytes() != b"0");
    if has_body || !allowed(req.method(), req.uri().path()) {
        tracing::warn!("blocked {} {}", req.method(), req.uri());
        return Ok(reply(StatusCode::FORBIDDEN, "blocked by the mypaas docker socket proxy"));
    }
    tracing::debug!("allowed {} {}", req.method(), req.uri());
    match forward(&upstream, req).await {
        Ok(r) => Ok(r),
        Err(e) => {
            tracing::warn!("upstream: {e:#}");
            Ok(reply(StatusCode::BAD_GATEWAY, "docker is not reachable"))
        }
    }
}

async fn forward(upstream: &Path, req: Request<Incoming>) -> Result<Response<Body>> {
    let stream = UnixStream::connect(upstream).await.with_context(|| format!("cannot connect to {}", upstream.display()))?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::debug!("upstream connection: {e}");
        }
    });
    let path_and_query = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/").to_string();
    let mut out = Request::builder().method(req.method()).uri(&path_and_query).header("host", "docker");
    for (name, value) in req.headers() {
        // Only headers Traefik's client needs; nothing that could change the request's meaning.
        if matches!(name.as_str(), "accept" | "user-agent") {
            out = out.header(name, value);
        }
    }
    let resp = sender.send_request(out.body(Empty::<Bytes>::new())?).await?;
    if is_inspect(&path_and_query) && resp.status().is_success() {
        return redact_inspect(resp).await;
    }
    Ok(resp.map(|b| b.boxed_unsync()))
}

fn is_inspect(path_and_query: &str) -> bool {
    let path = path_and_query.split('?').next().unwrap_or("");
    let segments: Vec<&str> = unversioned(path).trim_start_matches('/').split('/').collect();
    matches!(segments.as_slice(), ["containers", _, "json"])
}

/// Buffers an inspect answer and drops the environment, which holds the secrets.
async fn redact_inspect(resp: Response<Incoming>) -> Result<Response<Body>> {
    let (mut parts, body) = resp.into_parts();
    let bytes = http_body_util::Limited::new(body, MAX_INSPECT_BODY)
        .collect()
        .await
        .map_err(|e| anyhow::anyhow!("inspect answer: {e}"))?
        .to_bytes();
    let mut v: serde_json::Value = serde_json::from_slice(&bytes).context("inspect answer is not JSON")?;
    if let Some(cfg) = v.get_mut("Config").and_then(|c| c.as_object_mut()) {
        cfg.insert("Env".into(), serde_json::Value::Array(Vec::new()));
    }
    let out = Bytes::from(serde_json::to_vec(&v)?);
    parts.headers.remove("transfer-encoding");
    parts.headers.insert("content-length", out.len().into());
    let body = Full::new(out).map_err(|never: Infallible| match never {}).boxed_unsync();
    Ok(Response::from_parts(parts, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist() {
        let get = Method::GET;
        assert!(allowed(&get, "/v1.47/containers/json"));
        assert!(allowed(&get, "/containers/abc123/json"));
        assert!(allowed(&get, "/v1.47/events"));
        assert!(allowed(&get, "/_ping"));
        assert!(allowed(&get, "/v1.51/version"));
        assert!(allowed(&Method::HEAD, "/_ping"));
        assert!(!allowed(&get, "/v1.47/networks"));
        assert!(!allowed(&Method::POST, "/v1.47/containers/create"));
        assert!(!allowed(&Method::POST, "/v1.47/containers/abc/exec"));
        assert!(!allowed(&Method::DELETE, "/v1.47/containers/abc"));
        assert!(!allowed(&get, "/v1.47/containers/abc/logs"));
        assert!(!allowed(&get, "/v1.47/containers/abc/archive"));
        assert!(!allowed(&get, "/v1.47/images/json"));
        assert!(!allowed(&get, "/v1.47/secrets"));
        assert!(!allowed(&get, "/vX/containers/json/../../images/json"));
        assert!(!allowed(&get, "/containers/../json"));
    }

    #[tokio::test]
    async fn forwards_allowed_and_blocks_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        // A fake Docker that answers every request with its method and path.
        let upstream = dir.path().join("upstream.sock");
        let fake = UnixListener::bind(&upstream).unwrap();
        tokio::spawn(async move {
            loop {
                let (s, _) = fake.accept().await.unwrap();
                tokio::spawn(async move {
                    let svc = hyper::service::service_fn(|req: Request<Incoming>| async move {
                        let body = if req.uri().path().ends_with("/abc/json") {
                            r#"{"Id":"abc","Config":{"Env":["DB_PASSWORD=s3cret"],"Labels":{"traefik.enable":"true"}}}"#.to_string()
                        } else {
                            format!("{} {}", req.method(), req.uri())
                        };
                        Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(body))))
                    });
                    let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(s), svc).await;
                });
            }
        });
        let proxy = dir.path().join("proxy.sock");
        tokio::spawn(serve(UnixListener::bind(&proxy).unwrap(), upstream));

        async fn call(sock: &Path, method: &str, path: &str) -> (StatusCode, String) {
            let s = UnixStream::connect(sock).await.unwrap();
            let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(s)).await.unwrap();
            tokio::spawn(conn);
            let req = Request::builder().method(method).uri(path).header("host", "x").body(Empty::<Bytes>::new()).unwrap();
            let resp = sender.send_request(req).await.unwrap();
            let status = resp.status();
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            (status, String::from_utf8_lossy(&body).into_owned())
        }
        let (status, body) = call(&proxy, "GET", "/v1.47/containers/json?all=1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "GET /v1.47/containers/json?all=1");
        let (status, body) = call(&proxy, "GET", "/v1.47/containers/abc/json").await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body.contains("s3cret"), "{body}");
        assert!(body.contains("traefik.enable"));
        let (status, _) = call(&proxy, "POST", "/v1.47/containers/create").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = call(&proxy, "GET", "/v1.47/images/json").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
}

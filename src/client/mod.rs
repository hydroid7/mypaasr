//! The client CLI: keys, remotes, signed requests and `push`.

pub mod ops;

use std::collections::BTreeMap;
use std::fmt;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ed25519_dalek::SigningKey;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use ssh_key::{Algorithm, LineEnding, PrivateKey};

use crate::auth;
use crate::bundle::{self, Packed};
use crate::config::{home_dir, write_atomic};
use crate::server::daemon::{TicketRequest, TicketResponse};
use crate::spec::{is_valid_host, parse_dockerfile, parse_size};

pub const DEFAULT_MAX_UPLOAD: &str = "50m";
const WARN_UPLOAD: u64 = 20 << 20;

/// An error with a specific exit code; see the design document.
#[derive(Debug)]
pub struct Exit {
    pub code: u8,
    pub msg: String,
}

pub const EXIT_ERROR: u8 = 1;
pub const EXIT_DENIED: u8 = 3;
pub const EXIT_DEPLOY_FAILED: u8 = 4;

impl fmt::Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for Exit {}

pub(crate) fn exit(code: u8, msg: impl Into<String>) -> anyhow::Error {
    Exit { code, msg: msg.into() }.into()
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Output {
    pub json: bool,
    pub quiet: bool,
    pub color: bool,
}

pub const GREEN: &str = "32";
pub const RED: &str = "31";
pub const YELLOW: &str = "33";
pub const DIM: &str = "2";
pub const BOLD: &str = "1";

/// Wraps text in an ANSI colour when colour output is on.
pub fn paint(out: Output, code: &str, text: &str) -> String {
    if out.color { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_string() }
}

fn config_dir() -> Result<PathBuf> {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(d) if !d.is_empty() => Ok(PathBuf::from(d).join("mypaas")),
        _ => Ok(home_dir()?.join(".config").join("mypaas")),
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct ClientConfig {
    key: Option<String>,
}

fn load_client_config() -> Result<ClientConfig> {
    let path = config_dir()?.join("config.toml");
    match std::fs::read_to_string(&path) {
        Ok(t) => toml::from_str(&t).with_context(|| format!("invalid {}", path.display())),
        Err(_) => Ok(ClientConfig::default()),
    }
}

fn save_client_config(cfg: &ClientConfig) -> Result<()> {
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir)?;
    write_atomic(&dir.join("config.toml"), toml::to_string(cfg)?.as_bytes(), 0o600)
}

// ---- keys ----

/// The private key file in use: from `key init`, else mypaas' own, else `~/.ssh/id_ed25519`.
fn key_path() -> Result<PathBuf> {
    if let Some(p) = load_client_config()?.key {
        return Ok(PathBuf::from(p));
    }
    let own = config_dir()?.join("id_ed25519");
    if own.exists() {
        return Ok(own);
    }
    Ok(home_dir()?.join(".ssh").join("id_ed25519"))
}

fn read_private_key() -> Result<(PrivateKey, String)> {
    if let Ok(text) = std::env::var("MYPAAS_PRIVATE_KEY")
        && !text.trim().is_empty()
    {
        let key = PrivateKey::from_openssh(text.trim()).context("MYPAAS_PRIVATE_KEY is not an OpenSSH private key")?;
        return Ok((key, "MYPAAS_PRIVATE_KEY".into()));
    }
    let path = key_path()?;
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read key {}; create one with `mypaas key init`", path.display()))?;
    let key = PrivateKey::from_openssh(&text).with_context(|| format!("{} is not an OpenSSH private key", path.display()))?;
    Ok((key, path.display().to_string()))
}

/// Loads the signing key, asking for the passphrase once if it is encrypted.
pub fn load_signing_key() -> Result<(SigningKey, String)> {
    let (mut key, source) = read_private_key()?;
    if key.algorithm() != Algorithm::Ed25519 {
        bail!("{source} is not an Ed25519 key");
    }
    if key.is_encrypted() {
        let pass = rpassword::prompt_password(format!("passphrase for {source}: ")).with_context(|| {
            format!("{source} is encrypted and there is no terminal to ask for its passphrase; use a passwordless key (for CI: MYPAAS_PRIVATE_KEY)")
        })?;
        key = key.decrypt(pass.as_bytes()).map_err(|_| anyhow::anyhow!("wrong passphrase for {source}"))?;
    }
    let ed = key.key_data().ed25519().context("not an Ed25519 key")?;
    let signing = SigningKey::from_bytes(&ed.private.to_bytes());
    let key_id = auth::key_id(key.public_key());
    Ok((signing, key_id))
}

pub fn key_init(existing: Option<&Path>, no_passphrase: bool) -> Result<()> {
    if let Some(path) = existing {
        let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
        let key = PrivateKey::from_openssh(&text).context("not an OpenSSH private key")?;
        if key.algorithm() != Algorithm::Ed25519 {
            bail!("only Ed25519 keys are supported");
        }
        let abs = std::fs::canonicalize(path)?;
        save_client_config(&ClientConfig { key: Some(abs.display().to_string()) })?;
        println!("using {}", abs.display());
        println!("{}", public_line(&key)?);
        return Ok(());
    }
    let dir = config_dir()?;
    let path = dir.join("id_ed25519");
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    let pass = if no_passphrase {
        String::new()
    } else {
        let pass = rpassword::prompt_password("new passphrase (empty for none): ")?;
        if !pass.is_empty() && rpassword::prompt_password("repeat passphrase: ")? != pass {
            bail!("the passphrases differ");
        }
        pass
    };
    let mut key = PrivateKey::random(&mut rand::rngs::OsRng, Algorithm::Ed25519)?;
    key.set_comment(default_comment());
    let stored = if pass.is_empty() {
        eprintln!("warning: the key is stored without a passphrase; anyone who can read {} can use it", path.display());
        key.clone()
    } else {
        key.encrypt(&mut rand::rngs::OsRng, pass.as_bytes())?
    };
    std::fs::create_dir_all(&dir)?;
    write_atomic(&path, stored.to_openssh(LineEnding::LF)?.as_bytes(), 0o600)?;
    write_atomic(&path.with_extension("pub"), format!("{}\n", public_line(&key)?).as_bytes(), 0o644)?;
    save_client_config(&ClientConfig { key: Some(path.display().to_string()) })?;
    println!("created {}", path.display());
    println!("add it on the server with: mypaas server key add - --name <name> --scope admin");
    println!("{}", public_line(&key)?);
    Ok(())
}

fn default_comment() -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "mypaas".into());
    format!("{user}@mypaas")
}

fn public_line(key: &PrivateKey) -> Result<String> {
    Ok(key.public_key().to_openssh()?)
}

/// Prints a new passwordless key for `MYPAAS_PRIVATE_KEY` in CI.
pub fn key_gen() -> Result<()> {
    let mut key = PrivateKey::random(&mut rand::rngs::OsRng, Algorithm::Ed25519)?;
    key.set_comment("ci@mypaas");
    print!("{}", key.to_openssh(LineEnding::LF)?.as_str());
    eprintln!("public key for authorized_keys:\n{}", public_line(&key)?);
    Ok(())
}

pub fn key_show() -> Result<()> {
    let (key, source) = read_private_key()?;
    eprintln!("key: {source}");
    println!("{}", public_line(&key)?);
    eprintln!("id:  {}", auth::key_id(key.public_key()));
    Ok(())
}

// ---- remotes ----

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Remotes {
    remotes: BTreeMap<String, String>,
}

fn remotes_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("remotes.toml"))
}

fn load_remotes() -> Result<Remotes> {
    let path = remotes_path()?;
    match std::fs::read_to_string(&path) {
        Ok(t) => toml::from_str(&t).with_context(|| format!("invalid {}", path.display())),
        Err(_) => Ok(Remotes::default()),
    }
}

fn save_remotes(r: &Remotes) -> Result<()> {
    let path = remotes_path()?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    write_atomic(&path, toml::to_string(r)?.as_bytes(), 0o600)
}

fn normalize_remote(domain: &str) -> Result<String> {
    let host = domain.strip_prefix("https://").unwrap_or(domain).trim_end_matches('/');
    if !is_valid_host(host) {
        bail!("invalid admin domain {domain:?}");
    }
    Ok(format!("https://{host}"))
}

pub fn remote_add(name: &str, domain: &str) -> Result<()> {
    if !crate::spec::is_valid_name(name) {
        bail!("invalid remote name {name:?}");
    }
    let mut r = load_remotes()?;
    r.remotes.insert(name.to_string(), normalize_remote(domain)?);
    save_remotes(&r)
}

pub fn remote_rm(name: &str) -> Result<()> {
    let mut r = load_remotes()?;
    if r.remotes.remove(name).is_none() {
        bail!("no remote named {name}");
    }
    save_remotes(&r)
}

pub fn remote_ls(out: Output) -> Result<()> {
    let r = load_remotes()?;
    if out.json {
        println!("{}", serde_json::to_string(&r.remotes)?);
    } else {
        for (name, url) in &r.remotes {
            println!("{name}\t{url}");
        }
    }
    Ok(())
}

/// `-r name`, `-r https://domain`, `MYPAAS_REMOTE`, or the only remote.
pub fn resolve_remote(arg: Option<&str>) -> Result<String> {
    let env = std::env::var("MYPAAS_REMOTE").ok().filter(|s| !s.is_empty());
    let wanted = arg.map(str::to_string).or(env);
    let remotes = load_remotes()?;
    match wanted {
        Some(w) if w.starts_with("https://") => normalize_remote(&w),
        // Plain HTTP only to a daemon on this machine, for development.
        Some(w) if w.starts_with("http://127.0.0.1:") || w.starts_with("http://localhost:") => {
            Ok(w.trim_end_matches('/').to_string())
        }
        Some(w) => remotes.remotes.get(&w).cloned().with_context(|| format!("no remote named {w}; see `mypaas remote ls`")),
        None => match remotes.remotes.len() {
            1 => Ok(remotes.remotes.values().next().unwrap().clone()),
            0 => bail!("no remote configured; add one with `mypaas remote add <name> <admin-domain>`"),
            _ => bail!("several remotes are configured; pick one with -r <name>"),
        },
    }
}

// ---- API ----

pub struct Api {
    base: String,
    http: reqwest::Client,
    key: SigningKey,
    key_id: String,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: String,
    server_time: Option<u64>,
}

impl Api {
    pub fn connect(remote: Option<&str>) -> Result<Api> {
        let base = resolve_remote(remote)?;
        let (key, key_id) = load_signing_key()?;
        Self::new(base, key, key_id)
    }

    pub fn new(base: String, key: SigningKey, key_id: String) -> Result<Api> {
        let http = reqwest::Client::builder()
            .http1_only()
            .user_agent(concat!("mypaas/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(15))
            .build()?;
        Ok(Api { base, http, key, key_id })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// Sends a signed request and checks the status.
    pub async fn signed(&self, method: reqwest::Method, path: &str, body: Vec<u8>) -> Result<reqwest::Response> {
        let resp = self.send_signed(method, path, body).await.with_context(|| format!("cannot reach {}", self.base))?;
        check_status(resp).await
    }

    /// Sends a signed request and returns whatever came back.
    pub async fn send_signed(&self, method: reqwest::Method, path: &str, body: Vec<u8>) -> reqwest::Result<reqwest::Response> {
        let time = auth::unix_now();
        let nonce = hex::encode(rand::random::<[u8; 16]>());
        let canonical = auth::canonical_string(method.as_str(), path, time, &nonce, &body);
        let sig = auth::sign(&self.key, &canonical);
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.base))
            .header(auth::HEADER_KEY, &self.key_id)
            .header(auth::HEADER_TIME, time.to_string())
            .header(auth::HEADER_NONCE, nonce)
            .header(auth::HEADER_SIG, sig);
        if !body.is_empty() {
            req = req.body(body);
        }
        req.send().await
    }
}

/// Turns error statuses into errors with the right exit code and a hint.
pub(crate) async fn check_status(resp: reqwest::Response) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let retry = resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).map(str::to_string);
    let text = resp.text().await.unwrap_or_default();
    let body: Option<ErrorBody> = serde_json::from_str(&text).ok();
    let msg = body.as_ref().map(|b| b.error.clone()).unwrap_or_else(|| text.trim().to_string());
    match status.as_u16() {
        403 => {
            let mut m = "denied: unknown or expired key, missing scope or bad signature".to_string();
            if let Some(t) = body.and_then(|b| b.server_time) {
                let skew = auth::unix_now() as i64 - t as i64;
                if skew.abs() > 20 {
                    m.push_str(&format!("\nyour clock differs from the server's by {skew}s; requests are accepted within 30s"));
                }
            }
            Err(exit(EXIT_DENIED, m))
        }
        429 => Err(exit(EXIT_DENIED, format!("{msg}; retry after {}s", retry.as_deref().unwrap_or("a few ")))),
        409 => Err(exit(EXIT_ERROR, "the server is busy with another deploy; try again shortly")),
        _ => Err(exit(EXIT_ERROR, format!("server answered {status}: {msg}"))),
    }
}

/// Prints a stream of progress lines and returns whether it ended with `result: ok`.
pub(crate) async fn follow_progress(resp: reqwest::Response, out: Output) -> Result<()> {
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut result: Option<bool> = None;
    let mut handle = |line: &str| {
        if let Some(r) = line.strip_prefix("result: ") {
            result = Some(r == "ok");
        }
        if out.json {
            println!("{}", serde_json::json!({ "line": line }));
        } else if line == "result: ok" {
            println!("{}", paint(out, GREEN, line));
        } else if line.starts_with("result:") || line.starts_with("error:") || line.starts_with("deploy failed") {
            println!("{}", paint(out, RED, line));
        } else if line.starts_with("warning:") {
            println!("{}", paint(out, YELLOW, line));
        } else if !out.quiet {
            println!("{line}");
        }
    };
    while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk.context("connection lost during the deploy")?);
        while let Some(i) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=i).collect();
            handle(String::from_utf8_lossy(&line[..line.len() - 1]).as_ref());
        }
    }
    if !buf.is_empty() {
        handle(String::from_utf8_lossy(&buf).as_ref());
    }
    match result {
        Some(true) => Ok(()),
        Some(false) => Err(exit(EXIT_DEPLOY_FAILED, "deploy failed on the server")),
        None => Err(exit(EXIT_ERROR, "the connection ended before the server reported a result")),
    }
}

// ---- push ----

/// `PATH` is a project directory or a Dockerfile inside one.
pub fn resolve_project(path: &Path) -> Result<(PathBuf, PathBuf)> {
    if path.is_file() {
        let file = std::fs::canonicalize(path)?;
        let dir = file.parent().context("Dockerfile has no parent directory")?.to_path_buf();
        Ok((dir, file))
    } else if path.is_dir() {
        let dir = std::fs::canonicalize(path)?;
        let file = dir.join("Dockerfile");
        if !file.is_file() {
            bail!("{} has no Dockerfile; pass the Dockerfile path instead", dir.display());
        }
        Ok((dir, file))
    } else {
        bail!("{} does not exist", path.display())
    }
}

pub(crate) fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

fn print_largest(packed: &Packed) {
    eprintln!("largest files:");
    for f in packed.largest(10) {
        eprintln!("  {:>10}  {}", human(f.size), f.path);
    }
}

/// Packs into a temp file that is removed when dropped.
pub struct PackedBundle {
    pub path: PathBuf,
    pub packed: Packed,
}

impl Drop for PackedBundle {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn pack_project(dir: &Path, dockerfile: &Path) -> Result<PackedBundle> {
    let path = std::env::temp_dir().join(format!("mypaas-{}-{:08x}.tar.zst", std::process::id(), rand::random::<u32>()));
    let packed = bundle::pack(dir, dockerfile, &path).inspect_err(|_| {
        let _ = std::fs::remove_file(&path);
    })?;
    Ok(PackedBundle { path, packed })
}

pub async fn push(remote: Option<&str>, path: &Path, dry_run: bool, out: Output) -> Result<()> {
    let (dir, dockerfile) = resolve_project(path)?;
    let (spec, _) = parse_dockerfile(&std::fs::read_to_string(&dockerfile)?)?;
    let bundle = pack_project(&dir, &dockerfile)?;
    let p = &bundle.packed;
    for s in &p.skipped {
        eprintln!("warning: skipped {s} (symlinks and special files are not uploaded)");
    }
    if dry_run {
        if out.json {
            let files: Vec<_> = p.files.iter().map(|f| serde_json::json!({ "path": f.path, "size": f.size })).collect();
            println!(
                "{}",
                serde_json::json!({ "service": spec.service, "files": files, "size": p.size, "unpacked": p.total_unpacked(), "sha256": p.sha256 })
            );
        } else {
            println!("service {}: {} files, {} unpacked, {} compressed", spec.service, p.files.len(), human(p.total_unpacked()), human(p.size));
            print_largest(p);
        }
        return Ok(());
    }
    let max = parse_size(DEFAULT_MAX_UPLOAD)?;
    if p.size > max {
        print_largest(p);
        bail!("the bundle is {}, above the {} limit; check .gitignore", human(p.size), human(max));
    }
    if p.size > WARN_UPLOAD {
        eprintln!("warning: the bundle is {}; a stray binary is usually a missing .gitignore entry", human(p.size));
        print_largest(p);
    }
    let api = Api::connect(remote)?;
    if !out.quiet && !out.json {
        eprintln!("pushing {} ({} files, {})", spec.service, p.files.len(), human(p.size));
    }

    let req = TicketRequest { service: spec.service.clone(), size: p.size, sha256: p.sha256.clone() };
    let resp = api.signed(reqwest::Method::POST, "/v1/deploy-tickets", serde_json::to_vec(&req)?).await?;
    let ticket: TicketResponse = resp.json().await.context("invalid ticket response")?;

    let file = tokio::fs::File::open(&bundle.path).await?;
    let total = p.size;
    let show = !out.quiet && !out.json && std::io::stderr().is_terminal();
    let mut sent = 0u64;
    let stream = tokio_util::io::ReaderStream::new(file).inspect(move |chunk| {
        if let Ok(c) = chunk {
            sent += c.len() as u64;
            if show {
                let pct = sent * 100 / total.max(1);
                let bar = "#".repeat((pct / 4) as usize);
                eprint!("\rupload [{bar:<25}] {pct:>3}% {}", human(sent));
                if sent == total {
                    eprintln!();
                }
                let _ = std::io::stderr().flush();
            }
        }
    });
    let resp = api
        .http
        .put(format!("{}/v1/deploy", api.base))
        .bearer_auth(&ticket.ticket)
        .header("content-length", total.to_string())
        .header("content-type", "application/zstd")
        .body(reqwest::Body::wrap_stream(stream))
        .send()
        .await
        .context("upload failed")?;
    let resp = check_status(resp).await?;
    follow_progress(resp, out).await
}

pub async fn rollback(remote: Option<&str>, service: &str, out: Output) -> Result<()> {
    let api = Api::connect(remote)?;
    let resp = api.signed(reqwest::Method::POST, &format!("/v1/services/{service}/rollback"), Vec::new()).await?;
    follow_progress(resp, out).await
}

pub async fn rm(remote: Option<&str>, service: &str, out: Output) -> Result<()> {
    let api = Api::connect(remote)?;
    let resp = api.signed(reqwest::Method::DELETE, &format!("/v1/services/{service}"), Vec::new()).await?;
    follow_progress(resp, out).await
}

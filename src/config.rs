//! Server-side files under `~/_mypaas` and the server's `config.toml`.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::spec::{is_valid_host, parse_size};

pub const TRAEFIK_VERSION: &str = "3.7.17";
/// Digest of the multi-arch image for TRAEFIK_VERSION, so a re-pushed tag
/// cannot change what runs on the internet-facing port. Update both together.
pub const TRAEFIK_DIGEST: &str = "sha256:f4bb143d1135a579b630ed491e17ec50f6f7c9c6a30585ff3d385471d2a86f5a";

pub fn traefik_image() -> String {
    format!("traefik:v{TRAEFIK_VERSION}@{TRAEFIK_DIGEST}")
}
pub const ROUTER_CONTAINER: &str = "mypaas-router";

#[derive(Debug, Clone)]
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    /// `$MYPAAS_HOME`, or `~/_mypaas`.
    pub fn from_env() -> Result<Paths> {
        if let Some(p) = std::env::var_os("MYPAAS_HOME") {
            return Ok(Paths { root: PathBuf::from(p) });
        }
        Ok(Paths { root: home_dir()?.join("_mypaas") })
    }

    pub fn config(&self) -> PathBuf {
        self.root.join("config.toml")
    }
    pub fn authorized_keys(&self) -> PathBuf {
        self.root.join("authorized_keys")
    }
    pub fn traefik_toml(&self) -> PathBuf {
        self.root.join("traefik.toml")
    }
    pub fn staticroutes_toml(&self) -> PathBuf {
        self.root.join("staticroutes.toml")
    }
    pub fn acme_json(&self) -> PathBuf {
        self.root.join("acme.json")
    }
    pub fn secrets_dir(&self) -> PathBuf {
        self.root.join("secrets")
    }
    pub fn secrets_file(&self, service: &str) -> PathBuf {
        self.secrets_dir().join(format!("{service}.toml"))
    }
    pub fn services_dir(&self) -> PathBuf {
        self.root.join("services")
    }
    pub fn service_dir(&self, service: &str) -> PathBuf {
        self.services_dir().join(service)
    }
    pub fn volumes_dir(&self) -> PathBuf {
        self.root.join("volumes")
    }
    pub fn tmp(&self) -> PathBuf {
        self.root.join("tmp")
    }
}

pub fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).context("HOME is not set")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// The admin domain on which Traefik forwards `/v1` to the daemon.
    pub domain: String,
    /// Let's Encrypt account email; empty registers without one.
    pub email: String,
    pub listen: String,
    pub limits: Limits,
    pub defaults: Defaults,
    pub maximums: Maximums,
    pub volumes: VolumeConfig,
    pub router: RouterConfig,
    pub stats: StatsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub max_upload: String,
    pub max_unpacked: String,
    pub max_files: u64,
    pub max_depth: usize,
    pub build_timeout: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Defaults {
    pub cpu: f64,
    pub mem: String,
    pub pids: u32,
    pub log_max_size: String,
    pub log_max_file: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Maximums {
    pub cpu: f64,
    pub mem: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VolumeConfig {
    /// Absolute host paths that admin deploys may mount.
    pub allow: Vec<String>,
    /// Give empty named volumes to the image's non-root user.
    pub chown: bool,
}

impl Default for VolumeConfig {
    fn default() -> Self {
        VolumeConfig { allow: Vec::new(), chown: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RouterConfig {
    /// Traefik reaches Docker through mypaas' read-only socket proxy instead of
    /// the raw socket, so the internet-facing process is not root-equivalent.
    pub socket_proxy: bool,
    /// How long one request on port 443 may take to arrive, headers and body.
    /// Traefik has no separate header timeout, so this also bounds slow-header
    /// attacks; it must fit the largest upload at the slowest uplink.
    pub read_timeout: String,
    /// Refuse TLS handshakes for names without a certificate instead of serving
    /// Traefik's default one. Turn off only for testing without real certificates.
    pub strict_sni: bool,
    /// If not empty, only these client networks (CIDRs) reach the admin API;
    /// everyone else gets 403 from Traefik. The services are not affected.
    pub api_allow: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StatsConfig {
    /// Traefik writes an access log (no client addresses or paths) that the
    /// daemon turns into per-service traffic stats.
    pub enabled: bool,
    pub retention_days: u64,
}

impl Default for StatsConfig {
    fn default() -> Self {
        StatsConfig { enabled: true, retention_days: 90 }
    }
}

impl Default for RouterConfig {
    fn default() -> Self {
        RouterConfig { socket_proxy: true, read_timeout: "300s".into(), strict_sni: true, api_allow: Vec::new() }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            domain: String::new(),
            email: String::new(),
            listen: "127.0.0.1:88".into(),
            limits: Limits::default(),
            defaults: Defaults::default(),
            maximums: Maximums::default(),
            volumes: VolumeConfig::default(),
            router: RouterConfig::default(),
            stats: StatsConfig::default(),
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_upload: "100m".into(),
            max_unpacked: "200m".into(),
            max_files: 40_000,
            max_depth: 32,
            build_timeout: "15m".into(),
        }
    }
}

impl Default for Defaults {
    fn default() -> Self {
        Defaults { cpu: 1.0, mem: "512m".into(), pids: 512, log_max_size: "10m".into(), log_max_file: 3 }
    }
}

impl Default for Maximums {
    fn default() -> Self {
        Maximums { cpu: 4.0, mem: "4g".into() }
    }
}

/// Limits from the config, converted and validated.
#[derive(Debug, Clone, Copy)]
pub struct EffectiveLimits {
    pub max_upload: u64,
    pub max_unpacked: u64,
    pub max_files: u64,
    pub max_depth: usize,
    pub build_timeout_ms: u64,
    pub default_cpu: f64,
    pub default_mem: u64,
    pub max_cpu: f64,
    pub max_mem: u64,
}

impl ServerConfig {
    pub fn load(paths: &Paths) -> Result<ServerConfig> {
        let path = paths.config();
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}; run `mypaas server init` first", path.display()))?;
        let cfg: ServerConfig = toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;
        cfg.validate(paths).with_context(|| format!("invalid {}", path.display()))?;
        Ok(cfg)
    }

    pub fn validate(&self, paths: &Paths) -> Result<()> {
        if !self.domain.is_empty() && !is_valid_host(&self.domain) {
            bail!("domain {:?} is not a valid host name", self.domain);
        }
        if !self.email.is_empty() && !is_plausible_email(&self.email) {
            bail!("email {:?} does not look like an email address", self.email);
        }
        self.limits()?;
        for net in &self.router.api_allow {
            if !is_cidr(net) {
                bail!("router.api_allow entry {net:?} is not an address or CIDR like 203.0.113.0/24");
            }
        }
        let rt = crate::spec::parse_duration_ms(&self.router.read_timeout).context("router.read_timeout")?;
        if !(60_000..=3_600_000).contains(&rt) {
            bail!("router.read_timeout must be between 60s and 1h");
        }
        if !(1..=3650).contains(&self.stats.retention_days) {
            bail!("stats.retention_days must be between 1 and 3650");
        }
        for entry in &self.volumes.allow {
            check_allowlist_entry(paths, entry)?;
        }
        Ok(())
    }

    /// `router.read_timeout`, validated at load.
    pub fn read_timeout(&self) -> std::time::Duration {
        let ms = crate::spec::parse_duration_ms(&self.router.read_timeout).unwrap_or(300_000);
        std::time::Duration::from_millis(ms)
    }

    pub fn limits(&self) -> Result<EffectiveLimits> {
        let l = EffectiveLimits {
            max_upload: parse_size(&self.limits.max_upload).context("limits.max_upload")?,
            max_unpacked: parse_size(&self.limits.max_unpacked).context("limits.max_unpacked")?,
            max_files: self.limits.max_files,
            max_depth: self.limits.max_depth,
            build_timeout_ms: crate::spec::parse_duration_ms(&self.limits.build_timeout).context("limits.build_timeout")?,
            default_cpu: self.defaults.cpu,
            default_mem: parse_size(&self.defaults.mem).context("defaults.mem")?,
            max_cpu: self.maximums.cpu,
            max_mem: parse_size(&self.maximums.mem).context("maximums.mem")?,
        };
        if l.default_cpu > l.max_cpu || l.default_mem > l.max_mem {
            bail!("defaults are above maximums");
        }
        Ok(l)
    }

}

/// `203.0.113.7`, `203.0.113.0/24` or `2001:db8::/32`.
pub fn is_cidr(s: &str) -> bool {
    let (addr, prefix) = match s.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (s, None),
    };
    let Ok(ip) = addr.parse::<std::net::IpAddr>() else { return false };
    let max = if ip.is_ipv4() { 32 } else { 128 };
    prefix.is_none_or(|p| p.parse::<u8>().is_ok_and(|n| n <= max) && !p.starts_with('+'))
}

/// `local@domain.tld`, printable, and nothing that could break out of the TOML string
/// it is written into.
fn is_plausible_email(s: &str) -> bool {
    let Some((local, domain)) = s.split_once('@') else { return false };
    !local.is_empty()
        && domain.contains('.')
        && is_valid_host(&domain.to_ascii_lowercase())
        && s.bytes().all(|c| c.is_ascii_graphic() && c != b'"' && c != b'\\')
}

/// Lexically normalizes an absolute path: resolves `.` and `..` without touching
/// the disk. Returns `None` for relative paths or `..` above the root.
pub fn normalize(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut out = PathBuf::from("/");
    for c in path.components() {
        match c {
            Component::RootDir => {}
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::Normal(n) => out.push(n),
            Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

/// Expands a leading `~` to the server user's home directory.
pub fn expand_home(p: &str) -> Result<PathBuf> {
    if p == "~" {
        return home_dir();
    }
    if let Some(rest) = p.strip_prefix("~/") {
        return Ok(home_dir()?.join(rest));
    }
    if p.starts_with('~') {
        bail!("only ~/ is supported, not ~user: {p}");
    }
    Ok(PathBuf::from(p))
}

/// Paths that must never be mountable, whether an entry contains them or lies inside them.
fn forbidden_roots(paths: &Paths) -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from("/var/run"), PathBuf::from("/run"), paths.root.clone()];
    if let Ok(h) = home_dir() {
        v.push(h.join(".ssh"));
    }
    if let Ok(canon) = std::fs::canonicalize(&paths.root) {
        v.push(canon);
    }
    v
}

fn check_allowlist_entry(paths: &Paths, entry: &str) -> Result<()> {
    let p = normalize(&expand_home(entry)?).with_context(|| format!("volumes.allow entry {entry:?} must be absolute"))?;
    if p == Path::new("/") {
        bail!("volumes.allow must not contain /");
    }
    let mut candidates = vec![p.clone()];
    if let Ok(c) = std::fs::canonicalize(&p) {
        candidates.push(c);
    }
    for f in forbidden_roots(paths) {
        for c in &candidates {
            if f.starts_with(c) || c.starts_with(&f) {
                bail!("volumes.allow entry {entry:?} overlaps {}, which may never be mounted", f.display());
            }
        }
    }
    Ok(())
}

/// Resolves a host volume path for a deploy. It must lie inside an allowlisted root,
/// both lexically and after following symlinks. The directory is created if missing.
pub fn resolve_host_volume(paths: &Paths, cfg: &ServerConfig, source: &str) -> Result<PathBuf> {
    let wanted = normalize(&expand_home(source)?).with_context(|| format!("host path {source:?} must be absolute"))?;
    let root = cfg
        .volumes
        .allow
        .iter()
        .filter_map(|e| normalize(&expand_home(e).ok()?))
        .find(|root| wanted.starts_with(root))
        .with_context(|| format!("host path {source} is not inside an entry of [volumes] allow in config.toml"))?;
    std::fs::create_dir_all(&wanted).with_context(|| format!("cannot create {}", wanted.display()))?;
    let real = std::fs::canonicalize(&wanted)?;
    let real_root = std::fs::canonicalize(&root)?;
    if !real.starts_with(&real_root) {
        bail!("host path {source} resolves to {}, outside {}", real.display(), real_root.display());
    }
    for f in forbidden_roots(paths) {
        if real.starts_with(&f) || f.starts_with(&real) {
            bail!("host path {source} overlaps {}, which may never be mounted", f.display());
        }
    }
    Ok(real)
}

/// The directory for a named volume, created if missing.
pub fn resolve_named_volume(paths: &Paths, service: &str, name: &str) -> Result<PathBuf> {
    let base = paths.volumes_dir().join(service);
    let dir = base.join(name);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let real = std::fs::canonicalize(&dir)?;
    if !real.starts_with(std::fs::canonicalize(&base)?) {
        bail!("volume directory {} is a symlink out of the service tree", dir.display());
    }
    Ok(real)
}

/// Writes a file through a temp file and rename, with the given mode.
pub fn write_atomic(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path.parent().context("path has no parent")?;
    let tmp = dir.join(format!(
        ".{}.tmp{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::process::id()
    ));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&tmp)
        .with_context(|| format!("cannot write {}", tmp.display()))?;
    f.write_all(data)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

/// Per-service secrets, `KEY = "value"`.
pub fn load_secrets(paths: &Paths, service: &str) -> Result<BTreeMap<String, String>> {
    let path = paths.secrets_file(service);
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("invalid {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

pub fn save_secrets(paths: &Paths, service: &str, secrets: &BTreeMap<String, String>) -> Result<()> {
    std::fs::create_dir_all(paths.secrets_dir())?;
    write_atomic(&paths.secrets_file(service), toml::to_string(secrets)?.as_bytes(), 0o600)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidrs() {
        for ok in ["203.0.113.7", "203.0.113.0/24", "2001:db8::/32", "::1"] {
            assert!(is_cidr(ok), "{ok}");
        }
        for bad in ["203.0.113.0/33", "2001:db8::/129", "example.com", "1.2.3.4/", "1.2.3.4/x", "\"]"] {
            assert!(!is_cidr(bad), "{bad}");
        }
    }

    #[test]
    fn emails() {
        assert!(is_plausible_email("ops@example.com"));
        assert!(is_plausible_email("a.b+paas@Mail.Example.org"));
        assert!(!is_plausible_email("example.com"));
        assert!(!is_plausible_email("a@localhost"));
        assert!(!is_plausible_email("a@b.com\"\nx"));
        assert!(!is_plausible_email("a b@c.com"));
    }

    #[test]
    fn normalize_paths() {
        assert_eq!(normalize(Path::new("/a/./b/../c")).unwrap(), PathBuf::from("/a/c"));
        assert!(normalize(Path::new("/..")).is_none());
        assert!(normalize(Path::new("a/b")).is_none());
        // Component-wise, so /root2 is not under /root.
        assert!(!Path::new("/root2").starts_with("/root"));
    }

    #[test]
    fn allowlist_rules() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { root: dir.path().join("_mypaas") };
        assert!(check_allowlist_entry(&paths, "/").is_err());
        assert!(check_allowlist_entry(&paths, "/var").is_err());
        assert!(check_allowlist_entry(&paths, "/var/run/docker.sock").is_err());
        assert!(check_allowlist_entry(&paths, dir.path().to_str().unwrap()).is_err());
        assert!(check_allowlist_entry(&paths, "relative").is_err());
        assert!(check_allowlist_entry(&paths, "/srv/media").is_ok());
    }

    #[test]
    fn host_volume_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { root: dir.path().join("_mypaas") };
        let allowed = dir.path().join("media");
        let outside = dir.path().join("media2");
        std::fs::create_dir_all(&outside).unwrap();
        let mut cfg = ServerConfig::default();
        cfg.volumes.allow = vec![allowed.to_str().unwrap().into()];
        assert!(resolve_host_volume(&paths, &cfg, allowed.join("a").to_str().unwrap()).is_ok());
        assert!(resolve_host_volume(&paths, &cfg, outside.to_str().unwrap()).is_err());
        let sneaky = format!("{}/../media2", allowed.display());
        assert!(resolve_host_volume(&paths, &cfg, &sneaky).is_err());
        std::os::unix::fs::symlink(&outside, allowed.join("link")).unwrap();
        assert!(resolve_host_volume(&paths, &cfg, allowed.join("link").to_str().unwrap()).is_err());
    }

    /// A named volume always stays inside the service's own tree, even if
    /// someone planted a symlink where its directory should be.
    #[test]
    fn named_volume_cannot_escape() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { root: dir.path().join("_mypaas") };
        let ok = resolve_named_volume(&paths, "shop", "data").unwrap();
        assert!(ok.starts_with(std::fs::canonicalize(paths.volumes_dir().join("shop")).unwrap()));
        let outside = dir.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, paths.volumes_dir().join("shop/evil")).unwrap();
        assert!(resolve_named_volume(&paths, "shop", "evil").is_err());
        // Another service's volume is a different tree.
        let other = resolve_named_volume(&paths, "blog", "data").unwrap();
        assert_ne!(ok, other);
    }

    #[test]
    fn default_config_roundtrip() {
        let cfg = ServerConfig::default();
        let text = toml::to_string(&cfg).unwrap();
        let back: ServerConfig = toml::from_str(&text).unwrap();
        back.limits().unwrap();
    }
}


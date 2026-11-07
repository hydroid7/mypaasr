//! Parsing and validation of the `# mypaas.*` comments in a Dockerfile.
//!
//! The parsed [`ServiceSpec`] is the complete configuration of a service. It is
//! stored next to each deploy so a rollback can recreate the containers without
//! the Dockerfile.

use std::path::{Component, Path};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const MAX_SCALE: u32 = 32;
/// Keeps Traefik rules short: a rule's default priority is its length.
pub const MAX_URL_PATH: usize = 128;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceSpec {
    pub service: String,
    pub urls: Vec<ServiceUrl>,
    pub port: u16,
    pub volumes: Vec<Volume>,
    pub env: Vec<EnvVar>,
    pub scale: u32,
    pub strategy: Strategy,
    pub healthcheck: Option<Healthcheck>,
    pub maxcpu: Option<f64>,
    pub maxmem: Option<u64>,
    pub networks: Vec<String>,
    pub readonly: bool,
    pub cap_add: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceUrl {
    pub https: bool,
    pub host: String,
    /// Empty, or a path starting with `/` and without a trailing `/`.
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VolumeSource {
    /// Maps to `~/_mypaas/volumes/<service>/<name>`.
    Named(String),
    /// An absolute host path; needs the `admin` scope and the server allowlist.
    Host(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Volume {
    pub source: VolumeSource,
    pub target: String,
    pub readonly: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvVar {
    pub key: String,
    /// `None` means the value comes from the service's secrets.
    pub value: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Strategy {
    Roll,
    Safe,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Healthcheck {
    pub path: String,
    pub interval_ms: u64,
    pub timeout_ms: u64,
}

impl ServiceUrl {
    pub fn to_url(&self) -> String {
        let scheme = if self.https { "https" } else { "http" };
        format!("{scheme}://{}{}", self.host, self.path)
    }

    /// The Traefik rule for this URL.
    pub fn rule(&self) -> String {
        if self.path.is_empty() {
            format!("Host(`{}`)", self.host)
        } else {
            format!("Host(`{}`) && PathPrefix(`{}`)", self.host, self.path)
        }
    }

    /// Router name derived from host and path, every other character a dash.
    pub fn router_name(&self) -> String {
        format!("{}{}", self.host, self.path)
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect()
    }
}

impl ServiceSpec {
    /// Features that need the `admin` scope, described for an error message.
    pub fn privileged_features(&self) -> Vec<String> {
        let mut out = Vec::new();
        for c in &self.cap_add {
            out.push(format!("cap_add {c}"));
        }
        for v in &self.volumes {
            if let VolumeSource::Host(p) = &v.source {
                out.push(format!("host volume {p}"));
            }
        }
        out
    }
}

/// Information about the Dockerfile besides the `mypaas.*` comments.
#[derive(Debug, Clone, Default)]
pub struct DockerfileInfo {
    /// Some `FROM` line uses `:latest` or no tag, so the build should `--pull`.
    pub pull: bool,
}

pub fn parse_dockerfile(text: &str) -> Result<(ServiceSpec, DockerfileInfo)> {
    Ok((parse_spec(text)?, dockerfile_info(text)))
}

/// Returns the `mypaas.` key and value if the line is an active config comment.
fn config_line(line: &str) -> Option<&str> {
    let rest = line.trim_start_matches([' ', '\t']).strip_prefix('#')?;
    rest.trim_start_matches([' ', '\t']).strip_prefix("mypaas.")
}

pub fn parse_spec(text: &str) -> Result<ServiceSpec> {
    let mut service = None;
    let mut urls = Vec::new();
    let mut port = None;
    let mut volumes = Vec::new();
    let mut env: Vec<EnvVar> = Vec::new();
    let mut scale = None;
    let mut healthcheck = None;
    let mut maxcpu = None;
    let mut maxmem = None;
    let mut networks: Vec<String> = Vec::new();
    let mut readonly = None;
    let mut cap_add: Vec<String> = Vec::new();

    for (lineno, line) in text.lines().enumerate() {
        let Some(item) = config_line(line) else { continue };
        let ctx = || format!("Dockerfile line {}: {}", lineno + 1, line.trim());
        let (key, value) = item
            .split_once('=')
            .with_context(|| format!("{}: expected mypaas.<key>=<value>", ctx()))?;
        let key = key.trim();
        let value = value.trim();
        let once = |slot: bool| -> Result<()> {
            if slot {
                bail!("{}: mypaas.{key} given more than once", ctx());
            }
            Ok(())
        };
        match key {
            "service" => {
                once(service.is_some())?;
                service = Some(parse_service_name(value).with_context(ctx)?);
            }
            "url" => urls.push(parse_url(value).with_context(ctx)?),
            "port" => {
                once(port.is_some())?;
                port = Some(parse_port(value).with_context(ctx)?);
            }
            "volume" => volumes.push(parse_volume(value).with_context(ctx)?),
            "env" => {
                let var = parse_env(value).with_context(ctx)?;
                if env.iter().any(|e| e.key == var.key) {
                    bail!("{}: env {} given more than once", ctx(), var.key);
                }
                env.push(var);
            }
            "scale" => {
                once(scale.is_some())?;
                scale = Some(parse_scale(value).with_context(ctx)?);
            }
            "healthcheck" => {
                once(healthcheck.is_some())?;
                healthcheck = Some(parse_healthcheck(value).with_context(ctx)?);
            }
            "maxcpu" => {
                once(maxcpu.is_some())?;
                maxcpu = Some(parse_cpu(value).with_context(ctx)?);
            }
            "maxmem" => {
                once(maxmem.is_some())?;
                maxmem = Some(parse_size(value).with_context(ctx)?);
            }
            "network" => {
                let name = parse_name(value, "network").with_context(ctx)?;
                if !networks.contains(&name) {
                    networks.push(name);
                }
            }
            "readonly" => {
                once(readonly.is_some())?;
                readonly = Some(match value {
                    "true" => true,
                    "false" => false,
                    _ => bail!("{}: readonly must be true or false", ctx()),
                });
            }
            "cap_add" => {
                let cap = parse_capability(value).with_context(ctx)?;
                if !cap_add.contains(&cap) {
                    cap_add.push(cap);
                }
            }
            _ => bail!("{}: unknown key mypaas.{key}", ctx()),
        }
    }

    let service = service.context("the Dockerfile has no `# mypaas.service=<name>` line")?;
    let (scale, strategy) = scale.unwrap_or((0, Strategy::Roll));
    let mut targets: Vec<&str> = volumes.iter().map(|v: &Volume| v.target.as_str()).collect();
    targets.sort();
    if targets.windows(2).any(|w| w[0] == w[1]) {
        bail!("two volumes are mounted on the same container path");
    }
    let mut rules: Vec<(bool, String)> = urls.iter().map(|u: &ServiceUrl| (u.https, u.rule())).collect();
    rules.sort();
    if rules.windows(2).any(|w| w[0] == w[1]) {
        bail!("the same url is given twice");
    }

    Ok(ServiceSpec {
        service,
        urls,
        port: port.unwrap_or(80),
        volumes,
        env,
        scale,
        strategy,
        healthcheck,
        maxcpu,
        maxmem,
        networks,
        readonly: readonly.unwrap_or(false),
        cap_add,
    })
}

fn dockerfile_info(text: &str) -> DockerfileInfo {
    let mut info = DockerfileInfo { pull: false };
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let Some(instr) = words.next() else { continue };
        if instr.eq_ignore_ascii_case("FROM") {
            let image = words.find(|w| !w.starts_with("--")).unwrap_or("");
            let name = image.rsplit('/').next().unwrap_or(image);
            if !image.contains('@') && (!name.contains(':') || name.ends_with(":latest")) && image != "scratch" {
                // Stage names like `FROM builder` also have no tag; pulling those
                // would fail, so only images with a registry-like shape count.
                if !is_stage_reference(text, image) {
                    info.pull = true;
                }
            }
        }
    }
    info
}

fn is_stage_reference(text: &str, image: &str) -> bool {
    text.lines().any(|line| {
        let words: Vec<&str> = line.split_whitespace().collect();
        words.len() >= 4
            && words[0].eq_ignore_ascii_case("FROM")
            && words[words.len() - 2].eq_ignore_ascii_case("AS")
            && words[words.len() - 1].eq_ignore_ascii_case(image)
    })
}

/// Service names end up in container, image, network and file names.
pub fn parse_service_name(s: &str) -> Result<String> {
    parse_name(s, "service name")
}

pub fn is_valid_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'_')
}

fn parse_name(s: &str, what: &str) -> Result<String> {
    if !is_valid_name(s) {
        bail!("invalid {what} {s:?}: use 1 to 63 lowercase letters, digits, '-' or '_', starting with a letter or digit");
    }
    Ok(s.to_string())
}

fn parse_url(s: &str) -> Result<ServiceUrl> {
    let (https, rest) = if let Some(r) = s.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = s.strip_prefix("http://") {
        (false, r)
    } else {
        bail!("url {s:?} must start with http:// or https://");
    };
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if !is_valid_host(host) {
        bail!("invalid host {host:?} in url");
    }
    let path = path.trim_end_matches('/');
    if path.len() > MAX_URL_PATH {
        bail!("the path in url {s:?} is longer than {MAX_URL_PATH} characters");
    }
    let path_ok = path
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || b"/-_.~".contains(&c))
        && !path.contains("//")
        && !path.split('/').any(|seg| seg == "." || seg == "..");
    if !path_ok {
        bail!("invalid path {path:?} in url: use letters, digits and - _ . ~ /");
    }
    Ok(ServiceUrl { https, host: host.to_string(), path: path.to_string() })
}

pub fn is_valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        })
}

fn parse_port(s: &str) -> Result<u16> {
    match s.parse::<u16>() {
        Ok(p) if p > 0 => Ok(p),
        _ => bail!("invalid port {s:?}"),
    }
}

fn parse_volume(s: &str) -> Result<Volume> {
    let mut parts: Vec<&str> = s.split(':').collect();
    let readonly = match parts.last() {
        Some(&"ro") if parts.len() == 3 => {
            parts.pop();
            true
        }
        Some(&"rw") if parts.len() == 3 => {
            parts.pop();
            false
        }
        _ => false,
    };
    let [source, target] = parts[..] else {
        bail!("volume {s:?}: expected source:/container/path[:ro]");
    };
    let source = if source.starts_with('/') || source.starts_with('~') {
        if !is_clean_absolute(source.trim_start_matches('~')) && source != "~" {
            bail!("volume {s:?}: host path must be normalized, without . or ..");
        }
        VolumeSource::Host(source.to_string())
    } else {
        VolumeSource::Named(parse_name(source, "volume name")?)
    };
    if !is_clean_absolute(target) || target == "/" {
        bail!("volume {s:?}: the container path must be absolute and normalized");
    }
    Ok(Volume { source, target: target.to_string(), readonly })
}

/// Absolute, no `.`/`..` components, no empty components, printable characters only.
fn is_clean_absolute(p: &str) -> bool {
    p.starts_with('/')
        && !p.contains("//")
        && (p == "/" || !p.ends_with('/'))
        && p.bytes().all(|c| c.is_ascii_graphic() && c != b',' && c != b':')
        && Path::new(p).components().all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
        && !p.split('/').any(|seg| seg == "." || seg == "..")
}

pub fn is_valid_env_name(key: &str) -> bool {
    let b = key.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && (b[0].is_ascii_alphabetic() || b[0] == b'_')
        && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

fn parse_env(s: &str) -> Result<EnvVar> {
    let (key, value) = match s.split_once('=') {
        Some((k, v)) => (k, Some(v.to_string())),
        None => (s, None),
    };
    if !is_valid_env_name(key) {
        bail!("invalid env name {key:?}");
    }
    if let Some(v) = &value
        && v.contains(['\0', '\r'])
    {
        bail!("env {key}: value contains control characters");
    }
    Ok(EnvVar { key: key.to_string(), value })
}

fn parse_scale(s: &str) -> Result<(u32, Strategy)> {
    let mut words = s.split_whitespace();
    let n: u32 = words
        .next()
        .and_then(|w| w.parse().ok())
        .with_context(|| format!("scale {s:?}: expected a number"))?;
    if n > MAX_SCALE {
        bail!("scale {n} is above the maximum of {MAX_SCALE}");
    }
    let strategy = match words.next() {
        None | Some("roll") => Strategy::Roll,
        Some("safe") => Strategy::Safe,
        Some(other) => bail!("scale: unknown strategy {other:?}, use roll or safe"),
    };
    if words.next().is_some() {
        bail!("scale {s:?}: expected N [roll|safe]");
    }
    Ok((n, strategy))
}

fn parse_healthcheck(s: &str) -> Result<Healthcheck> {
    let words: Vec<&str> = s.split_whitespace().collect();
    let [path, interval, timeout] = words[..] else {
        bail!("healthcheck {s:?}: expected /path interval timeout, for example /health 10s 2s");
    };
    let path_ok = path.starts_with('/')
        && path.bytes().all(|c| c.is_ascii_alphanumeric() || b"/-_.~?=&%".contains(&c));
    if !path_ok {
        bail!("healthcheck: invalid path {path:?}");
    }
    let interval_ms = parse_duration_ms(interval)?;
    let timeout_ms = parse_duration_ms(timeout)?;
    if interval_ms < 1000 || timeout_ms < 100 || timeout_ms > interval_ms {
        bail!("healthcheck: interval must be at least 1s and timeout between 100ms and the interval");
    }
    Ok(Healthcheck { path: path.to_string(), interval_ms, timeout_ms })
}

pub fn parse_duration_ms(s: &str) -> Result<u64> {
    let (num, mult) = if let Some(n) = s.strip_suffix("ms") {
        (n, 1)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1000)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60_000)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3_600_000)
    } else if let Some(n) = s.strip_suffix('d') {
        (n, 86_400_000)
    } else {
        bail!("invalid duration {s:?}, use for example 500ms, 10s, 5m");
    };
    let n: u64 = num.parse().with_context(|| format!("invalid duration {s:?}"))?;
    n.checked_mul(mult).with_context(|| format!("duration {s:?} is too large"))
}

fn parse_cpu(s: &str) -> Result<f64> {
    match s.parse::<f64>() {
        Ok(v) if (0.01..=1024.0).contains(&v) => Ok(v),
        _ => bail!("invalid maxcpu {s:?}, expected a number of CPUs like 0.5 or 2"),
    }
}

/// Sizes like `512m`, `1g`, `64k` or a plain number of bytes.
pub fn parse_size(s: &str) -> Result<u64> {
    let lower = s.to_ascii_lowercase();
    let lower = lower.strip_suffix('b').unwrap_or(&lower);
    let (num, mult) = match lower.chars().last() {
        Some('k') => (&lower[..lower.len() - 1], 1u64 << 10),
        Some('m') => (&lower[..lower.len() - 1], 1 << 20),
        Some('g') => (&lower[..lower.len() - 1], 1 << 30),
        _ => (lower, 1),
    };
    let n: u64 = num.parse().with_context(|| format!("invalid size {s:?}, use for example 512m or 1g"))?;
    match n.checked_mul(mult) {
        Some(v) if v > 0 => Ok(v),
        _ => bail!("invalid size {s:?}"),
    }
}

fn parse_capability(s: &str) -> Result<String> {
    let upper = s.to_ascii_uppercase();
    let cap = upper.strip_prefix("CAP_").unwrap_or(&upper);
    if cap == "ALL" {
        bail!("cap_add=ALL is not allowed, name the capabilities");
    }
    if cap.is_empty() || !cap.bytes().all(|c| c.is_ascii_uppercase() || c == b'_') {
        bail!("invalid capability {s:?}");
    }
    Ok(cap.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_and_defaults() {
        let spec = parse_spec("FROM python\n# mypaas.service=shop\n").unwrap();
        assert_eq!(spec.service, "shop");
        assert_eq!(spec.port, 80);
        assert_eq!(spec.scale, 0);
        assert_eq!(spec.strategy, Strategy::Roll);
        assert!(spec.urls.is_empty());
    }

    #[test]
    fn comment_detection() {
        assert_eq!(config_line("  #\tmypaas.port=1"), Some("port=1"));
        assert_eq!(config_line("#mypaas.port=1"), Some("port=1"));
        assert_eq!(config_line("#--- mypaas.url=x"), None);
        assert_eq!(config_line("RUN echo # mypaas.x"), None);
    }

    #[test]
    fn missing_service_and_unknown_key() {
        assert!(parse_spec("FROM x\n").is_err());
        let err = parse_spec("# mypaas.service=a\n# mypaas.colour=red\n").unwrap_err();
        assert!(format!("{err:#}").contains("unknown key"));
        assert!(parse_spec("# mypaas.service=a\n# mypaas.service=b\n").is_err());
    }

    #[test]
    fn full_spec() {
        let text = "\
# mypaas.service=shop
# mypaas.url=https://shop.example.com
# mypaas.url=http://example.com/shop/
# mypaas.port=8080
# mypaas.volume=data:/var/lib/app
# mypaas.volume=/srv/media:/media:ro
# mypaas.env=MODE=prod
# mypaas.env=DB_PASSWORD
# mypaas.scale=3 safe
# mypaas.healthcheck=/health 10s 2s
# mypaas.maxcpu=0.5
# mypaas.maxmem=256m
# mypaas.network=db
# mypaas.readonly=true
# mypaas.cap_add=net_bind_service
";
        let s = parse_spec(text).unwrap();
        assert_eq!(s.urls[1].path, "/shop");
        assert_eq!(s.urls[1].rule(), "Host(`example.com`) && PathPrefix(`/shop`)");
        assert_eq!(s.urls[0].router_name(), "shop-example-com");
        assert_eq!(s.volumes[1].source, VolumeSource::Host("/srv/media".into()));
        assert!(s.volumes[1].readonly);
        assert_eq!(s.env[1], EnvVar { key: "DB_PASSWORD".into(), value: None });
        assert_eq!((s.scale, s.strategy), (3, Strategy::Safe));
        assert_eq!(s.healthcheck.as_ref().unwrap().timeout_ms, 2000);
        assert_eq!(s.maxmem, Some(256 << 20));
        assert_eq!(s.cap_add, vec!["NET_BIND_SERVICE"]);
        assert_eq!(s.privileged_features().len(), 2);
    }

    #[test]
    fn url_paths_are_bounded() {
        assert!(parse_url(&format!("https://a.com/{}", "x".repeat(MAX_URL_PATH - 1))).is_ok());
        assert!(parse_url(&format!("https://a.com/{}", "x".repeat(MAX_URL_PATH))).is_err());
    }

    #[test]
    fn rejects_rule_injection() {
        assert!(parse_url("https://a.com`) || Host(`b.com").is_err());
        assert!(parse_url("https://a.com/x`)").is_err());
        assert!(parse_url("https://A.com").is_err());
        assert!(parse_url("https://a.com/../x").is_err());
        assert!(parse_url("ftp://a.com").is_err());
    }

    #[test]
    fn volume_paths() {
        assert!(parse_volume("data:/x").is_ok());
        assert!(parse_volume("Data:/x").is_err());
        assert!(parse_volume("data:x").is_err());
        assert!(parse_volume("data:/").is_err());
        assert!(parse_volume("/srv/../etc:/x").is_err());
        assert!(parse_volume("data:/a/../b").is_err());
        assert!(parse_volume("data:/x:rw:ro").is_err());
    }

    #[test]
    fn sizes_and_durations() {
        assert_eq!(parse_size("1g").unwrap(), 1 << 30);
        assert_eq!(parse_size("512M").unwrap(), 512 << 20);
        assert_eq!(parse_size("100").unwrap(), 100);
        assert!(parse_size("0").is_err());
        assert!(parse_size("lots").is_err());
        assert_eq!(parse_duration_ms("500ms").unwrap(), 500);
        assert_eq!(parse_duration_ms("2m").unwrap(), 120_000);
    }

    /// Every Dockerfile under `examples/` must parse, so the examples cannot
    /// drift from what mypaas accepts.
    #[test]
    fn examples_are_valid() {
        fn walk(dir: &Path, found: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, found);
                } else if p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("Dockerfile")) {
                    found.push(p);
                }
            }
        }
        let mut files = Vec::new();
        walk(&Path::new(env!("CARGO_MANIFEST_DIR")).join("examples"), &mut files);
        assert!(files.len() >= 10, "examples not found: {files:?}");
        let mut names = std::collections::HashSet::new();
        for f in &files {
            let (spec, _) = parse_dockerfile(&std::fs::read_to_string(f).unwrap())
                .unwrap_or_else(|e| panic!("{}: {e:#}", f.display()));
            assert!(names.insert(spec.service.clone()), "{}: service name {} used twice", f.display(), spec.service);
        }
    }

    #[test]
    fn dockerfile_pull() {
        assert!(!dockerfile_info("FROM python:3.12\nUSER app\n").pull);
        assert!(dockerfile_info("FROM python\n").pull);
        assert!(!dockerfile_info("FROM rust:1 AS build\nFROM build\n").pull);
        assert!(dockerfile_info("FROM --platform=linux/amd64 nginx:latest\n").pull);
        assert!(!dockerfile_info("FROM scratch\n").pull);
    }


}

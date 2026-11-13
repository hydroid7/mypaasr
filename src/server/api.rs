//! Handlers for the authenticated read and operate endpoints: services, status,
//! logs and secrets. Authentication and scopes are checked in `daemon` before these run.

use std::collections::{BTreeMap, HashMap};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::deploy::{self, DeployRecord, service_network, shared_network};
use super::docker::{ContainerInfo, Usage, container_stats, list_containers};
use super::lan;
use super::stats::StatsStore;
use crate::config::{Paths, load_secrets, save_secrets};
use crate::spec::{ServiceSpec, VolumeSource, is_valid_env_name};

pub const MAX_SECRET_VALUE: usize = 16 * 1024;
const MAX_LOG_TAIL: u64 = 10_000;
const LOG_BUFFER_LINES: usize = 1024;

/// Containers of a service without the `.old.` ones of a running deploy, sorted by name.
fn current_containers<'a>(all: &'a [ContainerInfo], service: &str) -> Vec<&'a ContainerInfo> {
    let mut v: Vec<_> = all.iter().filter(|c| c.service == service && !c.name.contains(".old.")).collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

fn service_names(paths: &Paths) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(paths.services_dir())
        .map(|entries| entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    names.sort();
    names
}

pub fn current_record(paths: &Paths, service: &str) -> Result<Option<DeployRecord>> {
    deploy::read_record(&paths.service_dir(service).join("current.json"))
}

/// `healthy`/`unhealthy` when a healthcheck is configured, else `running`;
/// `degraded` or `down` when replicas are missing.
async fn health(spec: &ServiceSpec, containers: &[&ContainerInfo]) -> &'static str {
    let wanted = spec.scale.max(1) as usize;
    let running: Vec<&&ContainerInfo> = containers.iter().filter(|c| c.running).collect();
    if running.is_empty() {
        return "down";
    }
    if running.len() < wanted {
        return "degraded";
    }
    match spec.healthcheck.as_ref().filter(|_| spec.scale > 0) {
        None => "running",
        Some(hc) => {
            let probes = running.iter().map(|c| deploy::probe(&c.name, spec, hc));
            if futures_util::future::join_all(probes).await.iter().all(|r| r.is_ok()) {
                "healthy"
            } else {
                "unhealthy"
            }
        }
    }
}

/// Announce state of a service's `.local` URLs, from the announcer's state file:
/// `{ "shop.local": "announced" }`. Empty when LAN mode is off or has not run,
/// or the service has no LAN URL, so the JSON stays as it was.
fn lan_states(spec: &ServiceSpec, states: Option<&lan::LanState>) -> BTreeMap<String, String> {
    let Some(states) = states else { return BTreeMap::new() };
    spec.urls
        .iter()
        .filter(|u| lan::is_lan_host(&u.host))
        .filter_map(|u| states.hosts.get(&u.host).map(|h| (u.host.clone(), h.label())))
        .collect()
}

/// Adds the `lan` field, and only when there is something in it.
fn with_lan(mut obj: Value, lan: BTreeMap<String, String>) -> Value {
    if !lan.is_empty() {
        obj["lan"] = json!(lan);
    }
    obj
}

fn sum_usage(containers: &[&ContainerInfo], usage: &HashMap<String, Usage>) -> Usage {
    containers.iter().filter_map(|c| usage.get(&c.name)).fold(Usage::default(), |a, u| Usage {
        cpu_percent: a.cpu_percent + u.cpu_percent,
        mem_bytes: a.mem_bytes + u.mem_bytes,
    })
}

pub async fn list_services(paths: &Paths, stats: &std::sync::Mutex<StatsStore>) -> Result<Value> {
    let all = list_containers(None).await?;
    let running: Vec<&str> = all.iter().filter(|c| c.running).map(|c| c.name.as_str()).collect();
    let usage = container_stats(&running).await.unwrap_or_else(|e| {
        tracing::warn!("docker stats failed: {e:#}");
        HashMap::new()
    });
    let mut rows = Vec::new();
    for name in service_names(paths) {
        let Some(rec) = current_record(paths, &name)? else { continue };
        rows.push((name, rec));
    }
    let checks = rows.iter().map(|(name, rec)| {
        let mine = current_containers(&all, name);
        async move { health(&rec.spec, &mine).await }
    });
    let healths = futures_util::future::join_all(checks).await;
    let now = crate::auth::unix_now();
    let lan_state = lan::read_state(paths);
    let traffic: Vec<u64> = {
        let stats = stats.lock().unwrap_or_else(|e| e.into_inner());
        rows.iter().map(|(name, _)| stats.requests_last_hour(name, now)).collect()
    };
    let services: Vec<Value> = rows
        .iter()
        .zip(healths)
        .zip(traffic)
        .map(|(((name, rec), health), requests_1h)| {
            let mine = current_containers(&all, name);
            let u = sum_usage(&mine, &usage);
            let obj = json!({
                "service": name,
                "running": mine.iter().filter(|c| c.running).count(),
                "wanted": rec.spec.scale.max(1),
                "health": health,
                "requests_1h": requests_1h,
                "cpu_percent": u.cpu_percent,
                "mem_bytes": u.mem_bytes,
                "urls": rec.spec.urls.iter().map(|u| u.to_url()).collect::<Vec<_>>(),
                "deploy_id": rec.deploy_id,
                "deployed_at": rec.time,
                "deployed_by": rec.key_name,
                "has_previous": paths.service_dir(name).join("previous.json").exists(),
            });
            with_lan(obj, lan_states(&rec.spec, lan_state.as_ref()))
        })
        .collect();
    Ok(json!({ "services": services }))
}

/// Records one sample of every deployed service: replicas running, memory and CPU.
pub async fn sample_usage(paths: &Paths, stats: &std::sync::Mutex<StatsStore>) -> Result<()> {
    let all = list_containers(None).await?;
    let running: Vec<&str> = all.iter().filter(|c| c.running).map(|c| c.name.as_str()).collect();
    let usage = container_stats(&running).await?;
    let now = crate::auth::unix_now();
    let mut rows = Vec::new();
    for name in service_names(paths) {
        let Some(rec) = current_record(paths, &name)? else { continue };
        let mine = current_containers(&all, &name);
        let u = sum_usage(&mine, &usage);
        let up = mine.iter().filter(|c| c.running).count() as u32;
        rows.push((name, up, rec.spec.scale.max(1), u));
    }
    let mut store = stats.lock().unwrap_or_else(|e| e.into_inner());
    for (name, up, wanted, u) in rows {
        store.record_usage(&name, now, up, wanted, u.mem_bytes, u.cpu_percent);
    }
    store.save(now)
}

/// Runs forever in the daemon, once a minute.
pub async fn sample_loop(paths: Paths, stats: std::sync::Arc<std::sync::Mutex<StatsStore>>) {
    loop {
        if let Err(e) = sample_usage(&paths, &stats).await {
            tracing::warn!("usage sample: {e:#}");
        }
        tokio::time::sleep(super::stats::SAMPLE_INTERVAL).await;
    }
}

#[derive(Serialize)]
struct VolumeView {
    source: String,
    host: String,
    target: String,
    readonly: bool,
}

pub async fn status(paths: &Paths, service: &str) -> Result<Option<Value>> {
    let Some(rec) = current_record(paths, service)? else { return Ok(None) };
    let all = list_containers(Some(service)).await?;
    let mine = current_containers(&all, service);
    let running: Vec<&str> = mine.iter().filter(|c| c.running).map(|c| c.name.as_str()).collect();
    let usage = container_stats(&running).await.unwrap_or_default();
    let spec = &rec.spec;
    let health = health(spec, &mine).await;
    let mut networks = vec![service_network(service)];
    networks.extend(spec.networks.iter().map(|n| shared_network(n)));
    let volumes: Vec<VolumeView> = spec
        .volumes
        .iter()
        .map(|v| {
            let (source, host) = match &v.source {
                VolumeSource::Named(n) => (n.clone(), paths.volumes_dir().join(service).join(n).display().to_string()),
                VolumeSource::Host(p) => (p.clone(), p.clone()),
            };
            VolumeView { source, host, target: v.target.clone(), readonly: v.readonly }
        })
        .collect();
    let containers: Vec<Value> = mine
        .iter()
        .map(|c| {
            let u = usage.get(&c.name).copied().unwrap_or_default();
            json!({
                "name": c.name,
                "state": c.state,
                "status": c.status,
                "cpu_percent": u.cpu_percent,
                "mem_bytes": u.mem_bytes,
            })
        })
        .collect();
    let previous = deploy::read_record(&paths.service_dir(service).join("previous.json"))?;
    let lan = lan_states(spec, lan::read_state(paths).as_ref());
    Ok(Some(with_lan(json!({
        "service": service,
        "urls": spec.urls.iter().map(|u| u.to_url()).collect::<Vec<_>>(),
        "port": spec.port,
        "scale": spec.scale,
        "strategy": spec.strategy,
        "health": health,
        "running": running.len(),
        "wanted": spec.scale.max(1),
        "networks": networks,
        "volumes": volumes,
        "env": spec.env.iter().map(|e| if e.value.is_some() { e.key.clone() } else { format!("{} (secret)", e.key) }).collect::<Vec<_>>(),
        "containers": containers,
        "last_deploy": { "deploy_id": rec.deploy_id, "time": rec.time, "key_name": rec.key_name },
        "previous": previous.map(|p| json!({ "deploy_id": p.deploy_id, "time": p.time, "key_name": p.key_name })),
    }), lan)))
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct LogQuery {
    pub follow: bool,
    pub tail: Option<u64>,
    pub since_secs: Option<u64>,
    pub container: Option<u32>,
}

impl LogQuery {
    /// Parses `follow=1&tail=100&since=10m&container=2`. Unknown keys are errors.
    pub fn parse(query: &str) -> Result<LogQuery> {
        let mut q = LogQuery::default();
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            match k {
                "follow" => q.follow = matches!(v, "1" | "true" | ""),
                "tail" => {
                    let n: u64 = v.parse().context("tail must be a number")?;
                    q.tail = Some(n.min(MAX_LOG_TAIL));
                }
                "since" => q.since_secs = Some(crate::spec::parse_duration_ms(v)? / 1000),
                "container" => q.container = Some(v.parse().context("container must be a number")?),
                _ => bail!("unknown log option {k:?}"),
            }
        }
        Ok(q)
    }
}

/// Starts streaming logs as JSON lines `{"container", "time", "line"}`. The `docker logs`
/// processes stop as soon as the client goes away.
pub async fn logs(service: &str, q: LogQuery) -> Result<Option<mpsc::Receiver<String>>> {
    let all = list_containers(Some(service)).await?;
    let mut names: Vec<String> = current_containers(&all, service).iter().map(|c| c.name.clone()).collect();
    if let Some(n) = q.container {
        let wanted = if n == 0 { service.to_string() } else { format!("{service}.{n}") };
        names.retain(|c| *c == wanted || (n == 1 && c == service));
    }
    if names.is_empty() {
        return Ok(None);
    }
    let mut args: Vec<String> = vec!["logs".into(), "--timestamps".into()];
    if q.follow {
        args.push("--follow".into());
    }
    // Without a limit a long-running service would send its whole history.
    args.extend(["--tail".into(), q.tail.unwrap_or(if q.follow { 50 } else { 200 }).to_string()]);
    if let Some(s) = q.since_secs {
        args.extend(["--since".into(), format!("{s}s")]);
    }
    let (tx, rx) = mpsc::channel(LOG_BUFFER_LINES);
    if q.follow {
        for name in names {
            let (tx, args) = (tx.clone(), args.clone());
            tokio::spawn(async move { stream_container_logs(&name, &args, &tx).await });
        }
    } else {
        tokio::spawn(async move {
            for name in names {
                if !stream_container_logs(&name, &args, &tx).await {
                    break;
                }
            }
        });
    }
    Ok(Some(rx))
}

/// Returns false once the client has gone away.
async fn stream_container_logs(name: &str, args: &[String], tx: &mpsc::Sender<String>) -> bool {
    let child = Command::new("docker")
        .args(args)
        .arg(name)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(json!({ "container": name, "error": e.to_string() }).to_string()).await;
            return true;
        }
    };
    let mut out = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut err = BufReader::new(child.stderr.take().unwrap()).lines();
    let (mut out_done, mut err_done) = (false, false);
    while !(out_done && err_done) {
        let line = tokio::select! {
            l = out.next_line(), if !out_done => match l { Ok(Some(l)) => l, _ => { out_done = true; continue } },
            l = err.next_line(), if !err_done => match l { Ok(Some(l)) => l, _ => { err_done = true; continue } },
            _ = tx.closed() => return false,
        };
        let (time, text) = line.split_once(' ').unwrap_or(("", line.as_str()));
        let msg = json!({ "container": name, "time": time, "line": text }).to_string();
        if tx.send(msg).await.is_err() {
            return false;
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    true
}

pub fn secret_names(paths: &Paths, service: &str) -> Result<Vec<String>> {
    Ok(load_secrets(paths, service)?.into_keys().collect())
}

/// Checks a secret before it is stored; values reach containers through an env file.
pub fn validate_secret(key: &str, value: &[u8]) -> Result<String> {
    if !is_valid_env_name(key) {
        bail!("invalid secret name {key:?}");
    }
    if value.len() > MAX_SECRET_VALUE {
        bail!("secret values are limited to {MAX_SECRET_VALUE} bytes");
    }
    let value = std::str::from_utf8(value).context("secret values must be UTF-8")?;
    if value.contains(['\n', '\r', '\0']) {
        bail!("secret values cannot contain line breaks");
    }
    Ok(value.to_string())
}

pub fn set_secret(paths: &Paths, service: &str, key: &str, value: String) -> Result<()> {
    let mut secrets = load_secrets(paths, service)?;
    secrets.insert(key.to_string(), value);
    save_secrets(paths, service, &secrets)
}

/// Returns false if the secret did not exist.
pub fn remove_secret(paths: &Paths, service: &str, key: &str) -> Result<bool> {
    let mut secrets = load_secrets(paths, service)?;
    if secrets.remove(key).is_none() {
        return Ok(false);
    }
    if secrets.is_empty() {
        std::fs::remove_file(paths.secrets_file(service))?;
    } else {
        save_secrets(paths, service, &secrets)?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::lan::HostState;
    use std::os::unix::fs::PermissionsExt;

    fn spec_with_urls(urls: &[&str]) -> ServiceSpec {
        let mut text = String::from("FROM busybox\n# mypaas.service=shop\n# mypaas.port=8080\n");
        for u in urls {
            text.push_str(&format!("# mypaas.url={u}\n"));
        }
        crate::spec::parse_dockerfile(&text).unwrap().0
    }

    #[test]
    fn lan_state_is_reported_for_lan_urls_only() {
        let spec = spec_with_urls(&["http://shop.local", "https://shop.example.com"]);
        let mut states = lan::LanState::default();
        states.hosts.insert("shop.local".into(), HostState::Conflict);
        let got = lan_states(&spec, Some(&states));
        assert_eq!(got.len(), 1);
        assert_eq!(got["shop.local"], HostState::Conflict.label());
    }

    #[test]
    fn missing_state_or_unknown_host_adds_nothing() {
        let spec = spec_with_urls(&["http://shop.local"]);
        assert!(lan_states(&spec, None).is_empty());
        assert!(lan_states(&spec, Some(&lan::LanState::default())).is_empty());
        let public = spec_with_urls(&["https://shop.example.com"]);
        let mut states = lan::LanState::default();
        states.hosts.insert("shop.local".into(), HostState::Announced);
        assert!(lan_states(&public, Some(&states)).is_empty());
    }

    #[test]
    fn lan_field_is_omitted_when_empty() {
        let plain = with_lan(json!({ "service": "shop" }), BTreeMap::new());
        assert!(plain.get("lan").is_none());
        let mut m = BTreeMap::new();
        m.insert("shop.local".to_string(), "announced".to_string());
        assert_eq!(with_lan(json!({ "service": "shop" }), m)["lan"]["shop.local"], "announced");
    }

    #[test]
    fn log_query() {
        assert_eq!(LogQuery::parse("").unwrap(), LogQuery::default());
        let q = LogQuery::parse("follow=1&tail=999999&since=10m&container=2").unwrap();
        assert_eq!(q, LogQuery { follow: true, tail: Some(MAX_LOG_TAIL), since_secs: Some(600), container: Some(2) });
        assert!(LogQuery::parse("tail=x").is_err());
        assert!(LogQuery::parse("evil=1").is_err());
    }

    #[test]
    fn secrets_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { root: dir.path().to_path_buf() };
        assert!(secret_names(&paths, "a").unwrap().is_empty());
        set_secret(&paths, "a", "DB_PASSWORD", "p\"w'=x".into()).unwrap();
        set_secret(&paths, "a", "TOKEN", "t".into()).unwrap();
        set_secret(&paths, "b", "OTHER", "o".into()).unwrap();
        assert_eq!(secret_names(&paths, "a").unwrap(), vec!["DB_PASSWORD", "TOKEN"]);
        assert_eq!(load_secrets(&paths, "a").unwrap()["DB_PASSWORD"], "p\"w'=x");
        let mode = std::fs::metadata(paths.secrets_file("a")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(remove_secret(&paths, "a", "TOKEN").unwrap());
        assert!(!remove_secret(&paths, "a", "TOKEN").unwrap());
        assert!(remove_secret(&paths, "a", "DB_PASSWORD").unwrap());
        assert!(!paths.secrets_file("a").exists());
        assert_eq!(secret_names(&paths, "b").unwrap(), vec!["OTHER"]);
    }

    #[test]
    fn secret_validation() {
        assert!(validate_secret("OK_1", b"value").is_ok());
        assert!(validate_secret("1BAD", b"v").is_err());
        assert!(validate_secret("A", b"line\nbreak").is_err());
        assert!(validate_secret("A", &[0xff]).is_err());
        assert!(validate_secret("A", &vec![b'x'; MAX_SECRET_VALUE + 1]).is_err());
    }
}
